// made by CasualArclamp
//! The coding of the L1 signalling (EN 302 755 §7.3): L1-pre (200 bits with
//! its CRC) is BCH and LDPC coded as a short rate-1/4 frame, shortened and
//! punctured to 1840 BPSK cells; L1-post (a size the L1-pre gives) as a short
//! rate-1/2 frame, shortened by padding whole groups of 360 information bits
//! in a fixed order, punctured, bit-interleaved (16- and 64-QAM) and mapped to
//! BPSK, QPSK, 16- or 64-QAM cells.
//!
//! The constructions and tables are gr-dtv's `dvbt2_framemapper_cc_impl.cc`
//! (GPL-3.0-or-later, Copyright 2015-2019 Free Software Foundation, Inc.).
//! Bits travel one to a byte (0 or 1); LLRs are positive for 0.

use decdvbt_core::{CodeRate, FecFrame, Iq};

use crate::bch::Bch;
use crate::ldpc::{LdpcCode, LdpcDecoder};
use crate::params::FecParams;
use crate::t2_tables::*;

/// L1-pre's bits with its CRC-32, and its cells.
pub const PRE_BITS: usize = 200;
pub const PRE_CELLS: usize = 1840;
const KBCH_1_4: usize = 3072;
const NBCH_1_4: usize = 3240;
const KBCH_1_2: usize = 7032;
const NBCH_1_2: usize = 7200;
const NBCH_PARITY: usize = 168;
const SHORT: usize = 16200;
/// LLR of a bit known to be 0.
const KNOWN: f32 = 100.0;
/// LLRs to the LDPC decoder: steps per unit.
const SCALE: f32 = 4.0;
const LDPC_ITERATIONS: usize = 50;

/// CRC-32 (g = 0x04C11DB7, MSB first, register all ones, no final inversion)
/// over bits.
pub fn crc32_bits(bits: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bits {
        let fb = u32::from(b & 1) ^ (crc >> 31);
        crc <<= 1;
        if fb == 1 {
            crc ^= 0x04C1_1DB7;
        }
    }
    crc
}

/// `bits` and their CRC-32 after them.
pub fn with_crc(bits: &[u8]) -> Vec<u8> {
    let crc = crc32_bits(bits);
    let mut v = bits.to_vec();
    v.extend((0..32).rev().map(|n| ((crc >> n) & 1) as u8));
    v
}

/// Whether the last 32 bits are the CRC of those before.
pub fn crc_ok(bits_with_crc: &[u8]) -> bool {
    if bits_with_crc.len() < 32 {
        return false;
    }
    let (data, tail) = bits_with_crc.split_at(bits_with_crc.len() - 32);
    let crc = crc32_bits(data);
    tail.iter()
        .enumerate()
        .all(|(i, &b)| b == ((crc >> (31 - i)) & 1) as u8)
}

/// The constellation of L1-post (L1_MOD).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum L1Mod {
    Bpsk,
    Qpsk,
    Qam16,
    Qam64,
}

impl L1Mod {
    pub const fn from_code(c: u8) -> Option<L1Mod> {
        Some(match c {
            0 => L1Mod::Bpsk,
            1 => L1Mod::Qpsk,
            2 => L1Mod::Qam16,
            3 => L1Mod::Qam64,
            _ => return None,
        })
    }

    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Bits a cell carries.
    pub const fn eta(self) -> usize {
        match self {
            L1Mod::Bpsk => 1,
            L1Mod::Qpsk => 2,
            L1Mod::Qam16 => 4,
            L1Mod::Qam64 => 6,
        }
    }

    fn padding(self) -> &'static [u8; 20] {
        match self {
            L1Mod::Bpsk | L1Mod::Qpsk => &L1_POST_PADDING_BQPSK,
            L1Mod::Qam16 => &L1_POST_PADDING_16QAM,
            L1Mod::Qam64 => &L1_POST_PADDING_64QAM,
        }
    }

    fn puncture(self) -> &'static [u8; 25] {
        match self {
            L1Mod::Bpsk | L1Mod::Qpsk => &L1_POST_PUNCTURE_BQPSK,
            L1Mod::Qam16 => &L1_POST_PUNCTURE_16QAM,
            L1Mod::Qam64 => &L1_POST_PUNCTURE_64QAM,
        }
    }
}

