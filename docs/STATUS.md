<!-- made by CasualArclamp -->
# Status

Running log of what is done and what is next. `docs/DESIGN.md` holds the plan.

## Kickoff (2026-10-23)

- Folder created, scope agreed in Q&A (see DESIGN.md §1).

## M0 — skeleton (2026-10-23)

Rust 1.99.1 (stable MSVC) installed with rustup (winget). The workspace
builds, its 114 tests pass, and clippy (`-D warnings`) and rustfmt are clean.

Copied from DecSAT and renamed `decsat-*` → `decdvbt-*`, with the satellite
parts cut out:

- `decdvbt-core`: as in DecSAT, plus **`channel`** (new): the UHF raster
  (C21 = 474 MHz … C69), the UK list (21–48 less 38), channel parsing
  ("C34"). The S2 MODCOD/FECFRAME types remain only because the LDPC/BCH use
  them; T2's own types replace them in M6.
- `decdvbt-dsp`: DDC, FIR, SIMD dot product, AGC, DC blocker. Gone: RRC,
  Gardner timing, the carrier PLL (single-carrier only).
- `decdvbt-io`: IQ files (cs8/cs16/cf32, WAV), HackRF RX/TX behind
  `--features hackrf`. Unchanged.
- `decdvbt-fec`: DecSAT's LDPC + BCH (T2 uses the same codes) as they were,
  plus **`conv`** (K = 7 Viterbi, punctured rates) and **`rs`**
  (RS(204,188)), taken from `decsat-modem`. Both match DVB-T's outer and
  inner codes (EN 300 744 §4.3.2–4.3.3).
- `decdvbt-ts`: PSI/SI analyser, sections, sinks (file/UDP/TCP-HTTP), the
  keyframe backlog, UDP-in-TS finding. Gone: BBFRAME deframing, MPE, BISS/CSA,
  DVB-RCS, Newtec, uplink ID. `serve` moved in from `decsat-ip`.
- `decdvbt-audio`: cut down to the PCM player and resampler (ffmpeg decodes
  everything). Gone: multicast depacketising, Symphonia, libxaac, libopus,
  and so both `-sys` crates and the git submodules (and CMake).
- `decdvbt-video`: the ffmpeg TV player. Unchanged.
- `decdvbt-cli` (new, binary `decdvbt`): `channels`, `channel C34|578`.

Left in DecSAT: engine, GUI, GSE, IP, frame, modem (except conv/rs), mod.
CI is copied with the submodule/CMake steps removed; the release workflow
comes with the GitHub repo.

Not done in M0: the GUI app (came with M1) and the GitHub repo (needs `gh` or
a manual create; private, per kickoff). Committed locally.

## M1 — front end, waterfall, tuning, recorder (2026-10-23)

Builds clean; 134 tests pass; clippy and rustfmt clean. Airspy is not in yet.

- **`decdvbt-engine`** (new): the receive thread. One pass per block: source
  read, optional cs8 recording, a level reading (rms, peak, clipping, from
  every 8th sample), and waterfall rows built from a few FFT segments per
  block (`RowBuilder`: 4096 bins, 20 rows/s, 24 segments a row; a full-scale
  tone reads 0 dBFS). A file is paced to real time and loops. Commands in
  (retune, record), rows and stats out. CPU meters: whole-process
  (`GetProcessTimes` on Windows, `/proc/self/stat` elsewhere) and per stage.
- **`decdvbt-engine::snr`**: a C/N estimate from the spectrum, for before the
  receiver measures MER: the block's power against the quietest 100 kHz
  window in the zones 3.9–4.75 MHz either side (a gap between neighbouring
  muxes is empty to the noise). On the C26 and C30 captures: 25 dB and 22 dB.
- **`decdvbt-io`**: `IqSource::retune` (HackRF implements it); a cs8 writer
  that is bit-exact and writes a block at a time; `name_hint` reads centre and
  rate from a capture's name (`c26_514.000MHz_10Msps_….cs8`).
- **`decdvbt-core`**: `dvbt` (modulation, code rate, the C/N each needs for
  QEF, EN 300 744 Table A.1: typed from memory, **check against the spec in
  M3**).
- **`decdvbt` CLI**: `capture` (HackRF at a channel or a file; level, drops,
  CPU, per-stage load; `--record`), `scan` (steps C21–C48 on the HackRF and
  flags blocks of digital TV).
- **`decdvbt-gui`** (new): DecSAT's spectrum + waterfall (ring texture, colour
  map), the UHF channels marked and the VFO (the received channel's 7.61 MHz)
  bracketed, click a channel to tune (the VFO moves if the span holds the
  channel, else the HackRF retunes), channel box with ◀ ▶, rates 10–20 MS/s,
  LNA/VGA/amp, Start/Stop, Record, Open… / drag a file, input level meter with
  overload and weak warnings. Right panel: the **signal card**, laid out as
  DecSAT's DVB card (title bar in the standard's colour with MHz and channel,
  the mode big with the margin beside it in red/amber/green, a C/N bar ticked
  at the QEF threshold). Until the receiver (M3) reads the TPS there is no
  mode and so no margin: the bar shows the spectral C/N. `--assume 64qam-2/3`
  holds the C/N against a mode, for looking at the margin. Under it, a
  services placeholder. Bottom: receiver stats and CPU. `--screenshot`,
  `--after`, `--fps`.

**Findings at this location (rooftop aerial, HackRF):**

- A band scan finds six strong blocks: C22, C23, C25, C26, C28, C30
  (482–546 MHz); nothing above C31. Input is about −22 to −26 dBFS rms at
  LNA 24 / VGA 20, no clipping.
- Heavy multipath: the blocks show notches 15–20 dB deep across the 7.61 MHz.
  Channel estimation has to be good (M3).
- C30's block sits about 0.1 MHz below the nominal channel centre (UK
  transmitters use offsets of ±167 kHz or +333 kHz): M3 tracks the carrier
  offset, the VFO is only nominal.
- Captures for development are in `captures/` (not committed): C26 and C30,
  6 s each at 10 MS/s, cs8.

**CPU (the main requirement):**

- Headless engine on the live HackRF at 10 MS/s: about 3–6 % of a core,
  spectrum stage under 1 %. Recording adds about 7 % (mostly the disk).
- The GUI is the larger part. egui's repaint timer does not tick at the
  interval it is given (50 ms came back as 34 ms frames, vsync on or off), so
  the GUI paces itself from the gaps it sees. Whole app, file replay at
  10 MS/s: ~4–5 % at 10 fps, ~7 % at 15 fps (the default), 12 % at 20 fps,
  ~12 % at 30 fps (noisy, ±3). Our own code per frame is 0.3–0.4 ms; the rest
  is egui's tessellation and the GL driver. The "display" box sets the rate.

**Next (M2):** the DVB-T modulator and channel models (multipath, Doppler,
noise), so the receiver can be built against known signals as well as the
captures; then M3, the DVB-T receiver.


## M2 — modulator and channel models (2026-10-23)

Builds clean; 195 tests pass; clippy and rustfmt clean.

- **`decdvbt-ofdm`** (new, shared with the receiver): the continual and TPS
  carrier tables (2k and 8k), the scattered-pilot rule, the PRBS w_k and the
  pilot values (±4/3), `SymbolPlan` (each carrier's role per scattered-pilot
  phase); the 68-bit TPS block with sync words, mode fields and BCH(67,53),
  and reading it back; the bit interleaver (126-cell blocks, the six offsets)
  and the symbol-interleaver permutation H(q); the Gray-coded constellations
  (QPSK, 16-QAM, 64-QAM, hierarchical α), unit power. Tables are gr-dtv's,
  checked by tests that fail on a wrong entry: every symbol has exactly
  1512 / 6048 data carriers, H(q) is a true permutation, the constellation is
  Gray, the BCH matches an independent port of gr-dtv's register (which
  caught a missing x² in my generator polynomial).
- **`decdvbt-mod`** (new): a test transport stream (PAT/PMT/SDT/NIT that the
  real `TsAnalyser` parses, and a payload each packet of which can be checked
  alone) or a looped `.ts`; energy dispersal (first byte 0x03, 1503-byte
  period), RS(204,188), the I = 12 byte interleaver; the inner chain
  (K = 7 code, bit and symbol interleavers, mapper); the OFDM modulator
  (pilots, TPS, IFFT, guard interval; 2k/8k, all guards, 6/7/8 MHz).
  **Loopback tests** run an ideal receiver that undoes every stage and compare
  the transport packets bit for bit: five modes, every one passes. A loopback
  cannot catch a wrong table (the inverse uses the same one): the real
  captures do that, in M3.
- **Channel models** (`decdvbt-mod::channel`): propagation at the signal's
  rate: any number of paths with fractional delays (Kaiser-sinc, so a 0 dB
  echo gives true nulls), gain, phase, Doppler, Rayleigh or Rician fading
  (Clarke); presets `tu6`, `ra6`, `sfn20`, `0db-echo`, `urban` (a guess, fitted
  by eye). Then the resampler, then the **receiver** (`RadioSim`) at its own
  rate: white noise at a C/N against the nominal 8 MHz, impulse bursts, carrier
  offset, IQ imbalance, DC offset (the HackRF's spike), sampling clock offset.
  Noise after the resampler, not before: before it the resampler's roll-off
  shaped the noise and the C/N estimator read 5 dB high. The standard's own
  F1 / P1 profiles (Annex B, 20 paths) are not included.
- **`decdvbt-dsp::resample`**: a polyphase rational resampler (35/32 for
  10 MS/s, 35/16 for 20), 70 dB rejection.
- **`decdvbt modulate`**: `--mode 64qam-2/3-8k-1/32 --cn 25 --profile urban
  --path 2.5:-3 --fd 80 --cfo 1500 --sco 20 --impulse 100:8:4 --dc 0.03
  --iq 0.5:2 --rate 10 --format cs8|cs16|cf32 --ts file.ts`; files are named
  so the GUI reads their centre and rate. 3 s of 8k signal takes 2 s to make;
  a six-path fading channel is slower than real time.
- **Estimator check** (simulated C/N → what the card reads): 15 → 15.5,
  25 → 24.6, 35 → 29.9 (the 8-bit file's own quantisation limit, as theory
  gives ~30 dB at −20 dBFS rms).
- **Start-up pulse**: for the first 2244 bytes the byte interleaver is empty
  and data bypass the randomiser as zeros, so every data carrier carries the
  same point and the symbol is one huge pulse (peak 17× the rms). The
  generator discards those symbols (`warmup_symbols`); the loopback does not.
- Not done: hierarchical modulation (refused with an error); **no HackRF
  transmit** (broadcast UHF channels need a licence: test signals go to a file,
  or over a cable with an attenuator if you want them on the air).

**What the captures are** (a cyclic-prefix correlation search, as a blind
receiver would do it; a simulated 8k 1/32 signal peaks at 16× the median, the
others none):

| channel | MHz | what it is |
|---|---|---|
| C22, C23, C25, C26, C28 | 482–530 | **DVB-T, 8k, guard 1/32** (peak 10–12× the median) |
| C30 | 546 | **DVB-T2, 32k, guard 1/128**: no DVB-T peak, 7.7 MHz wide, a peak only at T2 32k/1/128's 36120-sample period |

So M3 can be built and tested on five real DVB-T muxes, and C30 is the T2
test for M6. Captures: `captures/c22…c30_…cs8` (1.6–6 s each, not committed).

## M3a — OFDM front end and channel sounder (2026-10-23)

Asked for: DecDRM's fading waterfall and delay–Doppler map. Both need a channel
estimate per symbol, which needs the first half of the receiver, so that is
what was built (the rest of M3 — equaliser, demapper, Viterbi, RS → TS — is
next). 214 tests pass; clippy and rustfmt clean.

- **`decdvbt-rx`** (new). `acquire`: from 58k samples at the signal's rate,
  the **cyclic-prefix search** (every FFT size and guard, the sums folded over
  the symbol periods, contrast of the peak over the median; pure noise reaches
  3.6, so a lock needs 5) gives the mode, the symbol timing to a fraction of a
  sample and the carrier offset within a spacing; the **spectrum's edges** give
  the offset in whole carriers; the **pilots** give the scattered phase and
  confirm DVB-T: the right phase and offset make the channel's impulse
  response collapse into a few taps, a wrong one leaves noise (a fake OFDM
  signal with a cyclic prefix is rejected). `interp`: 32-tap Kaiser-sinc
  interpolation at any position, AVX2+FMA at run time (13 ns a sample, 3.2×
  the plain loop). `sounder`: tracking, below. `delay_doppler`: DecDRM's
  estimator (IFFT over frequency, FFT over time, Hann windows) on DVB-T's
  rows.
- **Cheap by design**: the sounder tells the engine which stretch of samples
  it wants next (one symbol in 8, about 12 % of the stream), takes only those
  and reads them by interpolation at exact fractional positions; nothing is
  converted wholesale. Per symbol looked at: the cyclic prefix near the
  prediction (timing, carrier fraction; an α-β loop follows position, clock
  drift and carrier), the useful part read from the filtered timing, rotated
  by the **integrated** carrier phase (an absolute-time rotation made phase
  jumps at every update of the offset: found by the Doppler test) and
  transformed (15 µs), the scattered pilots divided by their values = a row of
  the transfer function at every 12th carrier. Measured: **2.5 % of real time**
  on a capture (3.3 % live on the HackRF), 0.9 % with no DVB-T signal.
- **Engine**: `Command::Sounder(Some(rf_hz))` / `None`; `Rx::take_sounder()`
  gives status, fading rows and a new delay–Doppler map (once a second). It
  runs only while the panels are open. A fourth stage in the CPU meter.
- **GUI**: a Channel panel under the band view (checkbox "channel", Doppler
  range ±5/15/30/60 Hz): the **fading waterfall** (gain per pilot carrier, 15 s,
  a row every 30 ms, colours anchored to the median) and the **delay–Doppler
  map** (inferno palette, 40 dB, guard-interval line, pointer readout), under a
  status line (lock, mode, carrier offset, clock offset, pilot coherence). The
  signal card now shows what the sounder read (FFT size, guard, symbol and
  useful time, carrier offset) with no "assumed" label.
- **CLI**: `decdvbt sound <file|C26>` locks and lists the strongest paths.

**Tested on simulated signals** (the modulator and channel models): a locked
carrier offset to within 300 Hz at +337 kHz, the clock offset (clean channel:
within 1 ppm; with an echo a few ppm), 8k and 2k, a 2 µs echo 6 dB down at its
delay, a +8 Hz path at its Doppler and not at the mirror, noise alone never
locking.

**On the real captures** (all five DVB-T muxes lock, the T2 mux does not):

| channel | carrier offset | clock | strongest paths (µs, dB) |
|---|---|---|---|
| C22 | +35 Hz | +4 ppm | −0.9 (0), 0.1 (−3), 1.1 (−4), 2.7 (−6) |
| C23 | +61 Hz | +6 ppm | 0.3 (0), −1.4 (−2), 0.5 (−7) |
| C25 | +59 Hz | +1 ppm | −0.5 (0), 0.3 (−1.5), 1.5 (−8) |
| C26 | +86 Hz | −5 ppm | −1.2 (0), −1.4 (−3), 0.7 (−4), 1.2 (−5), 2.6 (−6) |
| C28 | **−166 641 Hz** | +1 ppm | −1.1 (0), −0.5 (−2), −1.1 (−4) |
| C30 (T2) | no lock (contrast 3.7) | | |

C28's offset is the UK's −166.67 kHz transmitter offset, read from the signal;
the others sit within ±90 Hz of zero, which is the HackRF's LO error (a TCXO
good to 0.15 ppm). Every path is at 0 Hz Doppler (static): the test site's
multipath is a cluster of reflections from about −1.5 to +3 µs, the longer ones
out to ~15 µs, all inside the 28 µs guard.

