<!-- made by CasualArclamp -->
# DecDVB-T TX: design

A separate transmit app, `decdvbt-tx`, that takes a **video file** and sends it
as **DVB-T** through the **HackRF One**. Its own binary (`apps/decdvbt-tx`), not
a panel in `decdvbt-gui`, on a new library crate (`crates/decdvbt-tx`).

Status date: 2026-10-25. **The decisions in this document are final for the
builders.** If a number here proves wrong when measured, the builder says so in
§14 (Builder notes) and carries on with the nearest safe choice; he does not
redesign. The API skeleton in `crates/decdvbt-tx/src` compiles today
(`cargo build -p decdvbt-tx`, `cargo test -p decdvbt-tx` pass); the bodies marked
`todo!()` are the library builder's work.

Hard rules for everyone working on this (they come from the user):

1. **No test, example, demo or CI job ever opens the HackRF for transmit.** Only
   the user pressing TRANSMIT in the finished app does. Every test uses the `Null`
   or `IqFile` sinks. Every script sets `DECDVBT_TX_NO_RADIO=1` (the library then
   refuses to open the radio; see §6).
2. Never kill the user's processes (their `decdvbt-gui` may hold the HackRF) and
   never delete `%APPDATA%\DecDVB-T\prefs.txt`. The TX app has its own prefs file.
3. No `git commit`, `push` or history-changing command. Do not touch the other
   projects in `..\`. Do not touch `apps/decdvbt-gui`.
4. Never write the user's email address or location into any file (the repo is
   public).
5. CPU efficiency is the main requirement: measure, do not guess. Real time at
   the radio's sample clock must not underrun.
6. Style: match the surrounding code (comment density, naming); every file starts
   with `// made by CasualArclamp` (`<!-- made by CasualArclamp -->` in Markdown).
   Rust idioms are explained in comments, DSP basics are not.
7. Shell: multi-line Python heredocs through Bash fail. Write scripts with the
   Write tool into the scratchpad and run `python -I`. Build per binary:
   `cargo build -p decdvbt-tx`, `cargo build --release -p decdvbt-tx-app`.

---

## 1. What it does

```
 video file ──ffmpeg──> DVB-T-sized transport stream ──modulator──> IQ ──HackRF──> RF
 (mp4, mkv…)   (child     (CBR at the mode's rate,       (real time,   (cs8, USB,
               process)    PCR exact, null padded)        one core)     radio's clock)
```

Scope: DVB-T, 6/7/8 MHz, 2k/8k, non-hierarchical, any constellation, code rate and
guard. One service. Video file in; looped or played once. Not in scope: DVB-T2
(the T2 modulator exists but is 217 ms frame granular and needs 40 Mbit/s CBR; a
later project), hierarchical, MISO, live capture inputs, multiple services, EIT
schedules, subtitles, a CLI transmitter (the example in §11 is the headless test
tool and refuses the radio).

Package names: the library is `decdvbt-tx` (`crates/decdvbt-tx`, Rust name
`decdvbt_tx`); the app is the package `decdvbt-tx-app` (`apps/decdvbt-tx`) with the
binary `decdvbt-tx`, because two packages in one workspace cannot share a name.
`cargo run --release -p decdvbt-tx-app`.

## 2. Architecture

### 2.1 Threads and buffers

```
   ┌──────────── ffmpeg.exe (child process) ────────────┐
   │ decode ─ scale ─ encode ─ mux (-muxrate R, CBR)    │
   └──────────────────────┬─────────────────────────────┘
                          │ stdout: 4 MiB Windows pipe          stderr: -progress + warnings
                          ▼                                          ▼
                 [tx-ts-read]  resync on 0x47 x3,                [tx-ff-log]  parse key=value
                 whole packets, push_blocking                    count "dts < pcr", keep 20 lines
                          │
                          ▼
                 TsRing   1.5 s of programme (>= 4 MiB), blocking producer
                          │  pop 64 packets per lock
                          ▼
                 FeederTs (TsSource)  priming/lead-in nulls · PSI swap (NIT, TDT/TOT) ·
                          │           NULL STUFFING when empty (counted)
                          ▼
                 [tx-mod]  Modulator -> gain -> (resampler) -> cs8 pack -> preview tap
                          │  blocks of >= 128 KiB (whole OFDM symbols)
                          ▼
                 IqRing   40 blocks (~0.3 s of signal), blocking producer
                          │  try_pop, never blocks
                          ▼
                 radio thread  "hackrf-tx" (decdvbt-io) or "tx-virtual" (file/null)
                          │  3 USB transfers of 256 KiB in flight
                          ▼
                 HackRF One   (the sample clock is the only clock)
```

Four threads of our own plus the radio's: `tx-ts-read`, `tx-ff-log`, `tx-mod`, and
the sink thread. The GUI thread only calls `status()` and
`take_preview_rows()`; it never blocks on anything.

### 2.2 Who paces whom (the central rule)

**The radio's sample clock paces everything; no other clock exists.**

- The HackRF keeps 3 transfers in flight and its `write_sync` blocks until the
  oldest completes. The sink thread therefore asks for data exactly as fast as the
  radio plays it.
- The sink takes finished blocks from the **IqRing without ever blocking**. When
  the ring has room the modulator thread wakes and makes more, then blocks again
  when the ring is full. The modulator therefore runs at the radio's rate.
- The modulator pulls packets from the **TsRing without ever blocking**. The TS
  reader blocks when the ring is full, which fills ffmpeg's pipe, which blocks
  ffmpeg. The encoder runs at the radio's rate. **ffmpeg is never run with `-re`**
  (its wall-clock pacing would be a second clock and would drift).
- Everything is back-pressure; nothing needs a timer. A paused radio (a stalled
  USB host) stalls the whole chain without loss.

### 2.3 The rate policy for the TS FIFO

ffmpeg's output is constant rate by construction: `-muxrate R` with R =
`floor(bitrate_hp)` of the mode makes it emit a packet every 1504/R seconds in
stream time, null-padding what the video and audio do not use. Its **PCR is exact**
(measured: ±1 tick of 27 MHz, 0.0 µs worst deviation from ideal CBR position). The
radio drains at R·(1 ± its crystal error); R is rounded down, so ffmpeg is slower
than the radio by less than 1 bit/s plus the radio's few ppm:

| Situation | Policy |
|---|---|
| Ring full (normal: encoders are faster than real time) | The reader blocks; the pipe fills; ffmpeg stalls. Nothing lost. |
| Ring empty (encoder too slow, ffmpeg stalled, start-up) | `FeederTs` hands the modulator a **null packet** (PID 0x1FFF) and counts it in `stuffed`. The channel stays gapless at exactly its rate; the receiver sees the programme pause. Shown in the GUI in amber. |
| Radio a few ppm faster than ffmpeg's `R` | The ring drains at ~60 B/s (24 Mbit/s at 20 ppm): half a second of prefill lasts hours. Then stuffing starts, once per many seconds. Not a defect. |
| End of file, no loop | ffmpeg exits 0, the reader closes the ring, nulls follow the last packet; `Finishing`, then `Finished` once the IQ ring has drained, and the radio is stopped. |
| ffmpeg exits non-zero or dies | `Failed`; the radio is stopped. The session never transmits nulls indefinitely on its own. |
| Radio USB error | `Failed`; everything stops. |

