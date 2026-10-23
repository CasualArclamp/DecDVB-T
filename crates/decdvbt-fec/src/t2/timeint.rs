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
        CellInterleaver {
            cell_size,
            degree,
            perm,
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
        for (w, &v) in input.iter().enumerate() {
            out[(self.perm[w] as usize + shift) % self.cell_size] = v;
        }
    }

    /// Undone.
    pub fn deinterleave<T: Copy>(&self, shift: usize, input: &[T], out: &mut [T]) {
        for (w, o) in out.iter_mut().enumerate() {
            *o = input[(self.perm[w] as usize + shift) % self.cell_size];
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