**Not yet**: the lock restarts when a short capture loops (the map needs 2 s
of rows, so a 1.6 s file never shows one); the channel view only handles DVB-T
(T2 needs its own pilots); the interpolator's fractional timing is rough with
an echo (the clock reading wobbles a few ppm); a 20 MS/s span needs a
decimating filter first.

(What M3b did about these: see below.)

## M3b — the receiver: equaliser to transport stream (2026-10-23)

Asked for: the equaliser, TPS, demapper, deinterleavers, Viterbi, RS and
descrambler, to a transport stream on the five DVB-T captures. Done; the
whole chain runs from a capture (or the HackRF) to a `.ts` that the PSI
analyser reads, in the CLI (`decdvbt decode`), the engine thread and the GUI
("decode" checkbox). 240 tests pass, clippy and rustfmt clean.

**The chain** (`decdvbt-rx` unless said):

- `symbol`: the reader shared with the sounder (cyclic-prefix timing, carrier
  loop, interpolated window, integrated rotation, FFT). Now reads at the clock
  rate it has tracked, and takes a fine timing correction from the pilots.
- `chest` (the equaliser): the common phase and timing slope of each symbol
  from the 177 continual pilots against a running reference — **the maximum-
  likelihood slope** (peak of the pilots' coherent sum over ±6 rad, then a
  weighted line fit; the first try, from neighbouring pilots' phase
  differences, was 75× noisier and failed below ~15 dB pilot SNR); the
  channel on a 3-carrier grid from the scattered pilots (IIR, four symbols to
  fill it); every carrier by an 8-tap windowed-sinc interpolator (linear
  interpolation cost 8 dB with a 20 µs echo in 2k); a symbol whose pilots will
  not line up is equalised but not used to update anything. **Noise per
  carrier** from each scattered pilot's innovation against the estimate it joins,
  smoothed over ±3 grid points: an adjacent channel's skirt or a spur shows
  up where it is.
- `tps_rx`: DBPSK vote over the 17/68 TPS carriers weighted by |H|², sync word
  + BCH(67,53) parity for the frame start (`decdvbt-ofdm::tps::parse_correcting`
  mends up to two wrong bits by syndrome table; one is allowed while
  searching, two once following), then the block read every frame: position
  of every symbol, the constellation, rate, guard, hierarchy and cell id.
- `demap`: max-log soft bits per axis in closed form (checked against brute
  force), weighted by |H|²/σ²(carrier), 8 bits; the symbol deinterleaver as a
  gather and the bit deinterleaver as a scatter in the same pass.
- `decdvbt-fec::viterbi` (new): K = 7 (171, 133), eight-bit soft inputs,
  sixteen-bit metrics, AVX2 butterfly kernel keeping the 64 metrics in
  registers, a portable twin that must (and does, tested) give identical
  decisions, depuncturing built in, traceback in chunks of 512 bits. **10–12
  ns a decoded bit** (80–100 Mbit/s a core; the old float decoder was ~90 ns).
- `outer`: sync hunt on the 0x47/0xB8 bytes 204 apart at every bit alignment,
  the convolutional deinterleaver, Reed–Solomon (the syndromes by table
  lookup: a clean block costs a fraction of a microsecond), the descrambler
  as a precomputed 1503-byte sequence XORed on, transport-error flag on what
  the code could not fix, loss of sync after 20 bad blocks in a row.
  `Prbs` and `ConvInterleaver` moved to `decdvbt-fec::outer`, shared with the
  modulator.
- `receiver`: runs it all on pushed blocks (the same want-a-stretch method as
  the sounder, `window`): acquire → lock → TPS → decode, with the pilot phase
  checked against the frame position and the mode checked against the lock.
  Per-stage times in `status().profile`.

**Tested on simulated signals** (the modulator and channel models, through
the whole receiver): a clean UK signal and one through echoes at +9/+14 dB,
166 kHz of carrier offset and 15 ppm of clock error both decode to exactly the
test stream's packets, consecutive, none lost; 2k QPSK at 12 dB C/N; the
equaliser holds 27 dB MER at 28 dB C/N and 24 dB in 2k with a 20 µs echo.

**On the real captures.** Decoding the first five captures failed at first and
the reason was not the receiver: **the HackRF's RF amplifier was off**. It is
the +14 dB stage with a low noise figure; without it the front end's noise
figure (and the 8-bit ADC at −24 dBFS) leaves 24 dB of C/N at best and the
upper half of the band at 10–13 dB (the test channel has two near-equal paths
1.75 µs apart, so half the band is 6–10 dB down). With `--amp --lna 32 --vga 8`
the C/N is 32–33 dB, and all five channels were recorded again
(`captures/cNN_…_10Msps_amp.cs8`, 4 s each) and decode:

| channel | mode | packets | flagged (RS lost) | before RS | SNR |
|---|---|---|---|---|---|
| C22 | 64-QAM 3/4 1/32 8k | 70 525 | 30 (0.04 %) | 7.3e-4 | 18.3 dB |
| C23 | 64-QAM 2/3 | 62 374 | **0** | 4.5e-6 | 21.4 dB |
| C25 | 64-QAM 3/4 | 70 487 | 86 (0.12 %) | 2.3e-3 | 17.7 dB |
| C26 | 64-QAM 2/3 | 62 715 | **0** | 3.0e-5 | 18.6 dB |
| C28 | 64-QAM 3/4 | 69 632 | 1 597 (2.3 %) | 3.5e-3 | 19.8 dB |
| C30 (T2) | does not lock (contrast 3.7) | | | | |

C23 (the BBC mux, 26 services, the stream's clock 10:33:59 UTC, network
"(region)") and C26 are error-free (QEF is 2e-4 before the code). C28 still
loses packets: it has the UK's −166.67 kHz offset and the weakest margin. The
original amp-off captures decode only partly (C26 62 240 packets, 32 flagged;
the others far less): they are noise-limited, not a receiver fault.

**What it cost to find** (the diagnostics are in `examples/diag.rs`, with a
numpy cross-check outside the repo): pilots only 10 dB above their own
variation on a 24 dB C/N capture → the slope fit failing (75× noisier seed)
→ fixed; MER profile bad in the upper half of the band → not the interpolator
(−48 dB error everywhere), not phase noise (−38…−44 dB), not an impulse
response beyond the guard (nothing past −26 dB), not intermodulation
(gain sweep) → the front end's noise figure; edge carriers and an adjacent
channel make the noise uneven across the band → per-carrier noise from the
pilots, which took C22/C25/C28 from "nothing" to "almost clean" by itself.
The demapper's first noise figure was one global decision-directed estimate,
which read 0 dB whenever many cells were wrong and scaled every bit down.

**CPU (the main requirement), on this machine, one core, per second of
signal** (64-QAM, 8k, release build):

| stage | 2/3 mux (24 Mbit/s) | 3/4 mux (27 Mbit/s) |
|---|---|---|
| read symbols (interpolate + FFT) | 19 % | 22–23 % |
| equalise | 14 % | 18 % |
| demap + deinterleave | 7 % | 9 % |
| Viterbi | 26 % | 30 % |
| outer (RS etc.) | 3 % | 5–7 % |
| **total** | **72 %** | **89 %** |

(That was the first cut; the CPU pass below took it to 30–34 %.)

**Also**: `Stage::Decoder` in the CPU meter, `Command::Decode(Some(rf_hz))`
and `Rx::take_decoder()` (status and the transport stream, bounded); the GUI
card shows the TPS mode (no "assumed"), the pilots' SNR as "MER" against the
mode's QEF threshold, the stream rate, RS blocks fixed/lost and the error rate
before RS. The services list still waits for M4.

