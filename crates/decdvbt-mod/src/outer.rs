// made by CasualArclamp
//! The outer chain (EN 300 744 §4.3.1–4.3.2): energy dispersal on groups of
//! eight transport packets, the shortened Reed–Solomon code, and the
//! convolutional byte interleaver. DVB-S and DVB-T share all three.
//!
//! The randomiser and interleaver follow gr-dtv's `dvbt_energy_dispersal` and
//! `dvbt_convolutional_interleaver` (GPL-3, Free Software Foundation).

use std::collections::VecDeque;

use decdvbt_fec::rs::ReedSolomon;

use crate::ts::{TS_LEN, TS_SYNC};

/// The sync byte with its bits inverted, which opens each group of eight.
pub const SYNC_INVERTED: u8 = 0xB8;
/// Packets a PRBS group covers.
pub const GROUP: usize = 8;
/// Bytes in an RS-protected packet.
pub const RS_LEN: usize = 204;

/// The PRBS 1 + x¹⁴ + x¹⁵, initialised to 100101010000000 at each group
/// (§4.3.1). The register is kept with the spec's first stage as the top
/// bit, so the sequence starts 0x03….
pub struct Prbs {
    reg: u16,
}

impl Prbs {
    const INIT: u16 = 0xA9;

    pub fn new() -> Prbs {
        Prbs { reg: Self::INIT }
    }

    pub fn reset(&mut self) {
        self.reg = Self::INIT;
    }

    /// Clock `n` bits (≤ 8) and return them, the first in the top place.
    pub fn bits(&mut self, n: u32) -> u8 {
        let mut out = 0;
        for _ in 0..n {
            let fb = ((self.reg >> 13) ^ (self.reg >> 14)) & 1;
            self.reg = ((self.reg << 1) | fb) & 0x7FFF;
            out = (out << 1) | fb as u8;
        }
        out
    }
}

impl Default for Prbs {
    fn default() -> Self {
        Prbs::new()
    }
}

/// A convolutional interleaver of `I` = 12 branches (§4.3.2): branch j holds
/// j·17 bytes of delay, the bytes visiting the branches in turn, the sync
/// byte always on branch 0. The deinterleaver is the same with the delays
/// reversed.
pub struct ConvInterleaver {
    fifos: Vec<VecDeque<u8>>,
    pos: usize,
}

impl ConvInterleaver {
    pub const I: usize = 12;
    pub const M: usize = 17;

    /// `deinterleave`: branch j delays (I − 1 − j)·M instead of j·M.
    pub fn new(deinterleave: bool) -> ConvInterleaver {
        let fifos = (0..Self::I)
            .map(|j| {
                let n = if deinterleave { Self::I - 1 - j } else { j } * Self::M;
                VecDeque::from(vec![0u8; n])
            })
            .collect();
        ConvInterleaver { fifos, pos: 0 }
    }

    pub fn push(&mut self, b: u8) -> u8 {
        let f = &mut self.fifos[self.pos % Self::I];
        self.pos += 1;
        if f.is_empty() {
            return b;
        }
        f.push_back(b);
        f.pop_front().unwrap()
    }
}

/// Transport packets in, interleaved 204-byte blocks out.
pub struct OuterEncoder {
    prbs: Prbs,
    /// Packets since the last group start.
    in_group: usize,
    rs: ReedSolomon,
    interleaver: ConvInterleaver,
}

impl Default for OuterEncoder {
    fn default() -> Self {
        OuterEncoder {
            prbs: Prbs::new(),
            in_group: 0,
            rs: ReedSolomon::dvb(),
            interleaver: ConvInterleaver::new(false),
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
            self.prbs.reset();
            block[0] = SYNC_INVERTED;
        } else {
            // The generator runs through the sync byte slots of the other
            // seven packets, its bits unused.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prbs_starts_with_the_specs_bytes_and_repeats_after_1503() {
        let mut p = Prbs::new();
        // EN 300 744 / EN 300 421: the randomiser's first byte is 0000 0011.
        assert_eq!(p.bits(8), 0x03);
        // Maximal length: 2^15 − 1 bits, then the same again.
        p.reset();
        let a: Vec<u8> = (0..4096).map(|_| p.bits(8)).collect();
        let mut q = Prbs::new();
        let period_bits = (1 << 15) - 1;
        // Skip a whole period (in bits) and compare the next bytes.
        let mut skipped = 0;
        while skipped + 8 <= period_bits {
            q.bits(8);
            skipped += 8;
        }
        q.bits((period_bits - skipped) as u32);
        let b: Vec<u8> = (0..16).map(|_| q.bits(8)).collect();
        assert_eq!(&a[..16], &b[..]);
    }

    #[test]
    fn interleaver_then_deinterleaver_is_a_pure_delay() {
        // Together they delay every byte by (I − 1)·M·I = 2244.
        let mut i = ConvInterleaver::new(false);
        let mut d = ConvInterleaver::new(true);
        let delay = (ConvInterleaver::I - 1) * ConvInterleaver::M * ConvInterleaver::I;
        let input: Vec<u8> = (0..6000u32).map(|n| (n * 7 + 3) as u8).collect();
        let out: Vec<u8> = input.iter().map(|&b| d.push(i.push(b))).collect();
        assert_eq!(&out[delay..], &input[..input.len() - delay]);
        // And in between it really scrambles: the interleaved stream is not
        // the input delayed by anything simple.
        let mut i2 = ConvInterleaver::new(false);
        let mid: Vec<u8> = input.iter().map(|&b| i2.push(b)).collect();
        assert_ne!(&mid[..], &input[..]);
    }

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
        let delay: usize = 2244;
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
        let first = 2244usize.div_ceil(RS_LEN) * RS_LEN;
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
}
