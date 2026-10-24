// made by CasualArclamp
//! The effective SNR of a channel that is not flat: what the code sees.
//!
//! A mean SNR (the band's signal power over its noise power) reads the same
//! for a flat channel and for one with deep notches, but the notched one
//! decodes worse: the faded carriers' bits are near guesses, and the strong
//! carriers cannot make up for them, because a carrier's worth to the code
//! saturates at its constellation's bits. So each cell's SNR is turned into
//! the mutual information its constellation carries at that SNR (bit-
//! interleaved coded modulation, Gray-labelled square QAM: two independent
//! PAM axes), the information is averaged over the cells, and the average is
//! turned back into the SNR a Gaussian channel needs to carry the same. That
//! is the mutual-information effective SNR mapping (MIESM) of link-level
//! simulation; set against a mode's Gaussian-channel QEF C/N it gives a
//! margin that a notched channel does not flatter.
//!
//! The information of an axis with `L` levels and bit `b` of its Gray label:
//! `I_b = 1 − E[log2(1 + Σ_other p / Σ_same p)]`, where `p` are the levels'
//! likelihoods of the received value, `same` the levels whose bit `b` is the
//! transmitted level's, the expectation over the transmitted level and the
//! noise (integrated numerically, ±6σ in 48 steps).
//!
//! Lookups are by the SNR's float bits (its exponent and five mantissa
//! bits): no logarithm per cell, 0.13 dB steps.

use std::sync::OnceLock;

/// The SNRs tabled, linear: −12 to +48 dB.
const LO: f32 = 0.063;
const HI: f32 = 63_000.0;
/// Mantissa bits kept in the index.
const SHIFT: u32 = 23 - 5;

/// The mutual information of one constellation against SNR.
pub struct Capacity {
    /// Information bits a cell carries at most.
    pub bits: f32,
    base: u32,
    /// Information a cell carries, by index (increasing with the SNR), and
    /// each index's SNR.
    mi: Vec<f32>,
    snr: Vec<f32>,
}

fn index_of(x: f32) -> u32 {
    x.to_bits() >> SHIFT
}

impl Capacity {
    /// For square QAM of `bits_per_axis` bits an axis (1 QPSK, 2 16-QAM, 3
    /// 64-QAM, 4 256-QAM).
    fn new(bits_per_axis: u32) -> Capacity {
        let base = index_of(LO);
        let top = index_of(HI);
        let (mut mi, mut snr) = (Vec::new(), Vec::new());
        for i in base..=top {
            // The middle of the index's span of SNRs.
            let lo = f32::from_bits(i << SHIFT);
            let hi = f32::from_bits((i + 1) << SHIFT);
            let s = 0.5 * (lo + hi);
            snr.push(s);
            mi.push(2.0 * axis_information(bits_per_axis, f64::from(s)) as f32);
        }
        // Rounding can leave the top end a hair non-monotonic: make it so.
        for i in 1..mi.len() {
            mi[i] = mi[i].max(mi[i - 1]);
        }
        Capacity {
            bits: 2.0 * bits_per_axis as f32,
            base,
            mi,
            snr,
        }
    }

    /// Information bits a cell carries at SNR `snr` (linear, cell power over
    /// noise power).
    #[inline]
    pub fn information(&self, snr: f32) -> f32 {
        let i = index_of(snr.clamp(LO, HI)).saturating_sub(self.base) as usize;
        self.mi[i.min(self.mi.len() - 1)]
    }

    /// The SNR, dB, at which a Gaussian channel carries `mi` bits a cell.
    pub fn effective_db(&self, mi: f32) -> f32 {
        let i = self.mi.partition_point(|&v| v < mi).min(self.mi.len() - 1);
        10.0 * self.snr[i].log10()
    }
}

