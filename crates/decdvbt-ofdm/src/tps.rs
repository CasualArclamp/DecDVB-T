// made by CasualArclamp
//! Transmission parameter signalling (EN 300 744 §4.6): 68 bits sent one a
//! symbol, differentially, on the TPS carriers; the same on every one of
//! them, so a receiver can vote.
//!
//! | bits      | field                                              |
//! |-----------|----------------------------------------------------|
//! | s0        | initialisation (the DBPSK reference, not data)     |
//! | s1–s16    | sync word: one for frames 1 and 3, another for 2, 4 |
//! | s17–s22   | length indicator (23 bits to follow, 31 with a cell id) |
//! | s23–s24   | frame number in the superframe                     |
//! | s25–s26   | constellation                                      |
//! | s27–s29   | hierarchy (α)                                      |
//! | s30–s32   | code rate, HP stream                               |
//! | s33–s35   | code rate, LP stream                               |
//! | s36–s37   | guard interval                                     |
//! | s38–s39   | transmission mode (FFT size)                       |
//! | s40–s47   | cell identifier (high byte in frames 1, 3; low in 2, 4) |
//! | s48–s53   | zero                                               |
//! | s54–s67   | BCH(67,53) parity                                  |
//!
//! Field codes follow gr-dtv's `format_tps_data` (GPL-3, Free Software
//! Foundation) and the spec's Tables 9–12.

use decdvbt_core::dvbt::{Alpha, FftSize, Guard, Mode, Modulation, Rate};

/// Bits in a TPS block.
pub const TPS_BITS: usize = 68;

/// s1–s16 of the first and third frame of a superframe, and of the second
/// and fourth (§4.6.2.2).
const SYNC_EVEN: u16 = 0x35EE;
const SYNC_ODD: u16 = 0xCA11;

/// The BCH generator x¹⁴ + x⁹ + x⁸ + x⁶ + x⁵ + x⁴ + x² + x + 1, less its x¹⁴
/// (§4.6.2.11).
const BCH_G: u16 = 0x0377;

/// What a TPS block says about the transmission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tps {
    pub mode: Mode,
    /// The 16-bit cell identifier, sent a byte a frame; `None` leaves it
    /// out (length indicator 23 and the identifier bits zero).
    pub cell_id: Option<u16>,
}

