// made by CasualArclamp
//! The inner chain (EN 300 744 §4.3.3–4.3.5): the punctured convolutional
//! code, the bit interleaver, the symbol interleaver and the mapper, from
//! bytes to the data cells of one OFDM symbol at a time.
//!
//! Non-hierarchical only: the hierarchical modes need two streams through
//! two outer chains and a different demultiplexer.
//!
//! Speed (the transmitter has no spare CPU): the code is run a byte at a time
//! from tables instead of a bit at a time, the coded bits are packed, and the
//! interleavers work on whole arrays of bytes. The output is bit-for-bit the
//! old bit-at-a-time encoder's (kept as `reference` in the tests, and every
//! mode is compared against it).
//!
//! The convolutional code is linear over GF(2), so what a byte makes depends on
//! the byte and on the previous byte separately: `coded = A[byte] ^ E[prev]`,
//! two 256-entry tables per phase. A phase is the byte's place in the
//! puncturing cycle (8 bits against a period of 1, 2, 3, 5 or 7 input bits:
//! 1, 1, 3, 5 or 7 bytes to come round again). The code and its puncturing
//! carry on from symbol to symbol (a symbol holds a whole number of periods),
//! so the coded stream is one continuous bit string, cut into symbols.

use decdvbt_core::dvbt::{Alpha, Mode, Rate};
use decdvbt_fec::conv;
use decdvbt_ofdm::interleave::{BLOCK, symbol_permutation};
use decdvbt_ofdm::mapping::Constellation;
use num_complex::Complex32;

/// The code generators (§4.3.3): G1 = 171 octal (output X), G2 = 133 octal
/// (output Y) over the 7-bit register, the newest bit in bit 6.
const G1: u8 = 0o171;
const G2: u8 = 0o133;

/// The code rate in the FEC crate's terms.
pub fn conv_rate(r: Rate) -> conv::Rate {
    r.into()
}

/// What a byte of input turns into at each phase of the puncturing cycle.
struct ConvTables {
    /// Bytes in the cycle.
    cycle: usize,
    /// Coded bits a byte makes at each phase.
    count: Vec<u32>,
    /// `a[phase][byte]`: the coded bits of the byte from a zero register, the
    /// first sent in the top place of the `count` bits.
    a: Vec<[u16; 256]>,
    /// `e[phase][previous byte]`: what the previous byte's bits left in the
    /// register add to this byte's coded bits.
    e: Vec<[u16; 256]>,
}

fn parity(x: u8) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The coded bits of `byte` (first bit first), run through the register from
/// `reg`, its first bit being bit `start` of a puncturing period.
fn encode_byte(rate: conv::Rate, start: usize, mut reg: u8, byte: u8) -> (u16, u32, u8) {
    let (mut out, mut n) = (0u16, 0u32);
    for t in 0..8 {
        let bit = (byte >> (7 - t)) & 1;
        reg = (reg >> 1) | (bit << 6);
        let (x, y) = (parity(reg & G1), parity(reg & G2));
        let i = (start + t) % rate.period();
        // The pattern lists the kept bits in sending order, by input bit.
        for &(ii, is_x) in rate.pattern() {
            if ii == i {
                out = (out << 1) | u16::from(if is_x { x } else { y });
                n += 1;
            }
        }
    }
    (out, n, reg)
}

