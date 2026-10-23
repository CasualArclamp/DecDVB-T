// made by CasualArclamp
//! DVB-T (EN 300 744) parameters the receiver and the GUI share.
//!
//! Only what the signal-quality card needs for now; M3 adds the OFDM modes
//! (FFT size, guard interval, TPS) here.

/// The carrier modulation (EN 300 744 §4.3.5; non-hierarchical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modulation {
    Qpsk,
    Qam16,
    Qam64,
}

impl Modulation {
    pub fn name(self) -> &'static str {
        match self {
            Modulation::Qpsk => "QPSK",
            Modulation::Qam16 => "16-QAM",
            Modulation::Qam64 => "64-QAM",
        }
    }
}

/// The inner code rate (punctured K = 7 convolutional code, §4.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rate {
    R1_2,
    R2_3,
    R3_4,
    R5_6,
    R7_8,
}

impl Rate {
    pub fn name(self) -> &'static str {
        match self {
            Rate::R1_2 => "1/2",
            Rate::R2_3 => "2/3",
            Rate::R3_4 => "3/4",
            Rate::R5_6 => "5/6",
            Rate::R7_8 => "7/8",
        }
    }
}

/// The C/N, dB, a Gaussian channel needs for quasi error free reception: a
/// BER of 2×10⁻⁴ after the Viterbi decoder, which the Reed–Solomon code
/// takes to under one uncorrected error an hour. EN 300 744 Annex A,
/// Table A.1 (non-hierarchical, ideal receiver).
///
/// Typed from the table as remembered: check against the spec text when the
/// receiver lands (M3).
pub fn qef_cn_db(m: Modulation, r: Rate) -> f32 {
    use Modulation::*;
    use Rate::*;
    match (m, r) {
        (Qpsk, R1_2) => 3.1,
        (Qpsk, R2_3) => 4.9,
        (Qpsk, R3_4) => 5.9,
        (Qpsk, R5_6) => 6.9,
        (Qpsk, R7_8) => 7.7,
        (Qam16, R1_2) => 8.8,
        (Qam16, R2_3) => 11.1,
        (Qam16, R3_4) => 12.5,
        (Qam16, R5_6) => 13.5,
        (Qam16, R7_8) => 13.9,
        (Qam64, R1_2) => 14.4,
        (Qam64, R2_3) => 16.5,
        (Qam64, R3_4) => 18.0,
        (Qam64, R5_6) => 19.3,
        (Qam64, R7_8) => 20.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_rise_with_rate_and_with_constellation() {
        let rates = [Rate::R1_2, Rate::R2_3, Rate::R3_4, Rate::R5_6, Rate::R7_8];
        for m in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
            let v: Vec<f32> = rates.iter().map(|&r| qef_cn_db(m, r)).collect();
            assert!(v.windows(2).all(|w| w[0] < w[1]), "{m:?} {v:?}");
        }
        for r in rates {
            assert!(qef_cn_db(Modulation::Qpsk, r) < qef_cn_db(Modulation::Qam16, r));
            assert!(qef_cn_db(Modulation::Qam16, r) < qef_cn_db(Modulation::Qam64, r));
        }
    }
}
