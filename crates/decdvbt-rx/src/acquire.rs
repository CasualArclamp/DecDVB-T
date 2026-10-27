// made by CasualArclamp
//! Acquisition: from a few symbols of the signal at its own sample rate, find
//! what it is (FFT size and guard interval), where its symbols begin, and its
//! carrier offset and scattered-pilot phase.
//!
//! 1. **Mode and timing**, by the cyclic prefix: the last G samples of a
//!    symbol repeat its first G, so x[n]·x*[n+N] summed over G samples peaks
//!    once a symbol period. Each (N, G) is tried, the sums folded over the
//!    periods in the capture, and the one whose fold stands highest over its
//!    median wins. The peak's position is the symbol timing (to a fraction of
//!    a sample, by a parabola), and its phase the carrier offset within a
//!    carrier spacing (ε = −arg/2π).
//! 2. **The offset in whole carriers**, from the spectrum: the block fills K
//!    of the N bins, and lies where the power inside is highest against the
//!    power outside.
//! 3. **DVB-T, the pilot phase and the last fraction**, from the pilots: the
//!    scattered pilots' known signs make the channel's impulse response
//!    concentrate in a few taps only for the right scattered phase and the
//!    right offset; for the wrong ones it is noise. This tells DVB-T from
//!    anything that merely has a cyclic prefix (DVB-T2 at 8k 1/32 has the same
//!    one).

use std::sync::Arc;

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{FftSize, Guard};
use decdvbt_ofdm::pilots::{continual_pilots, pilot_value, prbs};
use rustfft::{Fft, FftPlanner};

/// The least contrast of the cyclic-prefix fold over its median that counts:
/// pure noise reaches about 3.6 (the largest of thousands of Rayleigh bins
/// over their median).
pub const MIN_CONTRAST: f32 = 5.0;
/// The least concentration of the impulse response (see [`concentration`])
/// that counts as a DVB-T signal.
pub const MIN_CONCENTRATION: f32 = 0.35;
/// How many whole carriers either side of the coarse guess the pilots are
/// tried over. The spectral guess (power inside the block against outside)
/// is no better than a few carriers on a real band, where the receiver's
/// filter blurs the block's edges: on a live capture it was nine carriers out
/// at a third of the positions the capture could be started from, and a
/// search of ±3 never reached the true offset, so a clean signal was not
/// found, and again 0.4 s later at another position. The continual pilots
/// (see [`pilot_offset`]) give a better guess; this is the margin round it.
const OFFSET_SEARCH: isize = 8;
/// The least coherence of the continual pilots between symbols that counts as
/// finding them (noise gives about 0.04).
const MIN_PILOT_OFFSET: f32 = 0.25;
/// Guard interval kinds, and FFT sizes, tried.
const FFTS: [FftSize; 2] = [FftSize::K8, FftSize::K2];

/// A mode and timing found by the cyclic prefix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpFind {
    pub fft: FftSize,
    pub guard: Guard,
    /// Where a symbol's guard interval starts, in samples of the array
    /// searched (fractional), somewhere in its first symbol period.
    pub apex: f64,
    /// The carrier offset within a carrier spacing, in spacings, ±0.5.
    pub eps: f64,
    /// The fold's peak over its median.
    pub contrast: f32,
}

/// Parabolic interpolation of a peak at `i` with neighbours `a`, `c` and
/// itself `b`: the offset from `i`, within ±0.5.
pub fn parabola(a: f32, b: f32, c: f32) -> f64 {
    let d = a - 2.0 * b + c;
    if d.abs() < 1e-20 {
        0.0
    } else {
        f64::from(0.5 * (a - c) / d).clamp(-0.5, 0.5)
    }
}

/// Search `x` (at the signal's rate) for a cyclic prefix, and say what is
/// found if its contrast reaches [`MIN_CONTRAST`].
pub fn search_mode(x: &[Iq]) -> Option<CpFind> {
    search_best(x).filter(|b| b.contrast >= MIN_CONTRAST)
}

