// made by CasualArclamp
//! The outer chain (EN 300 744 §4.3.1–4.3.2): energy dispersal on groups of
//! eight transport packets, the shortened Reed–Solomon code, and the
//! convolutional byte interleaver. DVB-S and DVB-T share all three.
//!
//! The randomiser and interleaver follow gr-dtv's `dvbt_energy_dispersal` and
//! `dvbt_convolutional_interleaver` (GPL-3, Free Software Foundation).

use decdvbt_fec::rs::ReedSolomon;

pub use decdvbt_fec::outer::{ConvInterleaver, GROUP, Prbs, RS_LEN, SYNC_INVERTED};

use crate::ts::{TS_LEN, TS_SYNC};

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
}