impl ConvTables {
    fn new(rate: conv::Rate) -> ConvTables {
        let p = rate.period();
        let cycle = p / gcd(8, p);
        let (mut count, mut a, mut e) = (Vec::new(), Vec::new(), Vec::new());
        for phase in 0..cycle {
            let start = (phase * 8) % p;
            let mut ta = [0u16; 256];
            let mut te = [0u16; 256];
            for b in 0..=255u8 {
                ta[b as usize] = encode_byte(rate, start, 0, b).0;
                // The register after `b` (a zero byte follows): all 7 bits
                // of it come from `b`.
                let reg = encode_byte(rate, 0, 0, b).2;
                te[b as usize] = encode_byte(rate, start, reg, 0).0;
            }
            count.push(encode_byte(rate, start, 0, 0).1);
            a.push(ta);
            e.push(te);
        }
        ConvTables { cycle, count, a, e }
    }
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The bit interleaver's offsets H_e(w) = (w + offset) mod 126, e = 0 … 5
/// (§4.3.4.1), and which of the cell word's bits feeds each interleaver: the
/// word y0…y(v−1) is demultiplexed to b0, b2, b4, b1, b3, b5 (64-QAM), b0, b2,
/// b1, b3 (16-QAM), b0, b1 (QPSK).
const OFFSETS: [usize; 6] = [0, 63, 105, 42, 21, 84];

pub struct InnerEncoder {
    mode: Mode,
    tables: ConvTables,
    constellation: Constellation,
    /// The symbol interleaver's H(q).
    h: Vec<u16>,
    /// Bits per cell and cells (data carriers) per symbol.
    v: usize,
    n_cells: usize,
    /// Coded bytes in a symbol.
    symbol_bytes: usize,
    /// The coded stream, whole bytes: `coded[coded_pos..]` is waiting.
    coded: Vec<u8>,
    coded_pos: usize,
    /// Coded bits not yet a byte (fewer than 8, in the low bits of `acc`).
    acc: u64,
    nbits: u32,
    /// Where the next input byte is in the puncturing cycle, and the byte
    /// before it (the register's contents).
    phase: usize,
    prev: u8,
    /// Per bit interleaver: the cell word's bit that feeds it, the place its
    /// output takes in the word, and its offset.
    mux: [(u32, u32, usize); 6],
    // Scratch, kept between symbols so nothing is allocated per symbol.
    words: Vec<u8>,
    interleaved: Vec<u8>,
    cells: Vec<Complex32>,
}

impl InnerEncoder {
    pub fn new(mode: Mode) -> Result<InnerEncoder, String> {
        if mode.hierarchical() {
            return Err("hierarchical modulation is not supported by the modulator yet".into());
        }
        let v = mode.modulation.bits();
        let n_cells = mode.fft.data_carriers();
        assert!(n_cells.is_multiple_of(BLOCK));
        let order = decdvbt_ofdm::interleave::demux_order(mode.modulation);
        let mut mux = [(0, 0, 0); 6];
        for e in 0..v {
            let k = order
                .iter()
                .position(|&x| x == e)
                .expect("e is in the order");
            mux[e] = ((v - 1 - k) as u32, (v - 1 - e) as u32, OFFSETS[e]);
        }
        let rate = conv_rate(mode.rate_hp);
        let coded_bits = n_cells * v;
        assert!(coded_bits.is_multiple_of(8) && coded_bits.is_multiple_of(rate.coded()));
        Ok(InnerEncoder {
            mode,
            tables: ConvTables::new(rate),
            constellation: Constellation::new(mode.modulation, Alpha::NonHierarchical),
            h: symbol_permutation(mode.fft),
            v,
            n_cells,
            symbol_bytes: coded_bits / 8,
            coded: Vec::new(),
            coded_pos: 0,
            acc: 0,
            nbits: 0,
            phase: 0,
            prev: 0,
            mux,
            words: vec![0; n_cells],
            interleaved: vec![0; n_cells],
            cells: vec![Complex32::new(0.0, 0.0); n_cells],
        })
    }

    /// Bytes still needed before another symbol can be made: the input bytes
    /// whose coded bits make up what the symbol lacks (a lower bound; the
    /// punctured phases give a few bits more or less per byte, so ask again).
    pub fn bytes_needed(&self) -> usize {
        let have = (self.coded.len() - self.coded_pos) * 8 + self.nbits as usize;
        let missing = (self.symbol_bytes * 8).saturating_sub(have);
        if missing == 0 {
            return 0;
        }
        // Code rate k/n: n coded bits come from k input bits.
        let r = conv_rate(self.mode.rate_hp);
        (missing * r.period() / r.coded()).div_ceil(8).max(1)
    }

    /// Code `bytes` (whole input bytes, in order).
    pub fn feed(&mut self, bytes: &[u8]) {
        // Drop what symbols have used, once there is a lot of it.
        if self.coded_pos > (1 << 16) {
            self.coded.drain(..self.coded_pos);
            self.coded_pos = 0;
        }
        self.coded.reserve(bytes.len() * 2 + 8);
        let t = &self.tables;
        let (mut acc, mut nbits) = (self.acc, self.nbits);
        let (mut phase, mut prev) = (self.phase, self.prev);
        for &byte in bytes {
            let n = t.count[phase];
            let bits = t.a[phase][byte as usize] ^ t.e[phase][prev as usize];
            prev = byte;
            phase += 1;
            if phase == t.cycle {
                phase = 0;
            }
            // New bits go in at the bottom; whole bytes leave from the top.
            // Up to 7 left over and 16 new bits never overflow 64 bits.
            acc = (acc << n) | u64::from(bits);
            nbits += n;
            while nbits >= 8 {
                nbits -= 8;
                self.coded.push((acc >> nbits) as u8);
            }
        }
        self.acc = acc & ((1u64 << nbits) - 1);
        self.nbits = nbits;
        self.phase = phase;
        self.prev = prev;
    }

