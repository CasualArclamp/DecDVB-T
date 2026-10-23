// made by CasualArclamp
//! Finding the P1 symbol in a stretch of samples at the signal's own rate, and
//! reading what it says (EN 302 755 §7.2): the start of every T2 frame, S1
//! (T2 or not, SISO or MISO) and S2 (the FFT size of the frame) and, from its
//! frequency-shifted copies and its carriers, the carrier offset.
//!
//! P1 is C (542 samples), A (1024) and B (482): C is A's first 542 samples and
//! B A's last 482, each shifted up by one P1 carrier spacing (fs/1024). So
//! C and the start of A, 542 samples apart, and B and the end of A, 482 apart,
//! are the same signal up to a rotation whose rate is known: taking that out,
//! two sums stand clear of the noise at the symbol's start, and their phases
//! give the carrier offset to within half a P1 spacing (4.5 kHz). The whole
//! spacings are found from where P1's carriers sit in the spectrum.

use decdvbt_core::Iq;
use decdvbt_ofdm::t2::p1::{self, CARRIERS, FFT};
use rustfft::FftPlanner;

const C_LEN: usize = p1::C_LEN;
const B_LEN: usize = p1::B_LEN;
const SAMPLES: usize = p1::SAMPLES;
/// The metric over which a position is taken to be a P1 symbol's start.
pub const MIN_METRIC: f32 = 0.25;

/// A P1 symbol found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct P1Hit {
    /// The symbol's first sample (fractional, in the stretch).
    pub start: f64,
    /// The detection metric, 0 to 1.
    pub metric: f32,
    /// The carrier offset modulo the P1 carrier spacing (±fs/2048), Hz.
    pub cfo_frac_hz: f64,
}

#[allow(clippy::needless_range_loop)]
/// P1 symbols in `y` (at rate `fs`), in order of position. Each is a local
/// maximum of the metric over a window of a symbol's length.
pub fn find(y: &[Iq], fs: f64) -> Vec<P1Hit> {
    let n = y.len();
    if n < SAMPLES + 2 {
        return Vec::new();
    }
    // The rotation that is taken out of the products, e^{-j2πn/1024}.
    let rot = rotation_table();
    let last = n - SAMPLES;
    // Products for C against A's start (p1) and for A's end against B (p2),
    // and the energies they could have, summed over their windows by running
    // sums: S1 over [s, s+542), S2 over [s+1084, s+1566).
    let p1_at = |i: usize| y[i] * y[i + C_LEN].conj() * rot[i % FFT];
    let p2_at = |i: usize| y[i + B_LEN] * y[i].conj() * rot[i % FFT];
    let e1_at = |i: usize| f64::from(y[i].norm_sqr() + y[i + C_LEN].norm_sqr());
    let e2_at = |i: usize| f64::from(y[i].norm_sqr() + y[i + B_LEN].norm_sqr());
    let sum_from = |s: usize| {
        let (mut s1, mut s2) = ((0f64, 0f64), (0f64, 0f64));
        let (mut en1, mut en2) = (0f64, 0f64);
        for i in s..s + C_LEN {
            let v = p1_at(i);
            s1 = (s1.0 + f64::from(v.re), s1.1 + f64::from(v.im));
            en1 += e1_at(i);
        }
        for i in s + 1084..s + 1084 + B_LEN {
            let v = p2_at(i);
            s2 = (s2.0 + f64::from(v.re), s2.1 + f64::from(v.im));
            en2 += e2_at(i);
        }
        (s1, s2, en1, en2)
    };
    let (mut s1, mut s2, mut en1, mut en2) = sum_from(0);
    let mut metric = vec![0f32; last + 1];
    for s in 0..=last {
        // Each sum, against the energy it could have: 1/2 for a clean P1
        // (|a||b| ≤ (|a|² + |b|²)/2), so the two add up to 1.
        metric[s] = (s1.0.hypot(s1.1) / en1.max(1e-12) + s2.0.hypot(s2.1) / en2.max(1e-12)) as f32;
        if s < last {
            let (a, b) = (p1_at(s), p1_at(s + C_LEN));
            s1 = (
                s1.0 - f64::from(a.re) + f64::from(b.re),
                s1.1 - f64::from(a.im) + f64::from(b.im),
            );
            en1 += e1_at(s + C_LEN) - e1_at(s);
            let (c, d) = (p2_at(s + 1084), p2_at(s + 1084 + B_LEN));
            s2 = (
                s2.0 - f64::from(c.re) + f64::from(d.re),
                s2.1 - f64::from(c.im) + f64::from(d.im),
            );
            en2 += e2_at(s + 1084 + B_LEN) - e2_at(s + 1084);
        }
        if s % 65536 == 65535 && s < last {
            // Re-sum from scratch now and then: the running sums drift.
            (s1, s2, en1, en2) = sum_from(s + 1);
        }
    }
    // Peaks: the best position in each stretch where the metric is over the
    // threshold, with a parabola's fraction.
    let mut hits = Vec::new();
    let mut i = 0;
    while i <= last {
        if metric[i] >= MIN_METRIC {
            let end = (i + SAMPLES).min(last + 1);
            let (mut best, mut at) = (0.0f32, i);
            for (j, &m) in metric.iter().enumerate().take(end).skip(i) {
                if m > best {
                    (best, at) = (m, j);
                }
            }
            let sub = if at > 0 && at < last {
                crate::acquire::parabola(metric[at - 1], metric[at], metric[at + 1])
            } else {
                0.0
            };
            hits.push(P1Hit {
                start: at as f64 + sub,
                metric: best,
                cfo_frac_hz: cfo_fraction(y, at, fs, &rot),
            });
            i = at + SAMPLES;
        } else {
            i += 1;
        }
    }
    hits
}

