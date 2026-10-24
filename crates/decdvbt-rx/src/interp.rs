// made by CasualArclamp
//! Interpolation at any position between samples, by a Kaiser-windowed sinc
//! from a table of 512 fractional phases.
//!
//! The front end reads a radio's samples (10 MS/s, say) at the OFDM signal's
//! own rate (64/7 MS/s) and at exact fractional positions, so the symbol
//! timing follows the signal to a fraction of a sample with no integer jumps
//! to smear the channel estimate, and only the samples of the symbols looked
//! at are ever converted.

use decdvbt_core::Iq;
use decdvbt_dsp::resample::bessel_i0;

/// Taps of the interpolator. The signal fills 0.38 of the radio's rate (7.61
/// MHz of 20 MS/s' worth... of a 10 MS/s one), so the kernel must be flat out
/// to there: 32 taps with ~50 dB of rejection are.
pub const TAPS: usize = 16;
/// Samples read before and after the one at floor(pos).
pub const BEFORE: usize = TAPS / 2 - 1;
pub const AFTER: usize = TAPS / 2;
const PHASES: usize = 512;
const BETA: f64 = 6.0;

pub struct Interpolator {
    table: Vec<[f32; TAPS]>,
    /// The CPU has AVX2 and FMA, found once: the dot products then run eight
    /// samples at a time (the same choice DecSAT's LDPC kernel makes). The
    /// plain loop is the fallback, and what the tests check the fast one
    /// against.
    avx2: bool,
}

/// The same dot products with AVX2 and FMA: `re` and `im` against the taps,
/// the 32 products of each in four vector fused multiply-adds.
///
/// # Safety
/// The CPU must have AVX2 and FMA (see `Interpolator::avx2`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot2_avx2(c: &[f32; TAPS], re: &[f32; TAPS], im: &[f32; TAPS]) -> (f32, f32) {
    use std::arch::x86_64::*;
    // SAFETY: the arrays are TAPS = 32 long, four loads of eight each, and
    // the caller has checked the CPU features.
    unsafe {
        let (mut a, mut b) = (_mm256_setzero_ps(), _mm256_setzero_ps());
        for k in 0..TAPS / 8 {
            let ck = _mm256_loadu_ps(c.as_ptr().add(8 * k));
            a = _mm256_fmadd_ps(ck, _mm256_loadu_ps(re.as_ptr().add(8 * k)), a);
            b = _mm256_fmadd_ps(ck, _mm256_loadu_ps(im.as_ptr().add(8 * k)), b);
        }
        // Each sum's two halves added, then both sums reduced together: two
        // horizontal adds for the pair, not two each.
        let a4 = _mm_add_ps(_mm256_castps256_ps128(a), _mm256_extractf128_ps(a, 1));
        let b4 = _mm_add_ps(_mm256_castps256_ps128(b), _mm256_extractf128_ps(b, 1));
        let ab = _mm_hadd_ps(a4, b4); // [a0+a1, a2+a3, b0+b1, b2+b3]
        let ab = _mm_hadd_ps(ab, ab); // [a, b, a, b]
        (_mm_cvtss_f32(ab), _mm_cvtss_f32(_mm_shuffle_ps(ab, ab, 1)))
    }
}