/// The best cyclic-prefix candidate in `x` whatever its contrast (for
/// saying how near a signal came).
pub fn search_best(x: &[Iq]) -> Option<CpFind> {
    let mut best: Option<CpFind> = None;
    for fft in FFTS {
        let n = fft.n();
        if x.len() < 3 * n {
            continue;
        }
        // The products x[s]·x*[s+N], and their running sum, so that a sum over
        // any G of them is two lookups.
        let mut cum = Vec::with_capacity(x.len() - n + 1);
        let mut acc = (0.0f64, 0.0f64);
        cum.push(acc);
        for s in 0..x.len() - n {
            let p = x[s] * x[s + n].conj();
            acc.0 += f64::from(p.re);
            acc.1 += f64::from(p.im);
            cum.push(acc);
        }
        for guard in Guard::ALL {
            let g = n / guard.divisor();
            let per = n + g;
            let span = cum.len() - 1 - g; // starts s with a full G-sum
            if span < 2 * per {
                continue;
            }
            // Fold the complex G-sums over the whole periods available.
            let periods = span / per;
            let mut fold = vec![(0.0f64, 0.0f64); per];
            for p in 0..periods {
                for (r, f) in fold.iter_mut().enumerate() {
                    let s = p * per + r;
                    f.0 += cum[s + g].0 - cum[s].0;
                    f.1 += cum[s + g].1 - cum[s].1;
                }
            }
            let mag: Vec<f32> = fold.iter().map(|f| (f.0.hypot(f.1)) as f32).collect();
            let mut sorted = mag.clone();
            sorted.sort_by(f32::total_cmp);
            let median = sorted[per / 2].max(1e-12);
            let (peak_at, peak) = mag
                .iter()
                .copied()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(&b.1))?;
            let contrast = peak / median;
            if best.is_none_or(|b| contrast > b.contrast) {
                let (a, c) = (mag[(peak_at + per - 1) % per], mag[(peak_at + 1) % per]);
                let f = fold[peak_at];
                best = Some(CpFind {
                    fft,
                    guard,
                    apex: peak_at as f64 + parabola(a, peak, c),
                    eps: -f.1.atan2(f.0) / std::f64::consts::TAU,
                    contrast,
                });
            }
        }
    }
    best
}

/// The bins of the transform that hold carrier `k` once the signal is shifted
/// by `d` bins: carrier k sits at k − kmid + d (negative frequencies wrap).
#[inline]
pub fn bin_of(k: usize, kmid: usize, d: isize, n: usize) -> usize {
    (k as isize - kmid as isize + d).rem_euclid(n as isize) as usize
}

/// The transform an acquisition works with, for one FFT size.
pub struct Fft1 {
    pub fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Iq>,
}