**Prefill before the radio starts**: `Ready` is entered only when the TS ring holds
`TS_PREFILL_SECONDS` = 0.5 s of programme and the IQ ring is full (0.3 s of signal),
so TRANSMIT starts with a cushion in both rings. Start-up costs 0.3 to 0.9 s of
ffmpeg latency (measured: 0.3 s on the GPU chain, 0.9 s on x264 with a 4K HEVC
source). `Preparing` shows this as a progress bar.

**Warm-up and lead-in**: the modulator's first `warmup_symbols()` symbols are a
pulse (17x rms) because the byte interleaver is empty; they are generated and
**discarded** while `FeederTs` is `priming` (it hands out nulls without touching the
ring, so no programme is lost to them). Then `lead_in_ms` = 600 ms of nulls (a
valid, locked-on signal with PAT/PMT absent), so a receiver locks before the
programme starts (the receiver's own lock time otherwise eats about 2000 packets).

**Rate mismatch with the radio's clock** (HackRF crystal ±20 ppm typical): PCR
drifts by the same ppm against real time. Receivers tolerate ±30 ppm in PCR. Not
corrected; the session reports the measured radio rate (`bytes_out` over wall
time) next to the nominal in the GUI's details.

**Mode changes** restart ffmpeg (the mux rate changes): the GUI disables mode,
guard, bandwidth, rate and file controls while a session exists, and a change is
`stop()` then `prepare()`. Only the **TX VGA** changes live.

### 2.4 Failure behaviour is silence

If any thread stalls or dies, the radio gets zeros (no carrier, no signal) after the
ring runs dry. The driver pads zeros itself. A panic anywhere unwinds and drops
`HackRfTx`, which stops the radio (`panic = "abort"` is **not** to be set for these
crates). The GUI polls `status().state` each frame and shows `Failed` with the
message.

## 3. Modes and bitrates

The useful TS rate is `data carriers x bits x code rate x 188/204 / Ts`
(EN 300 744 Table 17), the same for 2k and 8k at the same guard. 8 MHz, Mbit/s;
6 MHz x 6/8, 7 MHz x 7/8. `mux_rate_bps(mode)` is the floor of the exact figure
(64-QAM 2/3 1/32: 4512000000/187 = 24 128 342.246 -> 24 128 342).

| Constellation, rate | guard 1/4 | 1/8 | 1/16 | 1/32 |
|---|---|---|---|---|
| QPSK 1/2 | 4.98 | 5.53 | 5.85 | 6.03 |
| QPSK 2/3 | 6.64 | 7.37 | 7.81 | 8.04 |
| QPSK 3/4 | 7.46 | 8.29 | 8.78 | 9.05 |
| QPSK 5/6 | 8.29 | 9.22 | 9.76 | 10.05 |
| QPSK 7/8 | 8.71 | 9.68 | 10.25 | 10.56 |
| 16-QAM 1/2 | 9.95 | 11.06 | 11.71 | 12.06 |
| 16-QAM 2/3 | 13.27 | 14.75 | 15.61 | 16.09 |
| 16-QAM 3/4 | 14.93 | 16.59 | 17.56 | 18.10 |
| 16-QAM 5/6 | 16.59 | 18.43 | 19.52 | 20.11 |
| 16-QAM 7/8 | 17.42 | 19.35 | 20.49 | 21.11 |
| 64-QAM 1/2 | 14.93 | 16.59 | 17.56 | 18.10 |
| 64-QAM 2/3 | 19.91 | 22.12 | 23.42 | **24.13** |
| 64-QAM 3/4 | 22.39 | 24.88 | 26.35 | 27.14 |
| 64-QAM 5/6 | 24.88 | 27.65 | 29.27 | 30.16 |
| 64-QAM 7/8 | 26.13 | 29.03 | 30.74 | 31.67 |

Defaults and presets in the GUI:

| Preset | Mode | Mux rate | Picture | Use |
|---|---|---|---|---|
| **UK (default)** | 64-QAM 2/3, 8k, 1/32, 8 MHz | 24 128 342 | 1080p25 | the best picture; needs a clean link (C/N >= 18 dB) |
| Robust | 16-QAM 1/2, 8k, 1/32 | 12 064 171 | 720p25 | marginal link |
| Very robust | QPSK 1/2, 8k, 1/8 | 5 529 411 | 576p25 (SD) | the ~5 Mbit/s target; weak link |

The mode list shows every combination (`modes::mode_table`) with its bitrate and
the C/N it needs for QEF (`Mode::qef_cn_db`). Modes whose TS rate is above 31.67
Mbit/s do not exist. The modulator refuses hierarchical modes.

**Video profile** (`modes::VideoProfile::for_rate`, already implemented and tested):
under 9.5 Mbit/s 720x576 with 96 kbit/s audio; under 17 Mbit/s 1280x720 with 128k;
else 1920x1080 with 192k. Video `-b:v` = 0.85 R less the audio, `-maxrate` = 0.91 R
less the audio, `-bufsize` = maxrate/2 (kbit/s, rounded down to 10). The figures
held (no `dts < pcr` warnings, about 10 % nulls left) on 4K60 drone footage with
both NVENC and x264. The audio is subtracted for margin: the scout's runs used
0.85 R without subtracting it and also held; the library builder verifies with the
`dts_pcr_warnings` counter on hard content (§11, T2) and tightens if it is not zero.

## 4. ffmpeg

### 4.1 Finding it

`ffmpeg::find_ffmpeg` copies `decdvbt_video::ffmpeg::find` (configured path, beside
the exe, PATH, `%LOCALAPPDATA%\Microsoft\WinGet\Links`, the
`Packages\Gyan.FFmpeg*\*\bin` folders, Program Files, `C:\ffmpeg\bin`); it is
copied because that crate depends on the audio stack. `find_ffprobe` is
`ffprobe(.exe)` in the same folder. Missing ffmpeg: the GUI shows the message and
a "browse for ffmpeg.exe" button; missing ffprobe: the file info panel says
"(no ffprobe: info unavailable)" and transmission still works.

Process flags (reuse `decdvbt-video/src/lib.rs`): `CREATE_NO_WINDOW`, **normal**
priority (a below-normal ffmpeg starved), a 4 MiB `CreatePipe` for stdout (the
default Windows pipe holds 4 KB), `-nostdin`.

### 4.2 Probing the file

`ffmpeg::probe` runs `ffprobe -v error -show_entries
stream=codec_type,codec_name,width,height,r_frame_rate,field_order,channels,bits_per_raw_sample,pix_fmt:format=duration,bit_rate
-of compact=p=0 FILE` and reads the `|`-separated lines (no JSON crate). The result
is `VideoInfo` (shown in the GUI: size, fps, codecs, duration, interlaced, bit
depth).

### 4.3 Choosing the chain

`ffmpeg::detect_chain` runs a 0.2 s encode of a `color` source through each
candidate and keeps the first that works (NVENC 0.67 s, x264 0.15 s; cached in a
`OnceLock`). The encoder list alone is not enough: `h264_qsv` is compiled in here
but fails at run time.

| Chain | When | CPU per second of stream, A / B / C (measured) |
|---|---|---|
| **NVENC + CUDA** (`Chain::NvencCuda`) | `EncoderChoice::Auto` and NVENC works, source is progressive and 8-bit and H.264/HEVC/etc. CUDA can decode | 0.11 / 0.09 / 0.07 core |
| **x264 veryfast** (`Chain::X264`) | NVENC absent or failed, interlaced or 10-bit source, or `EncoderChoice::X264` | 1.23 / 0.76 / 0.51 core |
| MPEG-2 (`Chain::Mpeg2`) | `VideoCodec::Mpeg2` | 0.37 core at 720p |

Decode threading: **never leave ffmpeg's decoder threads at the default** (auto on
16 threads spent 6.3 s CPU on 25 s of 1080p30 against 3.4 s per 30 s on
`-threads 1`). On the x264 chain, a source taller than 1080 lines or faster than 30
fps adds `-hwaccel auto` (frames copied back; measured 2.3 to 2.7 cores on 4K60,
the only setup that did not underrun) and a warning in the GUI that this source is
heavy.