/// `resample_planar`'s loop with AVX2 and FMA. Four outputs at a time: each
/// one's taps against its 16 samples in two fused multiply-adds per part, and
/// then the eight sums reduced together by a tree of horizontal adds that
/// leaves them as the four outputs' (re, im) pairs, in the order they are
/// stored. Reducing each output alone cost more than its multiplies.
///
/// # Safety
/// The CPU must have AVX2 and FMA.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn resample_avx2(
    it: &Interpolator,
    x: &Planar,
    start: f64,
    step: f64,
    n: usize,
    out: &mut Vec<Iq>,
) {
    use std::arch::x86_64::*;
    out.reserve(n);
    let quads = n / 4;
    // SAFETY: the CPU has AVX2 and FMA (the caller's promise); each read is
    // `TAPS` samples from `first`, checked against the arrays' length; the
    // stores go to the `n` reserved places, which are then made the vector's.
    unsafe {
        let dst = out.as_mut_ptr().add(out.len()) as *mut f32;
        for qd in 0..quads {
            let mut sums = [_mm256_setzero_ps(); 8];
            for k in 0..4 {
                let (first, ph) = it.locate(start + (4 * qd + k) as f64 * step);
                assert!(first + TAPS <= x.re.len() && first + TAPS <= x.im.len());
                let c = it.table[ph].as_ptr();
                let (re, im) = (x.re.as_ptr().add(first), x.im.as_ptr().add(first));
                let (c0, c1) = (_mm256_loadu_ps(c), _mm256_loadu_ps(c.add(8)));
                let a = _mm256_mul_ps(c0, _mm256_loadu_ps(re));
                sums[2 * k] = _mm256_fmadd_ps(c1, _mm256_loadu_ps(re.add(8)), a);
                let b = _mm256_mul_ps(c0, _mm256_loadu_ps(im));
                sums[2 * k + 1] = _mm256_fmadd_ps(c1, _mm256_loadu_ps(im.add(8)), b);
            }
            // [re0, im0, re1, im1 | the same of the upper halves], and so for
            // outputs 2 and 3; the halves added.
            let h0 = _mm256_hadd_ps(sums[0], sums[1]);
            let h1 = _mm256_hadd_ps(sums[2], sums[3]);
            let h2 = _mm256_hadd_ps(sums[4], sums[5]);
            let h3 = _mm256_hadd_ps(sums[6], sums[7]);
            let g0 = _mm256_hadd_ps(h0, h1);
            let g1 = _mm256_hadd_ps(h2, h3);
            let lo = _mm_add_ps(_mm256_castps256_ps128(g0), _mm256_extractf128_ps(g0, 1));
            let hi = _mm_add_ps(_mm256_castps256_ps128(g1), _mm256_extractf128_ps(g1, 1));
            _mm256_storeu_ps(dst.add(8 * qd), _mm256_set_m128(hi, lo));
        }
        out.set_len(out.len() + 4 * quads);
    }
    for j in 4 * quads..n {
        let (first, ph) = it.locate(start + j as f64 * step);
        let taps = &it.table[ph];
        let re: &[f32; TAPS] = x.re[first..first + TAPS].try_into().unwrap();
        let im: &[f32; TAPS] = x.im[first..first + TAPS].try_into().unwrap();
        // SAFETY: this function itself requires AVX2 and FMA.
        let (r, i) = unsafe { dot2_avx2(taps, re, im) };
        out.push(Iq::new(r, i));
    }
}

/// Samples as two arrays, real and imaginary parts apart: what the dot
/// product wants. (A complex sample's parts interleaved make the compiler
/// fall back to one dependent addition after another, ~4× slower.)
#[derive(Default)]
pub struct Planar {
    pub re: Vec<f32>,
    pub im: Vec<f32>,
}

impl Planar {
    pub fn set(&mut self, x: &[Iq]) {
        self.re.clear();
        self.im.clear();
        self.re.extend(x.iter().map(|c| c.re));
        self.im.extend(x.iter().map(|c| c.im));
    }

    pub fn len(&self) -> usize {
        self.re.len()
    }

    pub fn is_empty(&self) -> bool {
        self.re.is_empty()
    }
}

/// The dot product of the taps with 32 samples, in 8 independent partial
/// sums (which vectorise) added at the end.
#[inline(always)]
fn dot(c: &[f32; TAPS], x: &[f32; TAPS]) -> f32 {
    let mut acc = [0f32; 8];
    for k in 0..TAPS / 8 {
        for l in 0..8 {
            acc[l] += c[k * 8 + l] * x[k * 8 + l];
        }
    }
    ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]))
}

