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
use decdvbt_core::dvbt::{FftSize, Guard};
use decdvbt_ofdm::pilots::{continual_pilots, prbs};

use crate::mobile::{Denoiser, Estimate, Ring, Tracker};

// The work is mostly loops over a few thousand carriers. They are written as
// plain loops over separate real and imaginary arrays (which the compiler
// turns into vector code) and compiled twice, once for the baseline CPU and
// once with AVX2 and FMA switched on, the right one chosen at run time (as the
// interpolator and the Viterbi do).

/// How much of each new pilot estimate goes into the grid (the rest being
/// the old): 1 for no smoothing. Half averages about three estimates, a few
/// ms for the pilots of one carrier.
const ALPHA: f32 = 0.5;
/// How fast the continual pilots' reference follows.
const BETA: f32 = 0.2;
/// The slopes tried (rad at the band's edge): ±SLOPE_MAX in steps of
/// SLOPE_STEP, a coherent sum of the pilots each. A step is well inside the
/// main lobe (π wide) of the sum. ±18 rad is ±7 samples: an Airspy R2 now and
/// then loses a few samples inside the radio (2–7 at 10 MS/s, seen live), and
/// a step the scan cannot see leaves the symbols after it unequalised until
/// the slow cyclic-prefix loop pulls the window back (~70 packets lost a
/// slip at ±6 rad, ~25 at ±18; +0.25 % of a core).
const SLOPE_MAX: f32 = 18.0;
const SLOPE_STEP: f32 = 0.5;
/// Pilot coherence below which a symbol is not used to update the estimate.
const MIN_COHERENCE: f32 = 0.35;
/// Symbols in a row below it that mean the reference is not this channel's:
/// start over from the symbol in hand (see `process_inner`).
const RESYNC_AFTER: u32 = 24;
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
/// How often (in symbols) the filters are fitted to the measured correlation,
/// and when the time filter takes over from the IIR and gives back: its
/// modelled error must be this many times better than the IIR's, and the IIR's
/// big enough to matter (against the pilots' power: −25 dB, and −27 dB to give
/// up). Two thresholds, so that it does not flap.
const TUNE_EVERY: u32 = 256;
/// Carriers either side of the centre whose continual pilots the measurement
/// leaves out (see `new`).
const DC_SPAN: usize = 32;
const ON_RATIO: f32 = 1.8;
const OFF_RATIO: f32 = 1.35;
const MIN_MSE_ON: f32 = 0.003;
const MIN_MSE_OFF: f32 = 0.002;
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
    /// Symbols in a row whose continual pilots did not match the reference.
    untrusted: u32,
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
    /// Fill `h` and `cp_resid` (for the diagnostics: the decoder does not
    /// read them) on each symbol.
    pub diagnostics: bool,
    avx2: bool,
    /// The pilots' positions across the band, (k − kmid)/kmid, padded to a
    /// multiple of eight with zeros.
    cp_x: Vec<f32>,
    /// e^(−j·b·x) for each slope tried and each pilot, as real and imaginary
    /// arrays of `np` (the padded pilot count) per slope.
    scan_r: Vec<f32>,
    scan_i: Vec<f32>,
    np: usize,
    /// Scratch: the pilots against their reference, and the grid and the
    /// interpolated points as real and imaginary arrays.
    zr: Vec<f32>,
    zi: Vec<f32>,
    ext_r: Vec<f32>,
    ext_i: Vec<f32>,
    mid: [[Vec<f32>; 2]; 2],
    hr: Vec<f32>,
    hi: Vec<f32>,
    /// The noise power at each carrier (channel-estimate units²), from the
    /// pilots' innovations.
    pub noise: Vec<f32>,
    /// The same per grid point, before smoothing across them.
    nz: Vec<f32>,
    /// The interpolator's taps for the carriers a third and two thirds of
    /// the way between grid points, for the grid points m−L+1 .. m+L.
    taps: [[f32; 2 * L]; 2],
    /// How the channel changes, measured on the continual pilots, and what
    /// the measurement says (see `mobile.rs`).
    trk: Tracker,
    est: Option<Estimate>,
    /// Symbols since the filters were last tuned to the measurement.
    since_tune: u32,
    /// Whether the grid's estimate comes from the time filter over the last
    /// four pilots (a moving channel) rather than the IIR (a still one).
    mobile: bool,
    /// Force the choice (tests, diagnostics) instead of the measurement's.
    pub forced: Option<bool>,
    ring: Ring,
    /// Counts groups of four symbols (a new one at pilot phase 0), and the
    /// phase of the symbol before.
    epoch: usize,
    last_phase: usize,
    /// The delay-domain filter of the grid, if the guard is short enough for
    /// it to matter, and whether it is on.
    denoiser: Option<Denoiser>,
    denoise_on: bool,
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
        let trk_used: Vec<bool> = cps.iter().map(|&k| k.abs_diff(kmid) > DC_SPAN).collect();
        let np = cps.len().next_multiple_of(8);
        let mut cp_x = cp_x;
        cp_x.resize(np, 0.0);
        let (mut scan_r, mut scan_i) = (Vec::new(), Vec::new());
        for i in 0..n_slopes {
            let b = -SLOPE_MAX + i as f32 * SLOPE_STEP;
            for &x in &cp_x {
                // The padding pilots have no weight: z is zero there.
                scan_r.push((b * x).cos());
                scan_i.push(-(b * x).sin());
            }
        }
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
            untrusted: 0,
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
            diagnostics: false,
            avx2: Self::detect(),
            cp_x,
            scan_r,
            scan_i,
            np,
            zr: vec![0.0; np],
            zi: vec![0.0; np],
            ext_r: vec![0.0; g + 2 * L],
            ext_i: vec![0.0; g + 2 * L],
            mid: std::array::from_fn(|_| std::array::from_fn(|_| vec![0.0; g])),
            hr: vec![0.0; k_total],
            hi: vec![0.0; k_total],
            taps: [interp_taps(1.0 / 3.0), interp_taps(2.0 / 3.0)],
            // The continual pilots by the band's centre are left out of the
            // measurement of the channel's changes: there the radio's DC
            // offset, and its correction (the front end's estimate moves a
            // step a block, every 13 ms at 10 MS/s), add a change of their
            // own that read as 3 % of the power fading at 76 Hz on a rooftop
            // aerial (C23), enough to take the time filter up on a still
            // channel.
            trk: Tracker::new(trk_used),
            est: None,
            since_tune: 0,
            mobile: false,
            forced: None,
            ring: Ring::new(g),
            epoch: 0,
            last_phase: 0,
            denoiser: None,
            denoise_on: false,
        }
    }

    pub fn fft(&self) -> FftSize {
        self.fft
    }

    /// The last symbol's channel estimate on every carrier, as real and
    /// imaginary arrays.
    pub fn channel(&self) -> (&[f32], &[f32]) {
        (&self.hr, &self.hi)
    }

    fn detect() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }

    /// Use the baseline code even where AVX2 is there (tests, comparison).
    pub fn without_avx2(mut self) -> Equalizer {
        self.avx2 = false;
        self
    }

    /// How many samples later than the reference the last symbol's window
    /// was, from the pilots' phase slope: the slope of 2π·k·τ/N across the
    /// band is `cpe.1` at the band's edge.
    pub fn timing_error(&self) -> f32 {
        self.cpe.1 * self.fft.n() as f32 / (std::f32::consts::TAU * self.kmid as f32)
    }

    /// The signal-to-noise ratio the last symbol's channel estimate gives, dB:
    /// the mean power gain over the mean noise power, across the carriers.
    pub fn snr_db(&self) -> f32 {
        let (gain, noise): (f32, f32) = (self.w.iter().sum(), self.noise.iter().sum());
        10.0 * (gain / noise.max(1e-20)).max(1e-9).log10()
    }

    /// Four symbols in, every third carrier has been seen: the estimate is
    /// whole.
    pub fn ready(&self) -> bool {
        self.symbols >= 4
    }

    pub fn reset(&mut self) {
        self.restart_filters();
        self.seen.fill(false);
        self.ref_ok = false;
        self.untrusted = 0;
        self.symbols = 0;
    }

    /// Take a symbol's carriers `c` (modified: its phase offset and slope
    /// taken out), the symbol being number `phase` (its index in the frame
    /// mod 4); fills `z`, `w` and `noise` (and, with `diagnostics`, `h`).
    pub fn process(&mut self, c: &mut [Iq], phase: usize) {
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: AVX2 and FMA were detected at construction.
            unsafe { self.process_avx2(c, phase) };
            return;
        }
        self.process_inner(c, phase);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn process_avx2(&mut self, c: &mut [Iq], phase: usize) {
        self.process_inner(c, phase);
    }

    #[inline(always)]
    fn process_inner(&mut self, c: &mut [Iq], phase: usize) {
        debug_assert_eq!(c.len(), self.k_total);
        if phase.is_multiple_of(4) {
            self.epoch = self.epoch.wrapping_add(1);
        }
        let kmid = self.kmid;
        let (np, n_cp) = (self.np, self.cps.len());

        // 1. The continual pilots against their reference.
        let mut trusted = true;
        let mut hcp = [Iq::new(0.0, 0.0); 192];
        let hcp = &mut hcp[..n_cp];
        for (h, &k) in hcp.iter_mut().zip(&self.cps) {
            *h = c[k] * self.inv[k];
        }
        if self.ref_ok && self.use_cpe {
            for (j, (h, r)) in hcp.iter().zip(&self.cp_ref).enumerate() {
                let z = h * r.conj();
                self.zr[j] = z.re;
                self.zi[j] = z.im;
            }
            // The coarse slope: the strongest coherent sum over the slopes
            // tried.
            let (mut best, mut best_i, mut best_sum) = (-1.0f32, 0usize, (0.0f32, 0.0f32));
            for (i, (sr, si)) in self
                .scan_r
                .chunks_exact(np)
                .zip(self.scan_i.chunks_exact(np))
                .enumerate()
            {
                let sum = cdot(&self.zr, &self.zi, sr, si);
                let p = sum.0 * sum.0 + sum.1 * sum.1;
                if p > best {
                    (best, best_i, best_sum) = (p, i, sum);
                }
            }
            let b0 = -SLOPE_MAX + best_i as f32 * SLOPE_STEP;
            let a0 = best_sum.1.atan2(best_sum.0);
            // How well the pilots line up (at the coarse slope, which is
            // within a quarter radian of the fine one).
            let mag: f32 = (0..n_cp)
                .map(|j| (self.zr[j] * self.zr[j] + self.zi[j] * self.zi[j]).sqrt())
                .sum();
            self.coherence = if mag > 0.0 { best.sqrt() / mag } else { 0.0 };
            trusted = self.coherence >= MIN_COHERENCE;
            // Refined by a weighted line fit through the little the coarse
            // one leaves (under a quarter radian, so its phase is
            // arctan(t) = t − t³/3 + t⁵/5 of the ratio t of its parts).
            let (sr, si) = (
                &self.scan_r[best_i * np..(best_i + 1) * np],
                &self.scan_i[best_i * np..(best_i + 1) * np],
            );
            let (ca, sa) = (a0.cos(), -a0.sin());
            let (mut s0, mut sx, mut sxx, mut sp, mut sxp) = (0.0f32, 0.0, 0.0, 0.0, 0.0);
            for j in 0..n_cp {
                let (zr, zi) = (self.zr[j], self.zi[j]);
                let (rr, ri) = (zr * sr[j] - zi * si[j], zr * si[j] + zi * sr[j]);
                let (qr, qi) = (rr * ca - ri * sa, rr * sa + ri * ca);
                let w = (qr * qr + qi * qi).sqrt();
                // A pilot a quarter turn or more from the fit is noise.
                let t = if qr > 1e-9 {
                    (qi / qr).clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                let t2 = t * t;
                let ph = t * (1.0 - t2 * (1.0 / 3.0 - t2 * 0.2));
                let w = if qr > 1e-9 { w } else { 0.0 };
                let x = self.cp_x[j];
                s0 += w;
                sx += w * x;
                sxx += w * x * x;
                sp += w * ph;
                sxp += w * x * ph;
            }
            let d = s0 * sxx - sx * sx;
            let (mut da, mut db) = (0.0, 0.0);
            if d > 0.0 {
                db = (s0 * sxp - sx * sp) / d;
                da = (sp - db * sx) / s0;
            }
            let (a, b) = (a0 + da, b0 + db);
            self.cpe = (a, b);
            // Taken out of every carrier: e^(−j(a + b·x)) with x linear in k,
            // so a phasor stepped out from the middle either way (renewed
            // exactly now and then against the rounding).
            let step = -b / kmid as f32;
            let (cs, sn) = (step.cos(), step.sin());
            let stepv = Iq::new(cs, sn);
            let mid = Iq::new(a.cos(), -a.sin());
            c[kmid] *= mid;
            let (mut up, mut down) = (mid, mid);
            let n_side = c.len() - 1 - kmid;
            for i in 1..=n_side {
                up *= stepv;
                down *= stepv.conj();
                if i % 256 == 0 {
                    let th = step * i as f32;
                    up = mid * Iq::new(th.cos(), th.sin());
                    down = mid * Iq::new(th.cos(), -th.sin());
                }
                c[kmid + i] *= up;
                c[kmid - i] *= down;
            }
        } else {
            self.cpe = (0.0, 0.0);
        }
        // A symbol that does not match the reference is not believed, and
        // neither the reference nor the channel estimate moves for it: right
        // for a fade, a burst of noise. But if the reference was made from
        // something that was not this channel (samples of the channel tuned
        // from, the transient of a retune, a channel that has changed), no
        // symbol will ever match it, the estimate stays frozen on the wrong
        // channel and everything equalised comes out as noise, for as long as
        // the signal lasts. So after a run of them, start over from this one.
        if trusted {
            self.untrusted = 0;
        } else {
            self.untrusted += 1;
            if self.untrusted >= RESYNC_AFTER {
                self.untrusted = 0;
                self.ref_ok = false;
                self.seen.fill(false);
                self.symbols = 0;
                self.restart_filters();
                trusted = true;
            }
        }
        // The reference follows the corrected pilots.
        let (mut err, mut pow) = (0.0f32, 0.0f32);
        if self.diagnostics {
            self.cp_resid.clear();
        }
        let mut hnow = [Iq::new(0.0, 0.0); 192];
        for (j, (&k, r)) in self.cps.iter().zip(&mut self.cp_ref).enumerate() {
            let h = c[k] * self.inv[k];
            hnow[j] = h;
            if self.ref_ok {
                err += (h - *r).norm_sqr();
                pow += r.norm_sqr();
                if self.diagnostics {
                    self.cp_resid.push(h * r.conj() / r.norm_sqr().max(1e-12));
                }
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
        if self.ref_ok {
            self.trk.push(&hnow[..n_cp]);
        }
        self.ref_ok = true;

        // 2. The grid: this symbol's scattered pilots (carriers 3m with m
        // congruent to the phase mod 4) and the continual ones.
        let m_max = self.grid.len();
        let alpha = self.alpha;
        if trusted && self.mobile {
            let inv = &self.inv;
            let predict = self.est.as_ref().map(|e| e.predict);
            self.ring.update(
                self.epoch,
                phase,
                self.k_total,
                predict.as_ref(),
                &mut self.nz,
                NOISE_RATE,
                INNOVATION,
                |m| c[3 * m] * inv[3 * m],
            );
        } else if trusted {
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
        // take 2·L taps: a short filter over the grid, as real and imaginary
        // arrays.
        if let (true, Some(est)) = (self.mobile, &self.est) {
            self.ring.eval(
                self.epoch,
                phase,
                &est.weights,
                &mut self.ext_r[L..L + m_max],
                &mut self.ext_i[L..L + m_max],
            );
        } else {
            for (m, g) in self.grid.iter().enumerate() {
                self.ext_r[L + m] = g.re;
                self.ext_i[L + m] = g.im;
            }
        }
        if self.denoise_on
            && let Some(d) = &mut self.denoiser
        {
            d.run(&mut self.ext_r[L..L + m_max], &mut self.ext_i[L..L + m_max]);
        }
        let (first, last) = (
            Iq::new(self.ext_r[L], self.ext_i[L]),
            Iq::new(self.ext_r[L + m_max - 1], self.ext_i[L + m_max - 1]),
        );
        self.ext_r[..L].fill(first.re);
        self.ext_i[..L].fill(first.im);
        self.ext_r[L + m_max..].fill(last.re);
        self.ext_i[L + m_max..].fill(last.im);
        for (ph, taps) in self.taps.iter().enumerate() {
            // Grid points m−L+1 .. m+L sit at ext[m+1 .. m+2L+1].
            fir(&self.ext_r[1..], taps, &mut self.mid[ph][0]);
            fir(&self.ext_i[1..], taps, &mut self.mid[ph][1]);
        }
        // Into carrier order.
        let k_total = self.k_total;
        for m in 0..m_max {
            let k = 3 * m;
            self.hr[k] = self.ext_r[L + m];
            self.hi[k] = self.ext_i[L + m];
            if k + 1 < k_total {
                self.hr[k + 1] = self.mid[0][0][m];
                self.hi[k + 1] = self.mid[0][1][m];
            }
            if k + 2 < k_total {
                self.hr[k + 2] = self.mid[1][0][m];
                self.hi[k + 2] = self.mid[1][1][m];
            }
        }

        // The noise at each carrier: the grid points' estimates averaged over
        // a few neighbours (a running sum), the nearest one's for the carriers
        // between.
        let span = NOISE_SPAN as isize;
        let mut run: f32 = (0..=NOISE_SPAN.min(m_max - 1)).map(|i| self.nz[i]).sum();
        let mut count = NOISE_SPAN.min(m_max - 1) as isize + 1;
        for m in 0..m_max {
            let avg = (run / count as f32).max(1e-9);
            for j in 0..3 {
                let k = 3 * m + j;
                if k < k_total {
                    self.noise[k] = avg;
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
        for ((((zk, wk), &hr), &hi), ck) in self
            .z
            .iter_mut()
            .zip(&mut self.w)
            .zip(&self.hr)
            .zip(&self.hi)
            .zip(c.iter())
        {
            let p = (hr * hr + hi * hi).max(1e-12);
            let q = 1.0 / p;
            *zk = Iq::new((ck.re * hr + ck.im * hi) * q, (ck.im * hr - ck.re * hi) * q);
            *wk = p;
        }
        if self.diagnostics {
            for (h, (&r, &i)) in self.h.iter_mut().zip(self.hr.iter().zip(&self.hi)) {
                *h = Iq::new(r, i);
            }
        }
        self.symbols += 1;
        self.last_phase = phase;
        self.since_tune += 1;
        if self.since_tune >= TUNE_EVERY {
            self.since_tune = 0;
            self.tune();
        }
    }

    /// Forget what was measured of the channel's changes (a new lock, a
    /// channel that is not the one measured): back to the IIR.
    fn restart_filters(&mut self) {
        self.trk.reset();
        self.est = None;
        self.mobile = false;
        self.since_tune = 0;
    }

    /// The guard interval, which tells the delay-domain filter where an echo
    /// can lie. Call once, after `new`.
    pub fn set_guard(&mut self, guard: Guard) {
        let n = self.fft.n();
        self.denoiser = Denoiser::new(self.grid.len(), n, n / guard.divisor());
    }

    /// Switch the delay-domain filter on or off (a no-op for a guard so long
    /// that it would keep most of the response). It costs two transforms of
    /// the grid a symbol; the receiver turns it on when the signal has little
    /// margin over what its mode needs.
    pub fn set_denoise(&mut self, on: bool) {
        let on = on && self.denoiser.is_some();
        if on != self.denoise_on {
            self.denoise_on = on;
            self.est = None;
            self.since_tune = TUNE_EVERY;
        }
    }

    pub fn denoising(&self) -> bool {
        self.denoise_on
    }

    /// Whether the time filter over the last four pilots is in use: the
    /// measured change of the channel says the IIR would lag it.
    pub fn mobile(&self) -> bool {
        self.mobile
    }

    /// The channel's maximum Doppler, Hz, from the Rician model fitted to the
    /// continual pilots' correlation in time (`ts` the symbol's length, s; 0
    /// for a steady channel); `None` until the first fit, a quarter second in.
    pub fn doppler_hz(&self, ts: f64) -> Option<f32> {
        self.est
            .as_ref()
            .map(|e| e.omega / (std::f32::consts::TAU * ts as f32))
    }

    /// The time filter's fit to the measurement, once there is one.
    pub fn filter_fit(&self) -> Option<&Estimate> {
        self.est.as_ref()
    }

    /// The channel estimate's modelled error, dB against the channel's power,
    /// of the IIR and of the time filter, from the measured correlation.
    pub fn estimate_error_db(&self) -> Option<(f32, f32)> {
        self.est.as_ref().map(|e| {
            (
                10.0 * e.mse_iir.max(1e-9).log10(),
                10.0 * e.mse_wiener.max(1e-9).log10(),
            )
        })
    }

    /// Fit the filters to the measurement: the time filter's weights, and
    /// whether to use it.
    fn tune(&mut self) {
        let scale = match (&self.denoiser, self.denoise_on) {
            (Some(d), true) => d.kept().max(0.05),
            _ => 1.0,
        };
        let Some(est) = self.trk.solve(scale) else {
            return;
        };
        // Stay on until the IIR would do about as well, or its error is small
        // beside the noise anyway.
        let measured = if self.mobile {
            !(est.mse_iir < OFF_RATIO * est.mse_wiener || est.mse_iir < MIN_MSE_OFF)
        } else {
            est.mse_iir >= ON_RATIO * est.mse_wiener && est.mse_iir >= MIN_MSE_ON
        };
        let want = self.forced.unwrap_or(measured);
        self.est = Some(est);
        if want != self.mobile && self.ready() {
            if want {
                // The ring starts as the grid, which the filter gives back.
                self.ring.seed(&self.grid);
            } else {
                // The grid takes up the filter's estimate.
                let g = self.grid.len();
                let (mut r, mut i) = (vec![0.0; g], vec![0.0; g]);
                let w = self.est.as_ref().map(|e| e.weights).unwrap();
                self.ring
                    .eval(self.epoch, self.last_phase, &w, &mut r, &mut i);
                for (m, v) in self.grid.iter_mut().enumerate() {
                    *v = Iq::new(r[m], i[m]);
                }
            }
            self.mobile = want;
        }
    }
}

/// Σ z·s over eight partial sums (which the compiler runs side by side as a
/// vector): the coherent sum of the pilots against a row of the slope table.
#[inline(always)]
fn cdot(zr: &[f32], zi: &[f32], sr: &[f32], si: &[f32]) -> (f32, f32) {
    let (mut ar, mut ai) = ([0.0f32; 8], [0.0f32; 8]);
    let (zr, zi) = (zr.as_chunks::<8>().0, zi.as_chunks::<8>().0);
    let (sr, si) = (sr.as_chunks::<8>().0, si.as_chunks::<8>().0);
    for (((zr, zi), sr), si) in zr.iter().zip(zi).zip(sr).zip(si) {
        for l in 0..8 {
            ar[l] += zr[l] * sr[l] - zi[l] * si[l];
            ai[l] += zr[l] * si[l] + zi[l] * sr[l];
        }
    }
    (ar.iter().sum(), ai.iter().sum())
}

/// out[m] = Σ taps[i]·x[m + i]: eight taps, over every m the output holds.
#[inline(always)]
fn fir(x: &[f32], taps: &[f32; 2 * L], out: &mut [f32]) {
    let n = out.len();
    let x = &x[..n + 2 * L - 1];
    for (m, o) in out.iter_mut().enumerate() {
        let w = &x[m..m + 2 * L];
        let mut acc = 0.0f32;
        for i in 0..2 * L {
            acc += taps[i] * w[i];
        }
        *o = acc;
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
    use decdvbt_ofdm::pilots::{Role, SymbolPlan, pilot_value};

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
        mer_with(mode_text, ch, seconds, 20, |_| {}).0
    }

    /// The same from symbol `from` on, the equaliser set up by `setup` first;
    /// it is handed back as it ended.
    fn mer_with(
        mode_text: &str,
        ch: Channel,
        seconds: f64,
        from: usize,
        setup: impl FnOnce(&mut Equalizer),
    ) -> (f64, Equalizer) {
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
            // A symbol with room before it for the interpolator to read.
            200.0,
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
        eq.set_guard(l.guard);
        setup(&mut eq);
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
            if n >= from {
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
        assert!(n > from + 100, "only {n} symbols");
        (10.0 * (count as f64 / errors).log10(), eq)
    }

    /// A channel that changes under the equaliser (as when the radio is
    /// retuned and the first symbols were the old channel's): its reference
    /// no longer matches, and it must take the new one up rather than
    /// freeze. The new channel here has a different, scrambled phase on every
    /// carrier, so nothing the old reference holds can be matched to it.
    #[test]
    fn a_changed_channel_is_taken_up_not_frozen_out() {
        let fft = FftSize::K2;
        let plan = SymbolPlan::new(fft);
        let k_total = fft.active_carriers();
        let w = prbs(k_total);
        let channel = |k: usize, which: u32| -> Iq {
            if which == 0 {
                Iq::new(1.0, 0.0)
            } else {
                let ph = ((k as u32).wrapping_mul(2_654_435_761) >> 8) as f32 * 1e-3;
                Iq::from_polar(1.0, ph)
            }
        };
        let symbol = |l: usize, which: u32| -> Vec<Iq> {
            (0..k_total)
                .map(|k| {
                    let x = match plan.roles(l)[k] {
                        Role::Pilot => Iq::new(pilot_value(w[k]), 0.0),
                        _ => Iq::new(0.5, -0.5),
                    };
                    x * channel(k, which)
                })
                .collect()
        };
        let mut eq = Equalizer::new(fft);
        for l in 0..40 {
            eq.process(&mut symbol(l, 0), l % 4);
        }
        assert!(eq.coherence > 0.95, "before: {}", eq.coherence);
        // The channel changes: the continual pilots match nothing for 24
        // symbols, then the equaliser starts over and finds them again.
        for l in 40..60 {
            eq.process(&mut symbol(l, 1), l % 4);
        }
        assert!(eq.coherence < MIN_COHERENCE, "{}", eq.coherence);
        for l in 60..100 {
            eq.process(&mut symbol(l, 1), l % 4);
        }
        assert!(eq.coherence > 0.95, "after: {}", eq.coherence);
    }

    /// A steady channel through the time filter: the estimate stays the
    /// channel.
    #[test]
    fn the_time_filter_gives_a_steady_channel_back() {
        let fft = FftSize::K2;
        let plan = SymbolPlan::new(fft);
        let k_total = fft.active_carriers();
        let w = prbs(k_total);
        let ch = |k: usize| Iq::from_polar(1.0 + 0.3 * ((k as f32) * 0.01).sin(), 0.002 * k as f32);
        let symbol = |l: usize| -> Vec<Iq> {
            (0..k_total)
                .map(|k| {
                    let x = match plan.roles(l)[k] {
                        Role::Pilot => Iq::new(pilot_value(w[k]), 0.0),
                        _ => Iq::new(0.5, -0.5),
                    };
                    x * ch(k)
                })
                .collect()
        };
        let mut eq = Equalizer::new(fft);
        eq.forced = Some(true);
        eq.diagnostics = true;
        for l in 0..700 {
            eq.process(&mut symbol(l), l % 4);
        }
        assert!(eq.mobile());
        let worst = (50..k_total - 50)
            .map(|k| (eq.h[k] - ch(k)).norm())
            .fold(0.0f32, f32::max);
        assert!(worst < 0.05, "worst error {worst}");
    }

    /// Two paths, each a steady part and a scattered one 6 dB under it at
    /// 120 Hz of Doppler (a 2k symbol is 0.28 ms: 0.21 rad a symbol, as 35 Hz
    /// is in 8k), at 35 dB: the IIR lags the channel, the time filter does
    /// not, and left to itself the equaliser measures the Doppler and takes
    /// the time filter up.
    #[test]
    fn a_moving_channel_takes_the_time_filter_and_gains_by_it() {
        let ch = || {
            Channel {
                paths: vec![Path::echo(0.0, 0.0), Path::echo(1.5, -3.0)],
                cn_db: Some(35.0),
                ..Default::default()
            }
            .with_rician(6.0, 120.0)
        };
        let from = 600;
        let (iir, _) = mer_with("16qam-3/4-2k-1/32", ch(), 0.4, from, |eq| {
            eq.forced = Some(false)
        });
        let (time, _) = mer_with("16qam-3/4-2k-1/32", ch(), 0.4, from, |eq| {
            eq.forced = Some(true)
        });
        let (_, auto) = mer_with("16qam-3/4-2k-1/32", ch(), 0.4, from, |_| {});
        assert!(
            time > iir + 1.5,
            "IIR {iir:.1} dB, time filter {time:.1} dB"
        );
        assert!(
            auto.mobile(),
            "the measurement did not take the time filter up"
        );
        // 2k with a 1/32 guard: the symbol is 2112 samples at 64/7 MHz.
        let fd = auto.doppler_hz(2112.0 / (64e6 / 7.0)).unwrap();
        assert!((80.0..160.0).contains(&fd), "Doppler {fd:.0} Hz");
    }

    /// A still channel: the IIR stays, and the Doppler reads nothing.
    #[test]
    fn a_still_channel_keeps_the_iir() {
        let ch = Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(1.5, -3.0)],
            cn_db: Some(30.0),
            ..Default::default()
        };
        let (_, eq) = mer_with("16qam-3/4-2k-1/32", ch, 0.3, 20, |_| {});
        assert!(!eq.mobile());
        let fd = eq.doppler_hz(2112.0 / (64e6 / 7.0)).unwrap();
        assert!(fd < 5.0, "Doppler {fd:.0} Hz");
    }

    /// The delay-domain filter keeps an echo near the end of the guard (which
    /// pins down its sign convention: a first version kept the wrong end, and
    /// a short channel did not show it), and takes noise out of the estimate
    /// of a short channel.
    #[test]
    fn the_delay_filter_keeps_echoes_and_takes_noise() {
        let long = || Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(24.0, -6.0)],
            cn_db: Some(30.0),
            ..Default::default()
        };
        let (off, _) = mer_with("16qam-3/4-8k-1/32", long(), 0.15, 20, |_| {});
        let (on, _) = mer_with("16qam-3/4-8k-1/32", long(), 0.15, 20, |eq| {
            eq.set_denoise(true)
        });
        assert!(
            on > off - 0.3,
            "a 24 us echo: off {off:.1} dB, on {on:.1} dB"
        );
        let short = || Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(1.0, -6.0)],
            cn_db: Some(20.0),
            ..Default::default()
        };
        let (off, _) = mer_with("16qam-3/4-8k-1/32", short(), 0.15, 20, |_| {});
        let (on, _) = mer_with("16qam-3/4-8k-1/32", short(), 0.15, 20, |eq| {
            eq.set_denoise(true)
        });
        assert!(on > off + 0.3, "20 dB: off {off:.1} dB, on {on:.1} dB");
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