/// The table for `bits_per_axis` (1 to 4), made the first time it is asked
/// for (tens of milliseconds for 256-QAM).
pub fn capacity(bits_per_axis: u32) -> &'static Capacity {
    static TABLES: [OnceLock<Capacity>; 4] = [const { OnceLock::new() }; 4];
    let b = bits_per_axis.clamp(1, 4);
    TABLES[(b - 1) as usize].get_or_init(|| Capacity::new(b))
}

/// The information of one PAM axis of square QAM (unit cell power, so the
/// axis's power is ½ and its noise variance 1/(2·snr)), summed over its bits.
fn axis_information(bits: u32, snr: f64) -> f64 {
    let l = 1usize << bits;
    let norm = (2.0 * ((l * l) as f64 - 1.0) / 3.0).sqrt();
    let level = |i: usize| (2.0 * i as f64 - (l as f64 - 1.0)) / norm;
    // The binary-reflected Gray label of each level.
    let gray: Vec<usize> = (0..l).map(|i| i ^ (i >> 1)).collect();
    let sigma2 = 1.0 / (2.0 * snr);
    let sigma = sigma2.sqrt();
    const STEPS: usize = 48;
    let span = 6.0 * sigma;
    let dn = 2.0 * span / STEPS as f64;
    let mut p = vec![0.0f64; l];
    let mut lost = 0.0f64;
    let mut weight_sum = 0.0f64;
    for t in 0..=STEPS {
        let n = -span + t as f64 * dn;
        // Trapezoid weights times the Gaussian density.
        let w = (-n * n / (2.0 * sigma2)).exp() * if t == 0 || t == STEPS { 0.5 } else { 1.0 };
        weight_sum += w;
        for tx in 0..l {
            let y = level(tx) + n;
            for (a, pa) in p.iter_mut().enumerate() {
                let d = y - level(a);
                *pa = (-d * d / (2.0 * sigma2)).exp();
            }
            for b in 0..bits {
                let bit = (gray[tx] >> b) & 1;
                let (mut same, mut other) = (0.0, 0.0);
                for a in 0..l {
                    if (gray[a] >> b) & 1 == bit {
                        same += p[a];
                    } else {
                        other += p[a];
                    }
                }
                lost += w * (1.0 + other / same).log2();
            }
        }
    }
    bits as f64 - lost / (weight_sum * l as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn information_runs_from_nothing_to_the_constellations_bits() {
        for b in 1..=4 {
            let c = capacity(b);
            assert!(c.information(0.07) < 0.2 * c.bits, "{b}");
            assert!((c.information(60_000.0) - c.bits).abs() < 0.01, "{b}");
        }
        // QPSK at 0 dB: two BPSK axes at 0 dB Es/N0 per axis, about 0.49
        // bits each (the BPSK capacity at an SNR of 1 on its axis... per
        // axis noise 1/2 of a unit-power cell: 0.486 bits).
        let q = capacity(1).information(1.0);
        assert!((q - 0.97).abs() < 0.05, "{q}");
    }

    #[test]
    fn a_flat_channel_maps_back_to_its_own_snr() {
        let c = capacity(3);
        for db in [5.0f32, 12.0, 16.5, 20.0, 25.0] {
            let s = 10f32.powf(db / 10.0);
            let back = c.effective_db(c.information(s));
            assert!((back - db).abs() < 0.2, "{db} -> {back}");
        }
    }

    #[test]
    fn a_notched_channel_reads_below_its_mean() {
        // 64-QAM: nine carriers in ten at 22 dB, one in ten at 0 dB. The mean
        // SNR is 21.6 dB; the effective one is well under it.
        let c = capacity(3);
        let (good, bad) = (10f32.powf(2.2), 1.0f32);
        let mean_db = 10.0 * (0.9 * good + 0.1 * bad).log10();
        let mi = 0.9 * c.information(good) + 0.1 * c.information(bad);
        let eff = c.effective_db(mi);
        assert!(eff < mean_db - 2.0, "mean {mean_db:.1}, effective {eff:.1}");
    }
}