**Not yet**: hierarchical transmission (the HP stream alone would need the
other demultiplexer; refused with a message); 4k; the TS goes nowhere in the
GUI yet (file, UDP and the player are M4); a 20 MS/s radio rate needs a
decimating filter before the interpolator (it aliases the adjacent channels
otherwise); the receiver and the sounder each acquire and read symbols (the
channel panels should come from the receiver's own estimates); MER/SNR
needs the threshold table in `dvbt.rs` checked against EN 300 744 Table A.1.

## CPU pass (2026-10-23)

Asked for: Viterbi, symbol reading and the equaliser, measured on the captures,
before M4. Measured with `decdvbt decode … --seconds 4` (the per-stage times
come from the receiver's own `profile`), one core, per second of signal, on
the four-second amp-on captures, release build.

| stage | before | after |
|---|---|---|
| read symbols | 19–20 % | **4.5 %** |
| equalise | 13–14 % | **4.3 %** |
| demap + deinterleave | 7.4 % | **4.7 %** |
| Viterbi | 25–27 % | **12.6–14.5 %** |
| outer | 3–6 % | **2.2–4.6 %** |
| **receiver total** | **72 % (2/3), 89 % (3/4)** | **30 % (2/3), 32–34 % (3/4)** |
| live GUI (the whole app, channel panels on) | 82–105 % | **41 %** (decoder 31 %) |

And the decode got *better*, not just cheaper: C28 went from 2.3 % lost
packets to none, C22 from 28 to 0, C25 from 86 to 2 (before RS 7e-4 → 2e-4 on
C22, 3.5e-3 → 9e-7 on C28), because the cheaper way of reading symbols is also
the more accurate one (below). What changed, largest first:

- **Reading symbols straight from the radio's samples.** At 10 MS/s a symbol
  is exactly 8960 samples and the carrier spacing is exactly one bin of the
  transform, so there is nothing to interpolate: the FFT is taken on the radio's
  samples from the nearest integer position (the 13 ns/sample interpolator was
  80 % of a read) and the fraction of a sample the position missed becomes a
  phase ramp over the carriers, the same one that undoes the early start. The
  window is copied and rotated by the carrier offset in one pass (eight
  independent phasor chains) and transformed (13 µs at 8960, 11.5 at 8192;
  rustfft is fast). It gives up correcting the clock rate *within* a symbol
  (the interpolating reader read the window at the tracked rate, which wandered
  by ±10 ppm and added more error than it removed: the clock is still followed
  from symbol to symbol). Any rate where 896 µs × fs is a whole number reads
  directly (8, 10, 12.5, 16, 20 MS/s: a 20 MS/s radio needs no decimating
  filter, the FFT does the channel selection); others fall back to the
  interpolator (tested at 9.6 MS/s). The sounder reads this way too (2.5 → 0.9 %
  of real time).
- **Viterbi, 11–12 → 4.8–5.5 ns a bit.** Depuncturing by byte shuffle (3.0 →
  0.45 ns/bit); the stream cut into overlapped segments (96 steps of
  acquisition ahead of the bits, 96 of lookahead after; 5 % more steps) decoded
  two at a time **one segment in each 128-bit lane**, so no operation crosses a
  lane (a cross-lane `vpermq` costs about 0.4 ns on this CPU, six of them a step
  in the first kernel: removing them was worth 2.5 ns a bit) and the traceback
  of the two runs interleaved (1.7 → 0.65 ns/bit). Output lags input by up to
  two segments (8k steps). An eight-bit-metric variant (4.7 vs 8.3 ns/bit then)
  was built and dropped: with noise-weighted soft bits the useful range is far
  wider than ±4 and at its best fixed scale it cost 0.3–0.4 dB.
- **The equaliser**: the correction applied to every carrier was a `sin` and a
  `cos` each (13 700 library calls a symbol), now a phasor stepped out from the
  middle; the slope fit's second pass with its `atan2` and `sincos` per pilot is
  one small-angle step from the coarse peak (the scan and the interpolation over
  the grid as plain loops over real and imaginary arrays, compiled for AVX2+FMA
  and for the baseline, chosen at run time); no allocations per symbol.
- **The demapper**: soft bits made a bit at a time over all the cells (vector
  code), the bit deinterleaver a rotation of each bit's array in a 126-cell
  block and a byte transposition by shuffles eight cells at a time.
- **Reed–Solomon**: a clean block (nearly all) is checked by running the
  encoder's shift register over its message, a 16-byte load and exclusive-or a
  byte (0.5 µs), and only a block that fails goes to the syndromes.

**What is left** (30 % of a core for a 24 Mbit/s mux): the Viterbi is still
40 % of it; its kernel is at about 47 vector operations a step and near what
that design gives (the in-place layout that rotates the state bits and crosses
a lane once in six steps would save perhaps another 25 % of the kernel and is
a lot of bookkeeping). Then the demapper's scalar gather of the cells in
deinterleaver order, reading (the FFT, the copy and the carrier ramp), the
equaliser's grid update, the byte deinterleaver. A second thread for the
Viterbi + outer would cut latency, not CPU.

## M4 — the player, PSI/SI and guide, service panel, scopes (2026-10-23)

Asked for: "the player, PSI/SI and EPG viewer, service panel and scopes."
Everything below runs in the GUI on the five live captures and live on the
HackRF. The player was first written without ffmpeg on this PC and then run
once it was installed (`winget install Gyan.FFmpeg`, 9.0.2; the app finds it in
winget's package folder without a PATH refresh): see "The player, run" below.

**Scopes and measurements** (`decdvbt-rx::scope`, `apps/decdvbt-gui/src/scopes.rs`).
`Receiver::request_scopes` copies the carriers before the equaliser touches
them (only when asked, once) and takes the TPS, pilot and data cells from the
equalised symbol; `Receiver::measure` turns the equaliser's channel estimate
into the gain and the per-carrier signal-to-noise ratio across the band
(groups of carriers, at most 1024 points) and the impulse response (an inverse
transform of the estimate under a Hann window, the window from half a guard
before to a guard and a half after the strongest path, echoes as its local
maxima within 35 dB). The engine makes the cells at 10 Hz and the
measurements at 4 Hz, only while the bottom panel shows them. On the C23
capture: four scopes (64-QAM clean at 21.6 dB MER), gain, SNR and an
impulse response with its echoes at −1.8 to +0.7 µs. The receiver test puts
an echo of 3 µs and −10 dB in and finds it at 3.0 µs and −10 dB.

**The scopes cost 1 % of a core, not 9 %.** The first version drew every cell
as a rectangle in an egui mesh (31 000 cells a frame): 9 % of a core, more than
the equaliser. Now each scope is a 160×160 phosphor grid (intensities that
fade with each symbol) sent to the graphics card as a texture only when
cells come, 10 times a second; a frame draws four quads. The traces are cut
to two points a pixel column (min and max of each stretch, so notches
survive).

**The transport stream hub** (`decdvbt-engine::hub`). The receive thread
hands 100 ms batches of packets to a thread that runs the DecSAT analyser and
the backlog and feeds the outputs: a `.ts` file, UDP (7 packets a datagram),
an HTTP/TCP server (VLC, PotPlayer) and the player's tap. Verified on C23:
the HTTP output gives 62 301 packets with no bad sync byte and the PAT; UDP
2216 datagrams a second (23.3 Mbit/s); the file is written. The hub's own load is
0.3 % of a core (shown in the status panel). The whole GUI on the C23 capture
costs 40–41 % of a core with the scopes off and about the same with them on,
against 39–40 % before M4.

**SI the analyser did not read** (`decdvbt-ts`, from DecSAT's satellite
analyser): the terrestrial delivery descriptor (centre frequency, mode,
constellation, rate, guard), the service list, the UK logical channel numbers
(descriptor 0x83) that order the services, the TOT's local time offset (so
BST shows), and the whole guide: `epg.rs` keeps the events of the EIT
present/following and schedule tables by service with their times as seconds
since 1970, text (short and extended), genre and age rating, and forgets what
ended an hour ago. C23 gives 26 services in the regional network, BBC One
and Two with what is on, and 800 guide events in 14 s.

**The GUI.** The right panel lists the services by channel number with type
(TV, HD, radio, data), bitrate, a lock for scrambled ones and the event now
on; double-click or Play to watch, VLC to open it in VLC (through the HTTP
output, with `--program` for the service), outputs (file, UDP, HTTP) in a
collapsible section. Windows: the **transport stream analyser** (services
and their streams, PIDs with rates and continuity errors, the tables heard,
the NIT's multiplexes with their services and channel numbers, stream clock
and local time) and the **programme guide** (services down, time across,
colour by genre, click for the description). The **player** window: service,
sound and subtitle choice, bob ×2, GPU decode, volume; a radio service shows
its name, what is on and the sound's levels. DVB subtitles are an ffmpeg
filter graph (`[0:i:V] … [v]; [0:i:S] scale … [s]; [v][s] overlay`), which works
(below). The bottom panel is now a choice of scopes, the channel sounder's panels or
none (`--bottom`); scopes are the default.

**The player, run** (live from the HackRF, C23, BBC One / BBC Radio 4):
- Video and sound: BBC News at One, then a drama, in the player window at
  704×576 → 960×540, "playing · timed by the sound", 0 pictures dropped, 450–560 ms
  of sound buffered, the first 35–60 pictures skipped to reach live.
- **DVB subtitles** (`--play 4164 --subs`): the broadcast's green speaker-coloured
  subtitles appear over the picture ("It wouldn't be me you were letting down").
  The overlay graph needed nothing more; HD subtitles on a 1920×1080 display
  definition are untried (a mux with them is not receivable here).
- **Radio** (`--play 6912`): BBC Radio 4 with the programme name and level meters.
- **AC-3, E-AC-3 and AAC** (none are on the DVB-T muxes here; Freeview HD is T2):
  three unit tests make an ffmpeg test stream in each and check the sound
  reaches the audio player and ffmpeg names the codec. ffmpeg does the decoding.
  The audio was run at 5 % volume and the picture and sound clock checked, not
  listened to.

**What the player cost, and what was fixed.** First run: the GUI process at
138 % of a core (the receiver's own thread being 33 %), ffmpeg on top.
- ffmpeg's output pipe was a Windows default 4 KB pipe: a 2 MB picture crossed
  it in 500 round trips and the reading thread cost 40 % of a core in system
  calls. The pipe is now made with `CreatePipe` holding two pictures
  (`decdvbt-video::output_pipe`).
- The window asked egui to repaint every 10 ms, which gave 140 frames a second;
  the app's own pacing now takes a floor from the player (30 fps, 50 with bob),
  which holds 30.
- The picture is converted with `from_rgba_premultiplied` (a copy; the pictures
  are opaque) rather than the per-pixel unmultiplied path.
- Bob ×2 now defaults to off. Deinterlacing to 25 pictures a second halves the
  pictures, uploads and ffmpeg's work.
- Only the PAT, PMT, PCR and the chosen video, sound and subtitle PIDs are fed
  to ffmpeg (the red button service's data carousels were being fed to it and
  thrown away, and gave a log full of "could not find codec parameters").

Now, playing BBC One at 25 pictures a second: the GUI at 52 % of a core
(40 % with no player), ffmpeg at 14 %. Bob ×2: 73 % and 37 %. GPU decode
(ffmpeg `-hwaccel auto`) on this PC's MPEG-2 SD: 59 % and 22 %, no better than
software, so it stays off. Left: a frame buffer pool for the 2 MB pictures
(each is allocated, zero-filled, read and header-trimmed by a memmove) and
handing the pixels to egui without the copy; perhaps 3–4 % of a core.

**Not done / to check**
- A BER before the Viterbi decoder.
- LCN uses descriptor 0x83 only (the UK's); Nordig's 0x87 and the HD
  simulcast descriptor 0x88 are not read.
- DVB text uses Latin-1 for tables other than UTF-8; ISO 6937's accents in the
  default table are not translated.

## App icon, no console window (2026-10-23)

- **Icon.** A dark-blue rounded square with a 16-QAM constellation (4 × 4 dots,
  the corners amber; 3 × 3 at 16 and 24 px where sixteen do not read), drawn by
  `apps/decdvbt-gui/examples/make_icon.rs` from distance functions at each size
  (16 to 256 px, PNG inside the .ico). It is the exe's icon (`build.rs`, through
  `winresource` and the SDK's `rc.exe`; without `rc.exe` the build warns and goes
  on without) and the window's (`ViewportBuilder::with_icon`, the 256 px PNG
  compiled in). The exe's file properties now name the product and version.
- **No console.** A release `decdvbt-gui.exe` is a Windows GUI-subsystem
  program (`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`),
  so double-clicking it opens no black window; debug builds keep theirs for the
  log. Started from a terminal it attaches to that terminal's console and points
  its output there unless redirected, so `--version`, usage errors and the
  `--screenshot` report still print (checked in a PowerShell terminal and
  through a pipe). ffmpeg already ran with no window. The CLI (`decdvbt.exe`)
  stays a console program.

## Services by type, click to play, and tuning that did not lock (2026-10-23)

**Services.** A television service with no picture or sound in its PMT (BBC
THREE and BBC FOUR share their capacity with CBBC and CBeebies and are off air
by day; BBC Red Button is data only) was listed as "data", because the list
went by the streams. It now goes by the SDT's service type first (EN 300 468
table 87; the C23 capture says 0x01, digital television, for all three) and
shows "off air" where the rate was. **Click a service and it plays** (an off-air
one says why in the control bar); in the guide a click on a programme that is
on now, on a service with something to show, plays that service (marked ▶), and
so does a click on the service's name. Tried with real clicks on a running
HackRF: BBC ONE from the guide, BBC TWO from the list, BBC FOUR's note.

**Tuning that did not lock.** Reported: sometimes after tuning the receiver
does not lock, and decode has to be turned off and on. There were four causes,
found with two new tools (`retune_soak`: the GUI's retune-then-decode sequence
against the HackRF, timed; `splice`: real captures fed to the receiver as a
retune feeds it, offline), and all four are fixed.

1. **Acquisition failed on a clean signal at a third of the positions.** The
   whole-carrier offset was guessed from the spectrum (power inside the block
   against outside), which on a real band, with the HackRF's filter blurring
   the block's edges, is good to a dozen carriers, and the pilot test looked
   ±3 round it. On the C26 capture 11 of 36 first attempts (the stretch started
   at 36 positions across a symbol) found a cyclic prefix of contrast 23 and then
   said "not DVB-T"; the retry came 0.4 s later at another position, and
   several in a row could fail. The offset now comes from the **continual
   pilots** (`acquire::pilot_offset`): they carry the same value in every
   symbol, so products of consecutive symbols' bins at their positions add up
   coherently (about 1) for the right shift of the block and not for any other
   (0.04); the spectrum's guess is the fallback. 0 of 36 failed on all five
   captures. Retries come after 0.2 s, not 0.4.
2. **A retune handed the old channel's samples to the new receiver.** The
   HackRF has up to 64 buffers queued and three transfers in flight when the
   centre moves, and both capture and receiver were centred on the new channel
   the moment the command went: the receiver locked onto the old mux. The source
   now flushes its queue and discards the transfers in flight on a retune
   (`HackRfSource::retune`), and the engine throws away 50 ms more
   (`Settle`).
3. **A lock on the wrong samples stayed locked.** Fed C23 then C26 as a retune
   does, 9 of 24 splice points left the receiver locked for good (1 lock,
   about 22 packets): the receiver's coherence test (neighbouring pilots) still
   read 0.5, the TPS flickered, and the MER wandered between −4 and 6 dB. Three
   guards now: the **equaliser starts over** after 24 symbols whose continual
   pilots match its reference (before, a reference made from the wrong channel
   froze the channel estimate for good, tested synthetically); a lock whose
   signal-to-noise ratio is more than 8 dB under what the mode needs for QEF
   (never under 2 dB) for four looks of 128 symbols is dropped; and a lock
   that has had TPS sync but not one clean Reed–Solomon block for four looks of
   512 symbols is dropped. Now 0 of 24 splice points fail, the worst recovering
   in under a second.
4. (The soak tool's own fault, found on the way: it left the HackRF receiving
   when it exited, and the next run sometimes got no samples at all. It stops
   the radio now.)

Live, 200 tunings between C22, C23, C25, C26 and C28 through the engine as the
GUI drives it: all decode, in 0.25–0.29 s on average, 0.3 s at the worst. The
five captures decode as before (one lock each, same Reed–Solomon counts, CPU
unchanged at 30 %). The receiver status counts the acquisitions that failed
and the locks given up (`acq_no_prefix`, `acq_not_dvbt`, `stalled_locks`).

## Frequency dial, SDR settings, gain optimiser, maps, 60 fps (2026-10-23)

Asked for after the C30 screenshot and "nearly ready to make public": more
channel metrics as 2-D heat maps that are not time on an axis (like DecDRM's
delay–Doppler chart), a 60 fps mode, DecSAT's frequency bar, retuning of
carriers that are off their channel's centre, SDR settings (DC removal and so
on), and an "optimise gain" button that finds the best MER.

**The dial.** DecSAT's big-digit frequency display (`freq_display.rs`) is the
top-left of the control bar: wheel or click a digit to step it, type a number
to jump, right-click zeroes the digit and those below. It tunes a live radio
or moves the VFO in a capture, like the channel box beside it.

**Carriers off their centre.** UK muxes sit at {-333.333, -166.667, 0,
+166.667, +333.333} kHz from the raster. After the receiver locks the app
reads the carrier's offset and retunes the radio onto it
(`auto_centre`; the note says "C28 is -167 kHz off: retuned"): from the
receiver's own carrier frequency error while it is locked, else (a signal it
cannot lock to, such as a T2 mux) from the spectrum's block edges, taken only
if the result is within 40 kHz of one of those five offsets (the spectral
estimate alone was out by +65 kHz on C23, whose real error is +78 Hz). Learned
offsets are remembered per channel (`offset_C28=-166667` in prefs.txt); the SDR
window lists them and can forget them. Live: C28 and C30 both go to -166.667
kHz, C26 and C23 stay at 0.

**SDR settings (the SDR button).** `frontend.rs` works on the samples after the
level reading and the recorder, so captures stay raw: **DC removal** (on by
default; the offset it removes is shown), **I/Q balance** (blind image
correction y = x - b x*, b = E[x^2] / 2E|x|^2, off by default; shows the image
rejection in dB), the HackRF's **baseband filter** (auto = 0.75 x the sample
rate, or any of the chip's 13 widths) and **frequency correction** in ppm
(`HackRfControl`'s trim). The front end costs 0.7 % of a core and is its own
line in the stage list. Settings persist (`sdr_dc`, `sdr_iq`, `sdr_filter`,
`sdr_ppm`, `auto_centre`).

**Optimise gain** (in the SDR window): tries total gains in 4 dB steps from 20
dB under the present setting (LNA first to 40 dB, the VGA for the rest), 0.8 s
to settle and 1.3 s to measure each, stopping on clipping, a level over -12
dBFS (an OFDM signal's peaks are 10 dB over its rms, so that keeps them off the
8-bit converter's limit), two points 3 dB under the best, or 90 dB; then the
LNA/VGA splits (16, 24, 32, 40) of the best total. The figure is the MER while
the receiver is locked, else the spectrum's C/N. Of the points within 0.3 dB of
the best it takes the coolest. It puts back the old gains if cancelled or if
nothing could be measured. Live on C28: from LNA 32 / VGA 8 (24.1 dB MER) it
found **LNA 40 / VGA 4, 24.6 dB MER at -14 dBFS in 11 settings** (about 25 s).
(The first version took -6 dBFS as its limit and chose LNA 32 / VGA 16, 24.9 dB
at a peak of 100 % of full scale: better on the day, no headroom for a fade.)

**Maps** (a new bottom tab, no time on any axis; the old "heatmaps" tab, which
is, is now **history**). From the left:

1. *paths: delay and Doppler shift*, the sounder's map as before.
2. *fading by carrier: frequency and Doppler shift* (new, `FreqDoppler` in
   `delay_doppler.rs`): the Doppler spectrum of the channel at each carrier,
   from the same history of pilot rows, every other pilot, about once a
   second. A static channel is a line at 0 Hz; two paths that move differently
   show as patches that swell and shrink from carrier to carrier, where they
   add and cancel. (The test puts a static path and a moving one 3.5 us later:
   the moving one is 6 dB down, as made, and its power varies by over 6 dB
   across the band.)
3. *error cloud* (`errormaps.rs`): every data cell's error against its nearest
   constellation point in one density map, with 1 and 2 sigma rings: round is
   noise, stretched across the radii is phase noise, an ellipse an I/Q
   imbalance or compression, off-centre a bias. The square edge on 64-QAM is
   the neighbouring decision regions.
4. *MER per point*: the signal-to-noise ratio at each point of the
   constellation (8 x 8 for 64-QAM). Outer points worse than inner ones show
   compression or a clipping converter; the corners worse than the rest a phase
   error. On C28 the corners are 1-2 dB under the rest.

3 and 4 are made in the GUI from the scope's data cells (a few thousand, ten
times a second, decayed by 0.9 per snapshot), so they cost the receiver
nothing; the sounder and the scopes run only while the tab is open.

**History** keeps the earlier three waterfalls: gain per carrier, SNR per
carrier and echoes over the last 25 s, from the receiver's own per-symbol
estimate (`Command::Heat`, 20 or 60 rows a second).

**60 fps ("smooth").** The display list has 60 fps; at that rate the engine
cuts the spectrum waterfall's rows three times as often (a third of the
segments per row) and publishes every 33 ms, history rows come at 60 a second,
and the traces' smoothing is reduced to match. Whole-app CPU on the C23
capture, decoding at 10 MS/s (percent of one core):

| bottom | 15 fps | 60 fps |
|---|---|---|
| none | 39.1 | 54.4 |
| scopes | 43.0 | 57.1 |
| maps | 41.7 | 61.6 |
| history | 40.3 | 55.8 |

Thread by thread (maps): the engine thread is 35 % at both rates; the GUI
thread goes from 3 % to 21 % and a second thread (unnamed, probably the
graphics driver's) from 2 % to 10 %. The
frames themselves cost 0.8 ms (logic and layout), so the rest, about 3 ms a
frame, is egui's tessellation and the OpenGL driver on this laptop (an AMD
iGPU and an RTX 3050 behind a remote display). It is opt-in, and 15 fps stays
the default.

