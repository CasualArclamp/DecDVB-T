// made by CasualArclamp
//! From a symbol's equalised cells to the soft coded bits, in the order the
//! inner decoder wants them: demapping, symbol deinterleaving and bit
//! deinterleaving in one pass over the cells.
//!
//! Each cell gives v max-log soft bits (EN 300 744 §4.3.5's Gray mapping,
//! per axis: closed forms of the distance differences, all piecewise
//! linear). They are weighted by the cell's channel gain |H|² (a cell in a
//! notch says little) over the noise power at its carrier (which the equaliser
//! measures from the pilots, so a band an adjacent channel has leaked into
//! counts for less), and quantised to eight bits. The
//! cells are taken in the order the symbol deinterleaver puts them (a
//! gather by a table, not a pass of moves). The soft bits are made a bit at
//! a time over all the cells (a loop the compiler turns into vector code),
//! as one array for each of the cell's v bits; the bit deinterleaver is then
//! a rotation of each of those within a 126-cell block, and putting the v
//! arrays' bytes side by side, cell by cell, a byte transposition done with
//! shuffles eight cells at a time.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{FftSize, Modulation};
use decdvbt_ofdm::interleave::{BLOCK, demux_order, symbol_permutation};
use decdvbt_ofdm::pilots::SymbolPlan;

/// The offsets of the bit interleavers' permutations (§4.3.4.1), as
/// `decdvbt_ofdm::interleave` has them.
const OFFSETS: [usize; 6] = [0, 63, 105, 42, 21, 84];

/// Quantisation: a soft bit of log-likelihood ratio 1 is this many steps.
const GAIN: f32 = 8.0;

/// How much of each symbol's signal-to-noise figure goes into the running one.
const SNR_RATE: f32 = 0.25;

/// A unit-power cell scaled to the odd-integer grid: ×√2, ×√10, ×√42.
fn unit_scale(m: Modulation) -> f32 {
    match m {
        Modulation::Qpsk => std::f32::consts::SQRT_2,
        Modulation::Qam16 => 10f32.sqrt(),
        Modulation::Qam64 => 42f32.sqrt(),
    }
}

#[inline(always)]
fn sgn(x: f32) -> f32 {
    f32::copysign(1.0, x)
}

/// The max-log soft bits of one cell, d(bit = 1)² − d(bit = 0)² in the odd-
/// integer grid (positive for a likely 0), for y0 … y(V−1). `x_i`, `x_q` are
/// on the grid.
///
/// Per axis, with a = |x|: the sign bit is sgn·(4a + 4·max(a−2, 0) [+
/// 4·max(a−4, 0) + 4·max(a−6, 0) for 64-QAM]); 16-QAM's magnitude bit
/// 4(a−2); 64-QAM's two magnitude bits (outer pair vs inner pair, then
/// 7 and 1 vs 5 and 3) 4a−16 corrected to 8a−24 below 2 and 8a−40 above 6,
/// and 4|a−4|−8.
#[inline(always)]
fn cell<const V: usize>(x_i: f32, x_q: f32) -> [f32; 6] {
    let mut l = [0.0f32; 6];
    let (ai, aq) = (x_i.abs(), x_q.abs());
    match V {
        2 => {
            l[0] = 4.0 * x_i;
            l[1] = 4.0 * x_q;
        }
        4 => {
            l[0] = sgn(x_i) * (4.0 * ai + 4.0 * (ai - 2.0).max(0.0));
            l[1] = sgn(x_q) * (4.0 * aq + 4.0 * (aq - 2.0).max(0.0));
            l[2] = 4.0 * (ai - 2.0);
            l[3] = 4.0 * (aq - 2.0);
        }
        _ => {
            let sign = |a: f32| {
                4.0 * a
                    + 4.0 * (a - 2.0).max(0.0)
                    + 4.0 * (a - 4.0).max(0.0)
                    + 4.0 * (a - 6.0).max(0.0)
            };
            let mag_a =
                |a: f32| 4.0 * a - 16.0 + 4.0 * (a - 2.0).min(0.0) + 4.0 * (a - 6.0).max(0.0);
            let mag_b = |a: f32| 4.0 * (a - 4.0).abs() - 8.0;
            l[0] = sgn(x_i) * sign(ai);
            l[1] = sgn(x_q) * sign(aq);
            l[2] = mag_a(ai);
            l[3] = mag_a(aq);
            l[4] = mag_b(ai);
            l[5] = mag_b(aq);
        }
    }
    l
}

