// made by CasualArclamp
//! Channel estimation for a moving receiver: three pieces the equaliser
//! (`chest.rs`) switches on when the signal calls for them, so that a static
//! signal with room to spare costs nothing extra.
//!
//! **[`Tracker`]: how fast the channel changes, measured.** The continual
//! pilots stand at the same carriers in every symbol, so each is a sample of
//! the channel once a symbol (±540 Hz of Doppler in 8k). Their autocorrelation
//! in time, over lags of 0 to 15 symbols, averaged over the 177 pilots and a
//! second or so, is the channel's own: thermal noise adds to lag 0 only. From
//! it come the rms Doppler (a number to show), the noise share of a pilot
//! (lag 0 against the lag-0 value the other lags extrapolate to), and, by the
//! Wiener-Hopf equations, the best linear filter over the last four pilots of a
//! carrier. Nothing is assumed of the vehicle's speed, the Doppler spectrum's
//! shape or the strength of a line-of-sight component: the weights follow the
//! measured correlation (the Rician channel's steady part is simply a
//! correlation that does not fall).
//!
//! **[`Ring`]: the time filter.** A carrier's scattered pilot comes every
//! fourth symbol, so the estimate for the others is a prediction from pilots
//! 0 to 3 symbols old. The equaliser's usual estimate is a first-order IIR on
//! the pilots (α = 1/2): right for a static channel, but it lags a moving one
//! (a vehicle at 35 Hz Doppler in a Rician channel with K = 6 dB: −12 dB of
//! estimate error, against −18 for the Wiener filter, both from the
//! correlation function). The ring keeps each grid point's last four pilots
//! and the filter weights its four values by the pilot's age.
//!
//! **[`Denoiser`]: the delay-domain filter.** The grid of channel estimates, a
//! point every third carrier, is a transfer function sampled every 3.35 kHz in
//! 8k: its impulse response is unambiguous over ~300 µs, but a real channel's
//! lies within the guard interval (28 µs at 1/32), a tenth of that. Zeroing the
//! rest of the impulse response before interpolating removes the same share of
//! the estimate's noise (up to 9 dB), which is worth its cost (two transforms
//! of the grid a symbol) only when the signal is near what the mode needs.
//!
//! The Wiener filter and the delay-domain filter are Wiener's and the usual
//! DFT-based channel estimator (see e.g. Hsieh and Wei, "Channel estimation
//! for OFDM systems based on comb-type pilot arrangement in frequency
//! selective fading channels", IEEE Trans. Consum. Electron. 44(1), 1998);
//! the use of the continual pilots to measure the correlation is this
//! receiver's.

use std::sync::Arc;

use decdvbt_core::Iq;
use rustfft::{Fft, FftPlanner};

/// Lags measured, in symbols. The time filter reaches four pilots back
/// (12 symbols) and predicts up to 3 symbols ahead: lags to 15.
pub const LAGS: usize = 16;
/// Pilots (of one carrier) in the time filter.
pub const TAPS: usize = 4;
/// The correlation is taken on every this many symbols, and each such
/// measurement joins the average with this weight.
const EVERY: u64 = 4;
const RATE: f64 = 1.0 / 96.0;
/// Measurements in the average before it is trusted (a quarter second).
const PRIMED: u32 = 24;
/// The IIR the time filter is judged against (the equaliser's, α = 1/2).
const IIR_ALPHA: f64 = 0.5;
/// The least scattered power (of the pilots') that the fit believes in.
const MIN_SCATTER: f64 = 0.012;

/// What the correlation says, normalised to the pilots' lag-0 power.
#[derive(Debug, Clone, Copy)]
pub struct Estimate {
    /// The time filter's weights by the age (0 to 3 symbols) of the newest
    /// pilot, newest first.
    pub weights: [[f32; TAPS]; 4],
    /// Predicts a pilot from the three before it (4, 8 and 12 symbols back).
    pub predict: [f32; 3],
    /// The mean (over the age) squared error of the Wiener filter and of the
    /// IIR, against the lag-0 power; the error of the channel estimate, noise
    /// included (as scaled by `noise_scale`).
    pub mse_wiener: f32,
    pub mse_iir: f32,
    /// The share of a pilot's power that is noise.
    pub noise_frac: f32,
    /// The share of the signal's power that is a steady component, and the
    /// scattered part's Doppler in radians per symbol, from the model fitted
    /// to the correlation.
    pub steady: f32,
    pub omega: f32,
}