fn rotation_table() -> Vec<Iq> {
    (0..FFT)
        .map(|i| {
            let ph = -std::f64::consts::TAU * i as f64 / FFT as f64;
            Iq::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect()
}

/// The carrier offset modulo the P1 spacing, from the phases of the two sums
/// at a P1 starting at `s`: S1 carries e^{-jφ1}, S2 e^{+jφ2} with
/// φ1 + φ2 = 2π ν·1024 (ν the offset in cycles a sample), once the rotations
/// that depend on the position are taken out.
fn cfo_fraction(y: &[Iq], s: usize, fs: f64, rot: &[Iq]) -> f64 {
    let (mut a, mut b) = (Iq::new(0.0, 0.0), Iq::new(0.0, 0.0));
    for i in s..s + C_LEN {
        a += y[i] * y[i + C_LEN].conj() * rot[i % FFT];
    }
    for i in s + 1084..s + 1084 + B_LEN {
        b += y[i + B_LEN] * y[i].conj() * rot[i % FFT];
    }
    let (ra, rb) = (rot[s % FFT].conj(), rot[(s + C_LEN) % FFT].conj());
    let p = (a * ra) * (b * rb).conj();
    -f64::from(p.arg()) / (std::f64::consts::TAU * FFT as f64) * fs
}

/// What reading a P1 symbol gave.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct P1Info {
    pub s1: u8,
    pub s2: u8,
    /// The carrier offset, Hz: the fraction and the whole P1 spacings found.
    pub cfo_hz: f64,
    pub quality: f32,
}

/// Read S1 and S2 of the P1 symbol at `hit`: the carrier offset's fraction
/// taken out, the transform of its A part, the offset in whole spacings
/// searched over ±`max_bins`.
pub fn read(y: &[Iq], hit: &P1Hit, fs: f64, max_bins: isize) -> Option<P1Info> {
    let start = hit.start.round() as usize;
    if start + SAMPLES > y.len() {
        return None;
    }
    // A, with the fractional offset taken out.
    let a0 = start + C_LEN;
    let w = -std::f64::consts::TAU * hit.cfo_frac_hz / fs;
    let mut buf: Vec<Iq> = (0..FFT)
        .map(|i| {
            let ph = w * (a0 + i) as f64;
            y[a0 + i] * Iq::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect();
    FftPlanner::<f32>::new()
        .plan_fft_forward(FFT)
        .process(&mut buf);
    // P1's carriers sit at bins (carrier + 86 − 512) of the centred transform:
    // bin index in the plain transform is that mod 1024.
    let bin = |i: usize, d: isize| -> usize {
        (p1::bin(i) as isize - (FFT / 2) as isize + d).rem_euclid(FFT as isize) as usize
    };
    // The power on the active carriers against the whole, for each shift.
    let mut best = (f32::MIN, 0isize);
    for d in -max_bins..=max_bins {
        let on: f32 = (0..CARRIERS).map(|i| buf[bin(i, d)].norm_sqr()).sum();
        if on > best.0 {
            best = (on, d);
        }
    }
    let d = best.1;
    let values: Vec<(f32, f32)> = (0..CARRIERS)
        .map(|i| {
            let c = buf[bin(i, d)];
            (c.re, c.im)
        })
        .collect();
    let sig = p1::detect(&values)?;
    Some(P1Info {
        s1: sig.s1,
        s2: sig.s2,
        cfo_hz: hit.cfo_frac_hz + d as f64 * fs / FFT as f64,
        quality: sig.quality,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_mod::t2::p1_symbol;

    fn noise(n: usize, sigma: f32, seed: u32) -> Vec<Iq> {
        let mut s = seed;
        let mut g = || {
            let mut a = 0.0;
            for _ in 0..4 {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                a += (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            }
            a * 1.7320508
        };
        (0..n).map(|_| Iq::new(g(), g()) * sigma).collect()
    }

    /// A stream with a P1 symbol (S1, S2) at `at`, offset by `cfo` Hz, in noise.
    fn stream(at: usize, cfo: f64, fs: f64, snr_db: f32, s1: u8, s2: u8) -> Vec<Iq> {
        let p = p1_symbol(s1, s2);
        let sigma = 10f32.powf(-snr_db / 20.0);
        let mut y = noise(6000, sigma, 5);
        for (i, v) in p.iter().enumerate() {
            y[at + i] += *v;
        }
        for (i, v) in y.iter_mut().enumerate() {
            let ph = std::f64::consts::TAU * cfo * i as f64 / fs;
            *v *= Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        y
    }

    #[test]
    fn a_p1_symbol_is_found_where_it_is() {
        let fs = 64e6 / 7.0;
        for at in [100usize, 1500, 3000] {
            let y = stream(at, 0.0, fs, 20.0, 0, p1::s2_value(5, 0));
            let hits = find(&y, fs);
            assert_eq!(hits.len(), 1, "{hits:?}");
            assert!(
                (hits[0].start - at as f64).abs() < 1.5,
                "{} vs {at}",
                hits[0].start
            );
        }
    }

    #[test]
    fn the_carrier_offset_and_the_signalling_are_read() {
        let fs = 64e6 / 7.0;
        let spacing = fs / 1024.0;
        for cfo in [-120_000.0, -3_000.0, 0.0, 4_000.0, 70_000.0, 250_000.0] {
            for (s1, f1) in [(0u8, 5u8), (1, 1), (3, 0)] {
                let s2 = p1::s2_value(f1, 0);
                let y = stream(800, cfo, fs, 15.0, s1, s2);
                let hits = find(&y, fs);
                assert_eq!(hits.len(), 1, "cfo {cfo}: {hits:?}");
                let info = read(&y, &hits[0], fs, 64).expect("a read");
                assert_eq!((info.s1, info.s2), (s1, s2), "cfo {cfo}");
                assert!(
                    (info.cfo_hz - cfo).abs() < 0.1 * spacing,
                    "cfo {cfo}: read {}",
                    info.cfo_hz
                );
            }
        }
    }

    #[test]
    fn noise_alone_has_no_p1() {
        let fs = 64e6 / 7.0;
        assert!(find(&noise(20_000, 1.0, 9), fs).is_empty());
    }
}
