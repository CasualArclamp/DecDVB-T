// made by CasualArclamp
//! Channel estimation and equalisation, a symbol at a time.
//!
//! 1. **Common phase and timing slope.** The continual pilots sit at the
//!    same carriers in every symbol, so each symbol's against a running
//!    reference gives the symbol's phase offset and the slope across the
//!    band that a sub-sample timing error makes (2.6 rad at the edge for a
//!    sample). The slope is found as the peak of the pilots' coherent sum
//!    over a range of slopes (the maximum-likelihood estimate: all 177 pilots
//!    count, and nothing has to be unwrapped, which fails once the pilots'
//!    SNR is under ~15 dB), refined by a weighted line fit. Both are taken out
//!    of all the carriers. A symbol whose pilots will not line up (low
//!    coherence) is equalised but taken no further: it would spoil the
//!    reference and the channel estimate.
//! 2. **The channel on a 3-carrier grid.** Scattered pilots stand every 12th
//!    carrier, shifted 3 each symbol, so four symbols between them touch
//!    every third carrier. The grid holds the latest estimate at each (each
//!    pilot's value divided out), smoothed over time by an IIR as the channel
//!    allows; the continual pilots refresh their carriers every symbol.
//! 3. **Every carrier**, by interpolating the grid with a short windowed-sinc
//!    polyphase filter (complex values, so the phase goes with it). Linear
//!    interpolation is not enough: an echo of 20 µs in 2k mode turns the
//!    channel by 1.7 rad between grid points and the middle of a line between
//!    them is a third short; 8 taps follow echoes up to about a third of the
//!    grid's period (25 µs in 2k, 100 µs in 8k).
//! 4. **Equalising**: each carrier divided by its estimate. The estimate's
//!    power is kept as the cell's weight: a cell in a spectral notch is
//!    unreliable, and the decoder is told so.
//! 5. **Noise per carrier.** Each scattered pilot is compared with the grid's
//!    estimate before it is folded in; the squared differences, averaged over
//!    time and a few neighbouring grid points, are the noise (and
//!    interference: an adjacent channel's skirt, a spur) at that carrier, in
//!    the channel estimate's units. The demapper weighs a cell by its gain
//!    over this: a cell where the noise is far above the thermal floor is
//!    told to the decoder as the poor evidence it is.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::FftSize;
use decdvbt_ofdm::pilots::{continual_pilots, prbs};

/// How much of each new pilot estimate goes into the grid (the rest being
/// the old): 1 for no smoothing. Half averages about three estimates, a few
/// ms for the pilots of one carrier.
const ALPHA: f32 = 0.5;
/// How fast the continual pilots' reference follows.
const BETA: f32 = 0.2;
/// The slopes tried (rad at the band's edge): ±SLOPE_MAX in steps of
/// SLOPE_STEP, a coherent sum of the pilots each. A step is well inside the
/// main lobe (π wide) of the sum.
const SLOPE_MAX: f32 = 6.0;
const SLOPE_STEP: f32 = 0.5;
/// Pilot coherence below which a symbol is not used to update the estimate.
const MIN_COHERENCE: f32 = 0.35;
/// How fast a grid point's noise estimate follows its innovations (one a
/// pilot, every fourth symbol), and how many grid points either side are
/// averaged into a carrier's.
const NOISE_RATE: f32 = 0.1;
const NOISE_SPAN: usize = 3;
/// What a pilot's innovation (new value against the estimate it joins) is of
/// the noise a data cell's channel sees: the pilot is 4/3 the amplitude (so its
/// value in channel units has 9/16 of the noise), and the innovation holds the
/// old estimate's noise too (α/(2−α) of it, a third at α = 1/2): 9/16 · 4/3.
const INNOVATION: f32 = 0.75;
/// The interpolator reaches this many grid points either side.
const L: usize = 4;
/// Kaiser β of its window.
const INTERP_BETA: f64 = 4.0;