pub struct Tracker {
    /// The continual pilots (padded to a multiple of 8 with zeros) of the last
    /// LAGS symbols, as real and imaginary arrays.
    np: usize,
    /// Pilots left out (those by the radio's DC: see `chest.rs`) read as 0.
    used: Vec<bool>,
    hr: Vec<f32>,
    hi: Vec<f32>,
    n: u64,
    /// The smoothed Re Σ h(n)·conj(h(n−l)).
    acc: [f64; LAGS],
    updates: u32,
}

impl Tracker {
    /// For pilots of which `used` says which to measure on.
    pub fn new(used: Vec<bool>) -> Tracker {
        let np = used.len().next_multiple_of(8);
        Tracker {
            np,
            used,
            hr: vec![0.0; LAGS * np],
            hi: vec![0.0; LAGS * np],
            n: 0,
            acc: [0.0; LAGS],
            updates: 0,
        }
    }

    pub fn reset(&mut self) {
        self.hr.fill(0.0);
        self.hi.fill(0.0);
        self.n = 0;
        self.acc = [0.0; LAGS];
        self.updates = 0;
    }

    /// Whether the average has settled.
    pub fn primed(&self) -> bool {
        self.updates >= PRIMED
    }

    /// Take the continual pilots of a symbol (as channel values, the
    /// symbol's common phase and slope already out).
    #[inline(always)]
    pub fn push(&mut self, h: &[Iq]) {
        let slot = (self.n % LAGS as u64) as usize * self.np;
        for (j, (v, &u)) in h.iter().zip(&self.used).enumerate() {
            let v = if u { *v } else { Iq::new(0.0, 0.0) };
            self.hr[slot + j] = v.re;
            self.hi[slot + j] = v.im;
        }
        self.n += 1;
        if self.n.is_multiple_of(EVERY) && self.n >= LAGS as u64 {
            self.measure();
        }
    }

    #[inline(always)]
    fn measure(&mut self) {
        let np = self.np;
        let cur = ((self.n - 1) % LAGS as u64) as usize * np;
        let (cr, ci) = (&self.hr[cur..cur + np], &self.hi[cur..cur + np]);
        let first = self.updates == 0;
        for l in 0..LAGS {
            let old = ((self.n - 1 + LAGS as u64 - l as u64) % LAGS as u64) as usize * np;
            let (or, oi) = (&self.hr[old..old + np], &self.hi[old..old + np]);
            // Eight partial sums, which the compiler runs side by side.
            let mut s = [0.0f32; 8];
            for (((a, b), c), d) in cr
                .as_chunks::<8>()
                .0
                .iter()
                .zip(or.as_chunks::<8>().0)
                .zip(ci.as_chunks::<8>().0)
                .zip(oi.as_chunks::<8>().0)
            {
                for t in 0..8 {
                    s[t] += a[t] * b[t] + c[t] * d[t];
                }
            }
            let v = f64::from(s.iter().sum::<f32>());
            self.acc[l] = if first {
                v
            } else {
                self.acc[l] + RATE * (v - self.acc[l])
            };
        }
        self.updates += 1;
    }