    /// The data cells of symbol `l` of the frame, in carrier order:
    /// convolutionally coded, bit-interleaved, symbol-interleaved and mapped.
    /// Needs [`InnerEncoder::bytes_needed`] to be 0.
    pub fn symbol_cells(&mut self, l: usize) -> &[Complex32] {
        let src = &self.coded[self.coded_pos..self.coded_pos + self.symbol_bytes];
        self.coded_pos += self.symbol_bytes;

        // v coded bits make a cell's word, the first the most significant.
        // `as_chunks` gives fixed-size arrays (and no remainder: the symbol is
        // a whole number of each), so the loops below have no bounds checks.
        match self.v {
            2 => {
                let (dst, _) = self.words.as_chunks_mut::<4>();
                for (d, &b) in dst.iter_mut().zip(src) {
                    *d = [b >> 6, (b >> 4) & 3, (b >> 2) & 3, b & 3];
                }
            }
            4 => {
                let (dst, _) = self.words.as_chunks_mut::<2>();
                for (d, &b) in dst.iter_mut().zip(src) {
                    *d = [b >> 4, b & 15];
                }
            }
            _ => {
                let (dst, _) = self.words.as_chunks_mut::<4>();
                let (triples, _) = src.as_chunks::<3>();
                for (d, s) in dst.iter_mut().zip(triples) {
                    *d = [
                        s[0] >> 2,
                        ((s[0] & 3) << 4) | (s[1] >> 4),
                        ((s[1] & 15) << 2) | (s[2] >> 6),
                        s[2] & 63,
                    ];
                }
            }
        }

        // The bit interleavers (§4.3.4.1): interleaver e takes bit k of every
        // word in a block of 126 and sends it back in bit position e, read
        // from `offset` words on, wrapping round the block. Eight bytes at a
        // time: the shift brings the wanted bit of each byte to its lane's
        // bit 0, the mask drops what came in from the next lane.
        const ONES: u64 = 0x0101_0101_0101_0101;
        let mut dbl = [0u8; 2 * BLOCK + 8];
        let mut out = [0u8; BLOCK + 2];
        let (src_blocks, _) = self.words.as_chunks::<BLOCK>();
        let (dst_blocks, _) = self.interleaved.as_chunks_mut::<BLOCK>();
        for (src, dst) in src_blocks.iter().zip(dst_blocks) {
            dbl[..BLOCK].copy_from_slice(src);
            dbl[BLOCK..2 * BLOCK].copy_from_slice(src);
            out.fill(0);
            for &(from, to, off) in &self.mux[..self.v] {
                let (rot, _) = dbl[off..off + BLOCK + 2].as_chunks::<8>();
                let (acc, _) = out.as_chunks_mut::<8>();
                for (o, r) in acc.iter_mut().zip(rot) {
                    let x = u64::from_le_bytes(*r);
                    let y = u64::from_le_bytes(*o);
                    *o = (y | (((x >> from) & ONES) << to)).to_le_bytes();
                }
            }
            dst.copy_from_slice(&out[..BLOCK]);
        }

        // Map, and permute the cells over the symbol's carriers (§4.3.4.2):
        // even symbols y[H(q)] = x[q], odd y[q] = x[H(q)].
        let mut pt = [Complex32::new(0.0, 0.0); 64];
        pt[..self.constellation.points().len()].copy_from_slice(self.constellation.points());
        let (il, h, cells) = (&self.interleaved, &self.h, &mut self.cells);
        if l.is_multiple_of(2) {
            for (&w, &hq) in il.iter().zip(h) {
                cells[hq as usize] = pt[(w & 63) as usize];
            }
        } else {
            for (c, &hq) in cells.iter_mut().zip(h) {
                *c = pt[(il[hq as usize] & 63) as usize];
            }
        }
        debug_assert_eq!(self.cells.len(), self.n_cells);
        &self.cells
    }
}

#[cfg(test)]
mod reference {
    //! The bit-at-a-time encoder this one replaced, as the standard for the
    //! new one's output.

    use std::collections::VecDeque;

    use decdvbt_core::dvbt::Mode;
    use decdvbt_ofdm::interleave::{BLOCK, bit_interleave, symbol_interleave, symbol_permutation};
    use decdvbt_ofdm::mapping::Constellation;
    use num_complex::Complex32;

    use super::*;

