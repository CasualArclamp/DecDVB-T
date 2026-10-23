// made by CasualArclamp
//! The cell interleaver and the time interleaver (EN 302 755 §6.3, §6.4) of a
//! PLP whose interleaving frame is one T2 frame (TIME_IL_TYPE 0): each FEC
//! block's cells are permuted pseudo-randomly with a cyclic shift of its own,
//! and the blocks of a TI block are written into a table of five columns per
//! block and read in rows.
//!
//! As in gr-dtv's `dvbt2_cellinterleaver_cc_impl.cc` (GPL-3.0-or-later,
//! Copyright 2015-2019 Free Software Foundation, Inc.).

use decdvbt_core::FecFrame;

use super::qam::Qam;

/// The cell interleaver's permutation of the cells of a FEC block, and the
/// shifts the blocks of a TI block take in turn.
pub struct CellInterleaver {
    pub cell_size: usize,
    degree: u32,
    perm: Vec<u16>,
    /// For each position in a FEC block's slice of the time interleaver's
    /// table: which of the table's 5 columns (of the block) and which row.
    column_row: Vec<(u8, u16)>,
    /// The permutation's inverse: cell w of the interleaved block is
    /// `inverse[w]` of the original.
    inverse: Vec<u16>,
}

fn config(frame: FecFrame, qam: Qam) -> (usize, u32, u32, &'static [u32]) {
    // (cells in a FEC block, register degree, mask, taps)
    const T15: [u32; 4] = [0, 1, 2, 12];
    const T14: [u32; 6] = [0, 1, 4, 5, 9, 11];
    const T13: [u32; 4] = [0, 1, 4, 6];
    const T12: [u32; 2] = [0, 2];
    const T11: [u32; 2] = [0, 3];
    match (frame, qam) {
        (FecFrame::Normal, Qam::Qpsk) => (32400, 15, 0x3fff, &T15),
        (FecFrame::Normal, Qam::Qam16) => (16200, 14, 0x1fff, &T14),
        (FecFrame::Normal, Qam::Qam64) => (10800, 14, 0x1fff, &T14),
        (FecFrame::Normal, Qam::Qam256) => (8100, 13, 0xfff, &T13),
        (_, Qam::Qpsk) => (8100, 13, 0xfff, &T13),
        (_, Qam::Qam16) => (4050, 12, 0x7ff, &T12),
        (_, Qam::Qam64) => (2700, 12, 0x7ff, &T12),
        (_, Qam::Qam256) => (2025, 11, 0x3ff, &T11),
    }
}

impl CellInterleaver {
    pub fn new(frame: FecFrame, qam: Qam) -> CellInterleaver {
        let (cell_size, degree, mask, taps) = config(frame, qam);
        let states = 1usize << degree;
        let mut perm = Vec::with_capacity(cell_size);
        let mut lfsr: u32 = 0;
        for i in 0..states {
            if i < 2 {
                lfsr = 0;
            } else if i == 2 {
                lfsr = 1;
            } else {
                let result = taps.iter().fold(0, |a, &t| a ^ ((lfsr >> t) & 1));
                lfsr &= mask;
                lfsr >>= 1;
                lfsr |= result << (degree - 2);
            }
            lfsr |= ((i % 2) as u32) << (degree - 1);
            if (lfsr as usize) < cell_size {
                perm.push(lfsr as u16);
            }
        }
        debug_assert_eq!(perm.len(), cell_size);
        let rows = cell_size / 5;
        let column_row = (0..cell_size)
            .map(|at| ((at / rows) as u8, (at % rows) as u16))
            .collect();
        let mut inverse = vec![0u16; cell_size];
        for (w, &p) in perm.iter().enumerate() {
            inverse[p as usize] = w as u16;
        }
        CellInterleaver {
            cell_size,
            degree,
            perm,
            column_row,
            inverse,
        }
    }

