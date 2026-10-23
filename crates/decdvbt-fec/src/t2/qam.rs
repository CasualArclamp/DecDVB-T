// made by CasualArclamp
//! DVB-T2's constellations (EN 302 755 §6.1.4): Gray-labelled QPSK, 16-, 64-
//! and 256-QAM, optionally rotated (§6.1.5): the whole constellation turned
//! by an angle, and the Q component of each cell delayed by one cell of its
//! FEC block, so that each constellation point's two components travel in
//! two cells and a deep fade on one leaves the other.
//!
//! A label has `bits` bits, MSB first; the I axis takes the 1st, 3rd, 5th …
//! of them and the Q axis the 2nd, 4th …, each axis's bits an index into a
//! table of levels. As in gr-dtv's `dvbt2_modulator_bc_impl.cc`
//! (GPL-3.0-or-later, Copyright 2015-2019 Free Software Foundation, Inc.).
//!
//! Soft demapping is max-log: a label bit's LLR is the least squared
//! distance to a point where it is 1, less the least to one where it is 0,
//! over the noise variance; positive means 0, the LDPC decoder's convention.

use decdvbt_core::Iq;

/// The constellations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Qam {
    Qpsk,
    Qam16,
    Qam64,
    Qam256,
}

impl Qam {
    pub const ALL: [Qam; 4] = [Qam::Qpsk, Qam::Qam16, Qam::Qam64, Qam::Qam256];

    /// Bits of a cell's label.
    pub const fn bits(self) -> usize {
        match self {
            Qam::Qpsk => 2,
            Qam::Qam16 => 4,
            Qam::Qam64 => 6,
            Qam::Qam256 => 8,
        }
    }

    /// The code of PLP_MOD in L1-post.
    pub const fn plp_mod(self) -> u8 {
        match self {
            Qam::Qpsk => 0,
            Qam::Qam16 => 1,
            Qam::Qam64 => 2,
            Qam::Qam256 => 3,
        }
    }

    pub const fn from_plp_mod(c: u8) -> Option<Qam> {
        Some(match c {
            0 => Qam::Qpsk,
            1 => Qam::Qam16,
            2 => Qam::Qam64,
            3 => Qam::Qam256,
            _ => return None,
        })
    }

    pub const fn name(self) -> &'static str {
        match self {
            Qam::Qpsk => "QPSK",
            Qam::Qam16 => "16-QAM",
            Qam::Qam64 => "64-QAM",
            Qam::Qam256 => "256-QAM",
        }
    }

    /// The levels of an axis, indexed by the axis's bits (MSB first), before
    /// normalisation (Tables 14-16).
    fn levels(self) -> &'static [f32] {
        match self {
            Qam::Qpsk => &[1.0, -1.0],
            Qam::Qam16 => &[3.0, 1.0, -3.0, -1.0],
            Qam::Qam64 => &[7.0, 5.0, 1.0, 3.0, -7.0, -5.0, -1.0, -3.0],
            Qam::Qam256 => &[
                15.0, 13.0, 9.0, 11.0, 1.0, 3.0, 7.0, 5.0, -15.0, -13.0, -9.0, -11.0, -1.0, -3.0,
                -7.0, -5.0,
            ],
        }
    }

    /// The divisor that makes the points' mean power 1.
    pub fn normalisation(self) -> f32 {
        match self {
            Qam::Qpsk => 2f32.sqrt(),
            Qam::Qam16 => 10f32.sqrt(),
            Qam::Qam64 => 42f32.sqrt(),
            Qam::Qam256 => 170f32.sqrt(),
        }
    }

    /// The rotation angle, degrees (Table 17).
    pub fn rotation_deg(self) -> f64 {
        match self {
            Qam::Qpsk => 29.0,
            Qam::Qam16 => 16.8,
            Qam::Qam64 => 8.6,
            Qam::Qam256 => 3.576334375,
        }
    }
}

/// A constellation, rotated or not, with its points by label.
#[derive(Debug, Clone)]
pub struct Constellation {
    pub qam: Qam,
    pub rotated: bool,
    points: Vec<Iq>,
    /// The axes' levels in cell units (unrotated), for the separable demapper.
    axis: Vec<f32>,
}