pub struct Equalizer {
    fft: FftSize,
    k_total: usize,
    kmid: usize,
    cps: Vec<usize>,
    /// 3/4 · (1 − 2 w_k) per carrier: the reciprocal of a pilot's value.
    inv: Vec<f32>,
    grid: Vec<Iq>,
    seen: Vec<bool>,
    cp_ref: Vec<Iq>,
    ref_ok: bool,
    symbols: u64,
    /// Per carrier, for the last symbol: the channel estimate, the equalised
    /// value and |H|².
    pub h: Vec<Iq>,
    pub z: Vec<Iq>,
    pub w: Vec<f32>,
    /// The last symbol's fitted phase offset (rad) and slope (rad across
    /// half the band).
    pub cpe: (f32, f32),
    /// How much of each new pilot estimate goes into the grid.
    pub alpha: f32,
    /// Whether the common phase and slope are fitted and taken out.
    pub use_cpe: bool,
    /// How far the continual pilots of the last symbol were from their
    /// reference after the fit, against the reference's power (a noise-to-
    /// signal ratio: the channel is as it was and the noise is this much).
    pub cp_nsr: f32,
    /// Each continual pilot of the last symbol against its reference, as a
    /// ratio (1 for no change).
    pub cp_resid: Vec<Iq>,
    /// How well the last symbol's continual pilots lined up with the
    /// reference once fitted: |Σ z|/Σ|z|, 1 for perfectly.
    pub coherence: f32,
    /// The pilots' positions across the band, (k − kmid)/kmid.
    cp_x: Vec<f32>,
    /// e^(−j·b·x) for each slope tried and each pilot.
    scan: Vec<Iq>,
    /// The noise power at each carrier (channel-estimate units²), from the
    /// pilots' innovations.
    pub noise: Vec<f32>,
    /// The same per grid point, before smoothing across them.
    nz: Vec<f32>,
    /// The grid with `L` copies of its end values either side, for the
    /// interpolator.
    ext: Vec<Iq>,
    /// The interpolator's taps for the carriers a third and two thirds of
    /// the way between grid points, for the grid points m−L+1 .. m+L.
    taps: [[f32; 2 * L]; 2],
}

