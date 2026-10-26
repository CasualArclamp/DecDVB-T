// made by CasualArclamp
//! The outer chain (EN 300 744 §4.3.1–4.3.2): energy dispersal on groups of
//! eight transport packets, the shortened Reed–Solomon code, and the
//! convolutional byte interleaver. DVB-S and DVB-T share all three.
//!
//! The randomiser and interleaver follow gr-dtv's `dvbt_energy_dispersal` and
//! `dvbt_convolutional_interleaver` (GPL-3, Free Software Foundation).
//!
//! Speed (the transmitter has no spare CPU), and the output is bit-for-bit the
//! old byte-at-a-time chain's (kept as `reference` in the tests):
//!
//! - the randomiser's sequence is the same in every group of eight packets, so
//!   it is run once into a table and each packet is XORed with its slice;
//! - the RS parity is the usual shift register with the multiplications of the
//!   generator by the feedback byte tabulated: a step is one 16-byte load and an
//!   exclusive-or (the receiver's `is_codeword` does the same);
//! - a packet is 204 = 17 × 12 bytes, so every packet starts on the byte
//!   interleaver's first branch and branch j (which delays 17·j bytes of its
//!   own) delays its bytes by exactly j packets: the interleaver is a ring of
//!   the last 12 packets and a copy.

use std::sync::OnceLock;

pub use decdvbt_fec::outer::{ConvInterleaver, GROUP, Prbs, RS_LEN, SYNC_INVERTED};

use crate::ts::{TS_LEN, TS_SYNC};

#[cfg(test)]
use decdvbt_fec::rs::ReedSolomon;

/// Branches of the byte interleaver.
const BRANCHES: usize = ConvInterleaver::I;
/// RS parity bytes.
const PARITY: usize = RS_LEN - TS_LEN;

/// What is worked out once.
struct Tables {
    /// The randomiser's bytes for each packet of a group, at its place in the
    /// packet (byte 0, the sync, is not randomised and holds 0).
    masks: [[u8; TS_LEN]; GROUP],
    /// `rows[f]` = the 16 generator coefficients times `f`, byte 0 first, as
    /// one little-endian number.
    rows: Vec<u128>,
}

/// Multiplication in GF(256), p(x) = x⁸ + x⁴ + x³ + x² + 1 (0x11D).
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1D;
        }
        b >>= 1;
    }
    p
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        // The PRBS runs through the sync slot of every packet but the first of
        // a group, its output unused there.
        let mut prbs = Prbs::new();
        let mut masks = [[0u8; TS_LEN]; GROUP];
        for (g, mask) in masks.iter_mut().enumerate() {
            if g != 0 {
                prbs.bits(8);
            }
            for m in &mut mask[1..] {
                *m = prbs.bits(8);
            }
        }
        // g(x) = Π (x − α^i), i = 0 … 15, highest degree first.
        let mut g = vec![1u8];
        let mut root = 1u8;
        for _ in 0..PARITY {
            let mut next = vec![0u8; g.len() + 1];
            for (j, &c) in g.iter().enumerate() {
                next[j] ^= c;
                next[j + 1] ^= gf_mul(c, root);
            }
            g = next;
            root = gf_mul(root, 2);
        }
        let rows = (0..256usize)
            .map(|f| {
                let row: [u8; PARITY] = std::array::from_fn(|i| gf_mul(f as u8, g[1 + i]));
                u128::from_le_bytes(row)
            })
            .collect();
        Tables { masks, rows }
    })
}

/// Transport packets in, interleaved 204-byte blocks out.
pub struct OuterEncoder {
    /// Packets since the last group start.
    in_group: usize,
    t: &'static Tables,
    /// The last 12 RS blocks, oldest overwritten first; `slot` is the newest.
    history: Vec<[u8; RS_LEN]>,
    slot: usize,
}

impl Default for OuterEncoder {
    fn default() -> Self {
        OuterEncoder {
            in_group: 0,
            t: tables(),
            history: vec![[0u8; RS_LEN]; BRANCHES],
            slot: 0,
        }
    }
}

impl OuterEncoder {
    /// Randomise (the first of eight with its sync byte inverted), add the
    /// 16 parity bytes and interleave one packet; 204 bytes go to `out`.
    pub fn push_packet(&mut self, pkt: &[u8; TS_LEN], out: &mut Vec<u8>) {
        debug_assert_eq!(pkt[0], TS_SYNC);
        let mut block = [0u8; RS_LEN];
        block[..TS_LEN].copy_from_slice(pkt);
        if self.in_group == 0 {
            block[0] = SYNC_INVERTED;
        }
        for (b, m) in block[1..TS_LEN]
            .iter_mut()
            .zip(&self.t.masks[self.in_group][1..])
        {
            *b ^= m;
        }
        self.in_group = (self.in_group + 1) % GROUP;

        // Parity: the shift register, a byte a step.
        let mut r = 0u128;
        for &m in &block[..TS_LEN] {
            let fb = usize::from(m ^ r as u8);
            r = (r >> 8) ^ self.t.rows[fb];
        }
        block[TS_LEN..].copy_from_slice(&r.to_le_bytes());

        // Interleave: byte k is on branch k % 12 and comes from the block
        // that many packets back.
        self.slot = (self.slot + 1) % BRANCHES;
        self.history[self.slot] = block;
        let mut mixed = [0u8; RS_LEN];
        for j in 0..BRANCHES {
            let src = &self.history[(self.slot + BRANCHES - j) % BRANCHES];
            for k in (j..RS_LEN).step_by(BRANCHES) {
                mixed[k] = src[k];
            }
        }
        out.extend_from_slice(&mixed);
    }
}

