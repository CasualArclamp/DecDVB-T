// made by CasualArclamp
//! `--selftest`: the safety logic and the defaults checked in the built exe,
//! with no window, no ffmpeg and no radio. Prints one line a check and exits
//! 0 if all hold. (The same checks are unit tests; this is for a release exe.)

use decdvbt_tx::{
    Arm, HackRfTxConfig, LICENCE_WARNING, LicenceAck, TxConfig, TxSession, TxSink, TxState,
    TxStatus,
};

use crate::modeinfo;
use crate::safety;
use crate::settings::Settings;

/// Run every check; the process exit code.
pub fn run() -> i32 {
    let mut failed = 0;
    let mut check = |name: &str, ok: bool| {
        println!("{} {name}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failed += 1;
        }
    };

    let status = |state: TxState| {
        let mut s = TxStatus::idle(&TxConfig::new("x.mp4".into()), &Arm::new());
        s.state = state;
        s
    };

    check("a new config uses the test sink", {
        let c = TxConfig::new("x.mp4".into());
        !c.sink.is_radio() && c.validate().is_ok()
    });
    check(
        "the default settings start on the UK mode and the test output",
        {
            let s = Settings::default();
            s.mode == modeinfo::presets()[0].mode && s.centre_hz == 546e6
        },
    );
    check("every preset validates", {
        modeinfo::presets().iter().all(|p| {
            let mut c = TxConfig::new("x.mp4".into());
            c.mode = p.mode;
            c.t2 = p.t2.clone();
            c.validate().is_ok()
        })
    });
    check("a new arm is disarmed", !Arm::new().is_armed());
    check(
        "the arm needs the exact licence text",
        LicenceAck::accept("I accept").is_none() && LicenceAck::accept(LICENCE_WARNING).is_some(),
    );
    check("transmit to the radio needs Ready and an arm", {
        let ready = status(TxState::Ready);
        !safety::can_transmit(&ready, false, true) && safety::can_transmit(&ready, true, true)
    });
    check("transmit is never offered outside Ready", {
        [
            TxState::Idle,
            TxState::Preparing,
            TxState::Transmitting,
            TxState::Finishing,
            TxState::Finished,
            TxState::Failed,
        ]
        .into_iter()
        .all(|s| !safety::can_transmit(&status(s), true, true))
    });
    check("the TX VGA moves at most 6 dB a step", {
        safety::vga_step(0, 47) == 6 && safety::vga_step(47, 0) == 41
    });
    check("the radio sink defaults are TX VGA 0, amp off", {
        let h = HackRfTxConfig::default();
        h.txvga_db == 0 && !h.amp
    });
    check("the library refuses an unarmed radio session", {
        let mut c = TxConfig::new("x.mp4".into());
        c.sink = TxSink::HackRf(HackRfTxConfig::default());
        // Never opens anything: with no arm, `transmit` stops at "not armed".
        match TxSession::new(c) {
            Ok(mut s) => s
                .transmit()
                .err()
                .is_some_and(|e| e.to_string().contains("not armed")),
            Err(_) => false,
        }
    });
    check("the prefs file is this app's own", {
        crate::prefs::file().is_some_and(|p| {
            let s = p.display().to_string();
            std::env::var_os("DECDVBT_TX_PREFS").is_some()
                || s.contains("DecDVB-T-TX")
                || s.contains("decdvbt-tx")
        })
    });

    if failed == 0 {
        println!("selftest: all checks passed");
        0
    } else {
        println!("selftest: {failed} check(s) FAILED");
        1
    }
}