/// What L1-post's size decides (§7.3.3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostLayout {
    /// Information bits with the CRC.
    pub k_sig: usize,
    /// Transmitted bits, and cells.
    pub n_post: usize,
    pub cells: usize,
    /// Parity bits punctured.
    pub n_punc: usize,
    pub l1_mod: L1Mod,
}

impl PostLayout {
    /// For `k_sig` bits (L1_POST_INFO_SIZE plus the CRC) in a frame with
    /// `n_p2` P2 symbols.
    pub fn new(k_sig: usize, l1_mod: L1Mod, n_p2: usize) -> Option<PostLayout> {
        if k_sig == 0 || k_sig > KBCH_1_2 {
            return None;
        }
        let eta = l1_mod.eta();
        let n_punc_temp = 6 * (KBCH_1_2 - k_sig) / 5;
        let n_post_temp = k_sig + NBCH_PARITY + 9000 - n_punc_temp;
        let unit = if n_p2 == 1 { 2 * eta } else { eta * n_p2 };
        let n_post = n_post_temp.div_ceil(unit) * unit;
        if n_post - n_post_temp > n_punc_temp {
            return None;
        }
        Some(PostLayout {
            k_sig,
            n_post,
            cells: n_post / eta,
            n_punc: n_punc_temp - (n_post - n_post_temp),
            l1_mod,
        })
    }

    /// The information bits' places among the 7032 of the BCH message: all but
    /// the padded ones (zero-padded groups, in the order of Table 31).
    fn info_positions(&self) -> Vec<usize> {
        let post_padding = self.l1_mod.padding();
        let mut pad = vec![false; KBCH_1_2];
        let (m, last) = if self.k_sig <= 360 {
            (19, 360 - self.k_sig)
        } else {
            let m = (KBCH_1_2 - self.k_sig) / 360;
            (m, KBCH_1_2 - self.k_sig - 360 * m)
        };
        for &g in &post_padding[..m] {
            let g = g as usize;
            let len = if g == 19 { 192 } else { 360 };
            for p in pad.iter_mut().skip(g * 360).take(len) {
                *p = true;
            }
        }
        let g = post_padding[m] as usize;
        let start = if g == 19 {
            g * 360 + 192 - last
        } else {
            g * 360 + 360 - last
        };
        for p in pad.iter_mut().skip(start).take(last) {
            *p = true;
        }
        (0..KBCH_1_2).filter(|&i| !pad[i]).collect()
    }

    /// Whether parity bit `p` of the LDPC code (0 to 8999) is punctured.
    fn punctured(&self) -> Vec<bool> {
        let order = self.l1_mod.puncture();
        let mut v = vec![false; SHORT - NBCH_1_2];
        let full = self.n_punc / 360;
        for &g in &order[..full] {
            for c2 in 0..360 {
                v[c2 * 25 + g as usize] = true;
            }
        }
        if full < 25 {
            let g = order[full] as usize;
            for c2 in 0..(self.n_punc - full * 360) {
                v[c2 * 25 + g] = true;
            }
        }
        v
    }

    /// The scrambling sequence of L1-post (§7.3.3.3).
    fn scrambler(len: usize) -> Vec<u8> {
        let mut sr: u32 = 0x4a80;
        (0..len)
            .map(|_| {
                let b = (sr ^ (sr >> 1)) & 1;
                sr >>= 1;
                if b == 1 {
                    sr |= 0x4000;
                }
                b as u8
            })
            .collect()
    }
}

/// The BCH and LDPC codes of L1 and the decoders' memory.
pub struct L1Coder {
    pre_bch: Bch,
    post_bch: Bch,
    pre: LdpcDecoder,
    post: LdpcDecoder,
}

impl Default for L1Coder {
    fn default() -> Self {
        L1Coder::new()
    }
}

fn pack(bits: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        v[i / 8] |= (b & 1) << (7 - i % 8);
    }
    v
}

fn unpack(bytes: &[u8], n: usize) -> Vec<u8> {
    (0..n).map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1).collect()
}

