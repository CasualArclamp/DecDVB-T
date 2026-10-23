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