### 4.4 Command lines (the contract; `ffmpeg::command_line` builds these)

Common to all (`$R` = `mux_rate_bps`, sizes and kbit/s from `VideoProfile`):

```
-hide_banner -nostdin -loglevel warning -progress pipe:2 -nostats
```

GPU chain, B (16-QAM 1/2 1/32, 720p) as measured; `scale_cuda` keeps the aspect
ratio (`force_original_aspect_ratio=decrease`), so the frame size varies, which DVB
receivers accept:

```
ffmpeg -hide_banner -nostdin -loglevel warning -progress pipe:2 -nostats
  -threads 1 -hwaccel cuda -hwaccel_output_format cuda [-stream_loop -1] -i IN
  -map 0:v:0 -map 0:a:0?
  -vf "scale_cuda=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,fps=25"
  -c:v h264_nvenc -preset p4 -tune hq -profile:v high -rc cbr
  -b:v 10250k -maxrate 10970k -bufsize 5500k -g 25 -bf 2
  -c:a aac -b:a 128k -ar 48000 -ac 2 -af aresample=async=1:first_pts=0
  -muxdelay 0.4 -f mpegts -muxrate 12064171
  -mpegts_flags +system_b+nit+latm -pcr_period 20
  -mpegts_service_id 1 -mpegts_original_network_id 65281 -mpegts_transport_stream_id 1
  -mpegts_service_type advanced_codec_digital_hdtv
  -metadata service_provider=DecDVB-T -metadata service_name=NAME pipe:1
```