impl Fft1 {
    pub fn new(n: usize) -> Fft1 {
        let fft = FftPlanner::new().plan_fft_forward(n);
        let scratch = vec![Iq::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        Fft1 { fft, scratch }
    }

    pub fn forward(&mut self, buf: &mut [Iq]) {
        self.fft.process_with_scratch(buf, &mut self.scratch);
    }
}

/// How much of a transfer function's impulse response lies in its best
/// window `window` taps wide (of the `nd` an inverse transform of `h`
/// zero-padded to `nd` gives): near 1 for a channel (a few taps within the
/// guard), near `window/nd` for noise.
pub fn concentration(h: &[Iq], nd: usize, window: usize, ifft: &mut Fft1) -> f32 {
    let mut buf = vec![Iq::new(0.0, 0.0); nd];
    buf[..h.len()].copy_from_slice(h);
    ifft.forward(&mut buf); // sign of the exponent does not matter for this
    let power: Vec<f32> = buf.iter().map(|c| c.norm_sqr()).collect();
    let total: f32 = power.iter().sum();
    if total <= 0.0 {
        return 0.0;
    }
    // Circular sliding window.
    let mut sum: f32 = power[..window].iter().sum();
    let mut best = sum;
    for i in 0..nd {
        sum += power[(i + window) % nd] - power[i];
        best = best.max(sum);
    }
    best / total
}

/// The carrier offset in whole carriers, from the continual pilots: they
/// carry the same value in every symbol (each symbol's carriers start their
/// phase afresh at its own start), so with the windows the same distance into
/// each symbol a bin holding one has the same content in each symbol's
/// spectrum but for one turn common to all the carriers (what a frequency
/// offset of `d` carriers does over a symbol), and the products of consecutive
/// symbols' bins add up in step for the right shift `d` and not for any other.
/// Returns the shift and how coherent the sum is (about 1 for the right one,
/// 0.04 for any other).
fn pilot_offset(spectra: &[Vec<Iq>], fft: FftSize, dmax: isize) -> (isize, f32) {
    let n = fft.n();
    let kmid = (fft.active_carriers() - 1) / 2;
    let cps = continual_pilots(fft);
    let mut best = (0isize, 0.0f32);
    for d in -dmax..=dmax {
        let (mut sum, mut mag) = (Iq::new(0.0, 0.0), 0.0f32);
        for &k in cps {
            let b = bin_of(k as usize, kmid, d, n);
            for pair in spectra.windows(2) {
                let p = pair[1][b] * pair[0][b].conj();
                sum += p;
                mag += p.norm();
            }
        }
        let score = if mag > 0.0 { sum.norm() / mag } else { 0.0 };
        if score > best.1 {
            best = (d, score);
        }
    }
    best
}

/// What acquisition found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Acquisition {
    pub fft: FftSize,
    pub guard: Guard,
    /// Start of a symbol's guard interval, in samples of the array searched.
    pub apex: f64,
    /// The carrier offset, spacings (whole and fractional).
    pub cfo_spacings: f64,
    /// Scattered-pilot phase (symbol index mod 4) of the symbol at `apex`.
    pub phase: usize,
    pub contrast: f32,
    pub concentration: f32,
}