/// A cell's soft bits as [`Demapper`] makes them before the noise scaling,
/// in the grid's units (for tests and tools).
pub fn cell_llr(m: Modulation, z: Iq) -> [f32; 6] {
    let u = unit_scale(m);
    let (x, y) = (z.re * u, z.im * u);
    match m {
        Modulation::Qpsk => cell::<2>(x, y),
        Modulation::Qam16 => cell::<4>(x, y),
        Modulation::Qam64 => cell::<6>(x, y),
    }
}

pub struct Demapper {
    modulation: Modulation,
    v: usize,
    cells: usize,
    unit: f32,
    /// For each symbol phase (index in the frame mod 4): the carrier each
    /// cell comes from, in the order the symbol deinterleaver outputs.
    src: [Vec<u16>; 4],
    /// The mean gain |H|² of the data cells over the mean noise power at
    /// their carriers, as a ratio, followed over the symbols.
    snr: f32,
    avx2: bool,
    /// The interleaver (the plane) each place of a cell's v bits comes from.
    order: Vec<usize>,
    /// Scratch: the gathered cells, and a soft-bit array for each bit, and the
    /// block's bit arrays rotated.
    zr: Vec<f32>,
    zi: Vec<f32>,
    gain: Vec<f32>,
    noise: Vec<f32>,
    planes: Vec<i8>,
    rot: Vec<i8>,
    /// The shuffle masks that put three eight-word arrays side by side.
    masks: [[[u8; 16]; 3]; 3],
}

impl Demapper {
    pub fn new(fft: FftSize, modulation: Modulation) -> Demapper {
        let v = modulation.bits();
        let h = symbol_permutation(fft);
        let cells = h.len();
        let mut hinv = vec![0u16; cells];
        for (q, &hq) in h.iter().enumerate() {
            hinv[hq as usize] = q as u16;
        }
        let plan = SymbolPlan::new(fft);
        // Symbol deinterleaving: x[q] = y[H(q)] on even symbols, x[H(q)] =
        // y[q] on odd (so x[p] = y[H⁻¹(p)]), y being the carriers' cells.
        let src = std::array::from_fn(|l| {
            let data = plan.data_carriers(l);
            let perm = if l % 2 == 0 { &h } else { &hinv };
            perm.iter().map(|&q| data[q as usize]).collect()
        });
        // Bit deinterleaving: received bit y_e of cell w belongs to the
        // original cell (w + offset_e) mod 126, at the word position k that
        // the demultiplexer sent to interleaver e; so the output's bit k of
        // cell i is plane e = order[k] at cell (i − offset_e) mod 126.
        let order = demux_order(modulation).to_vec();
        // Three eight-word arrays side by side: output word g (of 24) is word
        // g/3 of array g%3.
        let mut masks = [[[0x80u8; 16]; 3]; 3];
        for (o, row) in masks.iter_mut().enumerate() {
            for j in 0..8 {
                let g = 8 * o + j;
                let (c, comp) = (g / 3, g % 3);
                row[comp][2 * j] = (2 * c) as u8;
                row[comp][2 * j + 1] = (2 * c + 1) as u8;
            }
        }
        Demapper {
            modulation,
            v,
            cells,
            unit: unit_scale(modulation),
            src,
            snr: 0.0,
            avx2: Self::detect(),
            order,
            zr: vec![0.0; cells],
            zi: vec![0.0; cells],
            gain: vec![0.0; cells],
            noise: vec![0.0; cells],
            planes: vec![0; cells * v],
            rot: vec![0; BLOCK * v + 16],
            masks,
        }
    }