Tests: the workspace passes and clippy is clean (new: the frequency-Doppler
map, the heat rows and smooth pacing in the engine thread, the sounder's map in
the thread, the SDR front end, the optimiser, the error maps). New test aids:
`--bottom maps|history`, `--window sdr`, `--optimise` (start the optimiser
after the lock), `--fps 60`.

## Remembered settings (2026-10-23)

Asked for: remember the last channel, the gains, and whatever else is worth
keeping. The app now writes `%APPDATA%\DecDVB-T\prefs.txt` (beside the SDR
window's settings, the learned carrier offsets and the player's volume) with:
the radio's channel and exact frequency (so a carrier found off its centre, or
a frequency typed on the dial, comes back), the source (HackRF or the IQ file
last opened, offered again, not started), the HackRF's sample rate, LNA, VGA
and RF amp, the decode switch, the bottom panel, the display rate, the Doppler
range, and the window's size. A change is written once it has held still for
0.8 s (a gain dragged across many values writes once) and again on exit, and
nothing is started by it: the app still waits for Start. The command line wins
over what was kept (and what it gives is kept in turn). **Screenshot runs
neither read the view settings nor write anything**, so a test comes out the
same each time and leaves the next real run as it found it; they do read the
gains, the channel and the SDR settings, as the radio is the user's.

## M5 + M6 — DVB-T2: the modulator and the receiver (2026-10-23)

Asked for: the DVB-T2 modulator and receiver, validated on the real UK
multiplex on C30 (256-QAM 2/3 rotated, 32K extended, GI 1/128, PP7, 64K LDPC,
one PLP, 202 FEC blocks a frame). Done: it decodes C30 from a HackRF capture
and live, 13 services, no block lost in 120 s, the engine thread at about 36 %
of real time and the whole app at about 125 to 165 % of one core (the FEC on its
own threads). 360 tests pass, clippy and rustfmt are clean.

**What is supported.** SISO T2 base: every FFT size (1K to 32K, normal and
extended carriers), every guard interval and pilot pattern (PP1 to PP8), QPSK
to 256-QAM with or without rotation, 16K and 64K LDPC at rates 1/2 to 5/6, one
PLP of type 1 (the first data PLP) with TIME_IL_TYPE 0, frame-closing symbols,
and the PAPR tone-reservation carriers' positions. **Not yet:** MISO, FEF
parts, T2-Lite rates 1/3 and 2/5, time interleaving over several frames
(TIME_IL_TYPE 1), picking a PLP other than the first, several PLPs.

**The modulator** (`decdvbt-mod::t2`, `decdvbt-ofdm::t2`, `decdvbt-fec::t2`;
after gr-dtv's DVB-T2 blocks): P1 (C, A and B parts, S1 and S2 by DBPSK), P2
symbols with L1-pre (BCH 3072/3240 + LDPC 1/4 short, BPSK) and L1-post
(BCH 7032/7200 + LDPC 1/2 short, BPSK to 64-QAM, interleaved), data symbols with
scattered and continual pilots (the tables generated from gr-dtv's C by
`scratchpad/gen_t2_tables.py`), edge pilots and the PRBS-signed pilot values,
the frequency interleaver, the PLP chain (BB scrambler, BCH, LDPC with T2's
own tables for 2/3 normal and 3/5 short, parity and column-twist interleaver,
demultiplexer, QAM mapping with rotation and the one-cell Q delay, cell
interleaver, time interleaver, HEM BBFRAMEs with ISSY and CRC-8). `decdvbt
modulate --t2 uk --seconds 2.4 --cn 30 --cfo 50123 --sco 4 --path 0:0 --path
3:-12 x.cs8` makes a T2 test signal through the M2 channel models (`uk`, or
`small`, or a spec like `8k-1/16-pp4-16qam-3/4-short-ext`).

**The receiver** (`decdvbt-rx::t2`), a frame at a time (a T2 frame is 217 ms
here; the time interleaver spans one):

- `p1det`: P1 by two running coherent sums (C against A's start, A's end
  against B); the carrier offset from their phases to ±4.5 kHz and the whole P1
  spacings from where its carriers sit; S1 and S2 by DBPSK.
- `receiver`: acquisition on a quarter-second stretch resampled to the signal's
  native 64/7 MS/s; the guard interval chosen as the longest window whose
  cyclic-prefix correlation is within 90 % of the best, the extended mode and
  the rest confirmed by reading L1-pre; then per frame: P1 near where it is
  expected, the symbols' positions by their cyclic prefixes and a robust line
  through them (the clock from its slope, the carrier from the prefixes'
  phase), L1 read again every frame (and the lock dropped after three misses),
  the PLP's cells to the FEC stage.
- `frame`: each symbol's transform (early start, the fractional sample undone
  by a phase ramp), the symbols aligned to the first data symbol by the
  continual pilots (a coherent search over ±3·10⁻³ rad a carrier, then the
  slopes' trend fed back to the clock), the channel from the scattered-pilot
  lattice (a least-squares polynomial in time through the nearest four
  sightings, a six-point line past the ends; a polyphase windowed sinc across
  frequency), the noise from the continual pilots, per-cell variance
  `σ²/|H|²`, the data cells deinterleaved as they are written. P2 symbols are
  equalised from their own pilots, the closing symbol likewise.
- `plp` and `pipe`: the cells to transport packets on a thread of its own with
  two frames in flight and 2 to 4 worker threads (a quarter of the machine's
  threads): time and cell deinterleavers, the rotated-QAM demapper to bytes,
  the bit deinterleaver, LDPC, BCH, the HEM/NM unpacker. A block whose LDPC
  stalled is passed to BCH anyway (the information bits are usually right).
- `auto` (`AutoReceiver`): DVB-T and DVB-T2 acquisition run side by side while
  there is no lock and the one that locks keeps the stream. The engine thread,
  `decdvbt decode` and the GUI use it.

**What the real signal taught** (see also the `hackrf-rf-amp` memory): the
HackRF needs its RF amp on; C30 has two paths almost as strong as each other
(6.5 and 8.4 µs) and so deep notches, and the channel moves, so a pooled
static estimate fails and each symbol is aligned on its own; the 32K mode
needs the clock followed by resampling (the interpolator's step is steered
from the timing line); the first and last symbols of a frame got 3 to 4 dB
less SNR than the others until the pilots were extrapolated by a line, not a
cubic; a window that starts a little behind the block completing the last one
used to be zero-filled and cost a frame in twenty (`Window::with_history`).
Wider least-squares windows raise the MER the pilots show by up to 2 dB and
lose two or three times as many blocks: the estimate's errors are then
correlated, which the LDPC code likes less than independent noise.

**CPU.** The first receiver ran at 0.05× real time on the C30 capture. Now the
whole chain costs about 105 to 125 % of one core of the machine's (a Ryzen 7
4800H) in real time, the engine thread 36 to 44 % of it (the OFDM work), the
rest on the FEC stage's threads (two to three of them, 35 to 46 % of real time
each). The GUI on top is a few percent: the whole app, live on the HackRF with
the scopes up, reads about 107 % in the GUI's own meter.
Per stage, % of real time of one core, live: resampling 12, the symbols'
transforms 5, the channel estimate 9, the rotated demapper 13, deinterleaving 6,
bit deinterleaving 4, **LDPC 55 to 60**, BCH 2. Seconds of one thread for 6 s of
the capture, first and now: LDPC 56 → 3.9, rotated 256-QAM demap 12 → 0.8,
equaliser 2.2 → 0.7, deinterleave 0.75 → 0.4.

What did it, in the order of the gain: the LDPC loops compile to scalar code
(about ten cycles a lane) and a hand-written AVX2 layered min-sum kernel
(`ldpc/avx2.rs`, 58 µs an iteration for a 64K block against 760; 75 before two
blocks at a time were taken through a layer's edges with their accumulators in
registers) gives the same decisions bit for bit (a test compares them), with the
posteriors loaded and read out by vector code (272 µs to 22 µs a block); normalization 15/16 and an
LLR scale of 8 (found on the off-air signal, not on Gaussian noise) cut the
iterations a block from 12.3 to 7.5 to 8.6 and the failed blocks tenfold; the
rotated demapper is exact and O(G) a cell (for each level of one axis the best
level of the other is the lattice point nearest a parabola's vertex), eight
cells at a time in AVX2 with the transposition and the quantising in registers
(17 ns a cell against 1420); the equaliser's sines and cosines became phase
ramps by recurrence, the time interpolation's weights and the pilots' values
are worked out once, and the frequency interpolation puts the phases in vector
lanes; the deinterleavers run on the workers and scatter each received cell into
a block that fits the cache; the FEC stage takes as many worker threads as it
needs (threads running side by side slow each other, so four cost 20 % more CPU
than two for the same frames); the radio-rate interpolator has 16 taps. Per
stage times: `t2_decode`, `decdvbt decode` and `t2_bench`. **What is left**: the
LDPC (still about half of everything: 8 iterations of 58 µs a block; the
syndrome check could be folded into the layer pass for another 8 %), resampling (12 % of the engine thread: skipped if the radio ran at
64/7 MS/s, which the HackRF can), and the unrotated demapper (scalar: 17 ns a
cell).

**Thresholds** (`decdvbt-fec::t2::threshold`, generated by `t2_thresholds`): the
C/N this decoder needs per mode with the noise known exactly, 0.5 dB (QPSK) to
1.3 dB (256-QAM 2/3) above the standard's TS 102 831 Table 44 (non-iterative
demapping, 8-bit min-sum). The card's margin is against these: C30 shows
+1.4 dB (MER 20.8 dB against 19.4).

**GUI and CLI.** The card names DVB-T2, the mode, the PLP, the FEC and the
LDPC blocks lost and iterations a block; the scopes show the pilots and the
equalised cells (the ideal grid for an unrotated constellation), the channel's
gain, SNR and impulse response come from the frame's estimate, and the
services, the programme guide and the TS analyser work as for DVB-T. `decdvbt
decode C30 --amp --lna 32 --vga 8 -o x.ts` decodes live (the gain flags are
new); both it and the GUI say which standard they found. Tools: `cargo run
--release -p decdvbt-rx --example t2_decode -- x.cs8 out.ts [MS/s] [vfo_hz]
[seconds]` (env `THREADS`), `-p decdvbt-fec --example t2_bench -- 256 2/3 normal
rot 22`, `--example t2_thresholds`, `--example t2_tune`.

**Live results** (HackRF, C30, `--amp --lna 32 --vga 8`): 120 s, 551 frames, none
lost, 111302 LDPC blocks, none lost, 3.2 million transport packets with no
continuity jump and no bad section; `decdvbt decode --plp N` picks a PLP.

**Known gaps and next steps.** MISO, FEF, T2-Lite and
TIME_IL_TYPE 1 are not decoded, and only one PLP at a time. `decdvbt scan --classify 5` tunes each channel that looks like digital TV and
says DVB-T or DVB-T2 and its mode (live: C22, C23, C25, C26 and C28 are DVB-T,
C30 DVB-T2 256-QAM 2/3 rotated). The Channel and Maps tabs work on T2 too (see below). The channel's moving echoes at 32K are followed frame by
frame, not within one.

## The UK guide's compressed text (2026-10-24)

Asked for: decode the Freesat/Freeview Huffman text (DVB text type 0x1F), with
the tables taken from MythTV (the download approved). The UK terrestrial
guide sends event names and descriptions that way; they showed as noise. Now
`decdvbt-ts::huffman` decodes them (after MythTV's `freesat_huffman.cpp`,
GPL-2.0-or-later), with tables 1 and 2 (2027 and 3160 codes) generated into
`huffman_tables.rs` by `tools/gen_huffman.py` from MythTV's
`freesat_tables.cpp`; `dvb_text` uses it for a 0x1F string. On C30 the guide
reads "Pointless", "The Chase", "the regional news", "Gymnastics: World
Championships" and so on, and the services list's now-showing lines too. A test
round-trips text through both tables with an encoder built from the same
tables.

## BBC Red Button: the DSM-CC object carousel (2026-10-24)

Asked for: UK Red Button (MHEG-5 in DSM-CC object carousels), in three
stages: the carousel, an MHEG-5 engine, and a GUI overlay.

**Stage 1, the carousel.** `decdvbt-ts::dsmcc` reassembles an object
carousel (ISO/IEC 13818-6, TR 101 202) from a service's DSM-CC PIDs: the DSI
gives the service gateway, every DII of the carousel its modules (sizes,
versions, the zlib compressed-module descriptor), the DDBs fill them; a
complete module is inflated (miniz_oxide) and its BIOP messages
(`decdvbt-ts::biop`: directories, files, streams and stream event objects,
IORs) indexed by key. Paths are walked from the gateway on demand
(`lookup("/a")`, `file(..)`, `tree()`, `dump(dir)`), a DII with a new module
version replaces the module, and stream event descriptors (table 0x3D) are
queued for the engine. A block of a module already in is dropped after its
8-byte header, before the CRC. The PMT parser now keeps each stream's
component tag, carousel id and data broadcast id.

`decdvbt decode SOURCE --carousel SID DIR` writes the service's carousel to a
folder; SOURCE may now be a `.ts` file (read as it is, no receiver). On the
4 s C23 capture (decoded to a TS first): service 4416 (BBC Red Button) boots
carousel 1 on PID 7201 (data broadcast id 0x0106); in 4 s, 21 of its 125
modules come in (720 kB), 88 files: `/a` (the application, ASN.1 MHEG-5),
`/s/init.mhg` and other scenes, `/g/*.png`, configuration in `/d/r/*.cf`. Five
unit tests on synthetic carousels (two modules, one compressed; out-of-order
blocks; a new module version; TS packetised; a stream event).

## The channel sounder and the maps on DVB-T2 (2026-10-24)

Asked for: the Channel and Maps tabs for a T2 lock (they said "DVB-T only").

- **Sounding from the receiver's own estimates** (`decdvbt-rx::t2::sounding`):
  the T2 frame equaliser already has the channel at every node of the pilot
  lattice (every D_x-th carrier) at every data symbol. While the sounder is
  asked for, it hands out one row every D_y symbols. The rows have the
  window's early start taken out (a phase ramp, so the main path sits at 0 µs),
  each frame is turned to continue the last frame's final row (a common phase
  and a slope, as the in-frame alignment does symbol to symbol), and the rows
  are spread evenly in time across the P1/P2 gap by linear interpolation. They
  feed the same `ChannelHistory`, fading rows and status as DVB-T's sounder;
  a `Sounding` trait lets the engine publish either. While T2 has the lock,
  DVB-T's sounder rests. The cost is a copy of the frame's channel: sounder
  0.4–1.2 % (the maps once a second), decoder unchanged.
- **`sounder::Geometry`** now names its FFT size and guard as text, keeps
  DVB-T's own types in `dvbt`, and has `pilot_step` (12 for DVB-T, D_x for T2).
- **Rotated constellations on the grid**: the scopes' data cells and the error
  maps take one FEC block's points, with the interleavers undone, each point's
  I and Q rejoined from its two cells (the cyclic Q delay) and turned back by
  the rotation angle (`t2::plp::first_block_points`). The data scope shows
  256-QAM's grid ("rotated (shown turned back)"), and the error cloud and MER
  per point work for T2. The error maps are keyed by grid size, not by DVB-T's
  `Modulation`.