    pub struct RefInner {
        mode: Mode,
        enc: conv::Encoder,
        constellation: Constellation,
        h: Vec<u16>,
        bytes: VecDeque<u8>,
        bit_pos: u32,
        coded: Vec<u8>,
        words: Vec<u8>,
        interleaved: Vec<u8>,
        cells: Vec<Complex32>,
    }

    impl RefInner {
        pub fn new(mode: Mode) -> RefInner {
            RefInner {
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
            }
        }

        fn coded_bits(&self) -> usize {
            self.mode.fft.data_carriers() * self.mode.modulation.bits()
        }

        pub fn info_bits(&self) -> usize {
            let r = conv_rate(self.mode.rate_hp);
            (self.coded_bits() / r.coded()) * r.period()
        }

        pub fn bytes_needed(&self) -> usize {
            let have_bits = self.bytes.len() * 8 - self.bit_pos as usize;
            self.info_bits().saturating_sub(have_bits).div_ceil(8)
        }

        pub fn feed(&mut self, bytes: &[u8]) {
            self.bytes.extend(bytes);
        }

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
            self.words.clear();
            self.words.extend(
                self.coded
                    .chunks_exact(v)
                    .map(|c| c.iter().fold(0u8, |w, &b| (w << 1) | b)),
            );
            self.interleaved.resize(self.words.len(), 0);
            let (src_blocks, _) = self.words.as_chunks::<BLOCK>();
            let (dst_blocks, _) = self.interleaved.as_chunks_mut::<BLOCK>();
            for (src, dst) in src_blocks.iter().zip(dst_blocks) {
                bit_interleave(self.mode.modulation, src, dst);
            }
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
}

#[cfg(test)]
mod tests {
    use super::reference::RefInner;
    use super::*;
    use decdvbt_core::dvbt::{FftSize, Modulation};

    #[test]
    fn every_mode_takes_a_whole_number_of_puncturing_periods() {
        // The 126-cell blocks and the puncturing periods fit the symbol for
        // every combination.
        for fft in [FftSize::K2, FftSize::K8] {
            for m in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                for r in Rate::ALL {
                    let mut mode = Mode::uk(m, r);
                    mode.fft = fft;
                    let e = InnerEncoder::new(mode).unwrap();
                    assert_eq!(e.n_cells % BLOCK, 0);
                    assert_eq!((e.n_cells * e.v) % conv_rate(r).coded(), 0);
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

    /// The new encoder against the old one: every constellation, code rate and
    /// FFT size, several symbols (so the register, the puncturing phase and the
    /// symbol parity all carry on), fed in packet-sized pieces.
    #[test]
    fn bit_exact_with_the_bit_at_a_time_encoder_in_every_mode() {
        let mut seed = 0x0123_4567_89AB_CDEFu64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        };
        for fft in [FftSize::K2, FftSize::K8] {
            for m in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                for r in Rate::ALL {
                    let mut mode = Mode::uk(m, r);
                    mode.fft = fft;
                    let mut new = InnerEncoder::new(mode).unwrap();
                    let mut old = RefInner::new(mode);
                    let symbols = if fft == FftSize::K8 { 5 } else { 9 };
                    for l in 0..symbols {
                        while old.bytes_needed() > 0 || new.bytes_needed() > 0 {
                            let chunk: Vec<u8> = (0..204).map(|_| next()).collect();
                            // Both are fed the same bytes, as many as either
                            // wants; each keeps what it does not use yet.
                            old.feed(&chunk);
                            new.feed(&chunk);
                        }
                        let a = old.symbol_cells(l).to_vec();
                        let b = new.symbol_cells(l).to_vec();
                        assert_eq!(a, b, "{fft:?} {m:?} {r:?} symbol {l}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_byte_tables_match_the_reference_encoder() {
        for r in Rate::ALL {
            let rate = conv_rate(r);
            let t = ConvTables::new(rate);
            let mut enc = conv::Encoder::new(rate);
            let mut reference = Vec::new();
            let mut ours = Vec::new();
            let data: Vec<u8> = (0..70u32).map(|n| (n * 37 + 11) as u8).collect();
            let (mut phase, mut prev) = (0, 0u8);
            for &b in &data {
                for i in (0..8).rev() {
                    enc.push((b >> i) & 1, &mut reference);
                }
                let bits = t.a[phase][b as usize] ^ t.e[phase][prev as usize];
                for i in (0..t.count[phase]).rev() {
                    ours.push(((bits >> i) & 1) as u8);
                }
                phase = (phase + 1) % t.cycle;
                prev = b;
            }
            // The reference emits whole periods only.
            assert_eq!(&ours[..reference.len()], &reference[..], "{r:?}");
        }
    }
}