impl L1Coder {
    pub fn new() -> L1Coder {
        let short = |r: (u16, u16)| {
            FecParams::new(FecFrame::Short, CodeRate::new(r.0, r.1))
                .expect("a short rate 1/4 and 1/2 code")
        };
        L1Coder {
            pre_bch: Bch::new(FecFrame::Short, 12, NBCH_1_4),
            post_bch: Bch::new(FecFrame::Short, 12, NBCH_1_2),
            pre: LdpcDecoder::new(LdpcCode::new(short((1, 4)).ldpc_table())),
            post: LdpcDecoder::new(LdpcCode::new(short((1, 2)).ldpc_table())),
        }
    }

    /// The LDPC parity (bits) of a BCH codeword (bits).
    fn ldpc_parity(dec: &LdpcDecoder, bch_cw: &[u8]) -> Vec<u8> {
        let code = dec.code();
        let mut parity = vec![0u8; (code.n - code.k) / 8];
        code.encode(&pack(bch_cw), &mut parity);
        unpack(&parity, code.n - code.k)
    }

    /// L1-pre's 1840 transmitted bits from its information bits (168 of
    /// them: the CRC is added here).
    pub fn encode_pre(&self, info: &[u8]) -> Vec<u8> {
        let mut m = with_crc(info);
        assert_eq!(m.len(), PRE_BITS, "L1-pre is 168 bits and a CRC");
        m.resize(KBCH_1_4, 0);
        let mut bytes = pack(&m);
        bytes.resize(NBCH_1_4 / 8, 0);
        self.pre_bch.encode(&mut bytes);
        let cw = unpack(&bytes, NBCH_1_4);
        let parity = Self::ldpc_parity(&self.pre, &cw);
        let punctured = pre_punctured();
        let mut out = Vec::with_capacity(PRE_CELLS);
        out.extend_from_slice(&cw[..PRE_BITS]);
        out.extend_from_slice(&cw[KBCH_1_4..]);
        out.extend(
            parity
                .iter()
                .zip(&punctured)
                .filter(|(_, p)| !**p)
                .map(|(b, _)| *b),
        );
        debug_assert_eq!(out.len(), PRE_CELLS);
        out
    }

    /// L1-pre's information bits (without the CRC) from the LLRs of its 1840
    /// cells; `None` if the CRC fails.
    pub fn decode_pre(&mut self, llr: &[f32]) -> Option<Vec<u8>> {
        if llr.len() != PRE_CELLS {
            return None;
        }
        let mut full = vec![0f32; SHORT];
        full[..PRE_BITS].copy_from_slice(&llr[..PRE_BITS]);
        for v in &mut full[PRE_BITS..KBCH_1_4] {
            *v = KNOWN;
        }
        full[KBCH_1_4..NBCH_1_4].copy_from_slice(&llr[PRE_BITS..PRE_BITS + NBCH_PARITY]);
        let punctured = pre_punctured();
        let mut it = llr[PRE_BITS + NBCH_PARITY..].iter();
        for (p, &pu) in punctured.iter().enumerate() {
            if !pu {
                full[NBCH_1_4 + p] = *it.next()?;
            }
        }
        let bits = Self::decode_frame(&mut self.pre, &self.pre_bch, &full, NBCH_1_4)?;
        crc_ok(&bits[..PRE_BITS]).then(|| bits[..PRE_BITS - 32].to_vec())
    }

    /// LDPC then BCH decoding of a short frame's LLRs: the corrected BCH
    /// codeword's bits.
    fn decode_frame(dec: &mut LdpcDecoder, bch: &Bch, llr: &[f32], k: usize) -> Option<Vec<u8>> {
        let q: Vec<i8> = llr
            .iter()
            .map(|&l| (l * SCALE).round().clamp(-127.0, 127.0) as i8)
            .collect();
        let mut info = vec![0u8; k / 8];
        dec.decode(&q, &mut info, LDPC_ITERATIONS);
        bch.decode(&mut info).ok()?;
        Some(unpack(&info, k))
    }