    /// The Wiener-Hopf solution for the correlation measured so far, with the
    /// pilots' noise counted at `noise_scale` of its power (1 as it is; less
    /// when a later stage removes some of it, which favours a quicker filter).
    ///
    /// The correlation is first fitted by a model, that of a Rician channel
    /// with Clarke's Doppler spectrum: r(l) = a + b·J₀(ω·l), a steady part a,
    /// a scattered part b whose maximum Doppler turns ω radians a symbol. The
    /// measurement has errors of a few tenths of a percent, which are the
    /// whole of what separates a good filter from a bad one when the channel
    /// is slow; a fit to a valid correlation function cannot go astray, and
    /// gives the signal's power with the noise out (a + b, the model at lag
    /// 0, where the measurement has the noise in).
    pub fn solve(&self, noise_scale: f32) -> Option<Estimate> {
        if !self.primed() || self.acc[0] <= 0.0 {
            return None;
        }
        let r: Vec<f64> = self.acc.iter().map(|a| a / self.acc[0]).collect();
        let (mut a, mut b, mut omega) = fit_rician(&r);
        // A scattered part under 1 % of the power is not told from the
        // measurement's errors: the channel is steady.
        if b < MIN_SCATTER {
            (a, b, omega) = (a + b, 0.0, 0.0);
        }
        // The noise is never taken for under 0.2 % of a pilot's power (which
        // keeps the equations well-conditioned however still the channel), and
        // the model is scaled to the power that leaves.
        let s = (a + b).clamp(0.05, 0.998);
        let k = s / (a + b).max(1e-9);
        (a, b) = (a * k, b * k);
        let nf = 1.0 - s;
        let diag = s + nf * f64::from(noise_scale);
        let model = |l: usize| a + b * j0(omega * l as f64);
        let lag = |l: usize| if l == 0 { s } else { model(l) };
        let mut weights = [[0.0f32; TAPS]; 4];
        let (mut mse_w, mut mse_i) = (0.0, 0.0);
        // The IIR's weights over the same four pilots.
        let mut iir = [0.0f64; TAPS];
        let mut tot = 0.0;
        for (j, v) in iir.iter_mut().enumerate() {
            *v = IIR_ALPHA * (1.0 - IIR_ALPHA).powi(j as i32);
            tot += *v;
        }
        for v in &mut iir {
            *v /= tot;
        }
        let rm = |n: usize| -> Vec<Vec<f64>> {
            (0..n)
                .map(|i| {
                    (0..n)
                        .map(|j| {
                            if i == j {
                                diag
                            } else {
                                model(4 * i.abs_diff(j))
                            }
                        })
                        .collect()
                })
                .collect()
        };
        let full = rm(TAPS);
        for (age, row) in weights.iter_mut().enumerate() {
            let p: Vec<f64> = (0..TAPS).map(|j| lag(age + 4 * j)).collect();
            let w = solve(&full, &p);
            for (o, v) in row.iter_mut().zip(&w) {
                *o = *v as f32;
            }
            // Wiener's error is s − w·p (the quadratic form equals w·p there).
            mse_w += s - dot(&w, &p);
            mse_i += s - dot(&iir, &p) * 2.0 + quad(&full, &iir);
        }
        let pm = rm(3);
        let pp: Vec<f64> = (0..3).map(|j| model(4 * (j + 1))).collect();
        let pw = solve(&pm, &pp);
        Some(Estimate {
            weights,
            predict: [pw[0] as f32, pw[1] as f32, pw[2] as f32],
            mse_wiener: (mse_w / 4.0) as f32,
            mse_iir: (mse_i / 4.0) as f32,
            noise_frac: nf as f32,
            steady: (a / s) as f32,
            omega: omega as f32,
        })
    }
}

/// J₀(x) (Abramowitz and Stegun 9.4.1 and 9.4.3: good to 1e-7).
fn j0(x: f64) -> f64 {
    let ax = x.abs();
    if ax <= 3.0 {
        let t = (ax / 3.0).powi(2);
        1.0 + t
            * (-2.2499997
                + t * (1.2656208
                    + t * (-0.3163866 + t * (0.0444479 + t * (-0.0039444 + t * 0.0002100)))))
    } else {
        let t = 3.0 / ax;
        let f0 = 0.79788456
            + t * (-0.00000077
                + t * (-0.00552740
                    + t * (-0.00009512 + t * (0.00137237 + t * (-0.00072805 + t * 0.00014476)))));
        let th = ax - std::f64::consts::FRAC_PI_4
            + t * (-0.04166397
                + t * (-0.00003954
                    + t * (0.00262573 + t * (-0.00054125 + t * (-0.00029333 + t * 0.00013558)))));
        f0 * th.cos() / ax.sqrt()
    }
}

