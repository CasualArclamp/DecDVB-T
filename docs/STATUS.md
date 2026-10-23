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