fn kaiser_sinc(t: f64, half: f64) -> f64 {
    let r = t / half;
    if r.abs() >= 1.0 {
        return 0.0;
    }
    let sinc = if t == 0.0 {
        1.0
    } else {
        (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t)
    };
    sinc * bessel_i0(BETA * (1.0 - r * r).sqrt()) / bessel_i0(BETA)
}

impl Default for Interpolator {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpolator {
    pub fn new() -> Interpolator {
        let table = (0..PHASES)
            .map(|p| {
                let f = p as f64 / PHASES as f64;
                // The tap for sample floor(pos) − BEFORE + q.
                let mut t = [0f64; TAPS];
                for (q, c) in t.iter_mut().enumerate() {
                    *c = kaiser_sinc(f + BEFORE as f64 - q as f64, TAPS as f64 / 2.0);
                }
                let sum: f64 = t.iter().sum();
                std::array::from_fn(|q| (t[q] / sum) as f32)
            })
            .collect();
        #[cfg(target_arch = "x86_64")]
        let avx2 = is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma");
        #[cfg(not(target_arch = "x86_64"))]
        let avx2 = false;
        Interpolator { table, avx2 }
    }

    /// Use the plain loop even where AVX2 is there (for the tests and the
    /// benchmark).
    pub fn without_avx2(mut self) -> Interpolator {
        self.avx2 = false;
        self
    }

    /// The first sample read for `pos` and the table phase to use. `pos` is
    /// positive, so truncation is floor and adding a half before it is
    /// round: no `floor`/`round` calls, which without SSE4.1 are libm calls
    /// costing more than the dot product they serve.
    #[inline(always)]
    fn locate(&self, pos: f64) -> (usize, usize) {
        let i = pos as usize;
        let ph = (((pos - i as f64) * PHASES as f64 + 0.5) as usize).min(PHASES - 1);
        (i - BEFORE, ph)
    }

    /// x(pos): `pos` in samples of `x`. The caller keeps `BEFORE` samples
    /// before floor(pos) and `AFTER` after it inside `x`.
    #[inline]
    pub fn at(&self, x: &[Iq], pos: f64) -> Iq {
        let (first, ph) = self.locate(pos);
        let taps = &self.table[ph];
        let window = &x[first..first + TAPS];
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (c, s) in taps.iter().zip(window) {
            re += c * s.re;
            im += c * s.im;
        }
        Iq::new(re, im)
    }

    /// x(pos) from planar samples (see [`Planar`]).
    #[inline]
    pub fn at_planar(&self, x: &Planar, pos: f64) -> Iq {
        let (first, ph) = self.locate(pos);
        let taps = &self.table[ph];
        let re: &[f32; TAPS] = x.re[first..first + TAPS].try_into().unwrap();
        let im: &[f32; TAPS] = x.im[first..first + TAPS].try_into().unwrap();
        Iq::new(dot(taps, re), dot(taps, im))
    }

    /// `n` samples from `start`, `step` apart, appended to `out`, from
    /// planar samples.
    pub fn resample_planar(&self, x: &Planar, start: f64, step: f64, n: usize, out: &mut Vec<Iq>) {
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: `avx2` is only set when the CPU has AVX2 and FMA.
            unsafe { resample_avx2(self, x, start, step, n, out) };
            return;
        }
        out.reserve(n);
        for j in 0..n {
            out.push(self.at_planar(x, start + j as f64 * step));
        }
    }