- Test: `crates/decdvbt-rx/tests/t2_sounding.rs` (8K PP4, an echo 6 µs on,
  −6 dB, moving at +8 Hz: found at its delay and Doppler, nothing at the
  mirror). On C30 the map shows the main path and short echoes within 5 µs.
- A T2 signal with decoding off gets DVB-T's sounder, which says no DVB-T was
  found and to turn decoding on.


**Stage 2, the MHEG-5 engine.** New crate `decdvbt-mheg` (UK Engine Profile,
ETSI ES 202 184 / D-Book 7), with MythTV's freemheg (GPL-2.0-or-later) as the
reference, credited per module:

- `asn1`: the binary (BER) interchange form into a tree, typed by tag as
  freemheg does; `tags`: the ISO/IEC 13522-5 Annex A tags.
- `obj`: groups, links, the five variable types, resident programs,
  rectangles, bitmaps, text (and hypertext / entry fields drawn as text),
  token and list groups, streams with their video and audio components.
- `engine`: the life cycle (preparation, activation, deactivation,
  destruction), links fired by synchronous events onto the action stack and
  asynchronous ones queued, about 60 elementary actions (TransitionTo,
  Launch/Spawn/Quit, SetData, Test/SetVariable and arithmetic, Call/Fork,
  SetTimer, SendEvent, token-group Move and CallActionSlot, position, size,
  colours, stacking, persistent store, GetEngineSupport, Clone…), the UK keys
  and input registers, content from the carousel. Where freemheg blocks for a
  file still to come, the engine parks the action and retries on the next
  tick; a missing file's engine event comes after half a second (the BBC app
  retries at once, which spun); after the last application quits the boot
  application starts again.
- `programs`: the UK resident programs (GCD, FDa, GDW, Rnd, CTC, CTO, GSL,
  GSS, SSS, SES, GSI, TIn, BSI, GBI, CCR, CGR, VTG, GDA, GIS, WAI…). Strings
  are bytes (the BBC keeps colours as raw bytes in its strings).
- `render`: a 720x576 RGBA canvas drawn only when something changed:
  rectangles, PNG bitmaps (scaled, tiled, offset), text with the UK markup
  (ESC 'C' colours), CR breaks, tabs, wrapping and justification, video as a
  clear hole. Tiresias Screenfont is not on this PC: Segoe UI squeezed to
  85 % across stands in, and a line written for Tiresias that is up to 18 %
  too wide is squeezed to fit rather than wrapped (the BBC breaks its lines
  for Tiresias). MPEG-2 I-frame bitmaps are not decoded yet.