(The `-b:v/-maxrate/-bufsize` figures shown are the scout's run; the builder uses
`VideoProfile`'s.) For A: `scale_cuda=1920:1080`, `-b:a 192k`. For C:
`-vf "scale_cuda=720:576,setsar=64/45,fps=25"`, `-b:a 96k`, service type
`advanced_codec_digital_sdtv`.

Software chain (any machine): replace the pre-input and video part with

```
-threads 1 -filter_threads 1 [-stream_loop -1] -i IN
-vf "[yadif=deint=interlaced,]scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,fps=25,format=yuv420p"
-c:v libx264 -threads 4 -preset veryfast -profile:v high
-b:v ... -maxrate ... -bufsize ... -g 25 -keyint_min 25 -sc_threshold 0 -bf 2
```

(`yadif` only if `VideoInfo.interlaced`.) Everything from `-c:a` on is unchanged.

MPEG-2 + MP2 variant (old receivers): `-c:v mpeg2video -g 12 -bf 2 -b:v ... -maxrate
... -bufsize ... -c:a mp2 -b:a 192k`, service type `digital_tv` (SD) or
`mpeg2_digital_hdtv`, no `+latm`.

Audio flag by `AudioCodec`: `AacLatm` adds `+latm` to `-mpegts_flags` (PMT stream
type 0x11, decodes cleanly in ffmpeg; the UK convention); `AacAdts` omits it (stream
type 0x0F); `Mp2` uses `-c:a mp2`. **Unverified on a hardware receiver**: AAC vs
MP2, `-muxdelay 0.4` (the PTS leads the PCR by 2 x muxdelay + 0.08 s = 0.88 s; the
default 1.48 s is too large for TV decoder buffers; 0.2 gave one warning),
1080p25 H.264 over DVB-T. ffmpeg writes no DVB AAC descriptor (0x7C); adding it by
rewriting the PMT is a later job, not v1.

Invariants every command line must satisfy (unit tests in `ffmpeg.rs`):
`-muxrate` equals `mux_rate_bps`; `-threads 1` appears before `-i`; `-maxrate` and
`-bufsize` are always present; `-pcr_period 20`; `-muxdelay 0.4`;
`-mpegts_flags` includes `+system_b` and `+nit` only if `muxer_has_nit` (probe
`ffmpeg -h muxer=mpegts` for "nit"; an older ffmpeg may not know it); `-stream_loop
-1` before `-i` iff `loop_input`; no `-re`; the output is `pipe:1`; no
`+resend_headers` (it would add 75 kbit/s of PAT/PMT per frame; the default repeat
is 100.6 ms PAT/PMT, 500 ms SDT/NIT).

Looping is **inside ffmpeg** (`-stream_loop -1` before `-i`): counters and PCR stay
continuous and video DTS is gapless (audio has a gap of about 130 ms per loop when
the clip's audio is shorter than its video). Looping the `.ts` bytes (what
`FileTs` does) gives continuity errors and a PCR step; it is not used.

**Over-budget is silent**: if video + audio + overhead exceeds `-muxrate` ffmpeg
stays CBR but delays the data; the only sign is `[mpegts] dts < pcr, TS is invalid`
on stderr. The stderr parser counts it into `dts_pcr_warnings`; the GUI shows the
count in red and a hint ("the picture is too complex for this mode: lower the
bitrate or pick a faster mode").

### 4.5 Service identity and SI

Defaults use the private-use id ranges so nothing can be mistaken for a real
broadcaster's mux: original_network_id = network_id = 0xFF01, transport_stream_id 1,
service_id 1, provider "DecDVB-T", service name = the file's name stem (sanitised
to ASCII, 24 characters), network name "DecDVB-T". ffmpeg writes PAT, PMT (PMT PID
0x1000, video 0x100, audio 0x101, PCR on the video PID), SDT and, with `+nit`, a NIT
without delivery information. `si::SiInjector` (library builder, milestone 3)
swaps packets **in place** (no position changes, so CBR and PCR are untouched):

- PID 0x10 packet -> our NIT (one packet; ffmpeg's continuity counter kept): network
  name, one transport stream loop, service list (0x41), and the
  terrestrial_delivery_system_descriptor (0x5A: centre in 10 Hz units, bandwidth,
  constellation, hierarchy, code rates, guard, transmission mode) so a receiver
  scan learns the frequency and mode;
- null packets, when due: TDT every 1 s and TOT every 1 s (PID 0x14, own continuity
  counter, UTC from the PC clock).

Section builders are our own: `decdvbt_ts::psi::section` writes
`last_section_number = max(1)`, which is 1 for a single-section table (the spec
wants 0); `decdvbt-mod/src/ts.rs::long_section` is correct and is the model.
CRC-32 is `decdvbt_core::crc::crc32_mpeg2`. No EIT in v1.

## 5. The IQ chain

### 5.1 Rate

Default **native rate**: 64/7 MS/s for 8 MHz (48/7 for 6 MHz, 8 MS/s for 7 MHz),
passed to the radio as whole hertz (9 142 857; 0.016 ppm off, i.e. nothing;
`sample_rate_div` stays 1). No resampler, so none of its 18 to 25 % of a core.
`TxRate::Fixed(10_000_000 | 20_000_000)` (35/32 and 35/16, exact ratios) is offered
under "Advanced: radio sample rate" for users who want the DAC images further from
the channel; it uses `decdvbt_dsp::Resampler::for_rates(64e6/7, out, 7.61e6)`. The
choice is measured once the resampler is rewritten (§10); native stays the default
unless the images measure as a real problem on the user's hardware (a spectrum
analyser or SDR check that only the user can do; the GUI shows both and says so).

### 5.2 Level

Fixed gain from the modulator's known mean power (1.08 for DVB-T), not a probe
pass: `gain = 10^(level/20) / sqrt(mean_power())`, level default **-12 dBFS rms**
(range -30 to -6). Measured over 25 M samples of 64-QAM 2/3: -12 dBFS gives 2
clipped samples (1e-5 %) and 37.9 dB quantisation SNR; -10 gives 456 clipped
(0.0018 %) and 39.9 dB; -8 gives 0.083 % clipped; the `modulate` command's default
-20 loses 8 to 10 dB for nothing. The radio's power is then set by the TX VGA, not
by this level.

cs8 packing is the `IqFileWriter` conversion (`round(x*128)` saturating to i8, I then
Q), `ring::pack_cs8`, which also counts clipped samples.

### 5.3 The radio's baseband filter

`seify-hackrfone`'s `set_sample_rate` sets the filter to at most 75 % of the rate:
by the libhackrf table about 6 MHz at 9.14 MS/s, narrower than the 7.61 MHz signal
(the RX notes in STATUS.md record the same band-edge loss). `HackRfTxConfig` carries
`baseband_filter_hz` (default **8 MHz** for 8 MHz channels, scaled by bandwidth) and
the library builder adds the matching `baseband_filter_hz: Option<u32>` to
`decdvbt_io::HackRfTxSettings`, applied through the driver's
`set_baseband_filter_bandwidth` after the sample rate is set (§9). The filter table
is from memory and **not verified on hardware**: the builder checks the driver's
allowed values at the call and picks the smallest not below `bandwidth_hz` x 1.0
(8 MHz in the table), falling back to automatic with a log line if the call fails.

Not handled in v1, noted in the GUI's details: the HackRF leaks a carrier at its
LO and DVB-T has a data carrier at DC; the user checks the spectrum.

## 6. Safety model

**The app is a transmitter; the design makes transmitting hard to do by accident
and easy to stop.**

1. **Disarmed by default and on every start-up.** `Arm::new()` is disarmed. The arm
   state is never written to prefs, never read from the command line or the
   environment, and `TxConfig::new` uses the `Null` sink, never the radio.
2. **ARM, then TRANSMIT: two deliberate actions.** `TxSession::arm(LicenceAck)`
   needs a `LicenceAck`, and `LicenceAck::accept(text)` returns one only if `text`
   is exactly `arm::LICENCE_WARNING`, so the GUI must display the library's text and
   a checkbox "I have read this" gates the ARM button. The arm lasts 60 s
   (`ARM_TIMEOUT`), and **one `transmit()` consumes it** (success or failure). The
   TRANSMIT button is enabled only while armed **and** the session is `Ready`.
3. **The compiler enforces the gate.** `radio::open_hackrf(ArmedProof, ...)` is the
   only code that calls the HackRF start; `ArmedProof` has a private field and only
   `Arm::take` makes one (not `Clone`, not `Copy`, one shot). A unit test
   (`arm::tests::only_radio_rs_opens_the_hackrf`) reads every other source file and
   fails if it contains the start call or makes a proof.
4. **Hard refusal switch.** If the environment variable `DECDVBT_TX_NO_RADIO` is
   set, or the code is a test build, `open_hackrf` returns an error before touching
   USB. Every test script, CI job and benchmark sets it.
5. **Start gently.** TX VGA 0 dB, RF amp off, antenna power (bias tee) **never**
   (`antenna_enable: false`, in `hackrf_tx.rs`). TX VGA and amp are not persisted
   and the app always starts at 0 / off. The VGA slider is the only live control; it
   moves in steps (a single drag cannot jump more than 6 dB per frame) and the amp
   checkbox is only editable while stopped.
6. **Visible state.** While `status().radiating` the whole window gets a red frame
   and a banner "TRANSMITTING - 546.000 MHz - TX VGA 0 dB - amp off", the title bar
   reads "TRANSMITTING", the STOP button is the largest control and is bound to
   Escape and Space, and the taskbar title carries the same. `radiating` is true
   only for `Transmitting` with a `HackRf` sink. The null and file sinks show a
   neutral "Test output - no radio".
7. **Drop = off.** `stop()`, `Drop` of the session, window close (`on_exit`), an
   error, `Failed`, `Finished` and a panic (unwinding) all drop `HackRfTx`, whose
   `Drop` joins the thread and calls `dev.stop()` (radio mode off). Disarm happens
   on every one of those. Closing the window while transmitting stops it first.
8. **No auto-transmit.** Opening a file, finishing `prepare()`, a mode change, the
   arm timeout, window focus returning, command-line arguments, and prefs never
   start a transmission. Arm is cleared when the window loses focus for more than 5
   s, on any error, and on Stop.
9. **Licence and attenuator warning**, `arm::LICENCE_WARNING`, shown next to the
   ARM control at all times (not a dismissable popup):

   > Transmitting radio signals needs a licence for the frequency and power used.
   > Connect the HackRF to a receiver through a cable and an attenuator (or to a
   > dummy load) so nothing is radiated, unless you hold a licence for this
   > frequency, power and antenna. DVB-T channels are occupied by broadcasters: a
   > signal on an aerial will interfere with their viewers. You are responsible
   > for what this transmits.

   If the centre frequency is inside the UK Freeview channels (C21 to C48) or any
   UHF TV channel (C21 to C69) the app adds a red line: "This is a television
   channel."
10. **One user of the HackRF.** `HackRf::open_first` fails if the receive GUI holds
    the radio. The error is shown verbatim with "Close DecDVB-T (or whatever holds
    the HackRF) and try again." The TX app never touches another process.
11. `set_txvga`/`set_amp` in `hackrf_tx.rs` are not guarded the way the RX
    controls are; `set_txvga_gain` can panic on a zero reply byte. The library
    builder wraps them in `catch_unwind` (§9) so a bad USB reply is an error, not a
    crash with the radio keyed.

## 7. The GUI (`apps/decdvbt-tx`)

