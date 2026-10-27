<!-- made by CasualArclamp -->
# DecDVB-T — Design

A Rust **DVB-T / DVB-T2** receiver (and matching modulator) for a **HackRF One**
or **Airspy R2/Mini**, with IQ-file replay. Started from the DecSAT (ex-DecDVB)
code base.

Status date: 2026-10-23 (kickoff). This document holds the scope and the
milestone plan, and it is the source of truth for both. The decisions below come
from the kickoff Q&A with Rory.

---

## 0. The main requirement: CPU efficiency

Rory's top priority: **it must be CPU-efficient**. OnAir's DVB-T receiver uses
too much (the reference screenshot shows CPU 182 %, load 133 % and an 8.5 s
ring overflow on one DVB-T mux). Every design choice is weighed against this:

- Decimate to the OFDM sample rate (64/7 MHz for 8 MHz channels) as early and
  cheaply as possible. Run the HackRF at the lowest rate that holds the
  channel, not 20 MS/s.
- Track time and frequency from pilots once locked. Leave blind search (P1/TPS
  hunting, FFT/GI detection) running only until lock.
- Scopes, the waterfall and measurements run on decimated snapshots at display
  rate, never on every symbol. Nothing runs for a scope that is not shown.
- FEC (Viterbi, LDPC) uses SIMD (AVX2 on this machine), stops early on
  converged LDPC frames, and spreads across a thread pool only when one
  thread cannot keep up.
- Use fixed-size buffers with no allocation per symbol or frame.
- Measure CPU per stage from M1 onward (a per-stage meter in the GUI and a
  `bench` CLI command on captures). A regression counts as a bug.

## 1. Kickoff decisions

| Topic | Decision |
|---|---|
| Relation to DecSAT | **Fresh workspace**, crates copied from DecSAT where useful and renamed; no dependency on DecSAT afterwards. |
| Relation to OnAir | **Standalone.** OnAir (adswill/OnAir, GPL-3, Rory contributes) may be read and borrowed from: its DVB-T/T2 receiver and its Rust crates (`onair_ts`, `onair_dvbsi`, `onair_fec`). Credit it in comments. |
| Standards (first release) | **DVB-T** (EN 300 744) and **DVB-T2 base** (EN 302 755). T2-Lite, MISO, FEF and full multi-PLP are not in the first release. |
| Order | **DVB-T first**, then T2 on the proven OFDM front end. |
| Region / targets | **UK Freeview**, 8 MHz channels, UHF. Transmitter unknown; a band scan and the NIT will tell us which. Since 2026-10-27 also the **narrow channels** (§8): DVB-T scaled to 1 MHz and DVB-T2 at 1.7 MHz, anywhere from 1 MHz to 6 GHz. |
| Front ends | **HackRF One** (live; plugged into this PC), **Airspy R2** (live, 10 MS/s; Mini not supported), IQ files. |
| Antenna | Rooftop aerial with a TV-band band-pass filter. **Heavy multipath on some channels.** |
| Robustness | **Robust from the start**: SFN and long echoes (beyond the guard), Doppler and impulsive noise taken into account in the first design. |
| Real time | **Real-time DVB-T2 HD mux** (~40 Mbit/s, 256-QAM, 32k, LDPC 64800). |
| Test signals | Build a **DVB-T/T2 modulator** (IQ file; no over-the-air transmit) with channel models, and use live HackRF captures. |
| References | gr-dtv (DecSAT `reference/`), Ron Economos's DVB-T2 receiver work, DecDRM's OFDM code, OnAir, ETSI specs (EN 300 744, EN 302 755, TS 102 831). |
| Licence | GPL-3.0-or-later, `made by CasualArclamp` file headers. |
| Repo | Local git; GitHub **CasualArclamp/DecDVB-T, private**, with CI + release workflows copied from DecSAT. |

## 2. Application shape

- **Waterfall and tuning as in DecSAT**: a spectrum and waterfall over the span,
  with a VFO dropped or dragged on it. **One VFO at a time** (one mux).
- **From OnAir's DVB-T view, only two parts**:
  - the **bottom strip of scopes** (TPS/L1 carriers, pilots equalised, data
    cells equalised, raw OFDM cells), next to the receiver statistics
    (state, SNR/MER, CFO, FFT/GI, TPS/L1, modulation, packets, FEC counters);
  - the **right-side service panel** (services with bitrate, type tags,
    now/next EPG, click to play, small preview, file/UDP outputs).
- **Bottom panel**: scopes | channel (fading waterfall and delay–Doppler) |
  maps (no time axis: delay–Doppler, frequency–Doppler, the data cells' error
  cloud and the MER per constellation point) | history (gain, SNR and echoes
  over 25 s) | none. A frequency dial heads the control bar and an SDR window
  holds the front-end settings and the gain optimiser.
- **Discovery**: automatic T vs T2 detection (P1 symbol → T2, else T with TPS
  sync; FFT size and guard interval found blind), a **UHF band scan** (channels
  21–48, NIT network name and services listed), and **channel-number tuning**
  (`C34`, UK channel list).