#[cfg(test)]
mod reference {
    //! The byte-at-a-time chain this one replaced.

    use super::*;

    pub struct RefOuter {
        prbs: Prbs,
        in_group: usize,
        rs: ReedSolomon,
        interleaver: ConvInterleaver,
    }

    impl Default for RefOuter {
        fn default() -> Self {
            RefOuter {
                prbs: Prbs::new(),
                in_group: 0,
                rs: ReedSolomon::dvb(),
                interleaver: ConvInterleaver::new(false),
            }
        }
    }

    impl RefOuter {
        pub fn push_packet(&mut self, pkt: &[u8; TS_LEN], out: &mut Vec<u8>) {
            let mut block = [0u8; RS_LEN];
            block[..TS_LEN].copy_from_slice(pkt);
            if self.in_group == 0 {
                self.prbs.reset();
                block[0] = SYNC_INVERTED;
            } else {
                self.prbs.bits(8);
            }
            for b in &mut block[1..TS_LEN] {
                *b ^= self.prbs.bits(8);
            }
            self.in_group = (self.in_group + 1) % GROUP;
            self.rs.encode(&mut block);
            out.extend(block.iter().map(|&b| self.interleaver.push(b)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reference::RefOuter;
    use super::*;

    #[test]
    fn a_packet_comes_out_as_204_bytes_with_valid_parity() {
        let mut enc = OuterEncoder::default();
        let rs = ReedSolomon::dvb();
        let mut ts = crate::ts::TestStream::default();
        let mut pkt = [0u8; TS_LEN];
        let mut stream = Vec::new();
        for _ in 0..40 {
            crate::ts::TsSource::next_packet(&mut ts, &mut pkt);
            enc.push_packet(&pkt, &mut stream);
        }
        assert_eq!(stream.len(), 40 * RS_LEN);
        // Undo the interleaver, and every aligned 204-byte block (after the
        // pipeline's delay has passed) is an RS codeword.
        let mut d = ConvInterleaver::new(true);
        let plain: Vec<u8> = stream.iter().map(|&b| d.push(b)).collect();
        let delay = ConvInterleaver::DELAY;
        let first = delay.div_ceil(RS_LEN) * RS_LEN;
        let mut checked = 0;
        for block in plain[first..].as_chunks::<RS_LEN>().0 {
            let mut b = block.to_vec();
            assert_eq!(rs.decode(&mut b), Ok(0));
            checked += 1;
        }
        assert!(checked >= 25);
    }

    #[test]
    fn the_first_packet_of_a_group_has_its_sync_inverted() {
        let mut enc = OuterEncoder::default();
        let mut ts = crate::ts::TestStream::default();
        let mut pkt = [0u8; TS_LEN];
        let mut stream = Vec::new();
        for _ in 0..24 {
            crate::ts::TsSource::next_packet(&mut ts, &mut pkt);
            enc.push_packet(&pkt, &mut stream);
        }
        let mut d = ConvInterleaver::new(true);
        let plain: Vec<u8> = stream.iter().map(|&b| d.push(b)).collect();
        let first = ConvInterleaver::DELAY.div_ceil(RS_LEN) * RS_LEN;
        // The sync bytes of the blocks after the delay: 0xB8 once in eight.
        let syncs: Vec<u8> = plain[first..]
            .as_chunks::<RS_LEN>()
            .0
            .iter()
            .map(|b| b[0])
            .collect();
        let inverted = syncs.iter().filter(|&&s| s == SYNC_INVERTED).count();
        let normal = syncs.iter().filter(|&&s| s == TS_SYNC).count();
        assert_eq!(inverted + normal, syncs.len());
        assert!(inverted >= 1 && normal >= 2 * inverted, "{syncs:02x?}");
    }

    /// The new chain against the old one over many groups of random packets
    /// (so the PRBS group, the RS parity and the interleaver's delay lines
    /// all run), byte for byte.
    #[test]
    fn bit_exact_with_the_byte_at_a_time_chain() {
        let mut new = OuterEncoder::default();
        let mut old = RefOuter::default();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 16) as u8
        };
        for n in 0..400 {
            let mut pkt = [0u8; TS_LEN];
            for b in &mut pkt[1..] {
                *b = next();
            }
            pkt[0] = TS_SYNC;
            let (mut a, mut b) = (Vec::new(), Vec::new());
            old.push_packet(&pkt, &mut a);
            new.push_packet(&pkt, &mut b);
            assert_eq!(a, b, "packet {n}");
        }
    }
}