eframe/egui 0.36.2 with the glow renderer, same features and look as `decdvbt-gui`:
dark theme, monospace body text, `AMBER`/`RED`/`GREEN`. `rust-version = "1.95"`.
Version string `decdvbt-tx X.Y.Z` for `--version`. Windows build: release exe has no
console window (`windows_subsystem = "windows"`) and attaches to the parent
console for `--version`; its own `build.rs`, `assets/icon.ico` and
`assets/icon-256.png` (a different icon from the receiver: same style, red accent;
an `examples/make_icon.rs` redraws it) and ProductName "DecDVB-T TX",
FileDescription "DecDVB-T DVB-T transmitter".

Window (min 900 x 600), top to bottom:

```
┌ DecDVB-T TX ───────────────────────────────────────────────────────────────────────┐
│ [ state strip:  IDLE | PREPARING 60% | READY | TRANSMITTING | FINISHED | FAILED ]    │
├───────────────────────────────┬────────────────────────────────────────────────────┤
│ SOURCE                        │ SIGNAL (what is being sent)                         │
│  [Browse…]  clip.mp4          │   spectrum trace (dBFS) over waterfall,             │
│  (drop a file anywhere)       │   span = radio sample rate, centre = tuned freq,    │
│  1920x1080 25p H.264 + AAC    │   gridlines every 1 MHz, the 7.61 MHz occupied      │
│  duration 0:03:12  [x] Loop   │   band shaded; rms / peak / clipped readout         │
│                               │                                                    │
│ MODE                          ├────────────────────────────────────────────────────┤
│  Preset [UK 64-QAM 2/3 ▾]     │ METERS                                              │
│  Constellation  Rate  Guard   │  TS bitrate   24.13 Mbit/s (ffmpeg 24.13 in)        │
│  FFT 2k/8k  Bandwidth         │  TS buffer    ▓▓▓▓▓▓▓▓░░  (ffmpeg backs off if full)│
│  → 24.13 Mbit/s   needs 18.4  │  IQ buffer    ▓▓▓▓▓▓▓▓▓▓                            │
│     dB C/N   picture 1080p25  │  Stuffing     0 packets      Underruns   0          │
│                               │  Modulator    ▓▓▓░░░░░░░ 36 %  (of one core)        │
│ CHANNEL                       │  ffmpeg       ▓░░░░░░░░░ 11 %   speed 1.5x          │
│  [ 0 0 0 5 4 6 . 0 0 0 0 ] Hz │  This app     ▓▓░░░░░░░░ 41 %   Sent 0:02:41        │
│  UHF channel [C30 ▾]          │  Stream over budget (dts<pcr): 0                    │
│                               │                                                    │
│ RADIO                         │                                                    │
│  TX VGA  ─o───────  0 dB      │                                                    │
│  [ ] RF amp (+14 dB)          │                                                    │
│  Level -12 dBFS (Advanced…)   │                                                    │
├───────────────────────────────┴────────────────────────────────────────────────────┤
│ SAFETY   (licence text, always visible)        [x] I have read this                 │
│   [ ARM ]  armed for 47 s        [ TRANSMIT ]        [ STOP ]       Output: HackRF ▾│
└────────────────────────────────────────────────────────────────────────────────────┘
```

Behaviour:

- **File**: a Browse button (`rfd`) and a drop target (`with_drag_and_drop(true)`
  on the viewport; the whole window accepts a drop). Choosing a file starts a
  probe (a thread; never blocks the UI) and shows `VideoInfo`. Choosing a file
  does **not** start anything else.
- **Prepare**: a **Prepare** step is implicit: the main action row reads `[Prepare]`
  when idle (starts ffmpeg and the modulator; the radio stays off), then shows the
  progress bar in the state strip, then `READY`. ARM becomes available at `Ready`.
- **Output** selector (bottom right): `HackRF (radio)`, `IQ file…` (a file name
  box and seconds limit; paced or flat out), `Test only (no output)`. Default on
  every start: **Test only**. The choice of `HackRF` is not persisted.
- **Mode**: preset combo plus the explicit controls; changing any of them while a
  session exists is greyed out with the tooltip "Stop first". The line below shows
  the mux rate, the C/N needed (`qef_cn_db`) and the picture size/bitrates the
  profile will use.
- **Channel**: the 10-digit frequency dial (copy `freq_display.rs` from the
  receiver GUI, which is self-contained; it has unit tests) and a channel combo
  (`decdvbt_core::channel::{uhf_centre_hz, parse_channel}`, C21 to C69). Range
  1 MHz to 6 GHz. The red "This is a television channel." line as in §6.9.
- **Signal view**: spectrum and waterfall from `take_preview_rows()` (copy
  `waterfall.rs`; the 1024-bin rows are 20 a second). Shows exactly what the radio
  is given (post gain and cs8 rounding). Hidden when `preview` is off (a checkbox
  in Advanced; it costs one 2048-point FFT per 50 ms).
- **Meters** are the fields of `TxStatus`; amber when the TS ring fill is under 25 %
  or `ts_stuffed` rose in the last 5 s; red on any `underruns` > 0, any
  `dts_pcr_warnings` > 0, `modulator_load_pct` over 90 %, or `clipped` rising.
- **Prefs**: its own file `%APPDATA%\DecDVB-T\prefs-tx.txt` (`~/.config/decdvbt/
  prefs-tx.txt` elsewhere), a copy of the receiver's `prefs.rs` with the file name
  changed (the receiver's file is the user's; two processes would race on it).
  Persisted: window size, last folder, mode, channel/centre, loop, level, encoder,
  audio codec, ffmpeg path, preview on/off, rate. **Not** persisted: armed, TX VGA,
  amp, the output sink, the file itself.
