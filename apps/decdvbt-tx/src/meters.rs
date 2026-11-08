// made by CasualArclamp
//! The progress of `Preparing`. (The meters themselves are drawn by the
//! window, as DecSAT TX draws its own: `App::meters`.)

use decdvbt_tx::TxStatus;

/// How far `Preparing` has got, 0..1: half is the TS ring holding its prefill
/// (0.5 s of programme in a ring that holds 1.5 s), half the IQ ring full.
pub fn prepare_progress(s: &TxStatus) -> f32 {
    let ts = (s.ts_fill as f64 * decdvbt_tx::session::TS_RING_SECONDS
        / decdvbt_tx::session::TS_PREFILL_SECONDS)
        .min(1.0);
    (((ts + s.iq_fill as f64) / 2.0) as f32).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_tx::{Arm, TxConfig, TxState};

    #[test]
    fn preparing_progress_fills_to_one() {
        let mut s = TxStatus::idle(&TxConfig::new("x.mp4".into()), &Arm::new());
        s.state = TxState::Preparing;
        s.ts_fill = 0.0;
        s.iq_fill = 0.0;
        assert_eq!(prepare_progress(&s), 0.0);
        // The TS ring at a third (0.5 s of 1.5 s) and the IQ ring full.
        s.ts_fill = 1.0 / 3.0;
        s.iq_fill = 1.0;
        assert!((prepare_progress(&s) - 1.0).abs() < 1e-6);
        s.ts_fill = 1.0;
        assert_eq!(prepare_progress(&s), 1.0);
    }
}