    /// `n` samples from `start`, `step` apart, appended to `out`.
    pub fn resample(&self, x: &[Iq], start: f64, step: f64, n: usize, out: &mut Vec<Iq>) {
        out.reserve(n);
        for j in 0..n {
            out.push(self.at(x, start + j as f64 * step));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tone(f: f64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let p = TAU * f * k as f64;
                Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn the_planar_and_the_interleaved_readings_agree() {
        let it = Interpolator::new();
        let x = tone(0.31, 300);
        let mut p = Planar::default();
        p.set(&x);
        for i in 20..280 {
            let pos = i as f64 + 0.37;
            assert!(
                (it.at(&x, pos) - it.at_planar(&p, pos)).norm() < 1e-5,
                "{i}"
            );
        }
    }

    #[test]
    fn the_avx2_loop_agrees_with_the_plain_one() {
        let (fast, plain) = (Interpolator::new(), Interpolator::new().without_avx2());
        let x = tone(0.27, 3000);
        let mut p = Planar::default();
        p.set(&x);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        // Not a multiple of four: the batched loop and its tail both.
        fast.resample_planar(&p, 40.0, 1.09375, 2503, &mut a);
        plain.resample_planar(&p, 40.0, 1.09375, 2503, &mut b);
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(u, v)| (u - v).norm() < 1e-5));
    }

    #[test]
    fn a_whole_sample_position_returns_the_sample() {
        let it = Interpolator::new();
        let x = tone(0.1, 200);
        for i in 20..180 {
            assert!((it.at(&x, i as f64) - x[i]).norm() < 1e-5, "{i}");
        }
    }

    #[test]
    fn fractional_positions_follow_a_tone_out_to_the_band_edge() {
        // A tone at 0.38 of the rate (the 7.61 MHz block's edge at 10 MS/s)
        // read at fractions of a sample: within −40 dB.
        let it = Interpolator::new();
        for f in [0.05, 0.2, 0.38] {
            let x = tone(f, 400);
            let mut worst = 0.0f64;
            for i in 40..360 {
                for frac in [0.1, 0.37, 0.5, 0.83] {
                    let pos = i as f64 + frac;
                    let p = TAU * f * pos;
                    let want = Iq::new(p.cos() as f32, p.sin() as f32);
                    worst = worst.max((it.at(&x, pos) - want).norm() as f64);
                }
            }
            assert!(worst < 0.01, "tone at {f}: worst error {worst}");
        }
    }

    #[test]
    fn resampling_10_to_native_keeps_a_tone() {
        // 10 MS/s → 64/7: 1.09375 input samples a native sample.
        let it = Interpolator::new();
        let x = tone(0.2, 2000); // 2 MHz
        let mut y = Vec::new();
        it.resample(&x, 30.0, 10e6 / (64e6 / 7.0), 1500, &mut y);
        // The tone in native samples: 2 MHz / 9.142857 MHz.
        let f_native = 2e6 / (64e6 / 7.0);
        for (j, v) in y.iter().enumerate().skip(10) {
            let p = TAU * f_native * j as f64 + TAU * 0.2 * 30.0;
            let want = Iq::new(p.cos() as f32, p.sin() as f32);
            assert!((v - want).norm() < 0.01, "{j}");
        }
    }

    #[test]
    #[ignore = "a benchmark: cargo test --release -p decdvbt-rx bench_resample -- --ignored --nocapture"]
    fn bench_resample() {
        let it = Interpolator::new();
        let x = tone(0.2, 20_000);
        let mut p = Planar::default();
        p.set(&x);
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        let reps = 200;
        for _ in 0..reps {
            out.clear();
            it.resample_planar(&p, 30.0, 1.09375, 8192, &mut out);
        }
        let ns = t0.elapsed().as_secs_f64() * 1e9 / (reps * 8192) as f64;
        eprintln!("planar (avx2 {}): {ns:.1} ns per sample", it.avx2);
        let plain = Interpolator::new().without_avx2();
        let t0 = std::time::Instant::now();
        for _ in 0..reps {
            out.clear();
            plain.resample_planar(&p, 30.0, 1.09375, 8192, &mut out);
        }
        let ns = t0.elapsed().as_secs_f64() * 1e9 / (reps * 8192) as f64;
        eprintln!("planar, plain loop: {ns:.1} ns per sample");
        let t0 = std::time::Instant::now();
        for _ in 0..reps {
            out.clear();
            it.resample(&x, 30.0, 1.09375, 8192, &mut out);
        }
        let ns = t0.elapsed().as_secs_f64() * 1e9 / (reps * 8192) as f64;
        eprintln!("interleaved: {ns:.1} ns per sample");
    }
}