    fn detect() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            std::is_x86_feature_detected!("avx2")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }

    /// Use the baseline code even where AVX2 is there (tests, comparison).
    pub fn without_avx2(mut self) -> Demapper {
        self.avx2 = false;
        self
    }

    /// Soft bits a symbol gives.
    pub fn bits_per_symbol(&self) -> usize {
        self.cells * self.v
    }

    /// The signal-to-noise ratio of the data cells, dB: their mean power
    /// (the channel's gain) over the mean noise power at their carriers, as
    /// the pilots measure it. Unlike a figure got from the cells' distances
    /// to their nearest points it does not read low when a bad channel has
    /// many wrong.
    pub fn snr_db(&self) -> f32 {
        10.0 * self.snr.max(1e-6).log10()
    }

    pub fn reset(&mut self) {
        self.snr = 0.0;
    }

    /// Demap symbol `l` (its index in the frame) from the equalised carriers
    /// `z`, their gains `w` (|H|²) and the noise power at each carrier, the
    /// soft coded bits into `out` (cleared and filled): in the order the
    /// encoder made them, positive for 0 and negative for 1.
    pub fn symbol(&mut self, l: usize, z: &[Iq], w: &[f32], noise: &[f32], out: &mut Vec<i8>) {
        out.clear();
        out.resize(self.cells * self.v, 0);
        let (gain, power) = match self.modulation {
            Modulation::Qpsk => self.run::<2>(l, z, w, noise, out),
            Modulation::Qam16 => self.run::<4>(l, z, w, noise, out),
            Modulation::Qam64 => self.run::<6>(l, z, w, noise, out),
        };
        let now = gain / power.max(1e-30);
        self.snr = if self.snr == 0.0 {
            now
        } else {
            self.snr + SNR_RATE * (now - self.snr)
        };
    }

    /// Returns the sums of the cells' gains and of the noise at their
    /// carriers.
    fn run<const V: usize>(
        &mut self,
        l: usize,
        z: &[Iq],
        w: &[f32],
        noise: &[f32],
        out: &mut [i8],
    ) -> (f32, f32) {
        // The cells in the order the symbol deinterleaver puts them.
        let src = &self.src[l % 4];
        for (p, &k) in src.iter().enumerate() {
            let k = k as usize;
            self.zr[p] = z[k].re;
            self.zi[p] = z[k].im;
            self.gain[p] = w[k];
            self.noise[p] = noise[k];
        }
        let sums = (sum8(&self.gain), sum8(&self.noise));
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: AVX2 and FMA were checked; the transposition's loads and
            // stores stay inside the arrays (see there).
            unsafe { self.make_avx2::<V>(out) };
            return sums;
        }
        self.make::<V>(out);
        sums
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn make_avx2<const V: usize>(&mut self, out: &mut [i8]) {
        self.make::<V>(out);
    }

    /// The soft bits from the gathered cells, deinterleaved, into `out`.
    #[inline(always)]
    fn make<const V: usize>(&mut self, out: &mut [i8]) {
        let cells = self.cells;
        // Log-likelihood ratio = (d1² − d0²) / (2 s²) for s² the noise power
        // on an axis of the equalised cell, in grid units: s² = u²·σ²/(2|H|²)
        // (σ² the noise at the carrier in the channel estimate's units).
        // Quantised at GAIN, a bit of the cell at a time over all the cells.
        let (u, scale) = (self.unit, GAIN / (self.unit * self.unit));
        let planes = &mut self.planes;
        for p in 0..cells {
            let s = scale * self.gain[p] / self.noise[p];
            let llr = cell::<V>(self.zr[p] * u, self.zi[p] * u);
            for e in 0..V {
                // Rounded to the nearest (copysign is a bit operation where
                // `round` is a library call); the cast saturates at ±127/−128,
                // which the decoder clips to ±127.
                let v = llr[e] * s;
                planes[e * cells + p] = (v + 0.5f32.copysign(v)) as i8;
            }
        }
        // The bit deinterleaver, a block of 126 cells at a time: each bit's
        // array rotated by its interleaver's offset, then the arrays' bytes
        // put side by side.
        for (b, block) in out.chunks_exact_mut(BLOCK * V).enumerate() {
            let base = b * BLOCK;
            for k in 0..V {
                let e = self.order[k];
                let off = OFFSETS[e];
                let plane = &planes[e * cells + base..e * cells + base + BLOCK];
                let dst = &mut self.rot[k * BLOCK..(k + 1) * BLOCK];
                // dst[i] = plane[(i − off) mod 126].
                dst[off..].copy_from_slice(&plane[..BLOCK - off]);
                dst[..off].copy_from_slice(&plane[BLOCK - off..]);
            }
            interleave::<V>(&self.rot, &self.masks, block);
        }
    }
}

/// The sum of an array, eight partial sums at a time.
#[inline(always)]
fn sum8(x: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let (chunks, tail) = x.as_chunks::<8>();
    for c in chunks {
        for l in 0..8 {
            acc[l] += c[l];
        }
    }
    acc.iter().sum::<f32>() + tail.iter().sum::<f32>()
}