    /// The cyclic shift of the `r`-th block of a TI block: the bit-reversed
    /// counter, shifted up one, skipping the counters whose value is not
    /// under the block size.
    pub fn shift(&self, r: usize) -> usize {
        let mut n = 0usize;
        let mut accepted = 0;
        loop {
            let mut temp = n;
            let mut shift = 0usize;
            for _ in 0..self.degree {
                shift |= temp & 1;
                shift <<= 1;
                temp >>= 1;
            }
            n += 1;
            if shift < self.cell_size {
                if accepted == r {
                    return shift;
                }
                accepted += 1;
            }
        }
    }

    /// All the shifts for `blocks` blocks (cheaper than asking one by one).
    pub fn shifts(&self, blocks: usize) -> Vec<usize> {
        let mut out = Vec::with_capacity(blocks);
        let mut n = 0usize;
        while out.len() < blocks {
            let mut temp = n;
            let mut shift = 0usize;
            for _ in 0..self.degree {
                shift |= temp & 1;
                shift <<= 1;
                temp >>= 1;
            }
            n += 1;
            if shift < self.cell_size {
                out.push(shift);
            }
        }
        out
    }

    /// One FEC block interleaved: input cell w goes to
    /// (perm[w] + shift) mod cell_size.
    pub fn interleave<T: Copy>(&self, shift: usize, input: &[T], out: &mut [T]) {
        let cs = self.cell_size;
        for (&p, &v) in self.perm.iter().zip(input) {
            // (perm + shift) mod cs, as a subtraction: both are under cs.
            let mut at = p as usize + shift;
            if at >= cs {
                at -= cs;
            }
            out[at] = v;
        }
    }

    /// One FEC block straight from the cells of the TI block it is the `r`-th
    /// of (`f` blocks, as received): the time interleaver's rows and the cell
    /// interleaver's permutation undone in one pass, to the same cells
    /// [`deinterleave_frame`] makes. `columns` says whether the time
    /// interleaver is on (else the blocks are as sent, only cell-interleaved).
    pub fn gather_block<T: Copy>(
        &self,
        rx: &[T],
        f: usize,
        r: usize,
        shift: usize,
        columns: bool,
        out: &mut [T],
    ) {
        let cs = self.cell_size;
        if !columns {
            let base = &rx[r * cs..(r + 1) * cs];
            for (o, &p) in out.iter_mut().zip(&self.perm) {
                let mut at = p as usize + shift;
                if at >= cs {
                    at -= cs;
                }
                *o = base[at];
            }
            return;
        }
        // The table's position `r·cs + at` was read from row `at mod rows`
        // of the received matrix (`5f` cells wide) and column `5r + at / rows`.
        let cols = 5 * f;
        for (o, &p) in out.iter_mut().zip(&self.perm) {
            let mut at = p as usize + shift;
            if at >= cs {
                at -= cs;
            }
            let (c, k) = self.column_row[at];
            *o = rx[k as usize * cols + 5 * r + c as usize];
        }
    }

    /// [`Self::gather_block`] for two arrays at once (the cells and their noise
    /// variances), written the other way round: each received cell is read
    /// in turn, in the order it lies in the time interleaver's rows, and put
    /// where the cell interleaver says it goes, into a block that fits in the
    /// cache. Reading at random from the 13 MB of a frame's cells (a cache
    /// and TLB miss each) cost three times as much.
    #[allow(clippy::too_many_arguments)]
    pub fn deinterleave_block<A: Copy, B: Copy>(
        &self,
        rx_a: &[A],
        rx_b: &[B],
        f: usize,
        r: usize,
        shift: usize,
        columns: bool,
        out_a: &mut [A],
        out_b: &mut [B],
    ) {
        let cs = self.cell_size;
        // The block's cell `at` of the table goes to cell w with
        // perm[w] + shift ≡ at.
        let place = |at: usize| {
            let from = if at >= shift {
                at - shift
            } else {
                at + cs - shift
            };
            self.inverse[from] as usize
        };
        if !columns {
            for at in 0..cs {
                let w = place(at);
                out_a[w] = rx_a[r * cs + at];
                out_b[w] = rx_b[r * cs + at];
            }
            return;
        }
        let (cols, rows) = (5 * f, cs / 5);
        for k in 0..rows {
            let row = k * cols + 5 * r;
            for c in 0..5 {
                let w = place(c * rows + k);
                out_a[w] = rx_a[row + c];
                out_b[w] = rx_b[row + c];
            }
        }
    }

