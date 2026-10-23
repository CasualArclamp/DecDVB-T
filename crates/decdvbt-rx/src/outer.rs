// made by CasualArclamp
//! The receiver's outer chain (EN 300 744 §4.3.1–4.3.2 undone): from the
//! inner decoder's bytes, at whatever bit alignment, to transport packets.
//!
//! 1. **Hunt.** The packets' sync bytes (0x47, or 0xB8 once in eight) stand
//!    204 bytes apart in the interleaved stream, being on the interleaver's
//!    branch with no delay. A window of 15 packets' bytes is searched at
//!    each of the 8 bit alignments for the offset where nearly all of them
//!    fall on a sync byte: the odds of that by chance are around 10⁻¹⁸.
//! 2. **Follow.** Aligned bytes go through the convolutional deinterleaver
//!    (which delays them by 11 packets), are cut into 204-byte blocks, and
//!    each is corrected by the Reed–Solomon code (up to 8 bytes). A run of
//!    blocks that will not correct sends the decoder back to hunting.
//! 3. **Descramble.** The energy-dispersal sequence restarts at each packet
//!    whose sync byte is inverted and runs on through the next seven; it is
//!    laid out once and each packet XORed with its slice. A packet the code
//!    could not correct is passed on with its transport-error bit set.

use std::sync::OnceLock;

use decdvbt_fec::outer::{ConvInterleaver, GROUP, Prbs, RS_LEN, SYNC_INVERTED, TS_LEN, TS_SYNC};
use decdvbt_fec::rs::ReedSolomon;

/// Packets in the hunt window and how many of them must show a sync byte.
const HUNT_PACKETS: usize = 15;
const HUNT_NEED: usize = 11;
/// Uncorrectable blocks in a row that end the lock.
const LOST_AFTER: u32 = 20;

/// The scrambling sequence over a group of eight: byte i of packet p is
/// XORed with `seq[188p + i − 1]` (i = 1 … 187).
fn sequence() -> &'static [u8; GROUP * TS_LEN] {
    static SEQ: OnceLock<[u8; GROUP * TS_LEN]> = OnceLock::new();
    SEQ.get_or_init(|| {
        let mut seq = [0u8; GROUP * TS_LEN];
        let mut p = Prbs::new();
        // 187 bytes for the first packet, then a sync slot and 187 for each
        // of the others: 1503 bytes, the sequence's repeat.
        for (i, s) in seq.iter_mut().enumerate().take(GROUP * TS_LEN - 1) {
            *s = p.bits(8);
            let _ = i;
        }
        seq
    })
}

/// What the chain has seen.
#[derive(Debug, Clone, Copy, Default)]
pub struct OuterStats {
    /// Blocks run through the code since the lock, and how they came out.
    pub blocks: u64,
    pub clean: u64,
    pub corrected: u64,
    pub uncorrectable: u64,
    /// Wrong bits the code mended, and bytes.
    pub bits_fixed: u64,
    pub bytes_fixed: u64,
    /// Times the lock was lost.
    pub losses: u64,
    /// Packets handed on.
    pub packets: u64,
}

impl OuterStats {
    /// The bit error rate going into the code: the bits it mended over the
    /// bits of the blocks it could mend (an uncorrectable block says only
    /// that there were more than 64 wrong bytes' worth).
    pub fn pre_rs_ber(&self) -> f64 {
        let n = (self.clean + self.corrected) as f64 * (RS_LEN * 8) as f64;
        if n == 0.0 {
            0.0
        } else {
            self.bits_fixed as f64 / n
        }
    }
}

enum State {
    Hunt,
    Follow,
}