/// Fit r(l) ≈ a + b·J₀(ω·l) for l = 1…LAGS−1 by least squares over a grid of
/// ω (rad per symbol), a and b of each ω solved directly and kept in
/// 0 ≤ a, b with a + b ≤ 1. Returns (a, b, ω).
fn fit_rician(r: &[f64]) -> (f64, f64, f64) {
    let n = LAGS - 1;
    let mut best = (f64::MAX, 0.0, 0.0, 0.0);
    // ω = 0 (a steady channel) and 60 steps from 0.004 to 1.3 rad a symbol.
    for i in 0..=60 {
        let omega = if i == 0 {
            0.0
        } else {
            0.004 * (1.3f64 / 0.004).powf((i - 1) as f64 / 59.0)
        };
        let u: Vec<f64> = (1..=n).map(|l| j0(omega * l as f64)).collect();
        // r = a + b·u; the normal equations.
        let (su, suu): (f64, f64) = (u.iter().sum(), u.iter().map(|v| v * v).sum());
        let sr: f64 = r[1..=n].iter().sum();
        let sru: f64 = r[1..=n].iter().zip(&u).map(|(x, y)| x * y).sum();
        let nn = n as f64;
        let det = nn * suu - su * su;
        let b = if det.abs() < 1e-9 {
            0.0
        } else {
            ((nn * sru - su * sr) / det).clamp(0.0, 1.0)
        };
        // The steady part for that b (re-solved, as b may have been clamped).
        let a = ((sr - b * su) / nn).clamp(0.0, 1.0 - b);
        let sse: f64 = r[1..=n]
            .iter()
            .zip(&u)
            .map(|(x, y)| (x - a - b * y).powi(2))
            .sum();
        if sse < best.0 - 1e-12 {
            best = (sse, a, b, omega);
        }
    }
    (best.1, best.2, best.3)
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn quad(m: &[Vec<f64>], w: &[f64]) -> f64 {
    let mut s = 0.0;
    for (i, row) in m.iter().enumerate() {
        for (j, v) in row.iter().enumerate() {
            s += w[i] * v * w[j];
        }
    }
    s
}

/// Solve the small symmetric system `m·w = p` by Gaussian elimination (with a
/// hair added to the diagonal against a singular correlation).
#[allow(clippy::needless_range_loop)] // the elimination reads by row and column
fn solve(m: &[Vec<f64>], p: &[f64]) -> Vec<f64> {
    let n = p.len();
    let mut a: Vec<Vec<f64>> = (0..n)
        .map(|i| {
            let mut row = m[i].clone();
            row[i] += 1e-6;
            row.push(p[i]);
            row
        })
        .collect();
    for c in 0..n {
        let piv = (c..n)
            .max_by(|&x, &y| a[x][c].abs().total_cmp(&a[y][c].abs()))
            .unwrap();
        a.swap(c, piv);
        let d = a[c][c];
        for j in c..=n {
            a[c][j] /= d;
        }
        for i in 0..n {
            if i != c {
                let f = a[i][c];
                for j in c..=n {
                    let t = a[c][j];
                    a[i][j] -= f * t;
                }
            }
        }
    }
    (0..n).map(|i| a[i][n]).collect()
}

/// The grid points' last four pilots, in four slots by epoch (a group of four
/// symbols), for the time filter.
pub struct Ring {
    g: usize,
    re: [Vec<f32>; 4],
    im: [Vec<f32>; 4],
}

impl Ring {
    pub fn new(g: usize) -> Ring {
        let gp = g.next_multiple_of(4);
        Ring {
            g,
            re: std::array::from_fn(|_| vec![0.0; gp]),
            im: std::array::from_fn(|_| vec![0.0; gp]),
        }
    }

    /// Every slot holds the grid as it is (the filter then gives it back).
    pub fn seed(&mut self, grid: &[Iq]) {
        for s in 0..4 {
            for (m, v) in grid.iter().enumerate() {
                self.re[s][m] = v.re;
                self.im[s][m] = v.im;
            }
        }
    }

    /// The grid points with a pilot in the symbol of this `phase` (m ≡ phase
    /// mod 4, carrier 3m): `v(m)` gives the pilot as a channel value. Each is
    /// first compared with what the three before it predict (`predict`),
    /// the squared difference over `innovation` going into `nz` at `rate`,
    /// then written in the epoch's slot.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        epoch: usize,
        phase: usize,
        k_total: usize,
        predict: Option<&[f32; 3]>,
        nz: &mut [f32],
        rate: f32,
        innovation: f32,
        v: impl Fn(usize) -> Iq,
    ) {
        let slot = epoch & 3;
        let olds = [
            epoch.wrapping_sub(1) & 3,
            epoch.wrapping_sub(2) & 3,
            epoch.wrapping_sub(3) & 3,
        ];
        for m in (phase % 4..self.g).step_by(4) {
            let k = 3 * m;
            if k >= k_total {
                continue;
            }
            let x = v(m);
            if let Some(w) = predict {
                let (mut pr, mut pi) = (0.0f32, 0.0f32);
                for (t, &s) in olds.iter().enumerate() {
                    pr += w[t] * self.re[s][m];
                    pi += w[t] * self.im[s][m];
                }
                let e = ((x.re - pr).powi(2) + (x.im - pi).powi(2)) / innovation;
                nz[m] += rate * (e - nz[m]);
            }
            self.re[slot][m] = x.re;
            self.im[slot][m] = x.im;
        }
    }

    /// The time filter's estimate at every grid point for the symbol of this
    /// `epoch` and `phase`, written to `out_re` and `out_im` (the grid point
    /// m at index m).
    #[inline(always)]
    #[allow(clippy::needless_range_loop)] // slot by class, as the note says
    pub fn eval(
        &self,
        epoch: usize,
        phase: usize,
        w: &[[f32; TAPS]; 4],
        out_re: &mut [f32],
        out_im: &mut [f32],
    ) {
        // wt[slot][class]: class q = m mod 4 had its newest pilot (phase q)
        // in this epoch if q <= phase, else in the one before; its age is the
        // symbols since.
        let mut wt = [[0.0f32; 4]; 4];
        for q in 0..4 {
            let age = (phase + 4 - q) % 4;
            let newest = if q <= phase {
                epoch
            } else {
                epoch.wrapping_sub(1)
            };
            for j in 0..TAPS {
                wt[newest.wrapping_sub(j) & 3][q] = w[age][j];
            }
        }
        let n4 = self.g / 4;
        for i in 0..n4 {
            for q in 0..4 {
                let m = 4 * i + q;
                let (mut r, mut im) = (0.0f32, 0.0f32);
                for s in 0..4 {
                    r += wt[s][q] * self.re[s][m];
                    im += wt[s][q] * self.im[s][m];
                }
                out_re[m] = r;
                out_im[m] = im;
            }
        }
        for m in 4 * n4..self.g {
            let q = m % 4;
            let (mut r, mut im) = (0.0f32, 0.0f32);
            for s in 0..4 {
                r += wt[s][q] * self.re[s][m];
                im += wt[s][q] * self.im[s][m];
            }
            out_re[m] = r;
            out_im[m] = im;
        }
    }
}

