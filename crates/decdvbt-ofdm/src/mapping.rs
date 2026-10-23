// made by CasualArclamp
//! The constellations (EN 300 744 §4.3.5): QPSK, 16-QAM and 64-QAM, Gray
//! coded, optionally non-uniform (hierarchical α).
//!
//! A cell's v bits y0 … y(v−1) (y0 the most significant of the word) pick the
//! point: y0 the sign of I and y1 the sign of Q (0 is positive), and the rest
//! the magnitudes, I from y2, y4 and Q from y3, y5: Gray coded, the outermost
//! level on 0 (0, 0 for 64-QAM, then 0, 1; 1, 1; 1, 0 inwards). The levels are
//! α + 2n, n counted from the inside, and the whole is scaled to unit mean
//! power (1/√2, 1/√10, 1/√42 for α = 1).

use decdvbt_core::dvbt::{Alpha, Modulation};
use num_complex::Complex32;

/// A constellation: point `p[word]` for each bit word.
#[derive(Debug, Clone)]
pub struct Constellation {
    pub modulation: Modulation,
    points: Vec<Complex32>,
}

fn alpha_value(a: Alpha) -> f32 {
    match a {
        Alpha::NonHierarchical | Alpha::A1 => 1.0,
        Alpha::A2 => 2.0,
        Alpha::A4 => 4.0,
    }
}

/// The magnitude of one axis for the Gray-coded magnitude bits `g` (the
/// first of them the more significant), with `steps` + 1 levels.
fn level(alpha: f32, g: u32, steps: u32) -> f32 {
    // Gray to binary: the level index counted from the outermost.
    let mut x = g;
    let mut s = g >> 1;
    while s != 0 {
        x ^= s;
        s >>= 1;
    }
    alpha + 2.0 * (steps - x) as f32
}

impl Constellation {
    pub fn new(m: Modulation, alpha: Alpha) -> Constellation {
        let v = m.bits();
        let a = alpha_value(alpha);
        let mut points = Vec::with_capacity(1 << v);
        for word in 0..(1u32 << v) {
            // y_k, k = 0 … v−1, y0 the most significant.
            let y = |k: usize| (word >> (v - 1 - k)) & 1;
            let (i, q) = match m {
                // QPSK: the signs alone.
                Modulation::Qpsk => (1.0, 1.0),
                Modulation::Qam16 => (level(a, y(2), 1), level(a, y(3), 1)),
                Modulation::Qam64 => (
                    level(a, (y(2) << 1) | y(4), 3),
                    level(a, (y(3) << 1) | y(5), 3),
                ),
            };
            let sign = |b: u32| if b == 0 { 1.0 } else { -1.0 };
            points.push(Complex32::new(sign(y(0)) * i, sign(y(1)) * q));
        }
        // Unit mean power.
        let power = points.iter().map(|p| p.norm_sqr()).sum::<f32>() / points.len() as f32;
        let norm = 1.0 / power.sqrt();
        for p in &mut points {
            *p *= norm;
        }
        Constellation {
            modulation: m,
            points,
        }
    }

    /// The point for a bit word.
    pub fn point(&self, word: u8) -> Complex32 {
        self.points[word as usize]
    }

    pub fn points(&self) -> &[Complex32] {
        &self.points
    }

    /// The word of the nearest point (a hard decision).
    pub fn nearest(&self, z: Complex32) -> u8 {
        let mut best = (0usize, f32::MAX);
        for (w, p) in self.points.iter().enumerate() {
            let d = (z - p).norm_sqr();
            if d < best.1 {
                best = (w, d);
            }
        }
        best.0 as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Modulation; 3] = [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64];

    #[test]
    fn unit_mean_power_and_the_specs_scale_factors() {
        for m in ALL {
            let c = Constellation::new(m, Alpha::NonHierarchical);
            let p = c.points().iter().map(|p| p.norm_sqr()).sum::<f32>() / c.points().len() as f32;
            assert!((p - 1.0).abs() < 1e-5, "{m:?}");
        }
        // The unscaled corner of each: the spec's 1/√2, 1/√10, 1/√42.
        let corner = |m| Constellation::new(m, Alpha::NonHierarchical).point(0).re;
        assert!((corner(Modulation::Qpsk) - 1.0 / 2f32.sqrt()).abs() < 1e-6);
        assert!((corner(Modulation::Qam16) - 3.0 / 10f32.sqrt()).abs() < 1e-6);
        assert!((corner(Modulation::Qam64) - 7.0 / 42f32.sqrt()).abs() < 1e-6);
        // Hierarchical 16-QAM, α = 2: 1/√20 over levels 2 and 4.
        let c = Constellation::new(Modulation::Qam16, Alpha::A2);
        assert!((c.point(0).re - 4.0 / 20f32.sqrt()).abs() < 1e-6);
    }

    #[test]
    fn nearest_neighbours_differ_in_one_bit() {
        // The Gray property: the closest points to any point (one grid step
        // away on an axis) are one bit away. Fails for a wrong bit order.
        for m in ALL {
            for a in [Alpha::NonHierarchical, Alpha::A2] {
                let c = Constellation::new(m, a);
                let pts = c.points();
                let dmin = pts
                    .iter()
                    .enumerate()
                    .flat_map(|(i, p)| {
                        pts.iter()
                            .enumerate()
                            .filter(move |(j, _)| *j != i)
                            .map(move |(_, q)| (p - q).norm())
                    })
                    .fold(f32::MAX, f32::min);
                for (i, p) in pts.iter().enumerate() {
                    for (j, q) in pts.iter().enumerate() {
                        if i != j && (p - q).norm() < dmin * 1.001 {
                            assert_eq!(
                                (i ^ j).count_ones(),
                                1,
                                "{m:?} {a:?}: words {i:#b} and {j:#b} are neighbours"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_points_are_all_different_and_nearest_is_exact() {
        for m in ALL {
            let c = Constellation::new(m, Alpha::NonHierarchical);
            for w in 0..(1u32 << m.bits()) {
                assert_eq!(c.nearest(c.point(w as u8)), w as u8);
            }
        }
    }

    #[test]
    fn signs_and_levels_follow_the_spec() {
        let k = 1.0 / 42f32.sqrt();
        let c = Constellation::new(Modulation::Qam64, Alpha::NonHierarchical);
        // y = 000000: top right, outermost both axes.
        assert_eq!(c.point(0b000000), Complex32::new(7.0 * k, 7.0 * k));
        // y0 = 1: I negative; y1 = 1: Q negative.
        assert_eq!(c.point(0b100000), Complex32::new(-7.0 * k, 7.0 * k));
        assert_eq!(c.point(0b010000), Complex32::new(7.0 * k, -7.0 * k));
        // I magnitude from (y2, y4): 00 → 7, 01 → 5, 11 → 3, 10 → 1.
        for (y2y4, mag) in [(0b00, 7.0), (0b01, 5.0), (0b11, 3.0), (0b10, 1.0)] {
            let word = ((y2y4 >> 1) << 3) | ((y2y4 & 1) << 1);
            assert_eq!(c.point(word as u8).re, mag * k, "y2y4 = {y2y4:#b}");
        }
        // QPSK: 00 → (+, +), 01 → (+, −), 10 → (−, +), 11 → (−, −).
        let q = Constellation::new(Modulation::Qpsk, Alpha::NonHierarchical);
        let s = 1.0 / 2f32.sqrt();
        assert_eq!(q.point(0b01), Complex32::new(s, -s));
        assert_eq!(q.point(0b10), Complex32::new(-s, s));
    }
}
