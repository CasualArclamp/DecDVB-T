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
| Region / targets | **UK Freeview**, 8 MHz channels, UHF. Transmitter unknown; a band scan and the NIT will tell us which. |
| Front ends | **HackRF One** (live; plugged into this PC), **Airspy R2/Mini**, IQ files. |
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
- Measurements: MER per carrier, channel frequency and impulse response, echo
  profile, pre- and post-FEC BER, LDPC iterations.

## 4. What carries over from DecSAT

| DecSAT crate | Use here |
|---|---|
| `decsat-io` | HackRF RX/TX, IQ files, extension sniffing. Add Airspy. |
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
- **M3** — DVB-T receiver: time/frequency sync, TPS, channel estimation,
  demapping, inner deinterleavers, Viterbi, RS → TS. Live Freeview DVB-T.
- **M4** — player, PSI/SI, service panel, scopes, measurements.
- **M5** — DVB-T2 modulator.
- **M6** — DVB-T2 receiver: P1, P2/L1, LDPC/BCH, TI, rotated QAM → TS.
  Live Freeview HD.
- **M7** — real-time T2 performance, band scan, release.

## 6. Constraints found at kickoff

- Rust was not installed; rustup + stable MSVC were installed for M0.
- CPU is a **Ryzen 7 4800H** (Zen 2, 8 cores, **AVX2, no AVX-512**). DecSAT's
  AVX-512 LDPC path will not run here, so real-time T2 rests on the AVX2/portable
  kernel and the thread pool; this needs measuring early (M6 at the latest).
- HackRF tools (`hackrf_info` etc.) are not on PATH; the HackRF One is
  enumerated by Windows. DecSAT loads `libhackrf` at run time.

## 7. Open questions

- Ron Economos's DVB-T2 receiver: which repository exactly.
- Airspy R2 real-IF mode (10 MS/s real) or IQ only.
- Automatic gain (LNA/VGA) from ADC level and MER: wanted?
