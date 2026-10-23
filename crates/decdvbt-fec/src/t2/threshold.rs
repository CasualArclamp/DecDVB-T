// made by CasualArclamp
//! The C/N each T2 FEC mode needs from this receiver's decoder in a Gaussian
//! channel with the noise known exactly: the lowest C/N at which twelve
//! blocks in a row decode (a bisection in 0.1 dB steps, then 0.35 dB more so
//! that the decode is stable rather than lucky), per frame size, rotation,
//! constellation and code rate.
//!
//! These are this decoder's, not the standard's: non-iterative demapping and
//! 8-bit min-sum cost 0.5 dB (QPSK) to 1.3 dB (256-QAM 2/3) against the
//! figures of TS 102 831 Table 44 (genie-aided demapping, belief propagation:
//! 1,0 dB for QPSK 1/2, 13,5 dB for 64-QAM 2/3, 17,8 dB for 256-QAM 2/3). The
//! margin the signal card shows is against these.
//!
//! The table is generated: `cargo run --release -p decdvbt-fec --example
//! t2_thresholds`. Indexed `[short][rotated][qam][rate]`.

use decdvbt_core::{CodeRate, FecFrame};

use super::qam::Qam;

const CN_DB: [[[[f32; 6]; 4]; 2]; 2] = [
    [
        [
            [1.8, 3.3, 3.4, 4.3, 4.9, 5.3],
            [7.1, 8.5, 9.2, 10.5, 11.3, 11.8],
            [11.6, 13.1, 14.3, 15.9, 16.8, 17.4],
            [15.7, 17.5, 19.4, 21.0, 22.0, 22.8],
        ],
        [
            [1.8, 3.3, 3.4, 4.3, 5.0, 5.4],
            [7.1, 8.4, 9.1, 10.4, 11.3, 11.8],
            [11.5, 13.1, 14.2, 15.9, 16.8, 17.4],
            [15.7, 17.6, 19.4, 21.0, 22.1, 22.8],
        ],
    ],
    [
        [
            [1.0, 2.8, 3.4, 4.3, 4.9, 5.4],
            [6.2, 8.6, 9.2, 10.4, 11.1, 11.7],
            [10.2, 13.6, 14.3, 15.5, 16.4, 17.2],
            [14.4, 19.1, 19.2, 20.5, 21.6, 22.4],
        ],
        [
            [0.9, 2.8, 3.4, 4.3, 4.9, 5.6],
            [6.2, 8.6, 9.2, 10.4, 11.1, 11.8],
            [10.2, 13.5, 14.3, 15.7, 16.5, 17.2],
            [14.4, 19.0, 19.2, 20.5, 21.7, 22.5],
        ],
    ],
];

/// The C/N, dB, this decoder needs for a mode; `None` for a code rate the
/// table lacks (the T2-Lite rates 1/3 and 2/5).
pub fn qef_cn_db(frame: FecFrame, rate: CodeRate, qam: Qam, rotated: bool) -> Option<f32> {
    let r = match (rate.num, rate.den) {
        (1, 2) => 0,
        (3, 5) => 1,
        (2, 3) => 2,
        (3, 4) => 3,
        (4, 5) => 4,
        (5, 6) => 5,
        _ => return None,
    };
    let q = match qam {
        Qam::Qpsk => 0,
        Qam::Qam16 => 1,
        Qam::Qam64 => 2,
        Qam::Qam256 => 3,
    };
    let f = usize::from(frame == FecFrame::Short);
    Some(CN_DB[f][usize::from(rotated)][q][r])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn higher_orders_and_rates_need_more() {
        let at = |q, r| qef_cn_db(FecFrame::Normal, r, q, false).unwrap();
        let (half, five_sixths) = (CodeRate::new(1, 2), CodeRate::new(5, 6));
        assert!(at(Qam::Qpsk, half) < at(Qam::Qam16, half));
        assert!(at(Qam::Qam64, half) < at(Qam::Qam256, half));
        assert!(at(Qam::Qam64, half) < at(Qam::Qam64, five_sixths));
        assert!(qef_cn_db(FecFrame::Normal, CodeRate::new(1, 3), Qam::Qpsk, false).is_none());
    }
}