    /// Undone.
    pub fn deinterleave<T: Copy>(&self, shift: usize, input: &[T], out: &mut [T]) {
        let cs = self.cell_size;
        for (o, &p) in out.iter_mut().zip(&self.perm) {
            let mut at = p as usize + shift;
            if at >= cs {
                at -= cs;
            }
            *o = input[at];
        }
    }
}

/// The FEC blocks in each of a frame's TI blocks: `blocks` over `ti_blocks`,
/// the larger ones after the smaller (gr-dtv's order) or before.
pub fn ti_block_sizes(blocks: usize, ti_blocks: usize, big_first: bool) -> Vec<usize> {
    if ti_blocks == 0 {
        return vec![1; blocks];
    }
    let (small, big) = (blocks / ti_blocks, blocks.div_ceil(ti_blocks));
    let n_big = blocks % ti_blocks;
    let n_small = ti_blocks - n_big;
    let mut v = Vec::new();
    if big_first {
        v.extend(std::iter::repeat_n(big, n_big));
        v.extend(std::iter::repeat_n(small, n_small));
    } else {
        v.extend(std::iter::repeat_n(small, n_small));
        v.extend(std::iter::repeat_n(big, n_big));
    }
    v
}

/// Interleave a frame's PLP cells: `input` is `blocks` FEC blocks of
/// `cell_size` cells; each TI block (of `sizes[s]` blocks) has its blocks
/// cell-interleaved and the result read in rows of 5 × blocks columns.
pub fn interleave_frame<T: Copy + Default>(
    ci: &CellInterleaver,
    sizes: &[usize],
    ti_columns: bool,
    input: &[T],
) -> Vec<T> {
    let cs = ci.cell_size;
    let mut out = Vec::with_capacity(input.len());
    let mut at = 0;
    for &f in sizes {
        let shifts = ci.shifts(f);
        let mut table = vec![T::default(); f * cs];
        for (r, &sh) in shifts.iter().enumerate() {
            ci.interleave(
                sh,
                &input[at + r * cs..at + (r + 1) * cs],
                &mut table[r * cs..(r + 1) * cs],
            );
        }
        if ti_columns {
            let (cols, rows) = (5 * f, cs / 5);
            for k in 0..rows {
                for w in 0..cols {
                    out.push(table[rows * w + k]);
                }
            }
        } else {
            out.extend_from_slice(&table);
        }
        at += f * cs;
    }
    out
}