/// The spectra of `symbols` consecutive symbols from the guard interval `cp`
/// found (each window a quarter guard before the useful part, as the tracker's
/// are), the fractional offset taken out. The rotation's phase origin is the
/// array's start: all the symbols share it.
fn symbol_spectra(x: &[Iq], cp: &CpFind, symbols: usize) -> Vec<Vec<Iq>> {
    let n = cp.fft.n();
    let g = n / cp.guard.divisor();
    let per = n + g;
    let delta = (g / 4).clamp(8, 128);
    let first = cp.apex.round() as usize;
    let mut plan = Fft1::new(n);
    let mut spectra = Vec::with_capacity(symbols);
    for i in 0..symbols {
        let start = first + i * per + g - delta;
        let mut buf: Vec<Iq> = x[start..start + n]
            .iter()
            .enumerate()
            .map(|(j, &s)| {
                let ph = -std::f64::consts::TAU * cp.eps * (start + j) as f64 / n as f64;
                s * Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        plan.forward(&mut buf);
        spectra.push(buf);
    }
    spectra
}

/// Find the carrier offset in whole carriers (by the block's spectral edges),
/// then the scattered phase and the last carriers (by the pilots) in `x`.
/// `x` holds at least five symbols; `cp` is what [`search_mode`] found in it.
pub fn acquire(x: &[Iq], cp: &CpFind) -> Option<Acquisition> {
    let fft = cp.fft;
    let n = fft.n();
    let g = n / cp.guard.divisor();
    let per = n + g;
    let k_total = fft.active_carriers();
    let kmid = (k_total - 1) / 2;
    // Windows start a quarter guard early, as the tracker's do.
    let delta = (g / 4).clamp(8, 128);
    let first = cp.apex.round() as usize;
    let symbols = 4;
    if first + (symbols - 1) * per + g + n > x.len() || g < delta {
        return None;
    }
    let spectra = symbol_spectra(x, cp, symbols);

    // Whole carriers: the shift d that puts the most power inside the block
    // and the least outside, by prefix sums over the circular power array.
    let mut p = vec![0.0f32; n];
    for s in &spectra {
        for (a, c) in p.iter_mut().zip(s) {
            *a += c.norm_sqr();
        }
    }
    let total: f64 = p.iter().map(|&v| f64::from(v)).sum();
    let mut prefix = vec![0.0f64; 2 * n + 1];
    for i in 0..2 * n {
        prefix[i + 1] = prefix[i] + f64::from(p[i % n]);
    }
    let dmax = ((n - k_total) / 2) as isize + 8;
    let mut coarse = Vec::new();
    for d in -dmax..=dmax {
        // The block's bins: from (0 − kmid + d) for K bins, circular.
        let a = (d - kmid as isize).rem_euclid(n as isize) as usize;
        let inside = prefix[a + k_total] - prefix[a];
        let outside = total - inside;
        let ratio = (inside / k_total as f64) / (outside / (n - k_total) as f64).max(1e-30);
        coarse.push((ratio, d));
    }
    coarse.sort_by(|a, b| b.0.total_cmp(&a.0));
    // The pilots' own answer if they give a clear one, else the spectrum's.
    let (d_pilots, coherence) = pilot_offset(&spectra, fft, dmax);
    let d0 = if coherence >= MIN_PILOT_OFFSET {
        d_pilots
    } else {
        coarse.first()?.1
    };

    // The pilots: for each scattered phase and offsets about d0, how
    // concentrated is the channel they give, summed over the four symbols?
    let wk = prbs(k_total);
    let nd = 1024usize.max((k_total / 12 + 1).next_power_of_two() * 2);
    let spacing_s = 1.0 / (n as f64) * 12.0; // pilot spacing in units of 1/T_u... see window below
    let _ = spacing_s;
    // A 12 µs window in taps: the pilot samples are 12 carriers apart, so
    // the inverse transform's tap spacing is 1/(nd · 12 Δf) = N·T/(nd · 12).
    // T is the 8 MHz channel's at every width: the window is a fixed share of
    // the useful symbol (1/75 in 8k), 12 µs at 8 MHz, 96 µs at 1 MHz, which
    // is what the signal's own clock scales a channel's echoes to.
    let t_s = 1.0 / (64e6 / 7.0);
    let tap_s = n as f64 * t_s / (nd as f64 * 12.0);
    let window = ((12e-6 / tap_s).round() as usize).clamp(4, nd / 2);
    let mut ifft = Fft1::new(nd);
    let mut results = Vec::new();
    for d in d0 - OFFSET_SEARCH..=d0 + OFFSET_SEARCH {
        for phase in 0..4usize {
            let mut sum = 0.0f32;
            for (i, s) in spectra.iter().enumerate() {
                let k0 = 3 * ((phase + i) % 4);
                let h: Vec<Iq> = (k0..k_total)
                    .step_by(12)
                    .map(|k| {
                        let b = bin_of(k, kmid, d, n);
                        // The early start's ramp does not change the window
                        // metric, so it is left alone here.
                        s[b] * (0.75 * (1.0 - 2.0 * f32::from(wk[k])))
                    })
                    .collect();
                sum += concentration(&h, nd, window, &mut ifft);
            }
            results.push((sum / symbols as f32, d, phase));
        }
    }
    results.sort_by(|a, b| b.0.total_cmp(&a.0));
    let (best, d, phase) = results[0];
    // A DVB-T signal shows as one hypothesis well clear of the rest.
    let runner_up = results.get(1).map_or(0.0, |r| r.0);
    let _ = pilot_value;
    if best < MIN_CONCENTRATION || best < 1.3 * results[results.len() / 2].0.max(1e-6) {
        return None;
    }
    let _ = runner_up;
    Some(Acquisition {
        fft,
        guard: cp.guard,
        apex: cp.apex,
        cfo_spacings: f64::from(d as i32) + cp.eps,
        phase,
        contrast: cp.contrast,
        concentration: best,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::dvbt::{Mode, Modulation, Rate};
    use decdvbt_mod::Modulator;
    use decdvbt_mod::channel::{Channel, ChannelSim, Path};
    use decdvbt_mod::ts::TestStream;

    /// `symbols` of `mode` at its own rate through `ch`, from `skip` symbols
    /// in (past the start-up pulse).
    pub fn signal(mode: Mode, ch: &Channel, skip: usize, symbols: usize) -> Vec<Iq> {
        let mut m = Modulator::new(mode, None, Box::<TestStream>::default()).unwrap();
        let mut sim = ChannelSim::new(ch, mode.bandwidth.sample_rate_hz(), m.mean_power());
        let (mut sym, mut out) = (Vec::new(), Vec::new());
        for i in 0..skip + symbols {
            m.next_symbol(&mut sym);
            if i >= skip {
                sim.process(&sym, &mut out);
            }
        }
        out
    }

    /// Add noise and a carrier offset (the radio's side).
    pub fn radio(x: &[Iq], cfo_hz: f64, cn_db: f64) -> Vec<Iq> {
        use decdvbt_mod::channel::RadioSim;
        let ch = Channel {
            cfo_hz,
            cn_db: Some(cn_db),
            ..Default::default()
        };
        let fs = 64e6 / 7.0;
        let p = x.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / x.len() as f64;
        let mut r = RadioSim::new(&ch, fs, p, 8e6);
        let mut out = Vec::new();
        r.process(x, &mut out);
        out
    }

    #[test]
    fn the_cyclic_prefix_gives_the_mode_and_the_offset() {
        for (text, cfo) in [
            ("64qam-2/3-8k-1/32", 0.0),
            ("16qam-3/4-8k-1/8", 7_000.0),
            ("qpsk-1/2-2k-1/4", -3_000.0),
            ("64qam-7/8-2k-1/16", 1_200.0),
        ] {
            let mode = Mode::parse(text).unwrap();
            let x = radio(&signal(mode, &Channel::default(), 20, 8), cfo, 25.0);
            let f =
                search_mode(&x[..58_000.min(x.len())]).unwrap_or_else(|| panic!("{text}: no mode"));
            assert_eq!((f.fft, f.guard), (mode.fft, mode.guard), "{text}");
            // The offset within a spacing: cfo mod Δf, wrapped to ±0.5.
            let df = mode.bandwidth.sample_rate_hz() / mode.fft.n() as f64;
            let want = cfo / df - (cfo / df).round();
            assert!((f.eps - want).abs() < 0.02, "{text}: ε {} vs {want}", f.eps);
            assert!(f.contrast > 5.0, "{text}: contrast {}", f.contrast);
        }
    }

    #[test]
    fn noise_alone_has_no_mode() {
        let noise = radio(&vec![Iq::new(0.0, 0.0); 60_000], 0.0, 10.0);
        // `radio` of silence is silence (C/N is against the carrier): make
        // noise directly.
        let mut r = decdvbt_mod::rng::Rng::new(5);
        let x: Vec<Iq> = (0..60_000)
            .map(|_| {
                let (a, b) = r.gaussian_pair();
                Iq::new(a as f32, b as f32)
            })
            .collect();
        assert!(search_mode(&x).is_none());
        let _ = noise;
    }

    #[test]
    fn acquisition_finds_the_carrier_offset_and_the_pilot_phase() {
        let mode = Mode::parse("64qam-2/3-8k-1/32").unwrap();
        // 337 kHz up: a UK +333 kHz offset and a bit; 300 spacings of 1.116.
        let cfo = 337_000.0;
        let ch = Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(2.0, -6.0)],
            ..Default::default()
        };
        for skip in [20, 21, 22, 23] {
            let x = radio(&signal(mode, &ch, skip, 8), cfo, 25.0);
            let x = &x[..58_000.min(x.len())];
            let cp = search_mode(x).expect("mode");
            let a = acquire(x, &cp).expect("acquisition");
            let df = mode.bandwidth.sample_rate_hz() / 8192.0;
            assert!(
                (a.cfo_spacings * df - cfo).abs() < 100.0,
                "skip {skip}: found {} Hz",
                a.cfo_spacings * df
            );
            // The symbol at the apex is symbol (skip + i) of the frame, for
            // the i whose guard starts there: its scattered phase.
            let per = 8192 + 256;
            let first_symbol_start = (ChannelSim::bulk_delay()) as f64; // the channel's lag
            let i = ((a.apex - first_symbol_start) / per as f64).round() as usize;
            assert_eq!(
                a.phase,
                (skip + i) % 4,
                "skip {skip}, i {i}, apex {}",
                a.apex
            );
            assert!(a.concentration > 0.5, "{}", a.concentration);
        }
    }

    /// The continual pilots alone give the offset in whole carriers, sharply
    /// (about 1 at the right shift, a few hundredths elsewhere), where the
    /// spectral edges are only good to a few.
    #[test]
    fn the_continual_pilots_give_the_offset_in_whole_carriers() {
        let mode = Mode::parse("64qam-2/3-8k-1/32").unwrap();
        let df = mode.bandwidth.sample_rate_hz() / 8192.0;
        for cfo in [0.0, 337_000.0, -251_000.0] {
            let ch = Channel {
                paths: vec![Path::echo(0.0, 0.0), Path::echo(2.0, -6.0)],
                ..Default::default()
            };
            let x = radio(&signal(mode, &ch, 20, 8), cfo, 20.0);
            let x = &x[..58_000.min(x.len())];
            let cp = search_mode(x).expect("mode");
            let sp = symbol_spectra(x, &cp, 4);
            let (d, coherence) = pilot_offset(&sp, FftSize::K8, 700);
            // The whole carriers of the offset (cp.eps has the fraction).
            let whole = ((cfo / df) - cp.eps).round() as isize;
            assert_eq!(d, whole, "cfo {cfo}: {d} against {whole}");
            assert!(coherence > 0.5, "cfo {cfo}: coherence {coherence}");
        }
    }

    #[test]
    fn a_wrong_mode_is_not_acquired() {
        // A 2k signal searched as it is: fine. But a signal with no pilots of
        // DVB-T's kind (noise-like OFDM with a cyclic prefix) does not pass
        // the pilot test: build 8k-sized symbols of random carriers with a
        // 1/32 prefix.
        let n = 8192;
        let g = 256;
        let mut r = decdvbt_mod::rng::Rng::new(9);
        let fft = Fft1::new(n);
        let mut inv = FftPlanner::<f32>::new().plan_fft_inverse(n);
        let _ = (&fft, &mut inv);
        let plan = FftPlanner::<f32>::new().plan_fft_inverse(n);
        let mut x = Vec::new();
        for _ in 0..8 {
            let mut buf = vec![Iq::new(0.0, 0.0); n];
            for i in (0..3408).chain(n - 3408..n) {
                buf[i] = Iq::new(
                    (r.uniform() as f32 - 0.5) * 2.0,
                    (r.uniform() as f32 - 0.5) * 2.0,
                );
            }
            plan.process(&mut buf);
            x.extend_from_slice(&buf[n - g..]);
            x.extend_from_slice(&buf);
        }
        let x = &x[..58_000.min(x.len())];
        let cp = search_mode(x).expect("it has a cyclic prefix");
        assert_eq!((cp.fft, cp.guard), (FftSize::K8, Guard::G1_32));
        assert!(acquire(x, &cp).is_none(), "random carriers are not DVB-T");
        let _ = (Modulation::Qpsk, Rate::R1_2);
    }
}