/// The interpolation taps for a point `f` (0..1) of the way from grid point
/// m to m+1: a Kaiser-windowed sinc, normalised to unity gain.
fn interp_taps(f: f64) -> [f32; 2 * L] {
    let mut t = [0.0f32; 2 * L];
    let mut sum = 0.0;
    let i0b = decdvbt_dsp::resample::bessel_i0(INTERP_BETA);
    for (j, v) in t.iter_mut().enumerate() {
        let d = j as f64 - (L as f64 - 1.0) - f;
        let r = d / L as f64;
        let sinc = if d == 0.0 {
            1.0
        } else {
            (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d)
        };
        let w = decdvbt_dsp::resample::bessel_i0(INTERP_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0b;
        *v = (sinc * w) as f32;
        sum += sinc * w;
    }
    for v in &mut t {
        *v /= sum as f32;
    }
    t
}

impl Equalizer {
    pub fn new(fft: FftSize) -> Equalizer {
        let k_total = fft.active_carriers();
        let cps: Vec<usize> = continual_pilots(fft).iter().map(|&k| k as usize).collect();
        let wk = prbs(k_total);
        let inv = wk
            .iter()
            .map(|&w| 0.75 * (1.0 - 2.0 * f32::from(w)))
            .collect();
        let g = (k_total - 1) / 3 + 1;
        let kmid = (k_total - 1) / 2;
        let cp_x: Vec<f32> = cps
            .iter()
            .map(|&k| (k as f32 - kmid as f32) / kmid as f32)
            .collect();
        let n_slopes = (2.0 * SLOPE_MAX / SLOPE_STEP) as usize + 1;
        let scan = (0..n_slopes)
            .flat_map(|i| {
                let b = -SLOPE_MAX + i as f32 * SLOPE_STEP;
                cp_x.iter()
                    .map(move |&x| Iq::new((b * x).cos(), -(b * x).sin()))
            })
            .collect();
        Equalizer {
            fft,
            k_total,
            kmid: (k_total - 1) / 2,
            cp_ref: vec![Iq::new(0.0, 0.0); cps.len()],
            cps,
            inv,
            grid: vec![Iq::new(0.0, 0.0); g],
            seen: vec![false; g],
            ref_ok: false,
            symbols: 0,
            h: vec![Iq::new(0.0, 0.0); k_total],
            z: vec![Iq::new(0.0, 0.0); k_total],
            w: vec![0.0; k_total],
            cpe: (0.0, 0.0),
            alpha: ALPHA,
            use_cpe: true,
            cp_nsr: 0.0,
            noise: vec![1.0; k_total],
            nz: vec![0.0; g],
            cp_resid: Vec::new(),
            coherence: 1.0,
            cp_x,
            scan,
            ext: vec![Iq::new(0.0, 0.0); g + 2 * L],
            taps: [interp_taps(1.0 / 3.0), interp_taps(2.0 / 3.0)],
        }
    }

    pub fn fft(&self) -> FftSize {
        self.fft
    }

    /// How many samples later than the reference the last symbol's window
    /// was, from the pilots' phase slope: the slope of 2π·k·τ/N across the
    /// band is `cpe.1` at the band's edge.
    pub fn timing_error(&self) -> f32 {
        self.cpe.1 * self.fft.n() as f32 / (std::f32::consts::TAU * self.kmid as f32)
    }

    /// Four symbols in, every third carrier has been seen: the estimate is
    /// whole.
    pub fn ready(&self) -> bool {
        self.symbols >= 4
    }

    pub fn reset(&mut self) {
        self.seen.fill(false);
        self.ref_ok = false;
        self.symbols = 0;
    }

    /// Take a symbol's carriers `c` (modified: its phase offset and slope
    /// taken out), the symbol being number `phase` (its index in the frame
    /// mod 4); fills `h`, `z` and `w`.
    pub fn process(&mut self, c: &mut [Iq], phase: usize) {
        debug_assert_eq!(c.len(), self.k_total);
        let kmid = self.kmid as f32;

        // 1. The continual pilots against their reference.
        let hcp: Vec<Iq> = self.cps.iter().map(|&k| c[k] * self.inv[k]).collect();
        let mut trusted = true;
        if self.ref_ok && self.use_cpe {
            let z: Vec<Iq> = hcp
                .iter()
                .zip(&self.cp_ref)
                .map(|(h, r)| h * r.conj())
                .collect();
            // The coarse slope: the strongest coherent sum.
            let n = z.len();
            let (mut best, mut best_i, mut best_sum) = (-1.0f32, 0usize, Iq::new(0.0, 0.0));
            for (i, row) in self.scan.chunks_exact(n).enumerate() {
                let mut sum = Iq::new(0.0, 0.0);
                for (zj, r) in z.iter().zip(row) {
                    sum += zj * r;
                }
                let p = sum.norm_sqr();
                if p > best {
                    (best, best_i, best_sum) = (p, i, sum);
                }
            }
            let mut b = -SLOPE_MAX + best_i as f32 * SLOPE_STEP;
            let mut a = best_sum.arg();
            // Refined by a weighted line fit through what the coarse one
            // leaves (small, so nothing wraps).
            for _ in 0..2 {
                let (mut s0, mut sx, mut sxx, mut sp, mut sxp) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
                for (zj, &xj) in z.iter().zip(&self.cp_x) {
                    let th = a + b * xj;
                    let r = zj * Iq::new(th.cos(), -th.sin());
                    let (w, ph, xj) = (f64::from(r.norm()), f64::from(r.arg()), f64::from(xj));
                    s0 += w;
                    sx += w * xj;
                    sxx += w * xj * xj;
                    sp += w * ph;
                    sxp += w * xj * ph;
                }
                let d = s0 * sxx - sx * sx;
                if d > 0.0 {
                    let db = (s0 * sxp - sx * sp) / d;
                    let da = (sp - db * sx) / s0;
                    b += db as f32;
                    a += da as f32;
                }
            }
            // How well the pilots line up now.
            let (mut sum, mut mag) = (Iq::new(0.0, 0.0), 0.0f32);
            for (zj, &xj) in z.iter().zip(&self.cp_x) {
                let th = a + b * xj;
                sum += zj * Iq::new(th.cos(), -th.sin());
                mag += zj.norm();
            }
            self.coherence = if mag > 0.0 { sum.norm() / mag } else { 0.0 };
            trusted = self.coherence >= MIN_COHERENCE;
            self.cpe = (a, b);
            for (k, v) in c.iter_mut().enumerate() {
                let th = a + b * (k as f32 - kmid) / kmid;
                *v *= Iq::new(th.cos(), -th.sin());
            }
        } else {
            self.cpe = (0.0, 0.0);
        }
        // The reference follows the corrected pilots.
        let (mut err, mut pow) = (0.0f32, 0.0f32);
        self.cp_resid.clear();
        for ((&k, r), _) in self.cps.iter().zip(&mut self.cp_ref).zip(&hcp) {
            let h = c[k] * self.inv[k];
            if self.ref_ok {
                err += (h - *r).norm_sqr();
                pow += r.norm_sqr();
                self.cp_resid.push(h * r.conj() / r.norm_sqr().max(1e-12));
            }
            if !self.ref_ok {
                *r = h;
            } else if trusted {
                *r = *r * (1.0 - BETA) + h * BETA;
            }
        }
        if pow > 0.0 {
            self.cp_nsr = err / pow;
        }
        self.ref_ok = true;

        // 2. The grid: this symbol's scattered pilots (carriers 3m with m
        // congruent to the phase mod 4) and the continual ones.
        let m_max = self.grid.len();
        let alpha = self.alpha;
        if trusted {
            for m in (phase % 4..m_max).step_by(4) {
                let k = 3 * m;
                if k >= self.k_total {
                    continue;
                }
                let v = c[k] * self.inv[k];
                if self.seen[m] {
                    // The innovation: how far the new pilot is from what the
                    // estimate said.
                    let e = (v - self.grid[m]).norm_sqr() / INNOVATION;
                    self.nz[m] += NOISE_RATE * (e - self.nz[m]);
                    self.grid[m] = self.grid[m] * (1.0 - alpha) + v * alpha;
                } else {
                    self.nz[m] = 0.02 * v.norm_sqr();
                    self.grid[m] = v;
                    self.seen[m] = true;
                }
            }
            for &k in &self.cps {
                if k % 3 == 0 {
                    let (m, v) = (k / 3, c[k] * self.inv[k]);
                    if self.seen[m] {
                        self.grid[m] = self.grid[m] * (1.0 - alpha) + v * alpha;
                    } else {
                        self.grid[m] = v;
                        self.seen[m] = true;
                    }
                }
            }
        }

        // 3. Every carrier, from the grid, its ends held out `L` points. The
        // grid's own points are the carriers 3m; the two between each pair
        // take 2·L taps.
        self.ext[L..L + m_max].copy_from_slice(&self.grid);
        let (first, last) = (self.grid[0], self.grid[m_max - 1]);
        self.ext[..L].fill(first);
        self.ext[L + m_max..].fill(last);
        for m in 0..m_max {
            self.h[3 * m] = self.grid[m];
            for (j, taps) in self.taps.iter().enumerate() {
                let k = 3 * m + 1 + j;
                if k >= self.k_total {
                    break;
                }
                // Grid points m−L+1 .. m+L sit at ext[m+1 .. m+2L+1].
                let win = &self.ext[m + 1..m + 1 + 2 * L];
                let mut acc = Iq::new(0.0, 0.0);
                for (g, &t) in win.iter().zip(taps) {
                    acc += g * t;
                }
                self.h[k] = acc;
            }
        }

        // The noise at each carrier: the grid points' estimates averaged over
        // a few neighbours (a running sum), the nearest one's for the carriers
        // between.
        let span = NOISE_SPAN as isize;
        let mut run: f32 = (0..=NOISE_SPAN.min(m_max - 1)).map(|i| self.nz[i]).sum();
        let mut count = NOISE_SPAN.min(m_max - 1) as isize + 1;
        for m in 0..m_max {
            let avg = run / count as f32;
            for j in 0..3 {
                let k = 3 * m + j;
                if k < self.k_total {
                    self.noise[k] = avg.max(1e-9);
                }
            }
            // The window moves on: m + span + 1 in, m − span out.
            let (inn, out) = (m as isize + span + 1, m as isize - span);
            if (inn as usize) < m_max {
                run += self.nz[inn as usize];
                count += 1;
            }
            if out >= 0 {
                run -= self.nz[out as usize];
                count -= 1;
            }
        }

        // 4. Equalise.
        for (((zk, wk), &h), &ck) in self
            .z
            .iter_mut()
            .zip(&mut self.w)
            .zip(&self.h)
            .zip(c.iter())
        {
            let p = h.norm_sqr().max(1e-12);
            *zk = ck * h.conj() / p;
            *wk = p;
        }
        self.symbols += 1;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::interp::{Interpolator, Planar};
    use crate::lock::lock_on;
    use crate::symbol::SymbolReader;
    use decdvbt_core::dvbt::{Alpha, Mode};
    use decdvbt_mod::channel::{Channel, ChannelSim, Path, RadioSim};
    use decdvbt_mod::sim::{SimOptions, generate};
    use decdvbt_ofdm::mapping::Constellation;
    use decdvbt_ofdm::pilots::SymbolPlan;

    /// A signal at 10 MS/s: `seconds` of `mode` through `ch`.
    pub fn make(mode: &str, ch: Channel, seconds: f64) -> Vec<Iq> {
        let opts = SimOptions {
            mode: Mode::parse(mode).unwrap(),
            cell_id: None,
            ts_file: None,
            seconds,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: ch,
        };
        let mut v = Vec::new();
        generate(&opts, |b| {
            v.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        v
    }

    /// Lock, then read and equalise every symbol, returning the MER (dB) of
    /// the data cells against their nearest constellation points, for the
    /// symbols after the estimate settles.
    fn mer(mode_text: &str, ch: Channel, seconds: f64) -> f64 {
        let mode = Mode::parse(mode_text).unwrap();
        let x = make(mode_text, ch, seconds);
        let mut planar = Planar::default();
        planar.set(&x);
        let mut head = Planar::default();
        head.set(&x[..70_000]);
        let interp = Interpolator::new();
        let rho = 10e6 / mode.bandwidth.sample_rate_hz();
        let l = lock_on(
            &interp,
            &head,
            0,
            rho,
            mode.bandwidth.sample_rate_hz(),
            0.0,
            0.0,
        )
        .expect("lock");
        let mut reader = SymbolReader::new(
            l.fft,
            l.guard,
            l.apex,
            l.f_total,
            rho,
            mode.bandwidth.sample_rate_hz(),
            1,
        );
        let mut eq = Equalizer::new(l.fft);
        let plan = SymbolPlan::new(l.fft);
        let cons = Constellation::new(mode.modulation, Alpha::NonHierarchical);
        let (mut carriers, mut errors, mut count) = (Vec::new(), 0.0f64, 0usize);
        let mut phase = l.phase;
        let mut n = 0;
        reader.alpha_cp = 0.05;
        reader.beta_cp = 0.0005;
        while reader.read(&interp, &planar, 0, &mut carriers).is_ok() {
            eq.process(&mut carriers, phase);
            reader.correct(f64::from(eq.timing_error()));
            if n >= 20 {
                for &k in plan.data_carriers(phase) {
                    let z = eq.z[k as usize];
                    let near = cons.point(cons.nearest(z));
                    errors += f64::from((z - near).norm_sqr());
                    count += 1;
                }
            }
            phase = (phase + 1) % 4;
            n += 1;
        }
        assert!(n > 100, "only {n} symbols");
        10.0 * (count as f64 / errors).log10()
    }

    #[test]
    fn a_clean_channel_equalises_to_the_noise_limit() {
        // 28 dB against 8 MHz: the cells' MER is about that, less a little for
        // the channel estimate's own noise and the 7.61/8 bandwidth.
        let ch = Channel {
            cn_db: Some(28.0),
            ..Default::default()
        };
        let m = mer("64qam-2/3-8k-1/32", ch, 0.2);
        assert!((26.0..=29.0).contains(&m), "MER {m} dB");
    }

    #[test]
    fn an_echo_and_a_carrier_offset_are_equalised_away() {
        let ch = Channel {
            paths: vec![
                Path::echo(0.0, 0.0),
                Path::echo(2.5, -4.0),
                Path::echo(7.0, -9.0),
            ],
            cfo_hz: 166_667.0,
            sco_ppm: 15.0,
            cn_db: Some(30.0),
            ..Default::default()
        };
        // The echoes put notches in the band: the cells in them are noisy
        // whatever the equaliser does (19.4 dB without the offsets, too).
        let m = mer("64qam-3/4-8k-1/32", ch, 0.3);
        assert!(m > 18.5, "MER {m} dB");
    }

    #[test]
    fn a_2k_signal_with_a_long_guard_equalises_too() {
        let ch = Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(20.0, -6.0)],
            cn_db: Some(30.0),
            ..Default::default()
        };
        let m = mer("16qam-3/4-2k-1/4", ch, 0.15);
        assert!(m > 24.0, "MER {m} dB");
    }

    #[test]
    fn the_phase_and_slope_fit_finds_what_was_put_in() {
        // Pilots turned by a known phase and slope come back out of `cpe`.
        let mut eq = Equalizer::new(FftSize::K2);
        let k_total = 1705;
        let wk = prbs(k_total);
        let make = |a: f32, b: f32| -> Vec<Iq> {
            (0..k_total)
                .map(|k| {
                    let p = 4.0 / 3.0 * (1.0 - 2.0 * f32::from(wk[k]));
                    let th = a + b * (k as f32 - 852.0) / 852.0;
                    Iq::new(th.cos(), th.sin()) * p
                })
                .collect()
        };
        let mut first = make(0.0, 0.0);
        eq.process(&mut first, 0);
        let mut turned = make(0.3, -0.2);
        eq.process(&mut turned, 1);
        assert!(
            (eq.cpe.0 - 0.3).abs() < 1e-3 && (eq.cpe.1 + 0.2).abs() < 1e-3,
            "{:?}",
            eq.cpe
        );
        let _ = (ChannelSim::bulk_delay(), RadioSim::new);
    }
}