pub struct OuterDecoder {
    rs: ReedSolomon,
    state: State,
    hunt: Vec<u8>,
    /// Bits to shift the raw bytes left by to put the sync bytes on byte
    /// boundaries, and the previous raw byte.
    shift: u32,
    carry: u8,
    deint: ConvInterleaver,
    /// Bytes the deinterleaver has yet to give before they are the stream's.
    warm: usize,
    block: [u8; RS_LEN],
    fill: usize,
    /// The next block's place in its group of eight, once known.
    next_pos: Option<usize>,
    bad_run: u32,
    /// Pass uncorrectable packets on with the error bit set.
    pub emit_errors: bool,
    pub stats: OuterStats,
}

impl Default for OuterDecoder {
    fn default() -> Self {
        OuterDecoder::new()
    }
}

impl OuterDecoder {
    pub fn new() -> OuterDecoder {
        OuterDecoder {
            rs: ReedSolomon::dvb(),
            state: State::Hunt,
            hunt: Vec::new(),
            shift: 0,
            carry: 0,
            deint: ConvInterleaver::new(true),
            warm: 0,
            block: [0; RS_LEN],
            fill: 0,
            next_pos: None,
            bad_run: 0,
            emit_errors: true,
            stats: OuterStats::default(),
        }
    }

    /// Whether the packets' alignment is known.
    pub fn locked(&self) -> bool {
        matches!(self.state, State::Follow)
    }

    /// Forget the stream (a break in it); keeps the statistics.
    pub fn reset(&mut self) {
        self.state = State::Hunt;
        self.hunt.clear();
        self.fill = 0;
        self.next_pos = None;
        self.bad_run = 0;
    }

    /// The bit alignment and offset at which sync bytes line up in `raw`.
    fn find_sync(raw: &[u8]) -> Option<(u32, usize)> {
        let mut best: Option<(usize, u32, usize)> = None;
        for b in 0..8u32 {
            let mut count = [0u16; RS_LEN];
            let n = if b == 0 { raw.len() } else { raw.len() - 1 };
            for k in 0..n {
                let v = if b == 0 {
                    raw[k]
                } else {
                    (raw[k] << b) | (raw[k + 1] >> (8 - b))
                };
                if v == TS_SYNC || v == SYNC_INVERTED {
                    count[k % RS_LEN] += 1;
                }
            }
            let (o, &c) = count.iter().enumerate().max_by_key(|&(_, c)| *c).unwrap();
            if usize::from(c) >= HUNT_NEED && best.is_none_or(|(bc, _, _)| usize::from(c) > bc) {
                best = Some((usize::from(c), b, o));
            }
        }
        best.map(|(_, b, o)| (b, o))
    }

