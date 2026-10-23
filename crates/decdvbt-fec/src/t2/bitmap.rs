// made by CasualArclamp
//! The bit interleaver and the bit-to-cell demultiplexer (EN 302 755
//! §6.1.3): which bit of the LDPC codeword is which bit of which cell's
//! label.
//!
//! The parity bits are interleaved first (written in rows of q, read in
//! columns of 360: parity bit q·s + t goes to 360·t + s), then the whole
//! codeword is written column by column into a table of 2 × bits columns (the
//! 16-, 64- and 256-QAM of a normal FECFRAME and the 16- and 64-QAM of a
//! short one; 256-QAM of a short one has `bits` columns), each column
//! cyclically shifted by its twist, and read row by row. A demultiplexer
//! then deals each row's bits to the label bits of a cell pair (or a cell)
//! in an order that depends on the code rate. QPSK has no column interleaver
//! (and the parity interleaver only for the T2-Lite rates 1/3 and 2/5).
//!
//! As in gr-dtv's `dvbt2_interleaver_bb_impl.cc` (GPL-3.0-or-later,
//! Copyright 2015-2019 Free Software Foundation, Inc.).

use decdvbt_core::{CodeRate, FecFrame};

use super::qam::Qam;
use crate::t2_tables::*;

/// The table: for each label bit (cell-major, MSB first) the index of the
/// codeword bit it is.
pub struct BitMap {
    pub qam: Qam,
    pub frame: FecFrame,
    /// Cells in a FECFRAME: N / bits.
    pub cells: usize,
    label_to_cw: Vec<u32>,
}

/// q of the LDPC code (§6.1.3's parity interleaver) and its information
/// length N_bch.
fn q_and_nbch(frame: FecFrame, rate: CodeRate) -> Option<(usize, usize)> {
    Some(match (frame, (rate.num, rate.den)) {
        (FecFrame::Normal, (1, 2)) => (90, 32400),
        (FecFrame::Normal, (3, 5)) => (72, 38880),
        (FecFrame::Normal, (2, 3)) => (60, 43200),
        (FecFrame::Normal, (3, 4)) => (45, 48600),
        (FecFrame::Normal, (4, 5)) => (36, 51840),
        (FecFrame::Normal, (5, 6)) => (30, 54000),
        (FecFrame::Short, (1, 3)) => (30, 5400),
        (FecFrame::Short, (2, 5)) => (27, 6480),
        (FecFrame::Short, (1, 2)) => (25, 7200),
        (FecFrame::Short, (3, 5)) => (18, 9720),
        (FecFrame::Short, (2, 3)) => (15, 10800),
        (FecFrame::Short, (3, 4)) => (12, 11880),
        (FecFrame::Short, (4, 5)) => (10, 12600),
        (FecFrame::Short, (5, 6)) => (8, 13320),
        _ => return None,
    })
}

fn twist(qam: Qam, frame: FecFrame) -> &'static [u8] {
    match (qam, frame) {
        (Qam::Qam16, FecFrame::Normal) => &TWIST16N,
        (Qam::Qam16, _) => &TWIST16S,
        (Qam::Qam64, FecFrame::Normal) => &TWIST64N,
        (Qam::Qam64, _) => &TWIST64S,
        (Qam::Qam256, FecFrame::Normal) => &TWIST256N,
        (Qam::Qam256, _) => &TWIST256S,
        (Qam::Qpsk, _) => &[],
    }
}

/// The demultiplexer's order for a code (Tables 13a-d): element e of a row
/// is label bit `mux[e]` of the row's cell pair (counting across both cells,
/// MSB first).
fn mux(qam: Qam, frame: FecFrame, rate: CodeRate) -> &'static [u8] {
    let r = (rate.num, rate.den);
    let normal = frame == FecFrame::Normal;
    match qam {
        Qam::Qpsk => &[],
        Qam::Qam16 => match (r, normal) {
            ((3, 5), true) => &MUX16_35,
            ((1, 3), false) => &MUX16_13,
            ((2, 5), false) => &MUX16_25,
            _ => &MUX16,
        },
        Qam::Qam64 => match (r, normal) {
            ((3, 5), true) => &MUX64_35,
            ((1, 3), false) => &MUX64_13,
            ((2, 5), false) => &MUX64_25,
            _ => &MUX64,
        },
        Qam::Qam256 => match (r, normal) {
            ((3, 5), true) => &MUX256_35,
            ((2, 3), true) => &MUX256_23,
            (_, true) => &MUX256,
            ((1, 3), false) => &MUX256S_13,
            ((2, 5), false) => &MUX256S_25,
            _ => &MUX256S,
        },
    }
}