## 3. Outputs

- MPEG-TS to a file or UDP (the whole mux, or one PLP on T2).
- In-app player (ffmpeg via the DecSAT `video` crate): H.264/MPEG-2 video,
  **E-AC-3 / AC-3**, HE-AAC and MPEG-1 L2 audio, **DVB bitmap subtitles**
  (EN 300 743), and **radio services** (no video).
- PSI/SI viewer: NIT/SDT/EIT now/next, PMT tree, bitrate per PID.
- **Red Button** (added 2026-10-24): the UK's MHEG-5 interactive services
  (UK Engine Profile, ETSI ES 202 184 / D-Book 7) from the service's DSM-CC
  object carousel (`decdvbt-ts::dsmcc`), run by `decdvbt-mheg` and drawn over
  the player's picture. The carousel is received only while the application
  is open. Not in it: MPEG-2 I-frame bitmaps, the interaction channel (IP),
  stream events to the application, HD graphics.
- Measurements: MER per carrier, channel frequency and impulse response, echo
  profile, pre- and post-FEC BER, LDPC iterations. (Pre-FEC here is the BER
  into the Reed–Solomon code; a BER before the Viterbi decoder would need the
  decoded bits re-encoded and was left out for the CPU it costs.)
- Transport stream plumbing: the receive thread hands decoded packets to a
  thread of its own (`decdvbt-engine::hub`) that runs the analyser and feeds
  the outputs and the player's tap, so none of it can make a HackRF read late.
  The GUI polls its view; the guide is copied out only while its window is
  open.

## 4. What carries over from DecSAT

| DecSAT crate | Use here |
|---|---|
| `decsat-io` | HackRF RX/TX, IQ files, extension sniffing. Airspy R2 added as `airspy.rs` (pure Rust over `nusb`, host-side real-to-IQ; 2026-10-27). |
| `decsat-dsp` | AGC, DC, FIR, DDC. |
| `decsat-fec` | DVB-S2 LDPC + BCH: **T2 uses the same codes** (64800/16200), plus the bit interleaver ideas. The AVX2 kernels matter here (see §6). |
| `decsat-modem` (`conv`, `rs`, `dvbs`) | K = 7 Viterbi and RS(204,188) for DVB-T. |
| `decsat-ts` | TS deframing, PSI/SI, sinks, UDP. |
| `decsat-audio`, `decsat-video` | Player. |
| `decsat-gui` | Waterfall, spectrum, tuning, top bar, prefs (cut down). |

New: the OFDM core (sync, FFT, pilots, channel estimation and equalisation),
DVB-T TPS and inner interleavers, T2 P1/P2/L1, cell/time/frequency
interleavers, rotated-constellation demapping, and the T/T2 modulator.

## 5. Milestones (draft)

- **M0** — workspace skeleton, copied crates renamed, CI green. *(done)*
- **M1** — HackRF/Airspy/file front end, spectrum and waterfall, channel tuning,
  IQ recorder; first captures of the local muxes. *(done, 2026-10-23, except
  Airspy; the band scan CLI and the signal card came with it)*
- **M2** — DVB-T modulator plus channel models (multipath, Doppler, noise). *(done, 2026-10-23)*
- **M3a** — OFDM front end and channel sounder: acquisition, tracking, channel
  estimate, fading waterfall and delay–Doppler map. *(done, 2026-10-23)*
- **M3b** — the rest of the DVB-T receiver: equaliser, TPS, demapping, inner
  deinterleavers, Viterbi, RS → TS. Live Freeview DVB-T. *(done, 2026-10-23;
  five muxes decode from the HackRF with the RF amp on, two error-free)*
- **M4** — player, PSI/SI, service panel, scopes, measurements. *(done,
  2026-10-23; the player runs live with sound and DVB subtitles, see
  STATUS.md)*
- **M4b** — the frequency dial, SDR settings (DC removal, I/Q balance, filter,
  ppm), automatic centring of carriers that sit off their channel, the gain
  optimiser, the non-time maps (delay–Doppler, frequency–Doppler, error cloud,
  MER per constellation point) and a 60 fps mode. *(done, 2026-10-23, see
  STATUS.md)*
- **M5** — DVB-T2 modulator. *(done, 2026-10-23, see STATUS.md)*
- **M6** — DVB-T2 receiver: P1, P2/L1, LDPC/BCH, TI, rotated QAM → TS.
  Live Freeview HD. *(done, 2026-10-23: C30 decodes live from the HackRF, 13
  services, in real time; the LDPC runs on a hand-written AVX2 kernel)*
- **M7** — band scan with T2, the UK EPG's Huffman text, Airspy, release; the
  CPU work that is left (resampling, LDPC). *(Airspy R2 front end in, 2026-10-27:
  `decdvbt-io` feature `airspy`, `--radio airspy` in the CLI, "Airspy R2" in the
  GUI's source list; unit-tested offline, still to be checked on the radio, see
  STATUS.md.)*

## 6. Constraints found at kickoff