/// `rot` holds V arrays of 126 bytes; `out` gets 126 cells of V bytes: cell i
/// the i-th byte of each array in turn. Eight cells at a time by shuffles
/// (SSSE3, which AVX2 implies), the last six by hand.
#[inline(always)]
fn interleave<const V: usize>(rot: &[i8], masks: &[[[u8; 16]; 3]; 3], out: &mut [i8]) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        if std::is_x86_feature_detected!("ssse3") {
            // SAFETY: SSSE3 detected. Each 8-cell chunk reads 8 bytes from each
            // of V arrays at offset 126·k + i with i + 8 ≤ 120 + 8 ≤ 126, and
            // writes V·8 bytes (padded to 16 for the last register) inside
            // `out`'s 126·V: the last chunk's store is of 16 bytes at
            // V·112, ending at V·112 + 16 ≤ 126·V for V ≥ 2.
            unsafe {
                let ld = |k: usize, i: usize| {
                    _mm_loadl_epi64(rot.as_ptr().add(k * BLOCK + i).cast::<__m128i>())
                };
                let dst = out.as_mut_ptr();
                for i in (0..BLOCK - 6).step_by(8) {
                    match V {
                        2 => {
                            let r = _mm_unpacklo_epi8(ld(0, i), ld(1, i));
                            _mm_storeu_si128(dst.add(2 * i).cast(), r);
                        }
                        4 => {
                            let x = _mm_unpacklo_epi8(ld(0, i), ld(1, i));
                            let y = _mm_unpacklo_epi8(ld(2, i), ld(3, i));
                            _mm_storeu_si128(dst.add(4 * i).cast(), _mm_unpacklo_epi16(x, y));
                            _mm_storeu_si128(dst.add(4 * i + 16).cast(), _mm_unpackhi_epi16(x, y));
                        }
                        _ => {
                            let x = _mm_unpacklo_epi8(ld(0, i), ld(1, i));
                            let y = _mm_unpacklo_epi8(ld(2, i), ld(3, i));
                            let z = _mm_unpacklo_epi8(ld(4, i), ld(5, i));
                            for (o, mask) in masks.iter().enumerate() {
                                let m = |c: usize| _mm_loadu_si128(mask[c].as_ptr().cast());
                                let r = _mm_or_si128(
                                    _mm_or_si128(
                                        _mm_shuffle_epi8(x, m(0)),
                                        _mm_shuffle_epi8(y, m(1)),
                                    ),
                                    _mm_shuffle_epi8(z, m(2)),
                                );
                                _mm_storeu_si128(dst.add(6 * i + 16 * o).cast(), r);
                            }
                        }
                    }
                }
            }
            // The last cells (126 = 15 × 8 + 6).
            for i in 120..BLOCK {
                for k in 0..V {
                    out[i * V + k] = rot[k * BLOCK + i];
                }
            }
            return;
        }
    }
    let _ = masks;
    for i in 0..BLOCK {
        for k in 0..V {
            out[i * V + k] = rot[k * BLOCK + i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::dvbt::{Alpha, Mode, Rate};
    use decdvbt_fec::conv;
    use decdvbt_mod::inner::{InnerEncoder, conv_rate};
    use decdvbt_ofdm::mapping::Constellation;

    const MODS: [Modulation; 3] = [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64];

    #[test]
    fn the_closed_forms_are_the_max_log_soft_bits() {
        // Against the brute force: for each bit, the squared distance to the
        // nearest point with that bit 1, less that with 0.
        for m in MODS {
            let c = Constellation::new(m, Alpha::NonHierarchical);
            let u = unit_scale(m);
            let v = m.bits();
            let mut x = 0x2468_ACE1u32;
            for _ in 0..2000 {
                let mut r = || {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (x >> 8) as f32 / (1 << 24) as f32 * 2.4 - 1.2
                };
                let z = Iq::new(r(), r());
                let got = cell_llr(m, z);
                for (j, &llr) in got.iter().enumerate().take(v) {
                    let (mut d0, mut d1) = (f32::MAX, f32::MAX);
                    for (word, p) in c.points().iter().enumerate() {
                        let d = (z - p).norm_sqr() * u * u;
                        if (word >> (v - 1 - j)) & 1 == 0 {
                            d0 = d0.min(d);
                        } else {
                            d1 = d1.min(d);
                        }
                    }
                    assert!(
                        (llr - (d1 - d0)).abs() < 1e-3 * (1.0 + (d1 - d0).abs()),
                        "{m:?} bit {j} at {z}: {} vs {}",
                        llr,
                        d1 - d0
                    );
                }
            }
        }
    }

    #[test]
    fn a_symbol_demaps_to_the_bits_the_encoder_made() {
        // The encoder's cells for symbol l, put on the data carriers, come
        // back as the coded bits it made them from, whatever the
        // modulation, the symbol's parity and the transform size.
        for fft in [FftSize::K2, FftSize::K8] {
            for m in MODS {
                let mut mode = Mode::uk(m, Rate::R2_3);
                mode.fft = fft;
                let plan = SymbolPlan::new(fft);
                let k_total = fft.active_carriers();
                for l in [0usize, 1, 2, 3, 66, 67] {
                    let mut enc = InnerEncoder::new(mode).unwrap();
                    let data: Vec<u8> = (0..enc.bytes_needed() as u32)
                        .map(|n| (n.wrapping_mul(2_654_435_761) >> 11) as u8)
                        .collect();
                    enc.feed(&data);
                    let cells = enc.symbol_cells(l).to_vec();
                    // The expected coded bits, from a second encoder.
                    let mut ce = conv::Encoder::new(conv_rate(Rate::R2_3));
                    let mut coded = Vec::new();
                    for &b in &data {
                        for i in (0..8).rev() {
                            ce.push((b >> i) & 1, &mut coded);
                        }
                    }
                    let mut z = vec![Iq::new(0.0, 0.0); k_total];
                    for (&k, c) in plan.data_carriers(l).iter().zip(&cells) {
                        z[k as usize] = *c;
                    }
                    let w = vec![1.0f32; k_total];
                    let noise = vec![0.05f32; k_total];
                    let mut d = Demapper::new(fft, m);
                    let mut soft = Vec::new();
                    d.symbol(l, &z, &w, &noise, &mut soft);
                    assert_eq!(soft.len(), coded.len());
                    let wrong = soft
                        .iter()
                        .zip(&coded)
                        .filter(|(s, b)| (**s < 0) != (**b == 1))
                        .count();
                    assert_eq!(wrong, 0, "{fft:?} {m:?} symbol {l}");
                    // And none of them is a coin toss.
                    assert!(soft.iter().all(|&s| s != 0), "{fft:?} {m:?} l {l}");
                }
            }
        }
    }

    #[test]
    fn a_noisier_carrier_gives_softer_bits_and_the_snr_follows_the_pilots() {
        // The same cells with the noise at half the carriers raised 10 dB:
        // their soft bits are 10 dB (3x) weaker, the others' unchanged.
        let fft = FftSize::K2;
        let m = Modulation::Qam16;
        let mut mode = Mode::uk(m, Rate::R3_4);
        mode.fft = fft;
        let plan = SymbolPlan::new(fft);
        let mut enc = InnerEncoder::new(mode).unwrap();
        let data: Vec<u8> = (0..enc.bytes_needed() as u32)
            .map(|n| (n * 7) as u8)
            .collect();
        enc.feed(&data);
        let cells = enc.symbol_cells(0).to_vec();
        let k_total = fft.active_carriers();
        let mut z = vec![Iq::new(0.0, 0.0); k_total];
        // Off the constellation points a little, so no soft bit is at its
        // clip.
        for (&k, c) in plan.data_carriers(0).iter().zip(&cells) {
            z[k as usize] = *c * 0.93 + Iq::new(0.02, -0.01);
        }
        let w = vec![1.0f32; k_total];
        let quiet = vec![0.1f32; k_total];
        let mut loud = quiet.clone();
        for v in loud.iter_mut().take(k_total / 2) {
            *v = 1.0;
        }
        let mut d = Demapper::new(fft, m);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        d.symbol(0, &z, &w, &quiet, &mut a);
        assert!((d.snr_db() - 10.0).abs() < 0.01, "{}", d.snr_db());
        d.symbol(0, &z, &w, &loud, &mut b);
        // The sum of the soft bits' magnitudes over the symbol falls by a
        // factor between 1 and 10 (a bit under 5.5 for half the cells 10x
        // weaker, less where they clip).
        let sum = |v: &[i8]| v.iter().map(|&x| i32::from(x).abs()).sum::<i32>() as f32;
        let ratio = sum(&a) / sum(&b);
        assert!((1.5..9.0).contains(&ratio), "{ratio}");
    }
}