/// What one received block says, before the other half of the cell id is
/// known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpsFrame {
    /// The frame's place in the superframe, 0–3.
    pub frame: u8,
    pub modulation: Modulation,
    pub alpha: Alpha,
    pub rate_hp: Rate,
    pub rate_lp: Rate,
    pub guard: Guard,
    pub fft: FftSize,
    /// s17–s22: 23, or 31 when the cell identifier is sent.
    pub length: u8,
    /// s40–s47.
    pub cell_id_byte: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpsError {
    /// s1–s16 is neither sync word.
    Sync,
    /// The BCH parity does not match.
    Parity,
    /// A field holds a code the standard does not use (4k mode, say).
    Field(&'static str),
}

/// The 14 parity bits for s1–s53: the remainder of x¹⁴·m(x) over g(x), the
/// x¹³ coefficient first. The code is BCH(127,113) shortened to (67,53): 60
/// leading zeros, which change no remainder, are left out.
pub fn bch_parity(s1_s53: &[u8]) -> [u8; 14] {
    debug_assert_eq!(s1_s53.len(), 53);
    let mut reg: u16 = 0;
    for &b in s1_s53 {
        let feedback = ((reg >> 13) & 1) ^ u16::from(b & 1);
        reg = (reg << 1) & 0x3FFF;
        if feedback == 1 {
            reg ^= BCH_G;
        }
    }
    std::array::from_fn(|i| ((reg >> (13 - i)) & 1) as u8)
}

fn set_bits(w: &mut [u8; TPS_BITS], first: usize, last: usize, value: u32) {
    // `first`–`last` inclusive, the most significant bit first.
    let n = last - first + 1;
    for i in 0..n {
        w[first + i] = ((value >> (n - 1 - i)) & 1) as u8;
    }
}

fn get_bits(w: &[u8; TPS_BITS], first: usize, last: usize) -> u32 {
    w[first..=last]
        .iter()
        .fold(0, |acc, &b| (acc << 1) | u32::from(b & 1))
}

fn rate_code(r: Rate) -> u32 {
    match r {
        Rate::R1_2 => 0,
        Rate::R2_3 => 1,
        Rate::R3_4 => 2,
        Rate::R5_6 => 3,
        Rate::R7_8 => 4,
    }
}

fn rate_from(code: u32) -> Option<Rate> {
    Rate::ALL.get(code as usize).copied()
}

impl Tps {
    /// The 68 bits of frame `frame` (0–3) of the superframe. s0 is 0: the
    /// carriers' first symbol takes its reference from the PRBS instead.
    pub fn word(&self, frame: usize) -> [u8; TPS_BITS] {
        let m = &self.mode;
        let mut w = [0u8; TPS_BITS];
        let frame = frame % 4;
        set_bits(
            &mut w,
            1,
            16,
            u32::from(if frame.is_multiple_of(2) {
                SYNC_EVEN
            } else {
                SYNC_ODD
            }),
        );
        set_bits(&mut w, 17, 22, if self.cell_id.is_some() { 31 } else { 23 });
        set_bits(&mut w, 23, 24, frame as u32);
        set_bits(
            &mut w,
            25,
            26,
            match m.modulation {
                Modulation::Qpsk => 0,
                Modulation::Qam16 => 1,
                Modulation::Qam64 => 2,
            },
        );
        set_bits(
            &mut w,
            27,
            29,
            match m.alpha {
                Alpha::NonHierarchical => 0,
                Alpha::A1 => 1,
                Alpha::A2 => 2,
                Alpha::A4 => 3,
            },
        );
        set_bits(&mut w, 30, 32, rate_code(m.rate_hp));
        set_bits(&mut w, 33, 35, rate_code(m.rate_lp));
        set_bits(
            &mut w,
            36,
            37,
            Guard::ALL.iter().position(|g| *g == m.guard).unwrap() as u32,
        );
        set_bits(
            &mut w,
            38,
            39,
            match m.fft {
                FftSize::K2 => 0,
                FftSize::K8 => 1,
            },
        );
        if let Some(id) = self.cell_id {
            let byte = if frame.is_multiple_of(2) {
                id >> 8
            } else {
                id & 0xFF
            };
            set_bits(&mut w, 40, 47, u32::from(byte));
        }
        let parity = bch_parity(&w[1..=53]);
        w[54..].copy_from_slice(&parity);
        w
    }
}

/// Read a block: the sync word, the parity, then the fields.
pub fn parse(w: &[u8; TPS_BITS]) -> Result<TpsFrame, TpsError> {
    let sync = get_bits(w, 1, 16) as u16;
    if sync != SYNC_EVEN && sync != SYNC_ODD {
        return Err(TpsError::Sync);
    }
    if bch_parity(&w[1..=53])[..] != w[54..] {
        return Err(TpsError::Parity);
    }
    let frame = get_bits(w, 23, 24) as u8;
    // The sync word and the frame number agree on odd and even.
    if (sync == SYNC_ODD) != (frame % 2 == 1) {
        return Err(TpsError::Field("frame number"));
    }
    Ok(TpsFrame {
        frame,
        length: get_bits(w, 17, 22) as u8,
        modulation: match get_bits(w, 25, 26) {
            0 => Modulation::Qpsk,
            1 => Modulation::Qam16,
            2 => Modulation::Qam64,
            _ => return Err(TpsError::Field("constellation")),
        },
        alpha: match get_bits(w, 27, 29) {
            0 => Alpha::NonHierarchical,
            1 => Alpha::A1,
            2 => Alpha::A2,
            3 => Alpha::A4,
            _ => return Err(TpsError::Field("hierarchy")),
        },
        rate_hp: rate_from(get_bits(w, 30, 32)).ok_or(TpsError::Field("HP code rate"))?,
        rate_lp: rate_from(get_bits(w, 33, 35)).ok_or(TpsError::Field("LP code rate"))?,
        guard: Guard::ALL[get_bits(w, 36, 37) as usize],
        fft: match get_bits(w, 38, 39) {
            0 => FftSize::K2,
            1 => FftSize::K8,
            _ => return Err(TpsError::Field("transmission mode (4k is DVB-H)")),
        },
        cell_id_byte: get_bits(w, 40, 47) as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gr-dtv's `generate_bch_code`, as it is: a right-shifting register
    /// with the feedback put in at bit 13, over 60 zeros and s1–s53.
    fn grdtv_parity(s1_s53: &[u8]) -> [u8; 14] {
        let mut data = vec![0u8; 60];
        data.extend_from_slice(s1_s53);
        let mut reg: u32 = 0;
        for &d in &data {
            let feedback = 1 & (u32::from(d) ^ reg);
            reg >>= 1;
            reg |= feedback << 13;
            reg ^= (feedback << 12)
                ^ (feedback << 11)
                ^ (feedback << 9)
                ^ (feedback << 8)
                ^ (feedback << 7)
                ^ (feedback << 5)
                ^ (feedback << 4);
        }
        std::array::from_fn(|i| ((reg >> i) & 1) as u8)
    }

    fn uk() -> Tps {
        Tps {
            mode: Mode::uk(Modulation::Qam64, Rate::R2_3),
            cell_id: None,
        }
    }

    #[test]
    fn the_parity_matches_grdtvs_shift_register() {
        // Every one-hot message, and some mixed ones: a linear code, so
        // agreement on the basis and on mixtures is agreement everywhere.
        for i in 0..53 {
            let mut m = [0u8; 53];
            m[i] = 1;
            assert_eq!(bch_parity(&m), grdtv_parity(&m), "bit {i}");
        }
        let mut x: u32 = 0x1234_5678;
        for _ in 0..200 {
            let m: [u8; 53] = std::array::from_fn(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8 & 1
            });
            assert_eq!(bch_parity(&m), grdtv_parity(&m));
        }
    }

    #[test]
    fn words_carry_the_sync_words_and_frame_numbers() {
        let t = uk();
        for f in 0..4 {
            let w = t.word(f);
            let sync = get_bits(&w, 1, 16) as u16;
            assert_eq!(sync, if f % 2 == 0 { SYNC_EVEN } else { SYNC_ODD });
            assert_eq!(get_bits(&w, 23, 24), f as u32);
            assert_eq!(get_bits(&w, 17, 22), 23);
        }
        // The spec's sync words, bit for bit.
        let w = t.word(0);
        assert_eq!(
            &w[1..=16],
            &[0, 0, 1, 1, 0, 1, 0, 1, 1, 1, 1, 0, 1, 1, 1, 0]
        );
        let w = t.word(1);
        assert_eq!(
            &w[1..=16],
            &[1, 1, 0, 0, 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 1]
        );
    }

    #[test]
    fn every_mode_round_trips_through_a_word() {
        for m in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
            for r in Rate::ALL {
                for g in Guard::ALL {
                    for fft in [FftSize::K2, FftSize::K8] {
                        let mut mode = Mode::uk(m, r);
                        mode.guard = g;
                        mode.fft = fft;
                        let t = Tps {
                            mode,
                            cell_id: Some(0xABCD),
                        };
                        for f in 0..4 {
                            let p = parse(&t.word(f)).unwrap();
                            assert_eq!(p.frame as usize, f);
                            assert_eq!((p.modulation, p.rate_hp, p.guard, p.fft), (m, r, g, fft));
                            assert_eq!(p.length, 31);
                            // High byte on frames 1, 3; low on 2, 4.
                            assert_eq!(p.cell_id_byte, if f % 2 == 0 { 0xAB } else { 0xCD });
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn one_wrong_bit_is_caught_by_the_parity() {
        let w = uk().word(2);
        for i in 17..=67 {
            let mut bad = w;
            bad[i] ^= 1;
            assert!(parse(&bad).is_err(), "bit {i}");
        }
        // BCH(127,113) corrects two errors (distance at least 5), so any
        // four or fewer are caught: two are.
        let mut bad = w;
        bad[30] ^= 1;
        bad[50] ^= 1;
        assert_eq!(parse(&bad), Err(TpsError::Parity));
    }
}