/// The way back: a frame's cells (in transmitted order) to FEC blocks of
/// cells in the mapper's order.
pub fn deinterleave_frame<T: Copy + Default>(
    ci: &CellInterleaver,
    sizes: &[usize],
    ti_columns: bool,
    input: &[T],
) -> Vec<T> {
    let cs = ci.cell_size;
    let mut out = vec![T::default(); input.len()];
    let mut at = 0;
    for &f in sizes {
        let shifts = ci.shifts(f);
        let rx = &input[at..at + f * cs];
        let mut table = vec![T::default(); f * cs];
        if ti_columns {
            let (cols, rows) = (5 * f, cs / 5);
            for k in 0..rows {
                for w in 0..cols {
                    table[rows * w + k] = rx[k * cols + w];
                }
            }
        } else {
            table.copy_from_slice(rx);
        }
        for (r, &sh) in shifts.iter().enumerate() {
            ci.deinterleave(
                sh,
                &table[r * cs..(r + 1) * cs],
                &mut out[at + r * cs..at + (r + 1) * cs],
            );
        }
        at += f * cs;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block gathered straight from its TI block's cells is the block the
    /// frame's deinterleaver makes.
    #[test]
    fn blocks_gathered_one_at_a_time_match_the_frame_deinterleaver() {
        for (blocks, ti, columns) in [
            (9usize, 3usize, true),
            (7, 2, true),
            (5, 0, false),
            (4, 4, true),
        ] {
            for (frame, qam) in [
                (FecFrame::Normal, Qam::Qam256),
                (FecFrame::Short, Qam::Qam16),
            ] {
                let ci = CellInterleaver::new(frame, qam);
                let cs = ci.cell_size;
                let input: Vec<u32> = (0..(blocks * cs) as u32)
                    .map(|i| i.wrapping_mul(2654435761))
                    .collect();
                let sizes = ti_block_sizes(blocks, ti, false);
                let want = deinterleave_frame(&ci, &sizes, columns, &input);
                let mut at = 0;
                let mut b = 0;
                for &f in &sizes {
                    let shifts = ci.shifts(f);
                    let rx = &input[at..at + f * cs];
                    for (r, &sh) in shifts.iter().enumerate() {
                        let mut got = vec![0u32; cs];
                        ci.gather_block(rx, f, r, sh, columns, &mut got);
                        assert_eq!(
                            got,
                            want[b * cs..(b + 1) * cs],
                            "{frame:?} {qam:?} block {b}"
                        );
                        let (mut a, mut z) = (vec![0u32; cs], vec![0u8; cs]);
                        let low: Vec<u8> = rx.iter().map(|v| *v as u8).collect();
                        ci.deinterleave_block(rx, &low, f, r, sh, columns, &mut a, &mut z);
                        assert_eq!(a, got, "{frame:?} {qam:?} block {b}: scattered");
                        assert!(a.iter().zip(&z).all(|(v, l)| *v as u8 == *l));
                        b += 1;
                    }
                    at += f * cs;
                }
            }
        }
    }

    #[test]
    fn every_permutation_is_a_permutation_of_the_block() {
        for frame in [FecFrame::Normal, FecFrame::Short] {
            for qam in Qam::ALL {
                let ci = CellInterleaver::new(frame, qam);
                let mut seen = vec![false; ci.cell_size];
                for &p in &ci.perm {
                    assert!(!seen[p as usize], "{frame:?} {qam:?}: {p} twice");
                    seen[p as usize] = true;
                }
            }
        }
    }

    #[test]
    fn the_first_shifts_are_the_bit_reversed_counter() {
        let ci = CellInterleaver::new(FecFrame::Normal, Qam::Qam256);
        // n = 0 gives 0; n = 1 gives bit 0 at position 13, which is past the
        // block (8192 > 8100): skipped; n = 2 gives 4096.
        assert_eq!(ci.shifts(3)[..2], [0, 4096]);
        assert_eq!(ci.shift(0), 0);
        assert_eq!(ci.shift(1), 4096);
        assert_eq!(ci.shift(2), ci.shifts(3)[2]);
    }

    #[test]
    fn the_blocks_split_as_the_standard_says() {
        assert_eq!(ti_block_sizes(202, 3, false), vec![67, 67, 68]);
        assert_eq!(ti_block_sizes(202, 3, true), vec![68, 67, 67]);
        assert_eq!(ti_block_sizes(6, 3, false), vec![2, 2, 2]);
        assert_eq!(ti_block_sizes(4, 0, false), vec![1; 4]);
    }

    #[test]
    fn a_frame_interleaves_and_deinterleaves() {
        for frame in [FecFrame::Normal, FecFrame::Short] {
            let ci = CellInterleaver::new(frame, Qam::Qam256);
            let sizes = ti_block_sizes(7, 3, false);
            let n = 7 * ci.cell_size;
            let input: Vec<u32> = (0..n as u32).collect();
            for columns in [true, false] {
                let tx = interleave_frame(&ci, &sizes, columns, &input);
                assert_eq!(tx.len(), n);
                assert_ne!(tx, input);
                let rx = deinterleave_frame(&ci, &sizes, columns, &tx);
                assert_eq!(rx, input, "{frame:?} columns {columns}");
            }
        }
    }
}