    /// L1-post's transmitted bits (N_post of them, before mapping) from its
    /// information bits (k_sig − 32 of them; the CRC is added here).
    pub fn encode_post(&self, info: &[u8], layout: &PostLayout, scrambled: bool) -> Vec<u8> {
        let mut m = with_crc(info);
        assert_eq!(m.len(), layout.k_sig);
        if scrambled {
            let s = PostLayout::scrambler(m.len());
            for (b, x) in m.iter_mut().zip(s) {
                *b ^= x;
            }
        }
        let positions = layout.info_positions();
        let mut padded = vec![0u8; KBCH_1_2];
        for (&b, &p) in m.iter().zip(&positions) {
            padded[p] = b;
        }
        let mut bytes = pack(&padded);
        bytes.resize(NBCH_1_2 / 8, 0);
        self.post_bch.encode(&mut bytes);
        let cw = unpack(&bytes, NBCH_1_2);
        let parity = Self::ldpc_parity(&self.post, &cw);
        let punctured = layout.punctured();
        let mut out = Vec::with_capacity(layout.n_post);
        out.extend(positions.iter().map(|&p| cw[p]));
        out.extend_from_slice(&cw[KBCH_1_2..]);
        out.extend(
            parity
                .iter()
                .zip(&punctured)
                .filter(|(_, p)| !**p)
                .map(|(b, _)| *b),
        );
        debug_assert_eq!(out.len(), layout.n_post);
        out
    }

    /// L1-post's information bits (k_sig − 32 of them) from the LLRs of its
    /// N_post transmitted bits; `None` if the CRC fails.
    pub fn decode_post(
        &mut self,
        llr: &[f32],
        layout: &PostLayout,
        scrambled: bool,
    ) -> Option<Vec<u8>> {
        if llr.len() != layout.n_post {
            return None;
        }
        let positions = layout.info_positions();
        let mut full = vec![0f32; SHORT];
        for v in &mut full[..KBCH_1_2] {
            *v = KNOWN;
        }
        for (i, &p) in positions.iter().enumerate() {
            full[p] = llr[i];
        }
        let k = positions.len();
        full[KBCH_1_2..NBCH_1_2].copy_from_slice(&llr[k..k + NBCH_PARITY]);
        let punctured = layout.punctured();
        let mut it = llr[k + NBCH_PARITY..].iter();
        for (p, &pu) in punctured.iter().enumerate() {
            if !pu {
                full[NBCH_1_2 + p] = *it.next()?;
            }
        }
        let bits = Self::decode_frame(&mut self.post, &self.post_bch, &full, NBCH_1_2)?;
        let mut m: Vec<u8> = positions.iter().map(|&p| bits[p]).collect();
        if scrambled {
            let s = PostLayout::scrambler(m.len());
            for (b, x) in m.iter_mut().zip(s) {
                *b ^= x;
            }
        }
        crc_ok(&m).then(|| m[..m.len() - 32].to_vec())
    }
}

/// The punctured parity bits of L1-pre: 31 groups of 360 and 328 of the 32nd,
/// in the order of Table 29.
fn pre_punctured() -> Vec<bool> {
    let mut v = vec![false; SHORT - NBCH_1_4];
    for (c, &g) in L1_PRE_PUNCTURE.iter().enumerate().take(32) {
        let n = if c < 31 { 360 } else { 328 };
        for c2 in 0..n {
            v[c2 * 36 + g as usize] = true;
        }
    }
    v
}

// ---- mapping L1-post's bits to cells and back ---------------------------------

