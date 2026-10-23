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

So a UK mux decodes in real time on one core, barely at 3/4. M7 has to bring
this down, in order of size: the **Viterbi** (6 AVX2 cross-lane permutes a
step; 8-bit metrics halve the ops; the traceback is another third), **reading
symbols** (a decimating filter for the radio's rate and a window FFT with no
per-sample interpolation, the timing as a phase ramp), the **equaliser**
(per-carrier division, allocations, scalar interpolator, scalar atan2 in the
fit: all vectorisable), and running Viterbi + outer on a second thread.

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
