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
    /// The rotation's cosine and sine.
    cos: f32,
    sin: f32,
    /// The divisor that took the axes' integer levels to cell units.
    norm: f32,
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
            cos: c,
            sin: s,
            norm,
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
    ///
    /// Rust note: `is_x86_feature_detected!` picks, at run time, a copy of
    /// the same code compiled for AVX2 (a release build targets plain x86-64
    /// so that the one exe runs anywhere).
    pub fn demap_block(&self, y: &[Iq], var: &[f32], out: &mut [f32]) {
        debug_assert_eq!(var.len(), y.len());
        debug_assert_eq!(out.len(), y.len() * self.bits());
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU has the feature the function was compiled for.
            return unsafe { self.demap_block_avx2(y, var, out) };
        }
        self.demap_range(y, var, 0, y.len(), out);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn demap_block_avx2(&self, y: &[Iq], var: &[f32], out: &mut [f32]) {
        self.demap_range(y, var, 0, y.len(), out);
    }

    /// [`Self::demap_block`], quantised for the LDPC decoder: each soft bit
    /// times `scale`, rounded and held to ±127. Done a few hundred cells at a
    /// time so the floats never leave the cache.
    pub fn demap_block_q(&self, y: &[Iq], var: &[f32], scale: f32, out: &mut [i8]) {
        debug_assert_eq!(var.len(), y.len());
        debug_assert_eq!(out.len(), y.len() * self.bits());
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU has the feature the function was compiled for.
            return unsafe { self.demap_block_q_avx2(y, var, scale, out) };
        }
        self.demap_block_q_body(y, var, scale, out);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn demap_block_q_avx2(&self, y: &[Iq], var: &[f32], scale: f32, out: &mut [i8]) {
        self.demap_block_q_body(y, var, scale, out);
    }

    #[inline(always)]
    fn demap_block_q_body(&self, y: &[Iq], var: &[f32], scale: f32, out: &mut [i8]) {
        const CHUNK: usize = 256;
        let bits = self.bits();
        let mut buf = [0f32; CHUNK * 8];
        let mut at = 0;
        while at < y.len() {
            let len = CHUNK.min(y.len() - at);
            self.demap_range(y, var, at, len, &mut buf[..len * bits]);
            for (o, &l) in out[at * bits..(at + len) * bits].iter_mut().zip(&buf) {
                *o = (l * scale).clamp(-127.0, 127.0).round_ties_even() as i8;
            }
            at += len;
        }
    }

    /// Cells `at..at + len` of the block, into `out` (their bits only).
    #[inline(always)]
    fn demap_range(&self, y: &[Iq], var: &[f32], at: usize, len: usize, out: &mut [f32]) {
        match self.qam {
            Qam::Qpsk => self.block::<1, 2>(y, var, at, len, out),
            Qam::Qam16 => self.block::<2, 4>(y, var, at, len, out),
            Qam::Qam64 => self.block::<3, 8>(y, var, at, len, out),
            Qam::Qam256 => self.block::<4, 16>(y, var, at, len, out),
        }
    }

    /// `B` bits an axis, `G` = 2^B levels.
    #[inline(always)]
    fn block<const B: usize, const G: usize>(
        &self,
        y: &[Iq],
        var: &[f32],
        at: usize,
        len: usize,
        out: &mut [f32],
    ) {
        let n = y.len();
        let bits = 2 * B;
        for (t, j) in (at..at + len).enumerate() {
            let o = &mut out[t * bits..(t + 1) * bits];
            if self.rotated {
                let k = if j + 1 == n { 0 } else { j + 1 };
                self.cell_rotated::<B, G>(y[j].re, y[k].im, var[j].max(1e-9), var[k].max(1e-9), o);
            } else {
                self.cell_plain::<B, G>(y[j], var[j].max(1e-9), o);
            }
        }
    }

    /// An unrotated cell: the two axes apart.
    pub fn demap_axes(&self, y: Iq, var: f32, out: &mut [f32]) {
        match self.qam {
            Qam::Qpsk => self.cell_plain::<1, 2>(y, var, out),
            Qam::Qam16 => self.cell_plain::<2, 4>(y, var, out),
            Qam::Qam64 => self.cell_plain::<3, 8>(y, var, out),
            Qam::Qam256 => self.cell_plain::<4, 16>(y, var, out),
        }
    }

    #[inline(always)]
    fn levels_of<const G: usize>(&self) -> &[f32; G] {
        self.axis[..G].try_into().expect("an axis of G levels")
    }

    #[inline(always)]
    fn cell_plain<const B: usize, const G: usize>(&self, y: Iq, var: f32, out: &mut [f32]) {
        let ax = self.levels_of::<G>();
        let inv = 1.0 / var;
        let (mut mi, mut mq) = ([0f32; G], [0f32; G]);
        for i in 0..G {
            let (dr, di) = (y.re - ax[i], y.im - ax[i]);
            mi[i] = dr * dr * inv;
            mq[i] = di * di * inv;
        }
        axis_llrs::<B, G>(&mi, out, 0);
        axis_llrs::<B, G>(&mq, out, 1);
    }

    /// A rotated point's two observed components, `a` (its I, in a cell with
    /// noise variance `va`) and `b` (its Q, in one with `vb`).
    ///
    /// The distance to the point of axis levels (x, y), which the rotation by
    /// θ puts at (x·cos − y·sin, x·sin + y·cos), is, up to a constant,
    /// `P(x) + Q(y) + κ·x·y` with κ = 2·cos·sin·(1/vb − 1/va) (zero when the
    /// cells' noise is alike, when the two axes separate altogether). That is
    /// 2G² sums where the exhaustive search of G² points costs G²·bits
    /// compares and selects: the minima over each axis' levels are taken
    /// first, then the bits' by halving.
    #[inline(always)]
    pub fn cell_rotated<const B: usize, const G: usize>(
        &self,
        a: f32,
        b: f32,
        va: f32,
        vb: f32,
        out: &mut [f32],
    ) {
        let ax = self.levels_of::<G>();
        let (wa, wb) = (1.0 / va, 1.0 / vb);
        let (c, s) = (self.cos, self.sin);
        let alpha = wa * c * c + wb * s * s;
        let beta = wa * s * s + wb * c * c;
        let kappa = 2.0 * c * s * (wb - wa);
        let lx = 2.0 * (wa * a * c + wb * b * s);
        let ly = 2.0 * (wa * a * s - wb * b * c);
        // For one axis' level the other axis' best level is the lattice
        // point nearest the vertex of a parabola: P(x) + Q(y) + κxy is
        // convex in y for a fixed x, with its minimum at y = −(ly + κx)/2β.
        // So each of the two sets of minima (m over the I levels, n over the
        // Q levels) is G evaluations, not G².
        let (norm, top) = (self.norm, (G - 1) as f32);
        let (inv_2a, inv_2b) = (0.5 / alpha, 0.5 / beta);
        let (mut m, mut n) = ([0f32; G], [0f32; G]);
        for i in 0..G {
            let x = ax[i];
            let y = nearest_level(-(ly + kappa * x) * inv_2b, norm, top);
            m[i] = (alpha * x - lx) * x + (beta * y + ly + kappa * x) * y;
            let yq = ax[i];
            let xn = nearest_level((lx - kappa * yq) * inv_2a, norm, top);
            n[i] = (beta * yq + ly) * yq + (alpha * xn - lx + kappa * yq) * xn;
        }
        axis_llrs::<B, G>(&m, out, 0);
        axis_llrs::<B, G>(&n, out, 1);
    }

    /// The exhaustive search over the constellation's points: the reference
    /// for [`Self::cell_rotated`].
    #[cfg(test)]
    fn demap_joint(&self, a: f32, b: f32, va: f32, vb: f32, out: &mut [f32]) {
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

/// `b` if it is smaller: a compare and a select, which the compiler turns
/// into one `minps` (`f32::min` also has to treat a NaN specially).
#[inline(always)]
fn fmin(a: f32, b: f32) -> f32 {
    if b < a { b } else { a }
}

/// The level of an axis (cell units, odd integers up to ±`top` over `norm`)
/// nearest `v`.
#[inline(always)]
fn nearest_level(v: f32, norm: f32, top: f32) -> f32 {
    let z = v * norm;
    // The nearest odd integer: 2·floor(z/2) + 1, held inside the axis.
    let odd = 2.0 * (0.5 * z).floor() + 1.0;
    let odd = if odd > top {
        top
    } else if odd < -top {
        -top
    } else {
        odd
    };
    odd / norm
}

/// The soft bits of one axis from the best distance `m` at each of its `G`
/// levels (indexed by the axis's bits, MSB first): for each bit, the best
/// distance with it 1 less the best with it 0. By halving: the top bit
/// splits the levels in two, their minima are the bit's two distances, and
/// folding the halves together leaves the same problem a bit narrower.
/// Bit k goes to `out[2k + first]` (the I axis's bits are the label's even
/// ones, the Q axis's the odd ones).
#[inline(always)]
fn axis_llrs<const B: usize, const G: usize>(m: &[f32; G], out: &mut [f32], first: usize) {
    let mut cur = *m;
    let mut len = G;
    for k in 0..B {
        let h = len / 2;
        let (mut d0, mut d1) = (f32::MAX, f32::MAX);
        for i in 0..h {
            d0 = fmin(d0, cur[i]);
            d1 = fmin(d1, cur[h + i]);
        }
        out[2 * k + first] = d1 - d0;
        for i in 0..h {
            cur[i] = fmin(cur[i], cur[h + i]);
        }
        len = h;
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

    /// The fast rotated demapper gives the exhaustive search's soft bits,
    /// with the two cells' noise alike or far apart.
    #[test]
    fn the_fast_rotated_demapper_matches_the_exhaustive_one() {
        let mut seed = 77u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        for q in Qam::ALL {
            let c = Constellation::new(q, true);
            let bits = q.bits();
            for _ in 0..300 {
                let (a, b) = (1.6 * rnd() - 0.8, 1.6 * rnd() - 0.8);
                let (va, vb) = if rnd() < 0.3 {
                    let v = 0.001 + 0.2 * rnd();
                    (v, v)
                } else {
                    (0.001 + 0.2 * rnd(), 0.001 + 0.2 * rnd())
                };
                let (mut fast, mut slow) = ([0f32; 8], [0f32; 8]);
                match q {
                    Qam::Qpsk => c.cell_rotated::<1, 2>(a, b, va, vb, &mut fast),
                    Qam::Qam16 => c.cell_rotated::<2, 4>(a, b, va, vb, &mut fast),
                    Qam::Qam64 => c.cell_rotated::<3, 8>(a, b, va, vb, &mut fast),
                    Qam::Qam256 => c.cell_rotated::<4, 16>(a, b, va, vb, &mut fast),
                }
                c.demap_joint(a, b, va, vb, &mut slow);
                for j in 0..bits {
                    let tol = 1e-3 * slow[j].abs().max(1.0) + 1e-3 * (1.0 / va + 1.0 / vb);
                    assert!(
                        (fast[j] - slow[j]).abs() <= tol,
                        "{q:?} bit {j}: {} against {} (a {a} b {b} va {va} vb {vb})",
                        fast[j],
                        slow[j]
                    );
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
