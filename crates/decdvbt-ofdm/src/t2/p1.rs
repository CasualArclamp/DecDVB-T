// made by CasualArclamp
//! The P1 symbol's signalling (EN 302 755 §7.2): 384 of the 1024-carrier
//! symbol's carriers carry a DBPSK sequence made of the S1 pattern, the S2
//! pattern and S1 again, multiplied by a randomising sequence. S1 (three
//! bits) says SISO or MISO and whether the signal is T2, S2 (four bits) the
//! FFT size of the frame that follows.
//!
//! The tables and the construction are gr-dtv's `dvbt2_p1insertion_cc_impl.cc`
//! (GPL-3.0-or-later, Copyright 2015-2019 Free Software Foundation, Inc.).
//! The time-domain symbol (the transform, and the frequency-shifted copies
//! that make its guard intervals) is made by the modulator.

use super::tables::{P1_ACTIVE_CARRIERS, S1_PATTERNS, S2_PATTERNS};

/// Active carriers of the P1 symbol.
pub const CARRIERS: usize = 384;
/// The FFT size of the P1 symbol, and the symbol's length with its two
/// frequency-shifted guard parts (C of 542 samples, A of 1024, B of 482).
pub const FFT: usize = 1024;
pub const SAMPLES: usize = 2048;
pub const C_LEN: usize = 542;
pub const B_LEN: usize = 482;
/// The transform bin of active carrier `i`, with the centre at bin 512.
pub fn bin(i: usize) -> usize {
    P1_ACTIVE_CARRIERS[i] as usize + 86
}

/// The values of S1 (0 to 7): T2 SISO, T2 MISO, not T2, T2-Lite SISO, T2-Lite
/// MISO.
pub mod s1 {
    pub const T2_SISO: u8 = 0;
    pub const T2_MISO: u8 = 1;
    pub const NON_T2: u8 = 2;
    pub const LITE_SISO: u8 = 3;
    pub const LITE_MISO: u8 = 4;
}

/// S2 as the P1 symbol carries it: field 1 (the FFT size) and field 2's bit
/// (1: not every preamble in the frame is the same).
pub fn s2_value(field1: u8, field2: u8) -> u8 {
    (field1 << 1) | (field2 & 1)
}

/// The randomising sequence (a 15-bit register, x^15 + x^14 + 1, started with
/// 0x4E46), as ±1.
pub fn randomiser() -> [i8; CARRIERS] {
    let mut out = [1i8; CARRIERS];
    let mut sr: u32 = 0x4e46;
    for o in out.iter_mut() {
        let b = (sr ^ (sr >> 1)) & 1;
        *o = if b == 0 { 1 } else { -1 };
        sr >>= 1;
        if b == 1 {
            sr |= 0x4000;
        }
    }
    out
}

/// The 384 bits that are modulated: S1's pattern, S2's and S1's again.
pub fn modulation_bits(s1: u8, s2: u8) -> [u8; CARRIERS] {
    let mut bits = [0u8; CARRIERS];
    let mut i = 0;
    let mut push = |byte: u8| {
        for j in (0..8).rev() {
            bits[i] = (byte >> j) & 1;
            i += 1;
        }
    };
    for &b in &S1_PATTERNS[s1 as usize] {
        push(b);
    }
    for &b in &S2_PATTERNS[s2 as usize] {
        push(b);
    }
    for &b in &S1_PATTERNS[s1 as usize] {
        push(b);
    }
    bits
}

/// The values on the active carriers, ±1: DBPSK of the bits (the reference
/// is +1), times the randomiser.
pub fn carrier_values(s1: u8, s2: u8) -> [i8; CARRIERS] {
    let bits = modulation_bits(s1, s2);
    let rnd = randomiser();
    let mut d = [1i8; CARRIERS + 1];
    for i in 1..=CARRIERS {
        d[i] = if bits[i - 1] == 1 {
            -d[i - 1]
        } else {
            d[i - 1]
        };
    }
    let mut out = [0i8; CARRIERS];
    for i in 0..CARRIERS {
        out[i] = d[i + 1] * rnd[i];
    }
    out
}

/// What reading a P1 symbol's carriers gave.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Signalling {
    pub s1: u8,
    pub s2: u8,
    /// How well the bits matched the patterns, 0 to 1 (1 for a clean read).
    pub quality: f32,
}

/// Read S1 and S2 from the 384 active carriers' complex values (any common
/// phase, and an unknown sign of the first one, is fine: DBPSK is
/// differential). Each of the 8 × 16 patterns is correlated against the
/// differential products of neighbouring carriers.
pub fn detect(values: &[(f32, f32)]) -> Option<Signalling> {
    if values.len() != CARRIERS {
        return None;
    }
    let rnd = randomiser();
    // d[i] ~ (−1)^bit[i] for i = 1..383: the product of neighbouring carriers
    // (carrier i's value is dbpsk[i+1]·rnd[i]) with the randomiser taken off.
    let mut d = [0f32; CARRIERS];
    for i in 1..CARRIERS {
        let (a, b) = (values[i], values[i - 1]);
        let re = a.0 * b.0 + a.1 * b.1;
        d[i] = re * f32::from(rnd[i]) * f32::from(rnd[i - 1]);
    }
    let norm: f32 = d.iter().map(|x| x.abs()).sum::<f32>().max(1e-12);
    let mut best: Option<(f32, u8, u8)> = None;
    let score_one = |s1: u8, s2: u8| {
        let bits = modulation_bits(s1, s2);
        // The product of carrier i with i−1 is (−1)^mod[i], the sign of the
        // bit modulating step i.
        let m: f32 = (1..CARRIERS)
            .map(|i| d[i] * if bits[i] == 1 { -1.0 } else { 1.0 })
            .sum();
        m / norm
    };
    // S1 and S2 are independent fields of the sequence, but scoring the 128
    // whole sequences is cheap enough.
    for s1 in 0..8u8 {
        for s2 in 0..16u8 {
            let m = score_one(s1, s2);
            if best.is_none_or(|(b, _, _)| m > b) {
                best = Some((m, s1, s2));
            }
        }
    }
    let (m, s1, s2) = best?;
    Some(Signalling {
        s1,
        s2,
        quality: m.max(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_active_carriers_are_384_in_order() {
        assert_eq!(P1_ACTIVE_CARRIERS.len(), CARRIERS);
        assert!(P1_ACTIVE_CARRIERS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(bin(0), 130);
        assert_eq!(bin(383), 895);
    }

    #[test]
    fn every_s1_s2_pair_reads_back() {
        for s1 in [0u8, 1, 2, 3, 4] {
            for s2 in 0..16u8 {
                let v = carrier_values(s1, s2);
                // A clean read at an arbitrary common phase.
                let (c, s) = (0.6f32, 0.8f32);
                let vals: Vec<(f32, f32)> = v
                    .iter()
                    .map(|&x| (f32::from(x) * c, f32::from(x) * s))
                    .collect();
                let r = detect(&vals).unwrap();
                assert_eq!((r.s1, r.s2), (s1, s2));
                assert!(r.quality > 0.99, "{}", r.quality);
            }
        }
    }

    #[test]
    fn a_noisy_read_still_finds_the_pattern() {
        let v = carrier_values(0, s2_value(5, 0));
        let mut seed = 12345u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let vals: Vec<(f32, f32)> = v
            .iter()
            .map(|&x| (f32::from(x) + 1.2 * noise(), 1.2 * noise()))
            .collect();
        let r = detect(&vals).unwrap();
        assert_eq!((r.s1, r.s2), (0, s2_value(5, 0)));
    }
}