- Rust was not installed; rustup + stable MSVC were installed for M0.
- CPU is a **Ryzen 7 4800H** (Zen 2, 8 cores, **AVX2, no AVX-512**). DecSAT's
  AVX-512 LDPC path will not run here, so real-time T2 rests on the AVX2/portable
  kernel and the thread pool; this needs measuring early (M6 at the latest).
- HackRF tools (`hackrf_info` etc.) are not on PATH; the HackRF One is
  enumerated by Windows. DecSAT loads `libhackrf` at run time (DecDVB-T uses
  the pure-Rust `seify-hackrfone` driver).
- **The HackRF needs its RF amplifier on for DVB-T** (`--amp`, LNA 32, VGA 8
  on this roof): off, the noise figure leaves 24 dB of C/N and 64-QAM does not
  decode; on, 32–33 dB. The amp is a receive-side stage, not the antenna-port
  power, which stays off.
- At 10 MS/s the HackRF's default baseband filter and the neighbouring muxes'
  aliases cost the band edges some SNR; 20 MS/s with a decimating filter is
  the clean answer (the front end must then decimate before interpolating).

## 7. Open questions

- Ron Economos's DVB-T2 receiver: which repository exactly.
- ~~Airspy R2 real-IF mode (10 MS/s real) or IQ only.~~ Resolved (2026-10-27):
  the R2 streams a real 20 MS/s signal on a 5 MHz IF; the **host** turns it into
  10 MS/s IQ (`decdvbt-io::airspy::convert`, after libairspy: fs/4 translate and
  a half-band filter, ~4 % of a core), in the reader thread so the receive
  thread pays nothing. 10 MS/s is the only rate; the Mini is not supported.
- Automatic gain (LNA/VGA) from ADC level and MER: wanted?

## 8. Narrow channels: 1 MHz DVB-T and 1.7 MHz DVB-T2 (2026-10-27)

Added end to end (modulator, transmitter, receiver, both apps). The decisions:

- **One `Bandwidth` for both standards** (`decdvbt_core::dvbt::Bandwidth`: 1,
  1.7, 6, 7, 8 MHz) with `dvbt()` / `t2()` saying which standard is sent in
  it. It sets only the elementary period T; the signals' samples at 1/T are
  the same at every width, so the modulators and the receivers work in
  samples and carriers and take the width for the clock alone.
- **1 MHz DVB-T is not a standard**: the 8 MHz mode with every clock divided
  by 8 (T = 7/8 µs, 8/7 MS/s, 0.95 MHz occupied, an eighth of the 8 MHz
  rates: 0.62 to 3.96 Mbit/s), as narrowband amateur DATV does. All FFT
  sizes, guards, constellations and rates. DVB-T only (`Mode::parse` takes
  `-1mhz`; 1.7 MHz is refused).
- **1.7 MHz DVB-T2 is EN 302 755's** (T = 71/131 µs, Table 66; 1.712 MHz in
  the delivery descriptor). Nothing in P1 or L1 says the width (L1-pre's
  BWT_EXT is the extended carrier mode), so a receiver must be told it, and a
  network says it in the NIT. The TX offers 1K to 8K, normal carriers,
  rotated constellations, 64800-bit LDPC, the pilot pattern following the
  guard (`decdvbt_tx::modes::t2_mode`).
- **The radio runs at 2 MS/s** (its lowest) for both, and the **baseband
  filter at 1.75 MHz** (its narrowest). TX: `TxRate::Native` means 2 MS/s
  for them, reached by the polyphase resampler (7/4 and 142/131). RX: the
  receiver reads the radio's samples directly where the symbol is a whole
  number of them (1 MHz: rho = 7/4, a 14336-point transform for 8k) and
  interpolates otherwise (1.7 MHz: rho = 142/131).
- **The receiver is told the width** (`AutoReceiver::with_bandwidth`, the
  engine's `Command::Bandwidth`, `decode --bandwidth`, the GUI's width box):
  1 MHz runs the DVB-T receiver alone, 1.7 MHz the T2 receiver alone, 6 to 8
  MHz both. What was in Hz and assumed 8 MHz now scales with the width: the
  CW detector's band, the spectral C/N zones and block edges, T2's
  acquisition window (time-based: 0.27 s), the band view's brackets. What was
  in samples, carriers or symbols (acquisition, the offset search in
  carriers, guard and mode detection, the Doppler estimator, the notch) is
  already the same at every width: the Doppler a symbol is what the
  estimators fit, so a given Hz of Doppler is eight times harder at 1 MHz,
  as it should be. The lock's health and quality looks are in symbols, so
  they take eight times longer in seconds at 1 MHz (a dead 8k lock is given
  up after about 15 s instead of 2).
- **SI**: the terrestrial_delivery_system_descriptor has no code for 1 MHz
  (EN 300 468 Table 46: 4 to 7 reserved): DecDVB-T sends **7**, the last
  reserved value, and reads 7 back as 1 MHz. T2 goes in the
  T2_delivery_system_descriptor (bandwidth 0101, 1.712 MHz). Below 1.5
  Mbit/s the TX sends CIF (352x288, 64k audio) and its budget allows the
  tables' fixed 39 kbit/s.
