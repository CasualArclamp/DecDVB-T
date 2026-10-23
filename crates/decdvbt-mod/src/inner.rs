// made by CasualArclamp
//! The inner chain (EN 300 744 §4.3.3–4.3.5): the punctured convolutional
//! code, the bit interleaver, the symbol interleaver and the mapper, from
//! bytes to the data cells of one OFDM symbol at a time.
//!
//! Non-hierarchical only: the hierarchical modes need two streams through
//! two outer chains and a different demultiplexer.

use std::collections::VecDeque;

use decdvbt_core::dvbt::{Alpha, Mode, Rate};
use decdvbt_fec::conv;
use decdvbt_ofdm::interleave::{BLOCK, bit_interleave, symbol_interleave, symbol_permutation};
use decdvbt_ofdm::mapping::Constellation;
use num_complex::Complex32;

/// The code rate in the FEC crate's terms.
pub fn conv_rate(r: Rate) -> conv::Rate {
    r.into()
}

pub struct InnerEncoder {
    mode: Mode,
    enc: conv::Encoder,
    constellation: Constellation,
    h: Vec<u16>,
    /// Bytes waiting to be coded, and the bits of the first already used.
    bytes: VecDeque<u8>,
    bit_pos: u32,
    // Scratch, kept between symbols so nothing is allocated per symbol.
    coded: Vec<u8>,
    words: Vec<u8>,
    interleaved: Vec<u8>,
    cells: Vec<Complex32>,
}

impl InnerEncoder {
    pub fn new(mode: Mode) -> Result<InnerEncoder, String> {
        if mode.hierarchical() {
            return Err("hierarchical modulation is not supported by the modulator yet".into());
        }
        Ok(InnerEncoder {
            mode,
            enc: conv::Encoder::new(conv_rate(mode.rate_hp)),
            constellation: Constellation::new(mode.modulation, Alpha::NonHierarchical),
            h: symbol_permutation(mode.fft),
            bytes: VecDeque::new(),
            bit_pos: 0,
            coded: Vec::new(),
            words: Vec::new(),
            interleaved: Vec::new(),
            cells: Vec::new(),
        })
    }

    /// Coded bits one OFDM symbol takes.
    fn coded_bits(&self) -> usize {
        self.mode.fft.data_carriers() * self.mode.modulation.bits()
    }

    /// Information bits those are, at the code rate: a whole number of
    /// puncturing periods (the coded count divides by the coded bits of one).
    fn info_bits(&self) -> usize {
        let r = conv_rate(self.mode.rate_hp);
        let periods = self.coded_bits() / r.coded();
        debug_assert_eq!(periods * r.coded(), self.coded_bits());
        periods * r.period()
    }

    /// Bytes still needed before another symbol can be made.
    pub fn bytes_needed(&self) -> usize {
        let have_bits = self.bytes.len() * 8 - self.bit_pos as usize;
        self.info_bits().saturating_sub(have_bits).div_ceil(8)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
    }

    /// The data cells of symbol `l` of the frame, in carrier order:
    /// convolutionally coded, bit-interleaved, symbol-interleaved and mapped.
    /// Needs [`InnerEncoder::bytes_needed`] to be 0.
    pub fn symbol_cells(&mut self, l: usize) -> &[Complex32] {
        let v = self.mode.modulation.bits();
        self.coded.clear();
        for _ in 0..self.info_bits() {
            let byte = self.bytes[0];
            let bit = (byte >> (7 - self.bit_pos)) & 1;
            self.enc.push(bit, &mut self.coded);
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bit_pos = 0;
                self.bytes.pop_front();
            }
        }
        debug_assert_eq!(self.coded.len(), self.coded_bits());

        // v coded bits make a cell's word, the first the most significant.
        self.words.clear();
        self.words.extend(
            self.coded
                .chunks_exact(v)
                .map(|c| c.iter().fold(0u8, |w, &b| (w << 1) | b)),
        );
        self.interleaved.resize(self.words.len(), 0);
        // `as_chunks` gives whole `[u8; BLOCK]` blocks (the length is a
        // multiple of BLOCK, so nothing is left over).
        let (src_blocks, _) = self.words.as_chunks::<BLOCK>();
        let (dst_blocks, _) = self.interleaved.as_chunks_mut::<BLOCK>();
        for (src, dst) in src_blocks.iter().zip(dst_blocks) {
            bit_interleave(self.mode.modulation, src, dst);
        }

        // Map, then permute the cells over the symbol's carriers.
        let mapped: Vec<Complex32> = self
            .interleaved
            .iter()
            .map(|&w| self.constellation.point(w))
            .collect();
        self.cells.resize(mapped.len(), Complex32::new(0.0, 0.0));
        symbol_interleave(&self.h, l, &mapped, &mut self.cells);
        &self.cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::dvbt::{FftSize, Modulation};

    #[test]
    fn every_mode_takes_a_whole_number_of_puncturing_periods() {
        // The 126-cell blocks and the puncturing periods fit the symbol for
        // every combination; the debug assertions in `info_bits` would trip.
        for fft in [FftSize::K2, FftSize::K8] {
            for m in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                for r in Rate::ALL {
                    let mut mode = Mode::uk(m, r);
                    mode.fft = fft;
                    let e = InnerEncoder::new(mode).unwrap();
                    assert_eq!(e.coded_bits() % BLOCK / BLOCK, 0);
                    assert_eq!((fft.data_carriers()) % BLOCK, 0);
                    let _ = e.info_bits();
                }
            }
        }
    }

    #[test]
    fn a_symbol_needs_the_bytes_the_code_rate_says() {
        // 8k, 64-QAM 2/3: 6048 × 6 × 2/3 = 24192 bits = 3024 bytes.
        let mut e = InnerEncoder::new(Mode::uk(Modulation::Qam64, Rate::R2_3)).unwrap();
        assert_eq!(e.bytes_needed(), 3024);
        e.feed(&vec![0u8; 3024]);
        assert_eq!(e.bytes_needed(), 0);
        assert_eq!(e.symbol_cells(0).len(), 6048);
        assert_eq!(e.bytes_needed(), 3024);
    }

    #[test]
    fn cells_have_unit_power_on_average() {
        let mut e = InnerEncoder::new(Mode::uk(Modulation::Qam16, Rate::R3_4)).unwrap();
        let data: Vec<u8> = (0..20_000u32)
            .map(|n| (n.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        e.feed(&data);
        let cells = e.symbol_cells(1).to_vec();
        let p = cells.iter().map(|c| c.norm_sqr()).sum::<f32>() / cells.len() as f32;
        assert!((p - 1.0).abs() < 0.05, "{p}");
    }

    #[test]
    fn hierarchical_modes_are_refused_for_now() {
        let mut m = Mode::uk(Modulation::Qam64, Rate::R1_2);
        m.alpha = Alpha::A2;
        assert!(InnerEncoder::new(m).is_err());
    }
}