/// Keeps only the part of the grid's impulse response that a channel inside
/// the guard interval can have.
pub struct Denoiser {
    g: usize,
    nfft: usize,
    fwd: Arc<dyn Fft<f32>>,
    inv: Arc<dyn Fft<f32>>,
    buf: Vec<Iq>,
    scratch: Vec<Iq>,
    /// Impulse-response taps kept either side of zero. The grid runs up in
    /// carrier index and a delay τ turns carrier k by e^(−j2πkτ/N), so the
    /// forward transform puts a late echo at the negative end (wrapping round
    /// from nfft − 1 down) and an early one at the positive end.
    late: usize,
    early: usize,
}

impl Denoiser {
    /// For a grid of `g` points whose OFDM symbol has `n` useful samples, a
    /// guard of `guard` samples. `None` where it would keep most of the
    /// response anyway (a long guard).
    pub fn new(g: usize, n: usize, guard: usize) -> Option<Denoiser> {
        let nfft = (g + g / 2).next_power_of_two();
        // The grid's spacing is three carriers: its response repeats every
        // n/3 samples, which is nfft bins.
        let per_sample = nfft as f64 / (n as f64 / 3.0);
        // Late echoes up to the guard (an eighth more for the transform's
        // smearing); early ones, those before the strongest path, up to a
        // quarter of the guard and a few samples (the reader starts the
        // window that much early).
        let late = (per_sample * (guard as f64 * 1.125 + 8.0)).ceil() as usize;
        let early = (per_sample * (guard as f64 * 0.25 + 16.0)).ceil() as usize;
        if late + early > nfft * 2 / 5 {
            return None;
        }
        let mut p = FftPlanner::<f32>::new();
        let fwd = p.plan_fft_forward(nfft);
        let inv = p.plan_fft_inverse(nfft);
        let scratch = vec![
            Iq::new(0.0, 0.0);
            fwd.get_inplace_scratch_len()
                .max(inv.get_inplace_scratch_len())
        ];
        Some(Denoiser {
            g,
            nfft,
            fwd,
            inv,
            buf: vec![Iq::new(0.0, 0.0); nfft],
            scratch,
            late,
            early,
        })
    }