impl Constellation {
    pub fn new(qam: Qam, rotated: bool) -> Constellation {
        let bits = qam.bits();
        let norm = qam.normalisation();
        let axis: Vec<f32> = qam.levels().iter().map(|l| l / norm).collect();
        let theta = if rotated {
            qam.rotation_deg().to_radians()
        } else {
            0.0
        };
        let (c, s) = (theta.cos() as f32, theta.sin() as f32);
        let points = (0..1usize << bits)
            .map(|label| {
                // The label's even-numbered bits (counting from the MSB as 0)
                // index I, the odd-numbered ones Q.
                let (mut i_idx, mut q_idx) = (0, 0);
                for j in 0..bits {
                    let bit = (label >> (bits - 1 - j)) & 1;
                    if j % 2 == 0 {
                        i_idx = (i_idx << 1) | bit;
                    } else {
                        q_idx = (q_idx << 1) | bit;
                    }
                }
                let p = Iq::new(axis[i_idx], axis[q_idx]);
                Iq::new(p.re * c - p.im * s, p.re * s + p.im * c)
            })
            .collect();
        Constellation {
            qam,
            rotated,
            points,
            axis,
        }
    }

    pub fn point(&self, label: usize) -> Iq {
        self.points[label]
    }

    pub fn bits(&self) -> usize {
        self.qam.bits()
    }

    /// The cells for a FEC block's labels: unrotated, a cell is its point;
    /// rotated, cell j takes the I component of point j and the Q component
    /// of point j−1 (wrapping within the block).
    pub fn map_block(&self, labels: &[u16], out: &mut Vec<Iq>) {
        out.clear();
        let n = labels.len();
        if !self.rotated {
            out.extend(labels.iter().map(|&l| self.points[l as usize]));
            return;
        }
        for j in 0..n {
            let prev = self.points[labels[(j + n - 1) % n] as usize];
            out.push(Iq::new(self.points[labels[j] as usize].re, prev.im));
        }
    }

    /// Soft bits of a FEC block's cells, label bits MSB first per cell:
    /// `out[cell·bits + j]`. `y` are equalised cells and `var` each cell's
    /// noise variance (complex, in cell units). For a rotated constellation
    /// cell j's I component and cell (j+1)'s Q component are one point's.
    pub fn demap_block(&self, y: &[Iq], var: &[f32], out: &mut [f32]) {
        let n = y.len();
        let bits = self.bits();
        debug_assert_eq!(var.len(), n);
        debug_assert_eq!(out.len(), n * bits);
        if self.rotated {
            for j in 0..n {
                let k = (j + 1) % n;
                self.demap_joint(
                    y[j].re,
                    y[k].im,
                    var[j].max(1e-9),
                    var[k].max(1e-9),
                    &mut out[j * bits..(j + 1) * bits],
                );
            }
        } else {
            for j in 0..n {
                self.demap_axes(y[j], var[j].max(1e-9), &mut out[j * bits..(j + 1) * bits]);
            }
        }
    }

    /// An unrotated cell: the two axes apart.
    pub fn demap_axes(&self, y: Iq, var: f32, out: &mut [f32]) {
        let a = self.bits() / 2;
        let inv = 1.0 / var;
        for (comp, r) in [y.re, y.im].into_iter().enumerate() {
            for k in 0..a {
                let (mut d0, mut d1) = (f32::MAX, f32::MAX);
                for (idx, &l) in self.axis.iter().enumerate() {
                    let d = (r - l) * (r - l);
                    if (idx >> (a - 1 - k)) & 1 == 0 {
                        d0 = d0.min(d);
                    } else {
                        d1 = d1.min(d);
                    }
                }
                out[2 * k + comp] = (d1 - d0) * inv;
            }
        }
    }

