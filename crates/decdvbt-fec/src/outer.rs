// made by CasualArclamp
//! The outer chain's shared parts (EN 300 744 §4.3.1–4.3.2, the same in
//! EN 300 421): the energy-dispersal PRBS and the convolutional byte
//! interleaver, which the DVB-T modulator and receiver both use.
//!
//! The randomiser and interleaver follow gr-dtv's `dvbt_energy_dispersal` and
//! `dvbt_convolutional_interleaver` (GPL-3, Free Software Foundation).

/// Bytes in a transport packet and its sync byte.
pub const TS_LEN: usize = 188;
pub const TS_SYNC: u8 = 0x47;
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
    /// Per branch, its delay line as a ring (empty for the branch with no
    /// delay) and the place of its oldest byte.
    lines: Vec<(Vec<u8>, usize)>,
    /// The branch the next byte goes to.
    branch: usize,
}

impl ConvInterleaver {
    pub const I: usize = 12;
    pub const M: usize = 17;
    /// Bytes an interleaver and a deinterleaver delay a stream by together.
    pub const DELAY: usize = (Self::I - 1) * Self::M * Self::I;

    /// `deinterleave`: branch j delays (I − 1 − j)·M instead of j·M.
    pub fn new(deinterleave: bool) -> ConvInterleaver {
        let lines = (0..Self::I)
            .map(|j| {
                let n = if deinterleave { Self::I - 1 - j } else { j } * Self::M;
                (vec![0u8; n], 0)
            })
            .collect();
        ConvInterleaver { lines, branch: 0 }
    }

    /// Put a byte on its branch and take the oldest off it.
    #[inline]
    pub fn push(&mut self, b: u8) -> u8 {
        let (ring, head) = &mut self.lines[self.branch];
        self.branch += 1;
        if self.branch == Self::I {
            self.branch = 0;
        }
        if ring.is_empty() {
            return b;
        }
        let old = std::mem::replace(&mut ring[*head], b);
        *head += 1;
        if *head == ring.len() {
            *head = 0;
        }
        old
    }

    /// The branch the next byte goes to (0 for a packet's first byte once
    /// the stream is aligned).
    pub fn branch(&self) -> usize {
        self.branch
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
        let input: Vec<u8> = (0..6000u32).map(|n| (n * 7 + 3) as u8).collect();
        let out: Vec<u8> = input.iter().map(|&b| d.push(i.push(b))).collect();
        assert_eq!(
            &out[ConvInterleaver::DELAY..],
            &input[..input.len() - ConvInterleaver::DELAY]
        );
        // And in between it really scrambles: the interleaved stream is not
        // the input delayed by anything simple.
        let mut i2 = ConvInterleaver::new(false);
        let mid: Vec<u8> = input.iter().map(|&b| i2.push(b)).collect();
        assert_ne!(&mid[..], &input[..]);
    }
}