    /// Filter the grid in `re`/`im` (g points each) in place.
    pub fn run(&mut self, re: &mut [f32], im: &mut [f32]) {
        let (g, nfft) = (self.g, self.nfft);
        for (b, (&r, &i)) in self.buf.iter_mut().zip(re.iter().zip(im.iter())) {
            *b = Iq::new(r, i);
        }
        // The rest of the period: a straight line from the grid's last point
        // back to its first, so that the periodic extension has no jump (a
        // jump would spread over every delay).
        let (a, b) = (self.buf[g - 1], self.buf[0]);
        let tail = nfft - g;
        for j in 0..tail {
            let f = (j + 1) as f32 / (tail + 1) as f32;
            self.buf[g + j] = a * (1.0 - f) + b * f;
        }
        self.fwd
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        // Keep 0 ..= early and nfft − late .. nfft.
        self.buf[self.early + 1..nfft - self.late].fill(Iq::new(0.0, 0.0));
        self.inv
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        let sc = 1.0 / nfft as f32;
        for (b, (r, i)) in self.buf.iter().zip(re.iter_mut().zip(im.iter_mut())) {
            *r = b.re * sc;
            *i = b.im * sc;
        }
    }

    /// The share of the response kept: the noise left, as a fraction.
    pub fn kept(&self) -> f32 {
        (self.late + self.early + 1) as f32 / self.nfft as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid point whose pilot value is the symbol's number: a linear
    /// extrapolation from the two newest pilots must give the current symbol's
    /// number whatever the pilot's age, for every class of grid point.
    #[test]
    fn the_ring_ages_its_pilots_right() {
        let g = 40;
        let mut ring = Ring::new(g);
        let mut w = [[0.0f32; TAPS]; 4];
        for (a, row) in w.iter_mut().enumerate() {
            row[0] = 1.0 + a as f32 / 4.0;
            row[1] = -(a as f32) / 4.0;
        }
        let mut nz = vec![0.0f32; g];
        let (mut re, mut im) = (vec![0.0f32; g], vec![0.0f32; g]);
        let mut epoch = 0usize;
        for t in 0..100usize {
            let phase = t % 4;
            if phase == 0 {
                epoch += 1;
            }
            ring.update(epoch, phase, 3 * g, None, &mut nz, 0.1, 1.0, |m| {
                Iq::new(t as f32, -(t as f32) + m as f32 * 0.0)
            });
            ring.eval(epoch, phase, &w, &mut re, &mut im);
            if t >= 20 {
                for m in 0..g {
                    assert!(
                        (re[m] - t as f32).abs() < 1e-3 && (im[m] + t as f32).abs() < 1e-3,
                        "t {t} phase {phase} m {m}: {} {}",
                        re[m],
                        im[m]
                    );
                }
            }
        }
    }
}