- **Command line** (hand-rolled like the receiver's, usage on error): `--version`,
  `FILE` (preselects the file, does nothing else), `--mode "64qam-2/3-8k-1/32"`,
  `--channel C30`, `--sink null|iq:PATH` and `--start` (starts `prepare()` then
  `transmit()` **only for the `null` and `iq:` sinks**; `--sink hackrf` does not
  exist), `--screenshot PATH` with `--demo` (feed synthetic `TxStatus` and spectrum
  rows to render the window headless; no session at all). These are the GUI's test
  hooks; none of them can reach the radio.
- Errors (no ffmpeg, bad file, ffmpeg failure with its last log lines, HackRF not
  found or busy, USB error) are shown in a red strip under the state strip until
  dismissed; the log lines are selectable.

The GUI owns nothing about DSP. It builds a `TxConfig`, calls `TxSession`, and
draws `TxStatus` and `PreviewRow`s.

## 8. File ownership (no overlaps)

**Library builder** owns, and only he edits:

- `crates/decdvbt-tx/**` (everything: `Cargo.toml`, `src/*.rs`, `tests/`,
  `examples/`). He fills the `todo!()` bodies and may add private fields and
  private helpers. He keeps every `pub` item's name and signature in `config.rs`,
  `session.rs`, `arm.rs`, `modes.rs` and `preview.rs` (the GUI builder codes
  against them). Adding pub items is fine; changing or removing one is not
  without a note in §14.1 and a message to the GUI builder.
- In other crates, **only** these files, with the stated limits:
  - `crates/decdvbt-io/src/hackrf_tx.rs`: additive. Add `baseband_filter_hz:
    Option<u32>` to `HackRfTxSettings` (default `None`, so existing callers and
    the struct's `Default` behave as before; it has no other users); apply it
    after the sample rate; wrap `set_txvga`/`set_amp`/`set_center` in
    `catch_unwind`; keep the early range checks and the existing test. No change
    to what `start` does by default.
  - `crates/decdvbt-dsp/src/resample.rs`: the ring-buffer rewrite of
    `Resampler::process` (§10). Output must equal the old implementation's within
    1e-6 per sample on the existing tests plus a new golden test (the old code kept
    in `#[cfg(test)]` as the reference). It is shared with the receiver; `cargo
    test -p decdvbt-dsp -p decdvbt-rx` must still pass.
  - `crates/decdvbt-mod/src/inner.rs`, `outer.rs` (and `modulator.rs` only if the
    profile demands): speed work (§10), **bit-exact**: `cargo test -p decdvbt-mod`
    (34 unit tests and the 5 loopbacks) passes unchanged. Add a `Modulator`
    helper only if needed, additively.
  - `docs/STATUS.md`: a new dated section at the end for the library work.

**GUI builder** owns, and only he edits:

- `apps/decdvbt-tx/**` (`Cargo.toml`, `build.rs`, `assets/`, `examples/`,
  `src/*.rs`). He copies, does not import, `freq_display.rs`, `waterfall.rs` and
  `prefs.rs` (and `band.rs` if wanted) from `apps/decdvbt-gui/src` into
  `apps/decdvbt-tx/src` with the header kept and the prefs file name changed; the
  receiver GUI is not edited.
- `README.md` (a TX section replacing the stale "transmit panel" sentences at
  lines ~158-160) and `CLAUDE.md` (the TX command lines in the Commands block).
- `exe/decdvbt-tx.exe` (a copy of the release build; `exe/` is gitignored). Never
  overwrite another file in `exe/`.

**Shared, appended only**: `docs/TX_DESIGN.md` §14 (each builder writes in his own
subsection; nobody edits §1 to §13 without the owner of the change noting it in his
subsection).

**Nobody** edits `apps/decdvbt-gui/**`, `apps/decdvbt-cli/**`, any other crate,
`Cargo.toml` (workspace globs already include both new packages), CI workflows, or
`..\` siblings. `Cargo.lock` changes by cargo are expected (two builders running
cargo do not conflict; neither hand-edits it).

Parallel working: the GUI builder starts at once against the compiling skeleton.
Until the library builder delivers milestone 1 (below), `TxSession::prepare` is
`todo!()`, so the GUI is developed on `--demo` (synthetic status and spectrum) and
on `TxConfig`/`modes`/`arm`, which work today.

## 9. Required changes outside the new crates

All are in the library builder's list above.

| Change | Why |
|---|---|
| `HackRfTxSettings.baseband_filter_hz` | the automatic filter cuts the signal's edges (§5.3) |
| `catch_unwind` around the live controls | a zero reply byte panics `set_txvga_gain`; a keyed radio must not die with a panic |
| `Resampler::process` ring buffer | `hist.rotate_left(1)` on ~26 taps for every input sample is the dominant cost of a resampled TX and of the receiver's front end |
| `InnerEncoder` / `OuterEncoder` speed | 30 % of a core at 64-QAM 2/3; the TX has no spare |

Not needed: a sink trait in `decdvbt-io`. The pull contract
(`FnMut(&mut Vec<u8>)`) is already the right seam; `radio::pull` is the closure
body for every sink, and the virtual radio calls it exactly as the HackRF thread
does, including the zero padding on an empty ring.

## 10. CPU budget and the optimisation work

Measured (release, one core, a possibly loaded shared machine, indicative), per
second of signal at the native rate:

| Mode | Modulate | cs8 pack | Resampler 35/32 | Breakdown |
|---|---|---|---|---|
| 64-QAM 2/3 8k 1/32 | 36 % | 5.1 % | 18.4 % | outer 7.9, inner 22.7, pilots/IFFT/guard ~5.5 |
| 64-QAM 7/8 8k 1/32 | 83 % | 8.5 % | 24.5 % | outer 14.4, inner 40.1 |
| 16-QAM 3/4 8k 1/16 | 41 % | 6.7 % | 22.4 % | |
| QPSK 1/2 8k 1/4 | 10.6 % | 5.0 % | 18.1 % | |

Budget (the numbers to beat and the pass marks, measured with the `txfile` example
on the `Null` sink, unpaced, so speed x = 100 / load):

| Case | Modulator + pack, native | Pass mark |
|---|---|---|
| UK 64-QAM 2/3 8k 1/32 | 41 % today | <= 30 % after the work; must never exceed 50 % |
| 64-QAM 7/8 8k 1/32 | 92 % today | <= 70 %; above 80 % the pipeline split below is done |
| Whole app, UK mode, GPU chain, steady state | | this process <= 45 % of a core, ffmpeg <= 15 % |
| Whole app, UK mode, x264 chain | | this process <= 45 %, ffmpeg + app <= 200 % of a core |

Work, in order, each step measured before and after on the same clip and mode, and
kept only if bit-exact (`cargo test -p decdvbt-mod`):

1. Measure the per-stage split in the real pipeline (a `StageMeter` over outer,
   inner, IFFT, pack) with `txfile --stages`.
2. `inner.rs`: `symbol_cells` encodes bit by bit through a `VecDeque` and
   allocates `mapped: Vec` every symbol. Encode bytewise with a table (the
   convolutional encoder is a 6-bit shift register; a 256-entry table per
   puncture phase), no per-symbol allocation, no `VecDeque`.
3. `outer.rs`: RS(204,188) encoding with a table-driven or AVX2 (`decdvbt-fec`
   already has AVX2 kernels for the receiver) path; the energy dispersal as a
   precomputed 1503-byte PRBS XOR.
4. `resample.rs`: ring buffer instead of `rotate_left(1)` per input sample, and a
   polyphase inner loop the compiler vectorises.
5. Only if 7/8 modes miss the pass mark: **pipeline split**. Stage 1 thread
   (outer + inner, up to the symbol's data cells, in a `Vec<Complex32>` of cells
   passed by a bounded channel) and stage 2 (pilots, TPS, IFFT, guard, gain, pack)
   in `tx-mod`. This needs a public `Modulator` split that does not change a bit of
   the output (the library builder adds it additively in `modulator.rs`).

The T2 modulator is out of scope.

## 11. Test plan (zero RF)

Everything below runs with `DECDVBT_TX_NO_RADIO=1`, on the `Null` and `IqFile`
sinks. No step opens the HackRF, and the library builder reviews every example and
test for it (`grep -rn "HackRfTx" crates/decdvbt-tx apps/decdvbt-tx` finds only
`radio.rs` and its doc comments).

**Clips** are made, not downloaded: a synthetic clip from ffmpeg's `lavfi` sources
(`testsrc2` 1080p25 + `sine`, 20 s) in the scratchpad, plus the project's own
`out/promo/DecDVB-T_promo.mp4` (1080p30 H.264 + AAC) if present, plus a hard clip
(`-f lavfi nullsrc,geq=random(1)*255` noise, 10 s) for the over-budget test. None
is committed.

**Tool**: `crates/decdvbt-tx/examples/txfile.rs` (library builder):

```
cargo run --release -p decdvbt-tx --example txfile -- IN.mp4 OUT.cs8
    [--mode 64qam-2/3-8k-1/32] [--seconds 30] [--loop] [--rate native|10|20]
    [--chain auto|nvenc|x264] [--paced | --flat-out] [--null]
    [--save-ts sent.ts] [--stages]
```

It builds a `TxConfig` with `TxSink::IqFile` (or `Null`), runs `prepare`,
`transmit`, prints a status line a second, and at the end the totals. It has no
`--hackrf` option and is built so that it cannot (it never calls `arm`). The output
file is named `tx_<MHz>MHz_<rate>Msps.cs8` so the receiver reads the tuning from
it. `--save-ts` writes exactly the packets the modulator was given (after SI
injection, stuffing included), so a received stream can be compared.

| Test | What | Pass |
|---|---|---|
| **T1 units** | `cargo test -p decdvbt-tx`: rings (order, blocking, abort), `pack_cs8` vs `IqFileWriter`, arm gate (disarmed, one shot, text), the "only radio.rs opens the radio" test, `gain_for`, mode table (60 rows, A/B/C rates), `VideoProfile` fit, `command_line` invariants (§4.4), SI sections (CRC-32 valid, parsed back by `decdvbt_ts::TsAnalyser`: network name, service list, delivery descriptor frequency/mode; TDT/TOT time), `FeederTs` (priming nulls, lead-in count, stuffing counted, no consumption while priming) | all pass |
| **T2 ffmpeg stream** | the three targets A, B, C x both chains, 20 s of the synthetic clip through `Feeder` into a file; analyse (the scout's `tsan.py` logic, rewritten in Rust in the test or run as a scratchpad script) | sync never lost; CC clean on real PIDs; PCR-derived rate = `-muxrate` within 5 ppm; PCR interval <= 40 ms (expect 20.7); PAT/PMT <= 100.6 ms, SDT/NIT <= 500 ms; **0 `dts < pcr` warnings**; nulls >= 5 %; file length a multiple of 188 |
| **T3 over budget** | the noise clip on mode C with `-b:v` forced 3x too high | the counter reads > 0 and the GUI/summary says so (checks the detector; with the profile's own figures the same clip gives 0) |
| **T4 speed** | `txfile --flat-out --null` for A, B, C and the 7/8 mode, 30 s each | speed x >= 2.0 (A), the pass marks of §10; printed with the per-stage split |
| **T5 paced, no underruns** | `txfile --null --paced --seconds 120` with ffmpeg running (real CPU contention), A, B, C, GPU and x264 chains, plus a CPU-contended run (a second `txfile` flat-out at the same time) | `underruns` = 0; `ts_stuffed` = 0 after the lead-in; IQ ring never below 50 % after the first second; process CPU within §10 |
| **T6 loopback bit-exact** | `txfile ... OUT.cs8 --save-ts sent.ts` (30 s, A/B/C), then `target\release\decdvbt.exe decode OUT.cs8 -o rx.ts` (build `-p decdvbt-cli` first), then compare | receiver locks; 0 uncorrectable RS blocks; 0 continuity jumps; SNR >= 35 dB at -12 dBFS; after the first received packet's position in `sent.ts`, **0 mismatches** over the rest (the first ~2000 packets are the receiver's lock time; the lead-in covers it); the services list shows the name, the SDT service type, the NIT network name and the frequency |
| **T7 the picture survives** | `ffmpeg -v error -i rx.ts -f null -` after the first IDR | no decode errors; frame count within 1 of expected; audio present |
| **T8 loop continuity** | a 5 s clip with `--loop`, 60 s | in `sent.ts`: 0 CC errors on real PIDs, 0 PCR discontinuities, video DTS gapless |
| **T9 level** | 25 M samples at -12 dBFS | rms within 0.2 dB of the request; clipped <= 10; quantisation SNR >= 37 dB |
| **T10 native vs resampled** | `--rate native` vs `--rate 10` and `20` on the same input | all three decode bit-exact (T6); CPU of each recorded; spectrum shoulders of each (the `Spectrum` of `decdvbt-engine` on the file) recorded in STATUS.md |
| **T11 stress** | (a) kill ffmpeg mid-run: state `Failed`, sink stopped, no hang; (b) ffmpeg slowed with `-readrate 0.5`: `ts_stuffed` rises, no panic, decode shows the pause and recovers; (c) missing file, non-video file, no ffmpeg on the path, unwritable IQ path: clear error, no thread left; (d) `stop()` during `Preparing` and during `Transmitting`, 20 cycles of prepare/transmit/stop: no leaked threads or processes (count them), no deadlock; (e) mode change cycle A -> B -> C | all as stated |
| **T12 safety** | `TxSession::new(HackRf sink).transmit()` without arm: error "not armed"; armed but `DECDVBT_TX_NO_RADIO` set: error, no USB access (checked by building with `--no-default-features` and with the feature); the arm lapses after 60 s (a test with a settable clock or the constant lowered via a `cfg(test)` override); `stop()` clears the arm | pass |
| **T13 GUI headless** | `decdvbt-tx --demo --screenshot out/tx.png` renders; `decdvbt-tx FILE --sink null --start` runs a real session on the Null sink for 30 s and prints the CPU and counters; both exit 0 | pass; the screenshot is looked at (all panes, meters, the safety strip, the red state) |
| **T14 workspace** | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (receiver untouched: all existing tests pass), `cargo build -p decdvbt-tx --no-default-features`, `cargo build --release -p decdvbt-tx-app` | pass |

**First light on hardware is the user's alone.** Suggested checklist, not for the
builders to run: HackRF into a receiver (a second HackRF in `decdvbt-gui`, or a TV)
through a cable and at least 30 dB of attenuation (or a dummy load); TX VGA 0, amp
off; ARM, TRANSMIT; watch the receiver's signal card for lock and MER; raise the
VGA a few dB at a time; check the spectrum for the LO carrier at the centre and
the DAC images.

## 12. Milestones (library builder; the GUI builder runs in parallel)

1. **M1 pipeline on the Null sink** (the demo-able core): `ffmpeg.rs` (find, probe,
   chain, command lines), `feeder.rs`, `ts_source.rs` (no SI yet), `iq.rs`,
   `radio.rs` `VirtualRadio`, `session.rs` (`prepare`, `transmit`, `stop`,
   `status`), `txfile` example. T1 to T2, T4 to T6. **Tell the GUI builder when it
   lands** (he can then run real sessions on the Null sink).
2. **M2 preview and telemetry**: `preview.rs`, every `TxStatus` field, `cpu.rs`.
   T9, T11.
3. **M3 SI**: `si.rs` (NIT with delivery descriptor, TDT/TOT). T1 SI rows, T6 with
   services.
4. **M4 optimisation** (§10) and the `hackrf_tx.rs` additions. T4 and T5 pass marks,
   T10.
5. **M5 hardening and docs**: T11, T12, T14, STATUS.md.

GUI builder: (a) window, layout, prefs, dial, drag and drop, `--demo` with
synthetic status; (b) wire a real session on the Null sink once M1 lands; (c)
spectrum and waterfall once M2 lands; (d) the arm/transmit flow and the
transmitting state, tested only with the Null/`iq:` sinks and the unit tests for
the button-enable logic (a pure function `can_transmit(&TxStatus, armed, ...)`,
tested); (e) icon, `build.rs`, README, CLAUDE.md, `exe/` copy, T13.

## 13. Decisions that were weighed (so nobody reopens them)

- **Live ffmpeg pipe, not a pre-transcode.** Start-up 0.1 to 0.9 s against the
  whole transcode, no temp disk (2 h at 17 Mbit/s is ~15 GB), and the throttle is
  free. An optional "prepare first" mode (remux a finished `.ts` with `-c copy
  -muxrate`, 0.2 s CPU for 90 s of content) is deferred; it only pays when CPU is
  the constraint and the clip is short.
- **Modulator on its own thread behind an IQ ring**, not inside the USB callback:
  a scheduling hiccup in the modulator then costs ring margin, not an underrun,
  and the USB thread only does a 256 KiB copy.
- **Native sample rate** by default (no resampler); fixed rates are an option.
- **Own copies** of `freq_display.rs`, `waterfall.rs`, `prefs.rs` and the
  `find()` ffmpeg search rather than a shared UI crate: zero risk to the receiver
  GUI the user is running, and the project already copies between siblings. A
  shared `decdvbt-ui` crate is an easy later extraction.
- **Own `prefs-tx.txt`**: the receiver's file is the user's and has no locking.
- **No `FileTs`**: it reads the whole file into RAM and loops the bytes (CC and PCR
  breaks).
- **No new dependencies** beyond `eframe`/`rfd`/`winresource`/`windows-sys`
  already in the lock file; ffprobe output is parsed by hand.
- **Null packets for stuffing**, not repeating the last packets: the receiver
  gets a valid, rate-correct stream.
- **Private-use network ids** (0xFF01): the app cannot impersonate a real mux.
- **No T2** in this project.

## 14. Builder notes

### 14.1 Library builder

(Deviations from this document, measured numbers, API additions. Empty.)

### 14.2 GUI builder

What exists: `apps/decdvbt-tx` (package `decdvbt-tx-app`, binary `decdvbt-tx`),
written against the declared API of `crates/decdvbt-tx` (no change to the
library was needed, so there are **no GUI requests** beyond the two
"keep this" notes below). Files: `main.rs` (start-up, window, exit code),
`app.rs` (the window), `host.rs` (the session thread), `safety.rs` (the button
and banner rules as pure functions), `meters.rs`, `signal_view.rs`,
`waterfall.rs` and `freq_display.rs` (copies from the receiver GUI, header
kept; `waterfall.rs` gained `History::is_empty`), `prefs.rs`, `settings.rs`,
`modeinfo.rs` (presets and the mode lines), `source.rs` (ffprobe on a thread,
file description, service name), `cli.rs`, `demo.rs`, `selftest.rs`,
`thread_cpu.rs`; `build.rs`, `assets/icon.ico` and `icon-256.png` (red-accent
variant of the receiver's icon), `examples/make_icon.rs`. 48 unit tests.

Deviations from the sections above:

- **Prefs file** (§7): `%APPDATA%\DecDVB-T-TX\prefs.txt` (`~/.config/decdvbt-tx/
  prefs.txt` elsewhere), as the GUI task said, instead of `DecDVB-T\prefs-tx.txt`.
  Either way it is not the receiver's file. The environment variable
  `DECDVBT_TX_PREFS` names another file (scripts point it at a scratch file).
  A `--demo` or `--screenshot` run reads and writes nothing.
- **The session lives on its own thread** (`host.rs`), not on the UI thread:
  `prepare()` may run test encodes, `transmit()` opens USB and `stop()` joins
  threads, and a frame must not wait for any of them. The window sends
  commands and reads a `TxStatus` snapshot and the spectrum rows every 40 ms.
  Every library call is under `catch_unwind` (a panic becomes a message in the
  red strip, and the session is dropped, which stops the radio). The session is
  built inside that thread, so `TxSession` never needs to be `Send`. Closing
  the window joins the thread: the radio is off when `on_exit` returns.
- **Buttons.** `Prepare` (state strip, right) / `Cancel` (Preparing, Ready) /
  `Reset` (Finished, Failed); bottom: tick "I have read this", `ARM` (becomes
  `DISARM`), `TRANSMIT` (red) for the radio or `START` (green) for the test
  outputs, `STOP`. Esc stops; Space stops unless a text box has the keyboard.
  The tick is cleared whenever an arm ends (used, lapsed, dropped on blur) or
  the output changes, so every arm starts from a fresh, deliberate tick. The
  arm is dropped after 5 s without window focus (`safety::disarm_on_blur`).
- **TX VGA** is set before Prepare, or live while transmitting through
  `safety::vga_step` (at most 6 dB a step). It is disabled in Preparing and
  Ready because the library's `set_txvga` is for a running radio: the value at
  Prepare is the radio's start value (0 at every launch).
- **Output** row: `Test only` (a `paced` tick), `IQ file` (name, seconds limit,
  paced; an empty name suggests `tx_<MHz>MHz_<rate>Msps.cs8` in the working
  folder), `HackRF (radio)`. Always `Test only` at start-up.
- **Command line** adds `--run-secs N` (with `--start`: stop after N seconds of
  signal, print the counters and the window's own cost, stop the session like
  the STOP button, exit 0, or 1 on a failure), `--state` (with `--demo`: idle,
  preparing, ready, armed, transmitting, running, stuffing, trouble, failed,
  finished), `--after SECS`, `--size WxH`, `--modes` (open the mode list),
  `--selftest` (the safety logic checked in the built exe, no window) and
  `--help`. `--sink hackrf` is refused with an explanation.
- A demo has **no session thread at all** (`host` is `None`): its red
  TRANSMITTING state is a drawn picture, and no button can reach the library.
- The mode list ("All modes...") is a window listing every combination of the
  chosen FFT size and bandwidth with its bitrate, C/N and picture height.

Measured (release, Null sink, paced, the project's 1080p30 promo clip, UK mode,
GPU chain, library as of this build; the machine was shared, so indicative):

- The window costs **1 to 4 % of a core** (UI thread CPU, `GetThreadTimes`,
  19 frames a second, about 1.1 ms a frame; 1.3 % with the preview off, when it
  draws 7 frames a second). Whole process 14 to 20 %, modulator 9 to 10 %.
- egui's `request_repaint()` with no delay draws **every request twice** ("to
  let things settle"); the session thread's wake-ups therefore use
  `request_repaint_after(1 ms)`, which draws once. That halved the window's cost
  (38 to 19 frames a second at the same 20 spectrum rows a second).

Notes for the library builder (nothing to change, please keep):

- The GUI tells "ffmpeg could not be found" from other failures by the words
  "ffmpeg" and "not found" in the error text (it then offers "Browse for
  ffmpeg.exe..."), and "not armed" by that text (it then does not add the
  "close whatever holds the HackRF" advice). Both messages exist today.
- `TxStatus::level_rms_dbfs` as `-inf` before the first block is handled (the
  readout is hidden).