    /// Take bytes from the inner decoder; transport packets (188 bytes each)
    /// go to `out`.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        match self.state {
            State::Follow => self.follow(bytes, out),
            State::Hunt => {
                self.hunt.extend_from_slice(bytes);
                let need = HUNT_PACKETS * RS_LEN + 2;
                if self.hunt.len() < need {
                    return;
                }
                match Self::find_sync(&self.hunt) {
                    Some((b, o)) => self.lock(b, o, out),
                    None => {
                        // Nothing yet: keep the newest part and look again
                        // when as much again has come.
                        let keep = self.hunt.len() / 2;
                        self.hunt.drain(..self.hunt.len() - keep);
                    }
                }
            }
        }
    }

    fn lock(&mut self, shift: u32, offset: usize, out: &mut Vec<u8>) {
        self.state = State::Follow;
        self.shift = shift;
        self.deint = ConvInterleaver::new(true);
        self.warm = ConvInterleaver::DELAY;
        self.fill = 0;
        self.next_pos = None;
        self.bad_run = 0;
        // Replay the window from the first sync byte as if it had just
        // arrived: shifted byte k is made of raw bytes k and k + 1, so the
        // byte at `offset` goes in as the carry. (A loss part-way hands the
        // rest back to the hunt.)
        let raw = std::mem::take(&mut self.hunt);
        self.carry = raw[offset];
        let rest = if shift == 0 {
            &raw[offset..]
        } else {
            &raw[offset + 1..]
        };
        self.follow(rest, out);
    }

    fn follow(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for (i, &c) in bytes.iter().enumerate() {
            let b = if self.shift == 0 {
                c
            } else {
                let b = (self.carry << self.shift) | (c >> (8 - self.shift));
                self.carry = c;
                b
            };
            self.byte(b, out);
            if !self.locked() {
                // Lost: what is left starts a new hunt.
                self.push(&bytes[i + 1..], out);
                return;
            }
        }
    }

    #[inline]
    fn byte(&mut self, b: u8, out: &mut Vec<u8>) {
        let d = self.deint.push(b);
        if self.warm > 0 {
            self.warm -= 1;
            return;
        }
        self.block[self.fill] = d;
        self.fill += 1;
        if self.fill == RS_LEN {
            self.fill = 0;
            self.packet(out);
        }
    }

    /// A deinterleaved block is complete: correct it, descramble it, hand it
    /// on.
    fn packet(&mut self, out: &mut Vec<u8>) {
        let before = self.block;
        let result = self.rs.decode(&mut self.block);
        self.stats.blocks += 1;
        let good = match result {
            Ok(n) => {
                if n == 0 {
                    self.stats.clean += 1;
                } else {
                    self.stats.corrected += 1;
                    self.stats.bytes_fixed += n as u64;
                    self.stats.bits_fixed += before
                        .iter()
                        .zip(&self.block)
                        .map(|(a, b)| u64::from((a ^ b).count_ones()))
                        .sum::<u64>();
                }
                matches!(self.block[0], TS_SYNC | SYNC_INVERTED)
            }
            Err(_) => {
                self.stats.uncorrectable += 1;
                false
            }
        };
        if good {
            self.bad_run = 0;
        } else {
            self.bad_run += 1;
            if self.bad_run >= LOST_AFTER {
                self.stats.losses += 1;
                self.reset();
                return;
            }
        }

        // Where the block is in its group of eight.
        let pos = if good && self.block[0] == SYNC_INVERTED {
            Some(0)
        } else if good && self.next_pos == Some(0) {
            // Expected an inverted sync and found an ordinary one: the
            // count is wrong.
            None
        } else {
            self.next_pos
        };
        self.next_pos = pos.map(|p| (p + 1) % GROUP);
        let Some(p) = pos else { return };

        if !good && !self.emit_errors {
            return;
        }
        let seq = sequence();
        let base = out.len();
        out.extend_from_slice(&self.block[..TS_LEN]);
        out[base] = TS_SYNC;
        let mask = &seq[TS_LEN * p..TS_LEN * p + TS_LEN - 1];
        for (o, m) in out[base + 1..].iter_mut().zip(mask) {
            *o ^= m;
        }
        if !good {
            out[base + 1] |= 0x80;
        }
        self.stats.packets += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_mod::outer::OuterEncoder;
    use decdvbt_mod::ts::{TestStream, TsSource, check_test_payload};

    /// `n` packets through the encoder: the interleaved bytes.
    fn encoded(n: usize) -> Vec<u8> {
        let mut enc = OuterEncoder::default();
        let mut ts = TestStream::default();
        let mut pkt = [0u8; TS_LEN];
        let mut stream = Vec::new();
        for _ in 0..n {
            ts.next_packet(&mut pkt);
            enc.push_packet(&pkt, &mut stream);
        }
        stream
    }

    /// The bytes with `lead` bits of junk ahead of them (their alignment
    /// moved by that many bits).
    fn shifted(bytes: &[u8], lead: u32) -> Vec<u8> {
        let mut bits: Vec<u8> = (0..lead).map(|i| ((i * 5 + 1) & 1) as u8).collect();
        for &b in bytes {
            for i in (0..8).rev() {
                bits.push((b >> i) & 1);
            }
        }
        bits.chunks(8)
            .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b) << (8 - c.len()))
            .collect()
    }

    fn flip_bits(bytes: &mut [u8], rate: f64, seed: u64) {
        let mut s = seed | 1;
        for b in bytes.iter_mut() {
            for i in 0..8 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                if (s >> 11) as f64 / (1u64 << 53) as f64 <= rate {
                    *b ^= 1 << i;
                }
            }
        }
    }

    /// The data packets in `ts`: their counters, which must run on one by one.
    fn counters(ts: &[u8]) -> Vec<u32> {
        ts.as_chunks::<TS_LEN>()
            .0
            .iter()
            .filter_map(|p| check_test_payload(p))
            .collect()
    }

    #[test]
    fn a_clean_stream_comes_out_as_the_packets_whatever_the_bit_alignment() {
        let stream = encoded(400);
        for lead in 0..8 {
            let raw = shifted(&stream, lead);
            // In pieces of awkward sizes.
            let mut d = OuterDecoder::new();
            let mut out = Vec::new();
            let mut i = 0;
            let mut s = 7u32;
            while i < raw.len() {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (1 + (s >> 16) as usize % 900).min(raw.len() - i);
                d.push(&raw[i..i + n], &mut out);
                i += n;
            }
            assert!(d.locked(), "lead {lead}");
            assert_eq!(d.stats.uncorrectable, 0, "lead {lead}");
            assert!(
                out.len() / TS_LEN > 300,
                "lead {lead}: {} packets",
                out.len() / TS_LEN
            );
            assert!(
                out.as_chunks::<TS_LEN>()
                    .0
                    .iter()
                    .all(|p| p[0] == TS_SYNC && p[1] & 0x80 == 0)
            );
            let c = counters(&out);
            assert!(c.len() > 280, "lead {lead}: {} data packets", c.len());
            assert!(
                c.windows(2).all(|w| w[1] == w[0] + 1),
                "lead {lead}: not consecutive"
            );
        }
    }

    #[test]
    fn errors_up_to_the_codes_reach_are_mended() {
        let mut stream = encoded(600);
        // About 2.5 wrong bytes a block: well inside t = 8.
        flip_bits(&mut stream, 1e-3, 5);
        let raw = shifted(&stream, 3);
        let mut d = OuterDecoder::new();
        let mut out = Vec::new();
        d.push(&raw, &mut out);
        assert!(d.locked());
        assert!(d.stats.corrected > 100, "{:?}", d.stats);
        assert_eq!(d.stats.uncorrectable, 0, "{:?}", d.stats);
        let ber = d.stats.pre_rs_ber();
        assert!((0.5e-3..2e-3).contains(&ber), "ber {ber}");
        let c = counters(&out);
        assert!(c.len() > 450);
        assert!(c.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn garbage_is_flagged_and_the_lock_is_given_up_and_regained() {
        let good = encoded(300);
        let mut stream = good.clone();
        // Destroy a stretch in the middle: 40 blocks of noise.
        let mut s = 99u64;
        for b in &mut stream[60 * RS_LEN..100 * RS_LEN] {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (s >> 40) as u8;
        }
        let mut d = OuterDecoder::new();
        let mut out = Vec::new();
        d.push(&stream, &mut out);
        assert!(d.stats.losses >= 1, "{:?}", d.stats);
        assert!(d.locked(), "regained: {:?}", d.stats);
        // What came out of the damaged stretch carries the error flag; the
        // rest does not, and the good packets after it are intact.
        assert!(out.as_chunks::<TS_LEN>().0.iter().any(|p| p[1] & 0x80 != 0));
        let c = counters(&out);
        assert!(c.len() > 100);
        assert!(c.windows(2).filter(|w| w[1] != w[0] + 1).count() <= 1);
    }

    #[test]
    fn the_scrambling_sequence_is_the_prbs_run_through_the_group() {
        let seq = sequence();
        // Starts 0x03, as the PRBS does; 1503 bytes and then the repeat.
        assert_eq!(seq[0], 0x03);
        let mut p = Prbs::new();
        for (i, &s) in seq.iter().enumerate().take(1503) {
            assert_eq!(p.bits(8), s, "byte {i}");
        }
    }
}