`cargo run --release -p decdvbt-mheg --example redbutton -- out/live_c30.ts
--sid 17536 --keys wait,100,wait,15 --png out/rb.png` runs a service's app
headless and saves the canvas (`--trace` for the engine's trace); `census`
and `mhegdump` list and print MHEG objects.

**Results.** The 4 s C23 capture holds 21 of the BBC carousel's 125 modules,
and seven short C23 captures together 71: `/a` boots and reaches the trigger
scene, but the bridge scene is missing. The live C30 recording of yesterday
(BBC One HD, the same BBC application) holds 89 of its 94 modules, and there
the red key shows BBC Red Button Text: the bridge menu (News Index, Sport,
Weather…, with page numbers and the date), SELECT opens the News Index, then
Main Headlines, then a story with "Page 1 of 2"; page numbers can be typed,
GREEN gives full-screen TV, CANCEL steps back and out. The engine's whole
10-second session costs 10–20 ms and a full redraw 4–11 ms. A live C23 tune
was not possible (the HackRF read -44 dBFS on C23 today: aerial or radio).

## CPU pass on the DVB-T2 receiver (2026-10-24)

Asked for: more CPU savings after the T2 sounder. Measured on the C30 capture
with `t2_decode` (CPU time per 8 s of signal), the equaliser alone with the
new `cargo run --release -p decdvbt-rx --example t2_eq_bench` (the fastest of
40 runs of one UK frame: other work on the machine can only slow a run).

| stage | before | after |
|---|---|---|
| LDPC | 3.92 s | 3.43 s |
| resampling | 0.93 s | 0.64 s |
| channel estimate | 0.61 s | 0.46 s |
| receiver in all | ~1.0 core | ~0.9 core |

- **LDPC: the parity count folded into the layer pass.** Each layer XORs the
  posteriors it gathers (one instruction an edge block) and counts its
  unsatisfied checks as it read them. A count above `EXACT_BELOW` (64) stands
  in for the exact count in the plateau test; a small one is counted exactly,
  so a decode still stops on the iteration it converges in and the stall test
  sees the truth. A failed decode reports the exact count. 57 → 47 µs an
  iteration (rate 2/3 normal), the same iterations a block on C30 (8.6), every
  block decoded; the kernel still matches the portable code bit for bit (both
  do the same).
- **Resampling: four outputs at a time.** Each output's dot product used to end
  in its own horizontal reduction, which cost more than its multiplies; four
  outputs' eight sums now reduce together into their interleaved (re, im)
  pairs, stored in one go.
- **Equalising:** every carrier divided by the channel in an AVX2 pass (same
  arithmetic, same results; the plain loop compiled scalar, 2 ns a carrier),
  then the data cells written in their logical order, each read from its
  carrier through a precomputed table per symbol phase (`Demod::source`),
  instead of written where the frequency interleaver sends them. Medians by
  selection, not sorting. 17.5 → 15.4 ms a UK frame.
- **Cell buffers recycled**: the FEC stage hands each frame's cell and
  variance buffers back (`Done::buffers`), and the data cells are written
  straight after P2's into the job's buffer (`Demod::data_cells_into`), no
  longer copied into a second one.

Tried and dropped: dividing inside the ordered gather (dependent random reads
with a division: slower, 16 against 14 ms).

What is left, largest first: LDPC (~43 %), demapping (~12 %), resampling
(~8 %), the channel estimate (~6 %, of which the frequency interpolation is
the most), deinterleaving (~6 %), the FFTs (~5 %).


**Stage 3, the GUI.** The player window has a Red Button row under its
controls (the red button starts and stops it; the colour keys, the arrows,
OK, Back and Text are the remote), and the keyboard works as the remote
while it runs (R starts it; arrows, Enter, Backspace/Esc, 0–9, R G Y B, T).
`apps/decdvbt-gui/src/redbutton.rs` runs the engine on the GUI thread and
draws its canvas over the picture, stretched to it as a TV does; when the
application scales the video (the BBC's quarter-screen window) the picture
is drawn into that rectangle. The hub (`decdvbt-engine::hub`) receives the
service's carousel only while it is asked for, through a `CarouselLink` in
its outputs (so `rx.rs` did not change): it makes the carousel from the PMT,
hands it the service's DSM-CC packets a batch at a time under one lock, and
shares the stream's time, local offset and ids with the engine (the BBC's
clock then shows the broadcast's time). `DECDVBT_REDBUTTON=wait,15` starts it
on the playing service and presses keys, for screenshots.

**Test signal.** No capture had enough of a carousel for the GUI (it replays
IQ files, and the 8 s C30 capture holds 18 of 73 modules), so
`captures/sim_rb_c30_546.000MHz_10Msps.cs8` was made: the live C30 stream cut
to BBC One HD and the SI (padded with null packets to keep its pace at DVB-T's
24.13 Mbit/s), modulated as 64-QAM 2/3 8k at 32 dB. `decdvbt-gui
captures/sim_rb_c30_546.000MHz_10Msps.cs8 --decode --play 17536` with
`DECDVBT_REDBUTTON=wait,wait,wait,15` shows BBC One HD with the BBC News page
over it and the picture in the BBC's window. CPU: 59.4 % against 57.4 %
without the Red Button (a 40 ms repaint request first cost 9 points by adding
frames; the engine now asks for 100 ms).

**Not done.** A live C23 tune in the morning (the aerial was faulty; see
below for the C23 recording made after it was fixed). MPEG-2 I-frame bitmaps,
stream events to the application, the interaction channel, ListGroup's item
actions, sliders and entry-field editing are not done; the font is a
squeezed Segoe UI, not Tiresias.

**C23 (2026-10-24, later).** With the aerial fixed, the other session
recorded 120 s of C23 (`captures/c23_490.000MHz_10Msps_amp120.cs8`, amp on,
LNA 40, VGA 16; 18.5 dB, about 3 % of RS blocks lost), whose stream holds
123 of the BBC carousel's 125 modules. `decdvbt-gui
captures/c23_490.000MHz_10Msps_amp120.cs8 --decode --play 4164` then shows
BBC One's Red Button live from the IQ file: red opens Red Button Text
over the picture, SELECT opens the News Index, and again today's Main
Headlines ("Storm Boelo to bring more wind…"), the picture in the BBC's
window and the BBC clock reading the broadcast's local time (08:01, as on the
picture). The BBC Red Button service itself (4416) shows its splash page
("Welcome to BBC Red Button… Press RED to visit now").

Fixes found on C23: GetServiceIndex and GetBasicSI now read a service list
(this multiplex's services, then the SDT's others; "dvb://233a..1044" with
empty fields matching any), and TuneIndex to a service of the multiplex makes
the player play it, the Red Button following to its application (the splash's
red key moves to BBC One); font attributes with a line spacing of 0
("plain.36.0.0") take the font's own; and the red key that starts the Red
Button waits until the application has sat idle in a scene for 1.5 s (the
BBC's init scene took it and ignored it when the carousel was still coming).
`DECDVBT_REDBUTTON_TRACE=1` prints the engine's trace from the GUI.

## Effective SNR for the margin (2026-10-24)

Asked for: a margin that does not read "good" while the decode is bad (live
C23 with deep notches showed +2.3 dB and lost blocks). The band's mean SNR
flatters a notched channel: the faded carriers' bits are where the errors
are. `decdvbt-rx::capacity` turns each cell's SNR into the mutual information
its constellation carries (BICM, Gray square QAM as two PAM axes), averages
it over the cells, and maps the average back to the SNR of a Gaussian
channel that carries as much (MIESM). Tables are made once per constellation
and read by the SNR's float bits (no logarithm per cell); DVB-T measures one
symbol in four, T2 every eighth cell of a frame, a rotated constellation's
point at the mean SNR of two cells half a frame apart (its I and Q fade
apart). The card's margin is now the effective SNR less the QEF C/N; the
mean is the MER row. The CLI prints both.

| capture | mode | mean | effective | margin | BER before RS |
|---|---|---|---|---|---|
| C23 (Oct 23) | 64-QAM 2/3 | 21.5 | 18.0 | +1.5 | 3.4e-6 |
| C23 (Oct 24, 120 s) | 64-QAM 2/3 | 18.6 | 16.0 | -0.5 | 2.1e-3 |
| C26 | 64-QAM 2/3 | 19.1 | 17.5 | +1.0 | 1.5e-5 |
| C28 | 64-QAM 3/4 | 23.7 | 20.0 | +2.0 | 8.9e-7 |
| C30 (T2) | 256-QAM 2/3 rot | 20.8 | 19.4 | 0.0 | 0 blocks lost, 8.6 LDPC iterations |

The DVB-T QEF figures are EN 300 744's ideal receiver; per-carrier noise
estimates are noisy, which pulls the average information down a little, so
the effective SNR leans pessimistic (a clean channel reads 1.5–3.5 dB under
its mean). It ranks the captures as their error rates do.

## Waterfall: an even scroll, one whole pixel a row (2026-10-25)

Asked for: DecDAB's waterfall improvements (its commits b655ffc and 6dcef35:
smoothness, no sub-pixel blending).

- The waterfall's FFTs run on a thread of their own (`decdvbt-engine::waterfall`,
  ported from DecDAB): the receive thread copies out the windows a row needs,
  the thread transforms and averages them and lets the rows out on a steady
  clock at the display's rate, two rows behind, so the bursts in which the
  HackRF and the file reader deliver samples never reach the screen. Rows
  still held when the source ends go out (DecDAB's version dropped them).
- `Command::DisplayFps(fps)` replaces `Smooth`: a waterfall row a frame, the
  transforms a row scaled so the cost a second is unchanged; results, scopes
  and measurements handed over once a frame (every 33 to 100 ms).
- The GUI queues the rows and puts one on each frame (two to catch up, the
  backlog cut far behind). The waterfall holds exactly as many rows as it
  has physical pixels and sits on pixel boundaries, so each step of the
  scroll is one whole pixel and no row is smeared across two (linear
  filtering then samples texel centres only). The trace's smoothing keeps a
  0.12 s time constant at any rate. The fps list loses "(smooth)": the rate
  is the waterfall's speed (60, 30, 15 px/s).
- On the 120 s C23 capture: 94 % of frames take exactly one row at 60 fps, 83 %
  at 15 fps (the screenshot report counts frames that took 0, 1 and 2). CPU
  old against new, interleaved runs: 60 fps 80 % / 82 %, 15 fps 50 % / 58 %,
  within the runs' own spread (±15 points between identical runs).

## A T2 signal under the threshold: an honest margin and a bounded CPU (2026-10-25)

Live C30 stopped decoding: the receiver stays locked (L1 every frame, no frame
lost) but the signal measures MER 15.9 dB against the 19.4 dB 256-QAM 2/3
needs, so no LDPC block decodes. Lower gains read worse (LNA 32 12.3 dB, LNA 24
10.8 dB): noise-limited, the aerial's signal weaker than the day before. Two
things were wrong with how the app took it:

- The card showed **+2.7 dB** (the spectrum's C/N) because it used the
  measured SNR only while packets came. It now uses the effective SNR as soon
  as L1 is read: the simulated case reads −7.2 dB in red.
- The FEC tried every block of every frame for 50 iterations: **~360-400 %
  CPU** for nothing. Now (1) while the FEC stage is behind on all its threads
  the LDPC gives a block up once its unsatisfied checks stop falling
  (`decode_hurried`), and (2) after two frames with no block decoded only the
  first 12 blocks of each frame are tried (`PlpDecoder::set_limit`, the
  frame's block plan kept whole), until one decodes again; the card says
  "FEC: below threshold". Failing signal: 359 % → 59 %; live C30 at 15.9 dB:
  the FEC stage on 1 thread at 23 % of real time. A good signal is unchanged
  (C30 capture: 5252 blocks, 0 lost, 8.7 iterations).

## A moving vehicle: Doppler-adaptive channel estimate (2026-10-26)

Asked for: decoding from a moving vehicle without costing a static signal
any CPU. The recording: `exe/captures/c29_538.000MHz_10Msps_20261026-074850.cs8`
(C29, 538 MHz, 28 s, HackRF in a car).

**What the recording is (measured).** DVB-T 8k, guard 1/32, 64-QAM 3/4
non-hierarchical (QEF C/N 18.0 dB), 48 services. The level falls all the way:
in-band power over the band-edge floor 25 dB in the first seconds, 19–21 dB
at 8–13 s, 14 dB at 16 s, 6–10 dB from 17 s on; the ADC clips 0.1–0.6 % of
samples in the first 6 s (the gains were set for the strongest point).
Decoded before this work: seconds 0–8 mostly clean, 8–14 losing more and
more blocks, 15–16 almost nothing, 17–28 nothing (24 relocks: the signal is
under 64-QAM's threshold by 8 dB or more there, which no receiver change
can fix; only gain control or a more robust mode would).

**Why it failed (measured, `cargo run --release -p decdvbt-rx --example
doppler -- FILE START DUR`).** Not Doppler. The continual pilots' Doppler
spectrum (each is seen every symbol: ±540 Hz) is a line at 0 Hz holding
94–98 % of the power, with a skirt to about ±35 Hz and a weak tail to
±100 Hz; the mean Doppler is within ±2 Hz (the carrier loop follows it).
The ICI that spread implies is −26…−29 dB (derived: (2π·σ·Tu)²/12), under the
noise. The correlation fit (below) calls the channel steady at almost every
point. MER per symbol is narrow (16–20 dB) with no impulsive dips: the
recording is **noise-limited at the edge of 64-QAM 3/4**, MER 18–19 dB at
8–14 s against the 18 dB QEF figure. What the receiver gave away was in the
channel estimate's own noise: the IIR over the pilots (α = ½) leaves a third
of a pilot's noise in the estimate, about 0.8 dB of MER at 20 dB (derived);
and an echo-free channel was being estimated as if it could have echoes
over the whole 300 µs the pilot grid allows.

**What changed.**

- `crates/decdvbt-rx/src/mobile.rs` (new):
  - `Tracker`: the continual pilots' autocorrelation over lags 0–15 symbols
    (on every 4th symbol, smoothed over ~0.4 s), fitted by a Rician/Clarke
    model r(l) = a + b·J₀(ω·l) (a grid of ω, a and b by least squares). The
    fit gives the noise-free power (so the pilots' noise), the steady share
    and the maximum Doppler, and from them, by the Wiener-Hopf equations, the
    best filter over a grid point's last four pilots for each pilot age
    (0–3 symbols), and the modelled estimate error of that filter and of the
    IIR. A scattered share under 1.2 % is taken for measurement error (steady
    channel). The pilots within 32 carriers of the centre are left out: the
    radio's DC offset and its block-stepped correction (`engine::frontend`
    moves its estimate every 13 ms block) read as "3 % fading at 76 Hz" on
    the static C23 rooftop recording and took the time filter up wrongly.
  - `Ring`: the last four pilots of every grid point, in slots by epoch, and
    the time filter over them (8 FMAs a grid point).
  - `Denoiser`: the delay-domain filter: the grid (every 3rd carrier) is
    transformed (4096 points in 8k), only the delays a channel inside the
    guard can have are kept (late echoes to 9/8 of the guard, early ones to a
    quarter of it), and transformed back: in 8k 1/32 it keeps 14 % of the
    response, so 86 % of the estimate's noise goes (derived). Not made for a
    guard over ~1/8 of the period (it would keep most of it).
- `chest.rs`: the equaliser feeds the tracker, retunes every 256 symbols,
  uses the time filter when the model says the IIR's error is 1.8× the
  filter's and over −25 dB (off again under 1.35× or −27 dB), applies the
  delay-domain filter when the receiver asks. Also a fix: the grid points'
  own carriers took the raw IIR grid, not the filtered estimate.
- `receiver.rs`: the delay-domain filter goes on when the effective SNR is
  within 4 dB of the mode's QEF C/N **and** at least 2 % of RS blocks needed
  mending in the last 128 symbols; off over 6 dB or after 4 s of the code
  idle. `ReceiverStatus` has `doppler_hz`, `mobile`, `denoise`;
  `decode --progress` prints them. Env `DECDVBT_DENOISE=0|1`,
  `DECDVBT_MOBILE=0|1` force them, for comparisons.
- `outer.rs`: 20 uncorrectable blocks in a row no longer send the RS decoder
  back to hunting (15 packets read plus the 11 the deinterleaver holds lost
  each time); it goes quiet, keeps its alignment and carries on at the first
  block that corrects. Only 1500 in a row (~90 ms) start a new hunt.
- `modulate --k-db K` (with `--fd`): Rician fading on every path, for
  mobile test signals (`Channel::with_rician`).

**Results (measured, `decode --seconds 40`, packets without the error flag;
base = HEAD built in a worktree).**

| signal | base | now |
|---|---|---|
| C29 car (28 s) | 225 256 good, 7 291 flagged, 3 error-free s | 240 417 good (+6.7 %), 6 769 flagged, 4 error-free s |
| C29 good packets per second, s 9–14 | 14k 17k 15k 13k 12k 10k | 16k 17k 16k 15k 15k 14k |
| C23 rooftop (40 s of the 120 s) | 625 067 good, 13 904 flagged | 636 880, 3 131 flagged (−77 %) |
| C22/C25/C26/C28 rooftop | all clean | all the same |
| sim 64-QAM 2/3 8k cn27 | 88 005 good, 2 647 flagged | 105 036, 1 240 |
| sim 16-QAM 3/4 2k 1/8 TU6 cn22 | 1 833 packets (lost the lock) | 18 932 |
| sim car: ra6, Rician K 6 dB, 35 Hz, cn 26, 64-QAM 3/4 8k (4 s) | 11 791 | 60 101 (5.1×) |
| same at 70 Hz | 1 774 | 14 289 (8×) |
| same at 140 Hz | 117 | 418 (still unusable) |
| static sim cn 20 | 69 957, clean | the same |
| C30 (DVB-T2) | 7070 LDPC blocks, 0 lost | the same (T2 path untouched) |

On the moving sims the time filter is most of it (35 Hz: 12 400 good with
neither filter, 17 300 with the delay filter alone, 49 900 with the time
filter alone, 60 100 with both); its Doppler reads 32–38 Hz against the 35 Hz
put in, 120 Hz in the unit test. On C29 the time filter stays off (correctly:
forcing it on gives 231 220 good, worse), and the gain is the delay filter
and the RS change. MER on C29 by the doppler example (cells clipped at 0.1):
20.4 → 20.8 dB at 2 s, 18.5 → 18.8 at 9 s, 18.1 → 18.4 at 12 s.

**CPU (measured).** The equaliser, fastest of 5 on the C26 capture: 36.2 µs a
symbol as before (IIR), 40.1 with the time filter, 49.4 with the delay
filter, 53.6 with both; i.e. at 1082 symbols/s +0.4 % and +1.4 % of a core.
A static signal with margin (C26, C28; decode, fastest of 7): receiver
1154 → 1162 ms, equalise 3.9 → 4.0 % of real time (noise). A static signal at
its edge with the code mending blocks (C22: 25 % of blocks corrected; C23)
does get the delay filter, +1.6 % of a core.

**Not done / open.** The last 12 s of C29 are under threshold whatever the
estimate. The time filter is causal (the 4 pilots up to 3 symbols old): a
non-causal one (a few symbols' delay) would cut the estimate error by another
~5 dB at 35 Hz (derived from the model), and over ~100 Hz in 8k ICI
(−22 dB at 140 Hz) needs cancelling, neither done. The front end's DC
estimate steps once a 13 ms block (seen as a 76 Hz modulation on the centre
carriers): left as it is, the tracker now skips those carriers. Doppler
shows in `ReceiverStatus` but not yet in the GUI.

## Narrowband interference: CW tones cancelled and erased (2026-10-26)

Asked for: a CW carrier ("birdie") inside the channel must not wreck the OFDM
decode, for as near no CPU as possible on a clean signal.

**Two stages, because a tone does two different kinds of harm.**

1. **Carrier erasure after the FFT** (`decdvbt-rx::notch`, hooks in
   `receiver.rs` and `t2/frame.rs`). A tone lands on one carrier (and a sinc
   skirt) with the same power in every symbol. A slow average of each carrier's
   raw |Y|² against the local median (blocks of 192 carriers, interpolated; a
   second pass without the flagged ones, so a strong tone's skirt cannot raise
   its own reference) flags a carrier that is 7 dB over it *and* 4.8 dB over
   the carriers six either side (a fade's peak is wider than a tone; the first
   version without the narrowness test flagged 231 carriers on C25 and made it
   worse). Skirts are followed outwards while they stay 2.5 dB over. A flagged
   carrier's interference power (its excess over the level) is added to that
   carrier's noise estimate (DVB-T: `Equalizer::noise`; T2: the cell's
   variance), so the soft decoder gives it no weight. The pilots' known boosts
   are divided out (T2's continual pilots are 8/3 in amplitude: without that
   the detector flagged 183 pilots on a clean signal). T2 also rebuilds the
   pilot-lattice nodes under a flagged carrier from the nearest clean nodes, so
   the interpolator does not spread the tone's error over the neighbours.
2. **Frequency-shift-and-subtract before everything** (`decdvbt-rx::tone`,
   hook at the top of both `push`es). A strong CW (a few dB under the signal's
   power or more) is not harmed after the FFT but before it: the cyclic-prefix
   correlation and the carrier-offset estimate take a CW for a perfect echo, and
   the receiver never acquires (measured: a −6 dB tone, no lock; erasure cannot
   help, it comes after acquisition). The detector is an 8192-point Hann FFT of
   one block in 524288 samples, power-averaged, a bin 20 dB over the local
   median inside the channel's ±3.75 MHz (hysteresis: kept to 14 dB). The
   canceller is an oscillator at the tone's frequency (phase-continuous over
   blocks and gaps) whose complex amplitude is subtracted from every sample; a
   2048-sample sum of x·conj(osc) gives the amplitude, its turn the frequency
   error (a first-order loop follows a drift or a ppm shift). Up to 4 tones. It
   copies and subtracts only while a tone is held; otherwise the block is passed
   through untouched.

Why not an IIR notch in the time domain: it costs a few multiplies every sample
for ever, notches the signal's own energy and needs a tracker for drift anyway.
Subtraction takes the tone out and leaves the signal; only the tone's own
carrier is lost, to the FEC.

Both can be switched off for comparison: `DECDVBT_NOTCH=0`, `DECDVBT_TONE=0`.
Stats: `ReceiverStatus::notch/tone`, `T2Status::notch/tone`; the CLI decode
report prints a "narrowband interference" line, the GUI scope header shows
"N CW tone(s) cancelled" / "N carrier(s) erased".

### Measured (release; `crates/decdvbt-rx/examples/notch_sweep.rs`)

Tone powers are dB over the capture's own mean power, at +1.0003 MHz (between
bins), added in memory to `sim_64qam-2-3-8k-1-32_C30_..._cn25` (DVB-T, with
white noise added to bring it to MER 19.6 dB, 64-QAM 2/3's edge) and to
`sim_t2_uk_long` (T2 256-QAM, noise added to MER 19.2 dB; the first frame of
nine is lost with a tone, while the averages build: the "off" and "erase" lost
blocks of 171-182 are that frame). off / erasure only / both:

| case | off | erase only | cancel + erase |
|---|---|---|---|
| T, no tone: pre-RS BER | 3.7e-7 | 3.7e-7 | 3.7e-7 |
| T, −25 dB | 2.9e-7 | 1.2e-7 | 1.2e-7 (not engaged) |
| T, −17 dB | 9.6e-7, MER 17.9 | 3.7e-7 | 3.7e-7, MER 14.1 incl. the tone |
| T, −12 dB | 1.5e-6, MER 15.6 | 9.6e-7 | 3.5e-7, MER 19.6 |
| T, −8 dB | 6.9e-6, MER 13.1 | 3.2e-6 but 5 locks (re-acquired) | 1.8e-7, MER 19.6 |
| T, 0 dB | never locks | never locks | 1.4e-7, MER 19.6 |
| T, +10 dB | never locks | never locks | 2.9e-7, MER 19.3 |
| T2, no tone: LDPC iterations/block | 5.1 | 5.1 | 5.1 |
| T2, −30 dB | 5.9 | 5.5 | 5.5 |
| T2, −20 dB (1 block lost off/erase) | 7.3 | 6.5 | 6.5 |
| T2, −10 dB: blocks lost / iterations | 182 / 10.3 | 171 / 7.3 | 0 / 6.6 |
| T2, 0 dB | never locks | never locks | 0 lost, 6.6 |
| T2, +10 dB | never locks | never locks | 202 lost (the first frame), 7.5 |

A tone also drifting 20 kHz/s from −2.5 MHz (and one at +3.6 MHz, near the
band edge) is followed and cancelled to the same BER at −15 and −5 dB.
`tests/cw_tone.rs` runs the modulator's T2 signal with tones at −12, −4 and
+3 dB (all decode with no bad packet) and a clean one (nothing flagged).

A single CW is cheap for the code to carry: at MER 25 dB a −10 dB tone costs
a 64-QAM DVB-T decode nothing (0 pre-RS errors with or without anything), so
below about −20 dB the erasure gains a factor of 2-3 in BER and no more.
What breaks the receiver is a tone near or over the signal's power, and the
canceller fixes that case.

**Real captures** (`examples/tone_scan.rs`: long-term spectrum, 128-bin median;
C22, C23 (incl. the 120 s one), C25, C26, C28, C30 and the C29 of 26 Oct): no
persistent CW tone in any channel. What shows: the HackRF's DC spike at the
centre (2 to 23 dB over the median, larger without the RF amp, e.g. C25 +18 dB
with it, +23 dB without), the edge continual pilots at ±3.8 MHz and
adjacent-channel edges (C25 −3.80 MHz +15 dB), nothing else over 8.5 dB. The
DC spike is the one tone found: the canceller takes it out of C25 (amp)
(pre-RS BER 9.3e-4 → 6.8e-4, 25352 → 19879 RS-corrected blocks of 70508; MER
18.8 → 20.4 dB); erasing its carrier alone changes nothing (BER equal). C26/C28/
C23/C22 and the T2 C30 are unchanged within 1 % (C30 LDPC iterations 8.6 → 8.0).

**CPU on a clean signal** (derived from release micro-benchmarks, `cargo test
--release -p decdvbt-rx bench_ -- --ignored --nocapture`, best of 20):
carrier watch 1.64 µs a symbol observed (every second symbol) + 72 µs a refresh
(every 64 symbols) at 8k = about 1.7 ms/s; at T2 32k 6.7 µs + 303 µs a frame
= about 2.5 ms/s; detector 103 µs a pass, 19 passes/s = 2 ms/s. Together
about 0.4-0.5 % of one core with nothing found. With a tone held the canceller
adds 1.7 ns/sample = 1.7 % of a core at 10 Msps (measured). Whole-receiver
wall times, fastest of N, are inside this machine's run-to-run spread (±20 %
at the time) and are not a measurement of the stage:
C26 amp, 4 s: off 1.47 s / on 1.55 s (fastest of 5), then 1.85 / 1.68 s (fastest of 8, interleaved); C28 amp under heavy load from other builds: 5.31 / 5.58 s (fastest of 10), where an idle run takes about 1.5 s.

**Not done / limits.** The carrier erasure cannot recover the cells under a
tone, only discard them (the canceller can). DVB-T's own scattered-pilot grid
is not repaired under a tone (the Doppler work is changing the chest code; the
innovation-based noise estimate already lowers those pilots' weights). A tone
that moves faster than ~16 symbols (carrier erasure) or ~0.5 kHz in a
2048-sample chunk (canceller) is not followed. Tones above 0 dB overload the
8-bit ADC before any of this (the sweeps scale the file; a live radio would clip).
The detector's ON threshold (20 dB over the median) sits above the T2 continual
pilots' own lines (seen to 17 dB in a simulated 8k signal), so a tone weaker than
about −14 dB is left to the erasure.

## Airspy R2 front end (2026-10-27)

**Done offline, not yet seen on a radio.** The cloud session that wrote this
had no Airspy; everything below is checked by unit tests and a synthetic
signal, and the list at the end is what only the owner's machine can settle.

- **`decdvbt-io` feature `airspy`** (on by default in the CLI and the GUI,
  like `hackrf`): `airspy.rs` (device, reader thread, `AirspySource`,
  `AirspyControl`, `airspy_info()` for the board id / firmware string / rate
  list), `airspy/proto.rs` (every USB request as a pure `Command`: request,
  value, index, payload), `airspy/convert.rs` (the converter). Pure Rust on
  `nusb` 0.1.14, no libairspy / libusb / DLLs; the protocol, the start
  sequence, the gain presets and the packed-sample layout are libairspy's
  (BSD-3-Clause), credited in the module comment.
- **Bias tee is never on**: the only GPIO write is `proto::bias_tee_off()`
  (no argument), sent at every open; a test checks that no start sequence can
  carry a 1 there.
- **10 MS/s only**: other rates are refused with a clear error (before any USB
  access). The ADC runs at 20 MS/s and the bulk stream is that real signal.
- **Reader thread** (as `hackrf.rs`): 8 bulk transfers of 262 144 bytes in
  flight, converted to IQ *in the reader thread* (so the receive thread does
  no extra work), a bounded queue of 64 buffers, whole buffers dropped and
  counted (`overflows()`) when the consumer is behind, `retune` drains the
  queue and skips the next 9 buffers (the in-flight transfers + one),
  receiver mode off on stop and in `Drop`. Converted buffers are recycled
  between the threads, so there is no allocation per buffer in steady state.
- **Gains**: both AGCs off. `AirspyGains {lna 0–14, mixer 0–15, vga 0–15}`
  with libairspy's `linearity(n)` and `sensitivity(n)` presets (n 0–21);
  default is sensitivity 10, **untested** on a roof.
- **Packed transfers** (`AirspySettings::packed`, CLI `--airspy-packed`, GUI
  SDR window): 8 samples in 12 bytes, 30 instead of 40 MB/s. The unpacker is
  tested bit-exact against a packer written in the test (every 12-bit value
  in every position).
- **CLI**: `--radio hackrf|airspy` (default `hackrf`) on `scan`, `capture` and
  `decode`; `--airspy-preset KIND:N`, `--airspy-lna/-mixer/-vga`,
  `--airspy-packed`. The HackRF flags and defaults are untouched.
- **GUI**: "Airspy R2" in the source list; the gain row shows preset + LNA /
  mixer / VGA; the rate is fixed at 10 MS/s; the SDR window shows the same
  gains and the packed option, its baseband-filter / ppm block is disabled
  ("HackRF only") and the gain optimiser (HackRF's LNA/VGA grid) says so.
  Persisted in `prefs.txt` (`source=airspy`, `air_lna/mixer/vga/preset/packed`;
  old files load unchanged; screenshot runs write nothing). Carrier offsets
  learned while on the Airspy are kept for the run only: the ones in the prefs
  include the HackRF's own clock error.
- **Not done**: the Airspy Mini and the 2.5 MS/s rate; the gain optimiser for
  the Airspy; AGC.

**The converter** (after libairspy `iqconverter_float.c`): remove the DC, the
fs/4 translate with signs (−1, −hbc, +1, +hbc), a 31-tap half-band
(equiripple, 16 non-zero taps, centre 0.5; the taps sum to 0.99963), decimate
by 2. Only the even lane needs the FIR (8 multiplies a sample, the taps being
symmetric) and the odd lane is delayed and times the centre tap. Written as
fixed-size blocks (4096 real samples, no allocation) of slice loops that LLVM
vectorises; the DC is the block mean followed slowly (libairspy's per-sample
one-pole cannot be vectorised, and the offset lands on the half-band's
transition at 5 MHz if left in). Measured in `--release` with
`cargo run --release -p decdvbt-io --features airspy --example
airspy_convert_bench` (Xeon 2.8 GHz, plain x86-64 build, SSE2 only):

| input | ns per output sample | % of one core at 10 MS/s out |
|---|---|---|
| unpacked | 3.97 | 4.0 % |
| packed | 5.87 | 5.9 % |

(`-C target-cpu=native`, AVX2: 2.83 / 3.58 ns. Not built in: the project's
exe is a plain x86-64 build and the gain was about one point of a core.) It
runs on the reader thread, not the receive thread.

**Level.** A full-scale real tone (ADC swinging ±2048 about the midpoint)
gives a complex tone of magnitude 1.0, I and Q each peaking at ±1, the same
full scale as the HackRF's cs8, so the engine's level and clip logic read
alike (the engine counts a sample as clipped at 0.99). libairspy's own output
is half of that. A signal at the same fraction of the ADC's range as on the
HackRF reads the same dBFS.

**Tests** (`cargo test -p decdvbt-io --features airspy`, 25, one of them
`#[ignore]`d and live): command encoding for every request, rate-list
parsing, gain clamps and preset tables, the bias-tee check, the start
sequence's order, the half-band's response with `rustfft` (passband ripple
0.010 dB to 3.9 MHz, stopband −58.7 dB from 6.1 MHz, not the −62 dB the design note
gave: in a half-band the two ripples are tied; a DVB-T channel is ±3.81 MHz), the converter against a direct 31-tap complex FIR on a mix of
tones and noise (agrees to 1e-4), a full-scale tone = 1.0, DC removal, block
boundaries, packed = unpacked, a fake reader into the consumer (hand-over,
buffer recycling, the retune skip, a stalled stream). The orientation test
puts a real tone at 5 MHz − d on the IF and asserts the output lands at +d
(and the mirror IF at −d), for d = ±1, 2.5, −3.5 MHz.

### Live results, 2026-10-27 (Airspy R2, aerial, in a moving vehicle)

- `live_airspy` passes: board, firmware string and rate list read, 10 MS/s
  delivered with no dropped buffers. It first failed on the firmware string
  (the R2 returns 48 of the 127 bytes asked for); a short reply is now accepted
  for that one request.
- **Orientation is right** (`IF_SPECTRUM_INVERTED` stays `true`): a DVB-T2
  mux (32K ext, PP7, 256-QAM 2/3 rotated) acquired and read L1, and a DVB-T
  mux (8k 1/32 64-QAM 2/3) decoded 25 services with a clean constellation.
- **Tuning is right**: both muxes sat on their channel centre; carrier offsets
  within a few hundred Hz of the plan.
- **Streaming**: 0 dropped buffers over minutes of live use (unpacked, 40 MB/s).
- **Gain**: with the signal fading as the vehicle moved, MER swung by 20 dB
  between runs of the same preset, more than any preset differs from another,
  so no preset was shown better. Sensitivity 10 gave 15 dB MER on the DVB-T mux
  against 9 dB for linearity 16 in one pair of runs, and the opposite order in
  another: only a steady signal can settle it. Linearity 20 (LNA 14, mixer 12,
  VGA 12) looked worse in the first rounds, perhaps overloaded.
- Not shown: an error-free decode (the signal was too poor while moving) and a
  retune soak.

### Needs a live check on the owner's machine

Run `cargo test -p decdvbt-io --features airspy live_airspy -- --ignored
--nocapture` first (opens the radio at 100 MHz for 2 s, receive only, prints
the firmware string, rate, rms and dropped buffers).

1. **Spectral orientation (the likeliest thing to be wrong).** The sign
   pattern decides whether the band is mirrored. It is libairspy's, which
   assumes a tone at *centre + d* sits at *5 MHz − d* on the R820T's IF.
   Cannot be checked without the radio. Check: decode a real mux
   (`decdvbt decode C23 --radio airspy -o out/c23.ts`); a mirrored spectrum
   will not lock (the TPS / pilots are conjugated), or the waterfall shows
   the channel's pilots on the wrong side. **To flip it**: set
   `IF_SPECTRUM_INVERTED` to `false` in `crates/decdvbt-io/src/airspy/convert.rs`
   (the one named constant; it negates Q) and the orientation tests follow it.
2. **Tuned frequency accuracy**: tune a known UHF mux; its carrier offset
   (the GUI's CFO, `decode`'s report) should be within the plan's offsets
   (±167 kHz steps) plus a couple of kHz. A larger constant offset would mean
   the tuner's centre is not where `SET_FREQ` says.
3. **40 MB/s unpacked streaming without dropped buffers** on the owner's USB
   port (the `capture` / `decode --progress` drop count; GUI drop warning).
   If it drops, try `--airspy-packed` (30 MB/s).
4. **Gain preset for the aerial**: the default, sensitivity 10, is a guess.
   Watch the level / OVERLOAD notes and MER; try `--airspy-preset
   sensitivity:N` / `linearity:N` or the single-stage flags.
5. **Retune soak**: tune between channels in the GUI (and `scan --radio
   airspy`) many times; each lock should take about as long as on the HackRF,
   with no stale lock. (`retune_soak` is HackRF-only.)
6. **A DVB-T decode of a real mux**, e.g. `decdvbt decode C23 --radio airspy -o
   out/c23.ts` (and C26): compare MER and pre-RS errors with the HackRF, which
   gets about 32 dB C/N on C23/C26 with `--amp --lna 32 --vga 8`.

## Narrow channels: 1 MHz DVB-T and 1.7 MHz DVB-T2 (2026-10-27)

Asked for: two narrow bandwidths end to end (modulator, transmitter,
receiver), each proved in loopback. Done; the decisions are in DESIGN.md §8
and TX_DESIGN.md §15. 547 tests pass (`cargo test --workspace`), clippy
(`-D warnings`) is clean. All of it was checked offline (no HackRF in the
build machine); what needs a live check is listed at the end.

- **1 MHz DVB-T** (not a standard; narrowband amateur DATV runs DVB-T this
  way): the 8 MHz mode with every clock divided by 8, T = 7/8 µs, 8/7 MS/s,
  0.95 MHz occupied, 2k and 8k, every guard, constellation and rate, 0.62 to
  3.96 Mbit/s. `--mode ...-1mhz`.
- **1.7 MHz DVB-T2** (EN 302 755, T = 71/131 µs): every FFT size and pilot
  pattern the 8 MHz receiver has; nothing in P1 or L1 carries the width
  (L1-pre's BWT_EXT is the extended carrier mode), so the receiver is told
  it. `--t2 ...-1.7mhz`; `T2Config::{frame_s, ts_rate_bps}` (the UK mux
  checks at 40.0 Mbit/s).
- **One `Bandwidth`** (1, 1.7, 6, 7, 8 MHz) for both standards, with
  `dvbt()`/`t2()`; `AutoReceiver::with_bandwidth` runs only the standards
  sent in the width; the engine takes `Command::Bandwidth`.
- **What assumed 8 MHz**, found by going through `decdvbt-rx`,
  `decdvbt-dsp::resample` and `decdvbt-engine`: the CW detector's ±3.75 MHz
  band (now 0.41 of the signal's rate), T2's 2.5 M-sample acquisition window
  (now 0.27 s), T2's `FS_NATIVE` in the receiver, its sounder, its card and
  its measurements, the spectral C/N zones and the block-edge finder (now
  scaled by `Bandwidth::scale`), the band view's brackets, the GUI's
  "channel fits in the span" margin, the noise bandwidth of the T2 test
  signal's C/N. Already in samples, carriers or symbols, so right at every
  width: the cyclic-prefix search, the offset search (±(N−K)/2 + 8 carriers:
  ±97 kHz at 1 MHz 8k, ±115 kHz for 1.7 MHz P1's 64 spacings), guard and mode
  detection, the Doppler estimator and time filter (a given Hz of Doppler is
  eight times harder at 1 MHz, which is the physics), the carrier notch, the
  interpolator (the narrow signal is 0.48 of 2 MS/s against 0.76 of 10 MS/s at
  8 MHz). The acquisition's 12 µs echo window stays a fixed share of the
  symbol (96 µs at 1 MHz). The resampler has no 8 MHz in it (142/131 is
  within its 256 phases).
- **Radio**: 2 MS/s (the HackRF's lowest) and the 1.75 MHz baseband filter
  (its narrowest) for both, TX and RX. 1 MHz is read directly by the receiver
  (rho = 7/4: a 3584- or 14336-point transform), 1.7 MHz by interpolation.
- **SI**: 1 MHz DVB-T's terrestrial descriptor carries the reserved bandwidth
  code 7 (read back as 1 MHz); 1.7 MHz T2 a T2_delivery_system_descriptor
  (bandwidth 0101 = 1.712 MHz), which `decdvbt-ts` now parses
  (`Transponder::t2`); `Terrestrial::bandwidth_khz` replaces `bandwidth_mhz`.
- **TX**: the Bandwidth box (1, 1.7, 6, 7, 8 MHz), T2 controls and an All
  modes table at 1.7 MHz (`modes::t2_mode`, 24 rows per FFT size and guard),
  two narrow presets, CIF video (352x288) under 1.5 Mbit/s, the tables' 39
  kbit/s taken out of the low rates' budget. **RX GUI**: a width box beside
  the source, the channel box takes a frequency in MHz too (or `--freq`), 2
  and 4 MS/s offered for the narrow widths, the width remembered.
  **CLI**: `modulate --freq`, default 2 MS/s for narrow modes;
  `decode --bandwidth 1|1.7|6|7|8 [--freq MHZ]`, `decode hackrf --freq`.

**Loopback tests** (`crates/decdvbt-rx/tests/narrow.rs`): each mode modulated,
through the channel models (white noise, three static paths at 0, 6 and 21 µs,
a carrier offset of several carriers, a clock error of 5 to 25 ppm), resampled
to 2 MS/s and decoded; from the first packet received every packet equals the
modulator's, byte for byte, none missing:

| mode | C/N | CFO | clock | packets matched | SNR read |
|---|---|---|---|---|---|
| 1 MHz QPSK 1/2 2k 1/8 | 14 dB | +2.3 kHz | +12 ppm | 603 of 735 (1.6 s) | 13.7 dB |
| 1 MHz 16-QAM 3/4 8k 1/32 | 22 dB | −5.1 kHz | −15 ppm | 3308 of 4812 (3.2 s) | 20.3 dB |
| 1 MHz 64-QAM 2/3 2k 1/4 | 27 dB | +731 Hz | +25 ppm | 2143 of 2647 (1.6 s) | 25.8 dB |
| 1 MHz 64-QAM 5/6 8k 1/16 | 30 dB | +12 kHz | +5 ppm | 5302 of 7785 (3.2 s) | 28.6 dB |
| 1.7 MHz T2 2K 1/8 PP2 QPSK 1/2 rot. (closing symbol) | 12 dB | −1.8 kHz | −10 ppm | 1035, 224/224 blocks | 9.6 dB |
| 1.7 MHz T2 4K 1/32 PP7 64-QAM 2/3 rot., 64K LDPC | 26 dB | +650 Hz | +20 ppm | 3994, 140/140 | 24.1 dB |
| 1.7 MHz T2 8K 1/16 PP4 16-QAM 3/4 ext, 16K LDPC | 24 dB | +3.5 kHz | +8 ppm | 2822, 365/365 | 19.5 dB |

The rest of each run is the lock (acquisition, a TPS frame of 68 symbols, half
a second at 1 MHz 8k, or two T2 frames). The **TX pipeline**
(`crates/decdvbt-tx/tests/pipeline.rs`) writes a 1 MHz (QPSK 2/3 2k 1/8, CIF)
and a 1.7 MHz (T2 8K 1/16 16-QAM 3/4) IQ file at 2 MS/s from a real ffmpeg clip;
the receiver decodes both with no lost block, every packet equal to what the
modulator was given (6936 and 14459 compared), and reads the NIT's width and
centre back; the slowest mode's stream (0.62 Mbit/s) holds 8.8 % nulls, PCR
every 26.6 ms, no "dts < pcr". Before the low-rate budget it held 4.7 % nulls.

**CPU** (release, one core, per second of signal, best of three on this
4-core cloud machine, which is slower than the Ryzen: the 8 MHz UK mode at 10
MS/s took 45 % here, against 30 % in the CPU pass above):

| receiver, `decode` on 10 s at 2 MS/s | total | where |
|---|---|---|
| 1 MHz QPSK 1/2 2k 1/8 | 3.3 % | read 1.1, equalise 0.7, demap 0.5, Viterbi 0.6 |
| 1 MHz 16-QAM 3/4 8k 1/32 | 4.8 % | read 1.0, equalise 0.8, demap 0.7, Viterbi 1.7 |
| 1 MHz 64-QAM 2/3 8k 1/32 | 5.7 % | read 1.0, equalise 0.7, demap 0.8, Viterbi 2.2 |
| 1 MHz 64-QAM 7/8 8k 1/32 | 6.7 % | Viterbi 2.9 |
| 8 MHz 64-QAM 2/3 8k 1/32 at 10 MS/s (reference) | 45 % | an eighth of it is 5.6 % |
| 1.7 MHz T2 2K QPSK 1/2 | 7.2 % | read 3.5 (resample 1.8), equalise 2.8, FEC 2.5 |
| 1.7 MHz T2 8K 16-QAM 3/4 | 6.8 % | read 3.7 (resample 1.9), equalise 2.2, FEC 5.8 (LDPC 3.1) |
| 1.7 MHz T2 8K 1/128 256-QAM 5/6 (9.5 Mbit/s) | 26 % | FEC 25 (LDPC 19, 2.5 iterations at 24 dB) |

| transmitter, `txfile --stages` | modulator | resampler to 2 MS/s | pack |
|---|---|---|---|
| 1 MHz QPSK 1/2 2k | 0.6 % | 3.0 % | 0.1 % |
| 1 MHz 64-QAM 2/3 8k 1/32 | 1.2 % | 2.8 % | 0.1 % |
| 1 MHz 64-QAM 7/8 8k 1/32 | 1.2 % | 2.7 % | 0.1 % |
| 1.7 MHz T2 1K QPSK 1/2 | 4.2 % | 3.0 % | 0.2 % |
| 1.7 MHz T2 8K 16-QAM 3/4 | 8.8 % | 3.0 % | 0.2 % |
| 1.7 MHz T2 8K 1/128 256-QAM 5/6 | 16.3 % | 3.0 % | 0.2 % |

1 MHz DVB-T costs an eighth of 8 MHz, as it should; at 2 MS/s the resampler is
now the transmitter's largest stage (3 %), and the receiver's T2 interpolation
(2 %) its largest fixed one. The T2 modulator (frame at a time, allocating) is
the one thing heavier than its DVB-T counterpart; at these rates it is well
inside the budget and was left alone.

**Needs the HackRF** (not checkable here): the 1.75 MHz baseband filter's
response at the edges of a 1.54 MHz T2 signal (RX and TX), the TX's DAC
images at ±2 MS/s (only that filter attenuates them: look at a spectrum
before going on air), the HackRF's LO leak at the centre against 1 MHz
DVB-T's centre carrier (139.5 Hz carriers in 8k: the DC canceller and the
tone canceller should take it, unmeasured), real lock times and the clock
loops on a live narrow signal, and that receivers other than ours accept
the reserved bandwidth code 7 (a standard set-top box will not tune 1 MHz
anyway).

**Not done / limits.** The DVB-T lock's health and quality looks count
symbols, so at 1 MHz a dead lock is dropped after about 15 s (8k) instead of
2. The DVB-T sounder runs (and finds nothing) on a 1.7 MHz channel when the
channel panels are open (under 1 % of a core). T2 at 1.7 MHz is offered by
the TX with normal carriers only (whether the extended carrier mode applies
at 1.7 MHz was not checked against the spec text); the receiver reads either.
The T2 channel view's impulse response is drawn from the symbols' early
window start (128 samples: 69 µs at 1.7 MHz, 14 µs at 8 MHz), as before; the
echoes' relative delays are right.

## The signal card holds its lock and names the network (2026-10-27)

- **The card holds for 5 s** (`CARD_HOLD`, `shown_card`): on a marginal signal
  the TPS lock comes and goes, and the card flipped several times a second
  between the locked card (mode, margin, effective C/N, detail rows) and the
  greyed "DVB-T / T2" searching card. The last card that had a lock is now kept
  for 5 s after the lock is lost, with the channel, frequency and running state
  live; retuning, changing source or stopping the decoder clears it. Checked on
  a simulated marginal signal (`modulate --cn 3 --profile urban --fd 6`): the
  mode-only hold still flipped the C/N source and dropped rows in a fifth of
  the samples, the whole-card hold in none. Not yet judged on air for long.
- **The NIT's network name beside the standard**: the title bar reads
  "DVB-T · London". A DVB-T signal carries no transmitter name; the NIT's
  network name is the closest thing (on UK Freeview the transmitter's region).
  Only a London NIT has been seen.
- Portable builds: `RUSTFLAGS="-C target-feature=+crt-static"` with
  `--target x86_64-pc-windows-msvc` links the C runtime statically (no Visual
  C++ redistributable); the earlier releases' exes did need it.

## Radio autostart and the band scan strip (2026-10-28)

- **A radio service's MHEG app starts on tuning**, without the red key (`TvWindow::start`):
  BBC radio shows its album art / "Now Playing" screen as a set-top box does, and the
  Red Button toggle no longer presses red (which opened the menu over the art). The
  level meters show until the app has a scene (or when it has none). Checked headless on
  C39 (BBC Radio 2). Heart/Capital (C34) draw only their red brand
  rectangle: the logo Bitmap has no image (not investigated).
- **Band scan** (`apps/decdvbt-gui/src/band_scan.rs`, the "⇔ Scan" button; `DECDVBT_BANDSCAN=1`
  runs one for screenshots): the live radio steps over C21-C48, averages 8 engine
  rows per channel (32 columns across 8 MHz), puts itself back on the channel, and
  a short strip under the controls shows the stitched trace with channel numbers; a
  click tunes. It leaves the gains alone. Checked on the Airspy:
  C32-35, C39, C42 (DVB-T) and C45 (T2) stand out.

## Airspy R2 glitches: packed blanking, timing slips (2026-10-28)

Found with a live soak on a 64-QAM 2/3 8K mux (`--example airspy_live`) and replays of
raw USB transfers (`DECDVBT_AIRSPY_RAW=path`, `--example airspy_replay`). No USB
buffer is ever dropped and the delivered rate is 10.000 MS/s; both faults are inside
the radio.

- **Packed mode**: every ~2 s (24-30 a minute) a packed transfer carries bytes
  512..12288 as *unpacked* u16 words (the firmware's in-place packer not done, it
  seems). Read as packed that is near full-scale noise: +8 dB on the buffer, lost locks.
  `RealToIq::process_packed` now finds such blocks (top nibble of half the u16 words
  zero, `looks_unpacked`) and blanks them to the DC level, keeping the sample count;
  `AirspyControl::blanked`/`lost` count them and the apps' drop counter shows them.
  The radio also loses samples around these events (from 3 up to ~2050 IQ samples,
  measured from the cyclic prefix), so packed mode still relocks a few times a
  minute. **Use unpacked**: USB has the headroom (0 drops in every run).
- **Timing slips, both modes**: now and then the stream loses 2-7 IQ samples
  mid-transfer (never on a USB transfer or packet boundary: in the radio). The
  equaliser's slope scan only reached ±2.3 samples (`SLOPE_MAX` 6 rad), so the symbols
  after a slip went unequalised until the slow CP loop pulled the window back: ~70
  packets lost a slip and SNR down 5-6 dB for 50 ms. `SLOPE_MAX` is now 18 rad
  (±7 samples): ~25 packets (the one symbol the slip lands in), +0.25 % of a core,
  no change on weak or HackRF signals. The receiver's clock reading on the Airspy
  went from a false 20-80 ppm (the CP and pilot loops pulling against each other)
  to 1-4 ppm.
- Live 60 s unpacked before/after: 47 loss bursts → 13 single blips.

## Narrow channels on the Airspy R2 (2026-10-28)

- The 1 MHz (DVB-T) and 1.7 MHz (DVB-T2) widths are offered for the Airspy in the GUI and
  CLI (they were hidden: "its lowest rate is 10 MS/s"). No DSP change was needed: the
  receivers take the radio's rate and their own DDC/FFT front end decimates, so a narrow
  signal decodes from the 10 MS/s stream as it does from the HackRF's 2 MS/s.
  `crates/decdvbt-rx/tests/narrow_wide.rs` checks both byte for byte at 10 MS/s (the T2
  signal is generated at 2 MS/s and upsampled by 5: the modulator has no simple ratio from
  1.845 MS/s to 10): 1 MHz 2k QPSK ~5 % and 1.7 MHz 8k 16-QAM ~14 % of real time in the
  receiver alone. Not yet tried on air with the Airspy.

## Gain optimiser for the Airspy R2 (2026-10-29)

- `apps/decdvbt-gui/src/gain_opt.rs`, copied back from DecLAB: one optimiser for
  both radios. The HackRF's search is unchanged (total gain in 4 dB steps, then
  LNA/VGA splits), plus DecLAB's "no signal" guard (best C/N under 3 dB: keep
  the old gains). The Airspy climbs libairspy's sensitivity ladder (every other
  rung and 21), then up to three rounds stepping each stage by 2 about the best.
  "Optimise gain" now shows for the Airspy in the SDR window (and `--optimise`).
  Gains it sets that match no preset show as "custom". Unit-tested only; not
  yet run on air.

## Freeview's internet channels: the MHEG-5 interaction channel (2026-10-31)

Asked for: the data services of a UK commercial mux that a television plays (NHK WORLD,
Al Jazeera, FRANCE 24, EuroNews, Tiny Pop… listed as "data", "off air"). They are
MHEG-IC: the service's carousel has a launcher (`/startup`) and a table (`cse.cfg`)
naming, per service id, an MHEG-5 application on the internet; that application fetches
its scenes and a stream description, asks the operator's server for a device id and a
playlist, and plays an MPEG transport stream over HTTP (H.264 SD, AAC-LATM). Done as a
receiver would, with nothing pretended: the requests carry `WhoAmI` ("DDT DecDVB-T 010",
user agent `DecDVB-T MHEG-5 (…)`), and the applications' own logic then serves an unknown
receiver its SD streams.

- **`decdvbt-mheg::ic`**: `Http` (GETs on four threads, cached; ReturnData POSTs) and
  `IcFiles` (the carousel with URLs beside it, only to servers in the carousel's
  `auth.servers`). Group ids and content refs may be URLs; in an application that came
  over HTTP, "/x" and "//x" are from its folder (`obj::IC_ROOT`), "DSM:" still the
  carousel. `GetICStatus` 0 and `ICProfile` when `Engine::ic`.
- **TLS from the carousel** (`ic_tls.rs`): the carousel's `auth.tls.N` (16-bit count,
  24-bit length + DER each) are the server certificates to trust. Freeview's is
  self-signed, CA:TRUE, RSA-1024, SHA-1, which webpki (CA as end entity) and ring (key
  under 2048 bits) refuse: a server is taken only if it shows exactly one of those
  certificates, and the handshake signature is checked with RustCrypto `rsa`.
- **Engine fixes the application needed** (each was a stall): SetData drops the old
  content's queued ContentAvailable (it read the old file as the new); Run/Stop/Preload/
  Unload/SetData on object 0 (a group) are ignored, ISO/IEC 13522-5 gives them to
  ingredients only (its image table "freed" slot 0 and stopped its own scene); a
  close-down that stops its own group no longer recurses; the **ListGroup class**
  (`list.rs`, after freemheg's MHListGroup: AddItem, DelItem, GetListItem, GetListSize,
  GetCellItem, selection, scrolling, cells, the presented/head/tail events);
  `GetBitmapDecodeOffset`; programs `CSI` (CastToStringInt), `RDa` (ReturnData, a form
  POST; the action waits for the answer), `PST`, `MSP`, `PFG`.
- **Non-volatile store**: "pst://" names (the server's device id) are kept:
  `Engine::save_nonvolatile` / `load_nonvolatile`; the GUI keeps them in
  `%APPDATA%\DecDVB-T\mheg-store.bin`. The first tuning registers ("#duidAssigned"),
  later ones log in ("#duidOkay") and get the playlist.
- **`Request::IcStream(Some(url))`** when a Stream with an http(s) content runs (or is
  given one); `None` when it stops.
- **GUI**: a data service with an MHEG application (data broadcast id 0x0106) lists as
  "app" and plays: its application starts on tuning (as radio's), no broadcast ffmpeg.
  `ic_stream.rs` reads the stream on a thread, waits for its PAT/PMT, and hands the
  packets to the usual `VideoPlayer` **paced by the stream's PCR** (the server sends the
  first seconds in a burst; unpaced, the sound ran 5 s ahead and the player went by the
  wall clock and skipped pictures). "Back to broadcast" plays the service again.
- `redbutton` example: `--ic` (files over HTTP, real time) and `--nv FILE`; it prints
  the fetches and what the operator's server answered.
- **Checked live** (Airspy R2, a UK commercial mux): NHK WORLD plays in the TV window,
  timed by the sound (1911 pictures shown, 1 dropped in ~75 s), whole app ~70 % of a
  core with the receiver running (the stream itself is ffmpeg's H.264 decode). Only NHK
  WORLD was tried; the other channels use the same launcher with their own applications
  and may need more of the engine. The application's own graphics (its menus, now/next
  banner) were not checked. Not done: the IC stream's counter actions
  (`GetCounterMaxPosition`, `SetCounterEndPosition`), cookies, HTTPS streams to
  carousel-pinned servers (ureq only), a setting to turn the interaction channel off.
- **On Demand 365** (same mux, its app builds its EPG from a server template) needed two more
  fixes, checked headless against a photo of a television showing it: bitmaps larger than their
  box are scaled into it, as MythTV draws them ("the BBC expects bitmaps to be scaled"), not
  clipped (its titles and logos are HD-plane sized: 500x70 for a 282x56 box; downscaled by
  averaging, cached); and a ListGroup applies its update behaviour only while running (the app
  stops a list and keeps its background panels and grid cells in it; updating hid them). The
  BBC Red Button pages are unchanged.