/// The cells of L1-post from its transmitted bits: block interleaved for 16-
/// and 64-QAM, the bits of a cell pair dealt out by Table 32's order, mapped
/// with the PLP constellations (BPSK: ±1).
pub fn post_cells(bits: &[u8], l1_mod: L1Mod, out: &mut Vec<Iq>) {
    out.clear();
    let stream = post_interleave(bits, l1_mod);
    let cst = |q| crate::t2::qam::Constellation::new(q, false);
    match l1_mod {
        L1Mod::Bpsk => out.extend(
            stream
                .iter()
                .map(|&b| Iq::new(1.0 - 2.0 * f32::from(b), 0.0)),
        ),
        L1Mod::Qpsk => {
            let c = cst(crate::t2::qam::Qam::Qpsk);
            out.extend(
                stream
                    .chunks(2)
                    .map(|p| c.point(usize::from(p[0] << 1 | p[1]))),
            );
        }
        L1Mod::Qam16 | L1Mod::Qam64 => {
            let (q, mux): (_, &[u8]) = if l1_mod == L1Mod::Qam16 {
                (crate::t2::qam::Qam::Qam16, &L1_MUX16)
            } else {
                (crate::t2::qam::Qam::Qam64, &L1_MUX64)
            };
            let c = cst(q);
            let word = 2 * q.bits();
            for g in stream.chunks(word) {
                let mut pack = 0usize;
                for e in 0..word {
                    pack = (pack << 1) | usize::from(g[mux[e] as usize]);
                }
                out.push(c.point(pack >> q.bits()));
                out.push(c.point(pack & ((1 << q.bits()) - 1)));
            }
        }
    }
}

/// L1-post's transmitted-bit LLRs from its cells.
pub fn post_llrs(cells: &[Iq], var: &[f32], l1_mod: L1Mod, out: &mut Vec<f32>) {
    use crate::t2::qam::{Constellation, Qam};
    let mut stream: Vec<f32> = Vec::new();
    match l1_mod {
        L1Mod::Bpsk => {
            for (c, v) in cells.iter().zip(var) {
                // BPSK: (y+1)² − (y−1)² = 4y over the axis's noise variance
                // (the other axis's half, the complex variance, is ours).
                stream.push(4.0 * c.re / v.max(1e-9));
            }
        }
        L1Mod::Qpsk => {
            let c = Constellation::new(Qam::Qpsk, false);
            let mut l = [0f32; 2];
            for (y, v) in cells.iter().zip(var) {
                c.demap_axes(*y, v.max(1e-9), &mut l);
                stream.extend_from_slice(&l);
            }
        }
        L1Mod::Qam16 | L1Mod::Qam64 => {
            let (q, mux): (_, &[u8]) = if l1_mod == L1Mod::Qam16 {
                (Qam::Qam16, &L1_MUX16)
            } else {
                (Qam::Qam64, &L1_MUX64)
            };
            let c = Constellation::new(q, false);
            let b = q.bits();
            let mut l = vec![0f32; b];
            for (pair, vs) in cells.chunks(2).zip(var.chunks(2)) {
                // The pair's word bits MSB first: cell A's then cell B's.
                let mut word = vec![0f32; 2 * b];
                for (k, (y, v)) in pair.iter().zip(vs).enumerate() {
                    c.demap_axes(*y, v.max(1e-9), &mut l);
                    word[k * b..(k + 1) * b].copy_from_slice(&l);
                }
                // Word bit e is the stream's bit mux[e] of the group.
                let mut g = vec![0f32; 2 * b];
                for e in 0..2 * b {
                    g[mux[e] as usize] = word[e];
                }
                stream.extend_from_slice(&g);
            }
        }
    }
    out.clear();
    out.extend(post_deinterleave(&stream, l1_mod));
}

/// The block interleaver of 16- and 64-QAM L1-post: the bits are written in
/// columns (8 or 12 of them) and read in rows.
fn post_interleave(bits: &[u8], l1_mod: L1Mod) -> Vec<u8> {
    let cols = match l1_mod {
        L1Mod::Qam16 => 8,
        L1Mod::Qam64 => 12,
        _ => return bits.to_vec(),
    };
    let rows = bits.len() / cols;
    let mut out = Vec::with_capacity(bits.len());
    for k in 0..rows {
        for w in 0..cols {
            out.push(bits[rows * w + k]);
        }
    }
    out
}

fn post_deinterleave<T: Copy + Default>(stream: &[T], l1_mod: L1Mod) -> Vec<T> {
    let cols = match l1_mod {
        L1Mod::Qam16 => 8,
        L1Mod::Qam64 => 12,
        _ => return stream.to_vec(),
    };
    let rows = stream.len() / cols;
    let mut out = vec![T::default(); stream.len()];
    for k in 0..rows {
        for w in 0..cols {
            out[rows * w + k] = stream[k * cols + w];
        }
    }
    out
}