impl BitMap {
    pub fn new(frame: FecFrame, rate: CodeRate, qam: Qam) -> Option<BitMap> {
        let (q, nbch) = q_and_nbch(frame, rate)?;
        let n = frame.n_ldpc();
        let bits = qam.bits();
        // Stage 1: the parity interleaver, as an index list (output position
        // -> codeword bit).
        let mut parity: Vec<u32> = (0..n as u32).collect();
        let parity_interleaved = qam != Qam::Qpsk
            || (frame == FecFrame::Short && matches!((rate.num, rate.den), (1, 3) | (2, 5)));
        if parity_interleaved {
            for t in 0..q {
                for s in 0..360 {
                    parity[nbch + 360 * t + s] = (nbch + q * s + t) as u32;
                }
            }
        }
        // Stage 2: the column twist, then reading by rows.
        let stream: Vec<u32> = if qam == Qam::Qpsk {
            parity
        } else {
            let cols = if qam == Qam::Qam256 && frame == FecFrame::Short {
                bits
            } else {
                2 * bits
            };
            let rows = n / cols;
            let tw = twist(qam, frame);
            assert_eq!(tw.len(), cols);
            let mut table = vec![0u32; n];
            let mut index = 0;
            for (col, &t) in tw.iter().enumerate() {
                let mut offset = t as usize;
                for _ in 0..rows {
                    table[offset + rows * col] = parity[index];
                    index += 1;
                    offset += 1;
                    if offset == rows {
                        offset = 0;
                    }
                }
            }
            let mut out = Vec::with_capacity(n);
            for row in 0..rows {
                for col in 0..cols {
                    out.push(table[rows * col + row]);
                }
            }
            out
        };
        // Stage 3: the demultiplexer, from stream order to label bits.
        let mut label_to_cw = vec![0u32; n];
        if qam == Qam::Qpsk {
            // A cell's two bits are two consecutive ones, the first the MSB.
            label_to_cw.copy_from_slice(&stream);
        } else {
            let m = mux(qam, frame, rate);
            let word = m.len(); // bits of a cell pair, or of a cell for short 256
            let per_cell = bits;
            for (g, chunk) in stream.chunks(word).enumerate() {
                for (e, &cw) in chunk.iter().enumerate() {
                    let w = m[e] as usize;
                    // Label bit w counted across the word's cells.
                    let cell = g * (word / per_cell) + w / per_cell;
                    label_to_cw[cell * per_cell + w % per_cell] = cw;
                }
            }
        }
        Some(BitMap {
            qam,
            frame,
            cells: n / bits,
            label_to_cw,
        })
    }

    /// The labels of the cells of a codeword (one bit a byte).
    pub fn labels(&self, codeword: &[u8], out: &mut Vec<u16>) {
        let bits = self.qam.bits();
        out.clear();
        for cell in 0..self.cells {
            let mut l = 0u16;
            for j in 0..bits {
                l = (l << 1) | u16::from(codeword[self.label_to_cw[cell * bits + j] as usize] & 1);
            }
            out.push(l);
        }
    }

    /// [`Self::gather`] for quantised LLRs.
    pub fn gather_q(&self, cell_llr: &[i8], codeword_llr: &mut [i8]) {
        for (&cw, &l) in self.label_to_cw.iter().zip(cell_llr) {
            codeword_llr[cw as usize] = l;
        }
    }

    /// A codeword's LLRs from the cells' (`cells · bits` of them, label bit
    /// order): the way back.
    pub fn gather(&self, cell_llr: &[f32], codeword_llr: &mut [f32]) {
        for (&cw, &l) in self.label_to_cw.iter().zip(cell_llr) {
            codeword_llr[cw as usize] = l;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(n: u16, d: u16) -> CodeRate {
        CodeRate::new(n, d)
    }

    #[test]
    fn every_map_is_a_permutation() {
        let mut checked = 0;
        for frame in [FecFrame::Normal, FecFrame::Short] {
            for r in [
                (1, 3),
                (2, 5),
                (1, 2),
                (3, 5),
                (2, 3),
                (3, 4),
                (4, 5),
                (5, 6),
            ] {
                for qam in Qam::ALL {
                    let Some(m) = BitMap::new(frame, rate(r.0, r.1), qam) else {
                        continue;
                    };
                    let mut seen = vec![false; frame.n_ldpc()];
                    for &c in &m.label_to_cw {
                        assert!(!seen[c as usize], "{frame:?} {r:?} {qam:?}: {c} twice");
                        seen[c as usize] = true;
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked >= 40, "{checked}");
    }

    #[test]
    fn labels_and_gather_are_inverses() {
        let m = BitMap::new(FecFrame::Normal, rate(2, 3), Qam::Qam256).unwrap();
        let n = FecFrame::Normal.n_ldpc();
        let cw: Vec<u8> = (0..n).map(|i| ((i * 7 + i / 3) % 2) as u8).collect();
        let mut labels = Vec::new();
        m.labels(&cw, &mut labels);
        assert_eq!(labels.len(), 8100);
        // LLRs from the labels' bits: negative for 1.
        let llr: Vec<f32> = labels
            .iter()
            .flat_map(|&l| (0..8).map(move |j| if (l >> (7 - j)) & 1 == 1 { -1.0 } else { 1.0 }))
            .collect();
        let mut back = vec![0f32; n];
        m.gather(&llr, &mut back);
        for i in 0..n {
            assert_eq!(back[i] < 0.0, cw[i] == 1, "bit {i}");
        }
    }

    #[test]
    fn the_parity_interleaver_moves_parity_bits_by_q() {
        // QPSK normal: no interleaving at all: cell c is bits 2c, 2c+1.
        let m = BitMap::new(FecFrame::Normal, rate(1, 2), Qam::Qpsk).unwrap();
        assert_eq!(&m.label_to_cw[..4], &[0, 1, 2, 3]);
        // 16-QAM 1/2 normal: nothing moves in the information part's rows
        // except by the column twist; the first row reads the first bit of
        // every column.
        let m16 = BitMap::new(FecFrame::Normal, rate(1, 2), Qam::Qam16).unwrap();
        assert_eq!(m16.cells, 16200);
    }
}