    /// A rotated point's two observed components, `a` (its I, in a cell with
    /// noise variance `va`) and `b` (its Q, in one with `vb`).
    pub fn demap_joint(&self, a: f32, b: f32, va: f32, vb: f32, out: &mut [f32]) {
        let bits = self.bits();
        let (wa, wb) = (1.0 / va, 1.0 / vb);
        let mut d0 = [f32::MAX; 8];
        let mut d1 = [f32::MAX; 8];
        for (label, p) in self.points.iter().enumerate() {
            let d = (a - p.re) * (a - p.re) * wa + (b - p.im) * (b - p.im) * wb;
            for j in 0..bits {
                if (label >> (bits - 1 - j)) & 1 == 0 {
                    d0[j] = d0[j].min(d);
                } else {
                    d1[j] = d1[j].min(d);
                }
            }
        }
        for j in 0..bits {
            out[j] = d1[j] - d0[j];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_have_unit_mean_power_rotated_or_not() {
        for q in Qam::ALL {
            for rot in [false, true] {
                let c = Constellation::new(q, rot);
                let p: f32 = (0..1 << q.bits())
                    .map(|l| c.point(l).norm_sqr())
                    .sum::<f32>()
                    / (1 << q.bits()) as f32;
                assert!((p - 1.0).abs() < 1e-4, "{q:?} {rot}: {p}");
            }
        }
    }

    #[test]
    fn labels_follow_the_standards_corners() {
        // 16-QAM: label 0000 is (3, 3)/√10; 1111 is (−1, −1)/√10; 1000 has
        // I from bits (1, 0) = index 2: −3, Q from (0, 0) = 3.
        let c = Constellation::new(Qam::Qam16, false);
        let n = 10f32.sqrt();
        assert!((c.point(0b0000) - Iq::new(3.0 / n, 3.0 / n)).norm() < 1e-6);
        assert!((c.point(0b1111) - Iq::new(-1.0 / n, -1.0 / n)).norm() < 1e-6);
        assert!((c.point(0b1000) - Iq::new(-3.0 / n, 3.0 / n)).norm() < 1e-6);
        // QPSK: 00 is (+, +), 01 (+, −), 10 (−, +), 11 (−, −).
        let q = Constellation::new(Qam::Qpsk, false);
        let h = 0.5f32.sqrt();
        assert!((q.point(1) - Iq::new(h, -h)).norm() < 1e-6);
        assert!((q.point(2) - Iq::new(-h, h)).norm() < 1e-6);
    }

    #[test]
    fn a_clean_cell_demaps_to_its_own_bits() {
        for q in Qam::ALL {
            let c = Constellation::new(q, false);
            let bits = q.bits();
            for label in 0..1usize << bits {
                let mut llr = vec![0f32; bits];
                c.demap_axes(c.point(label), 0.01, &mut llr);
                for (j, l) in llr.iter().enumerate() {
                    let b = (label >> (bits - 1 - j)) & 1;
                    assert_eq!(*l < 0.0, b == 1, "{q:?} label {label:b} bit {j}: {l}");
                }
            }
        }
    }

    #[test]
    fn a_rotated_block_demaps_to_its_labels() {
        for q in Qam::ALL {
            let c = Constellation::new(q, true);
            let bits = q.bits();
            let n = 64;
            let labels: Vec<u16> = (0..n)
                .map(|i| ((i * 37 + 11) % (1 << bits)) as u16)
                .collect();
            let mut cells = Vec::new();
            c.map_block(&labels, &mut cells);
            let var = vec![0.001f32; n];
            let mut llr = vec![0f32; n * bits];
            c.demap_block(&cells, &var, &mut llr);
            for (j, &label) in labels.iter().enumerate() {
                for k in 0..bits {
                    let b = (label >> (bits - 1 - k)) & 1;
                    assert_eq!(llr[j * bits + k] < 0.0, b == 1, "{q:?} cell {j} bit {k}");
                }
            }
        }
    }

    #[test]
    fn rotation_spreads_a_point_over_two_cells() {
        // With rotation a point's I and Q are both nonzero: its two cells
        // each carry information about it.
        let c = Constellation::new(Qam::Qam256, true);
        let p = c.point(0);
        assert!(p.re.abs() > 0.01 && p.im.abs() > 0.01);
        // The rotation angle is applied to the whole constellation.
        let u = Constellation::new(Qam::Qam256, false).point(0);
        let th = (p.im.atan2(p.re) - u.im.atan2(u.re)).to_degrees();
        assert!((th - 3.576_334_4).abs() < 1e-3, "{th}");
    }
}