/// L1-pre's cells from its transmitted bits: BPSK.
pub fn pre_cells(bits: &[u8]) -> Vec<Iq> {
    bits.iter()
        .map(|&b| Iq::new(1.0 - 2.0 * f32::from(b), 0.0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits_of(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 31) as u8
            })
            .collect()
    }

    /// A BPSK cell's LLR as the decoder wants it (positive for 0) at a
    /// strong signal.
    fn hard(bits: &[u8]) -> Vec<f32> {
        bits.iter()
            .map(|&b| if b == 0 { 8.0 } else { -8.0 })
            .collect()
    }

    #[test]
    fn the_crc_matches_the_mpeg_one() {
        let bytes = [0x12u8, 0x34, 0x56, 0x78];
        let bits: Vec<u8> = bytes
            .iter()
            .flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1))
            .collect();
        assert_eq!(crc32_bits(&bits), decdvbt_core::crc::crc32_mpeg2(&bytes));
        assert!(crc_ok(&with_crc(&bits)));
    }

    #[test]
    fn l1_pre_survives_the_round_trip_and_some_noise() {
        let mut c = L1Coder::new();
        let info = bits_of(168, 7);
        let tx = c.encode_pre(&info);
        assert_eq!(tx.len(), PRE_CELLS);
        assert_eq!(&c.decode_pre(&hard(&tx)).unwrap(), &info);
        // A tenth of the bits wrong, softly: still decodes (the code is 1/4).
        let mut llr = hard(&tx);
        for i in (0..llr.len()).step_by(13) {
            llr[i] = -llr[i] * 0.2;
        }
        assert_eq!(&c.decode_pre(&llr).unwrap(), &info);
    }

    #[test]
    fn l1_pre_rejects_a_wrong_frame() {
        let mut c = L1Coder::new();
        let tx = c.encode_pre(&bits_of(168, 3));
        // Pure noise.
        let junk: Vec<f32> = bits_of(tx.len(), 99)
            .iter()
            .map(|&b| if b == 0 { 1.0 } else { -1.0 })
            .collect();
        assert!(c.decode_pre(&junk).is_none());
    }

    #[test]
    fn the_layout_of_gr_dtvs_l1_post() {
        // gr-dtv's single PLP: 350 bits with the CRC, QPSK, N_P2 = 1.
        let l = PostLayout::new(350, L1Mod::Qpsk, 1).unwrap();
        // N_punc_temp = 6·(7032−350)/5 = 8018; N_post_temp = 350+168+9000−8018
        // = 1500; N_post = 1500 (a multiple of 4): 750 cells.
        assert_eq!((l.n_post, l.cells, l.n_punc), (1500, 750, 8018));
        let l64 = PostLayout::new(350, L1Mod::Qam64, 1).unwrap();
        assert_eq!(l64.n_post % 12, 0);
        assert_eq!(l64.cells * 6, l64.n_post);
    }

    #[test]
    fn l1_post_round_trips_in_every_constellation_and_scrambled_or_not() {
        let mut c = L1Coder::new();
        for k_sig in [350usize, 700, 100] {
            for l1_mod in [L1Mod::Bpsk, L1Mod::Qpsk, L1Mod::Qam16, L1Mod::Qam64] {
                for scrambled in [false, true] {
                    let layout = PostLayout::new(k_sig, l1_mod, 1).unwrap();
                    let info = bits_of(k_sig - 32, k_sig as u32 + 1);
                    let tx = c.encode_post(&info, &layout, scrambled);
                    assert_eq!(tx.len(), layout.n_post);
                    let mut cells = Vec::new();
                    post_cells(&tx, l1_mod, &mut cells);
                    assert_eq!(cells.len(), layout.cells);
                    let var = vec![0.01f32; cells.len()];
                    let mut llr = Vec::new();
                    post_llrs(&cells, &var, l1_mod, &mut llr);
                    // Scale: the decoder expects LLRs of the order of ±10.
                    let got = c.decode_post(&llr, &layout, scrambled);
                    assert_eq!(
                        got.as_deref(),
                        Some(&info[..]),
                        "k_sig {k_sig} {l1_mod:?} scrambled {scrambled}"
                    );
                }
            }
        }
    }
}
