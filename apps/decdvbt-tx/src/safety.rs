// made by CasualArclamp
//! The safety rules of the window as pure functions, so each can be tested
//! without a window (docs/TX_DESIGN.md §6): which buttons are enabled, what
//! the banner says, when the arm is dropped, how fast the TX VGA may move.
//!
//! The window only *draws* what these decide. The library enforces the same
//! rules again underneath (`TxSession::transmit` refuses without an arm, and
//! `radio::open_hackrf` needs an `ArmedProof`), so a mistake here can make a
//! button look enabled but cannot key the radio.

use std::time::Duration;

use decdvbt_core::channel::uhf_channel_at;
use decdvbt_tx::{TxState, TxStatus};

/// The HackRF's TX VGA range, dB.
pub const VGA_MAX: u16 = 47;
/// The TX VGA moves at most this far in one step (one frame), so a slider
/// dragged to the end cannot jump the output power at once.
pub const VGA_STEP_MAX: u16 = 6;
/// The arm is dropped when the window has not had focus for this long.
pub const BLUR_DISARM: Duration = Duration::from_secs(5);

/// A session can be started: a file is chosen and nothing is running.
pub fn can_prepare(session_exists: bool, have_file: bool) -> bool {
    !session_exists && have_file
}

/// The ARM button works: the radio is the output, the session is `Ready`, and
/// the user ticked "I have read this". (A test output needs no arm.)
pub fn can_arm(state: TxState, sink_is_radio: bool, read_ack: bool) -> bool {
    sink_is_radio && read_ack && state == TxState::Ready
}

/// The TRANSMIT button works: `Ready`, and for the radio also armed. Test
/// outputs need no arm.
pub fn can_transmit(status: &TxStatus, armed: bool, sink_is_radio: bool) -> bool {
    status.state == TxState::Ready && (armed || !sink_is_radio)
}

/// The Pluto's most attenuation, dB.
pub const ATTEN_MAX: f32 = decdvbt_tx::PLUTO_ATTEN_MAX_DB;

/// One step of the Pluto's attenuation from `current` toward `target`, at
/// most [`VGA_STEP_MAX`] dB (as the TX VGA).
pub fn atten_step(current: f32, target: f32) -> f32 {
    let (current, target) = (current.clamp(0.0, ATTEN_MAX), target.clamp(0.0, ATTEN_MAX));
    let max = f32::from(VGA_STEP_MAX);
    current + (target - current).clamp(-max, max)
}

/// One step of the TX VGA from `current` toward `target` (both clamped to the
/// range), at most [`VGA_STEP_MAX`] dB.
pub fn vga_step(current: u16, target: u16) -> u16 {
    let (current, target) = (current.min(VGA_MAX), target.min(VGA_MAX));
    if target > current {
        current + (target - current).min(VGA_STEP_MAX)
    } else {
        current - (current - target).min(VGA_STEP_MAX)
    }
}

/// The big red banner: only while the radio is keyed (`status.radiating`, the
/// library's flag, and nothing else).
pub fn banner(status: &TxStatus, centre_hz: f64, pluto: bool) -> Option<String> {
    status.radiating.then(|| {
        if pluto {
            return format!(
                "TRANSMITTING - {:.3} MHz - Pluto attenuation {:.2} dB",
                centre_hz / 1e6,
                status.atten_db
            );
        }
        format!(
            "TRANSMITTING - {:.3} MHz - TX VGA {} dB - amp {}",
            centre_hz / 1e6,
            status.txvga_db,
            if status.amp { "on" } else { "off" }
        )
    })
}

/// The window's title bar.
pub fn window_title(status: &TxStatus) -> &'static str {
    if status.radiating {
        "TRANSMITTING - DecDVB-T TX"
    } else {
        "DecDVB-T TX"
    }
}

/// `centre_hz` is on a UHF television channel: the channel number and whether
/// it is one of the UK's Freeview channels (21 to 48 less 38).
pub fn television_channel(centre_hz: f64) -> Option<(u8, bool)> {
    let n = uhf_channel_at(centre_hz)?;
    Some((n, n <= 48 && n != 38))
}

/// The red line under the licence text, if the tuning is on a TV channel.
pub fn television_warning(centre_hz: f64) -> Option<String> {
    let (n, uk) = television_channel(centre_hz)?;
    Some(if uk {
        format!("This is a television channel (C{n}, UK Freeview).")
    } else {
        format!("This is a television channel (C{n}).")
    })
}

/// The arm is to be dropped: the window has been out of focus for longer than
/// [`BLUR_DISARM`].
pub fn disarm_on_blur(unfocused_for: Option<Duration>) -> bool {
    unfocused_for.is_some_and(|d| d > BLUR_DISARM)
}

/// Why a control is greyed out, for its tooltip.
pub const STOP_FIRST: &str = "Stop first";

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_tx::{Arm, TxConfig};

    fn status(state: TxState) -> TxStatus {
        let mut s = TxStatus::idle(&TxConfig::new("x.mp4".into()), &Arm::new());
        s.state = state;
        s
    }

    #[test]
    fn transmit_needs_ready_and_for_the_radio_an_arm() {
        let ready = status(TxState::Ready);
        // The radio: only when armed.
        assert!(!can_transmit(&ready, false, true));
        assert!(can_transmit(&ready, true, true));
        // A test output: no arm needed.
        assert!(can_transmit(&ready, false, false));
        // Never outside Ready, armed or not.
        for st in [
            TxState::Idle,
            TxState::Preparing,
            TxState::Transmitting,
            TxState::Finishing,
            TxState::Finished,
            TxState::Failed,
        ] {
            let s = status(st);
            assert!(!can_transmit(&s, true, true), "{st:?}");
            assert!(!can_transmit(&s, true, false), "{st:?}");
        }
    }

    #[test]
    fn arming_needs_ready_the_radio_and_the_tick() {
        assert!(can_arm(TxState::Ready, true, true));
        assert!(!can_arm(TxState::Ready, true, false), "no tick");
        assert!(!can_arm(TxState::Ready, false, true), "not the radio");
        assert!(!can_arm(TxState::Preparing, true, true), "not ready");
        assert!(!can_arm(TxState::Transmitting, true, true), "already on");
        assert!(!can_arm(TxState::Idle, true, true));
    }

    #[test]
    fn prepare_needs_a_file_and_no_session() {
        assert!(can_prepare(false, true));
        assert!(!can_prepare(false, false));
        assert!(!can_prepare(true, true));
    }

    #[test]
    fn the_vga_never_jumps_more_than_six_db() {
        assert_eq!(vga_step(0, 47), 6);
        assert_eq!(vga_step(6, 47), 12);
        assert_eq!(vga_step(44, 47), 47);
        assert_eq!(vga_step(30, 0), 24);
        assert_eq!(vga_step(3, 0), 0);
        assert_eq!(vga_step(10, 10), 10);
        // Out-of-range values are pulled in.
        assert_eq!(vga_step(0, 200), 6);
        assert_eq!(vga_step(200, 200), 47);
        // From 0 to 47 takes 8 steps.
        let (mut v, mut n) = (0, 0);
        while v != 47 {
            v = vga_step(v, 47);
            n += 1;
        }
        assert_eq!(n, 8);
    }

    #[test]
    fn the_banner_is_the_radiating_flag_and_nothing_else() {
        let mut s = status(TxState::Transmitting);
        // Transmitting to a test sink: no banner.
        assert_eq!(banner(&s, 546e6, false), None);
        assert_eq!(window_title(&s), "DecDVB-T TX");
        s.radiating = true;
        s.txvga_db = 12;
        s.amp = false;
        assert_eq!(
            banner(&s, 546e6, false).as_deref(),
            Some("TRANSMITTING - 546.000 MHz - TX VGA 12 dB - amp off")
        );
        assert_eq!(window_title(&s), "TRANSMITTING - DecDVB-T TX");
        s.amp = true;
        assert!(banner(&s, 546e6, false).unwrap().ends_with("amp on"));
        // Even Ready or Failed with the flag up would show it: the flag is
        // the library's statement that the radio is keyed.
        s.state = TxState::Failed;
        assert!(banner(&s, 546e6, false).is_some());
    }

    #[test]
    fn television_channels_are_flagged() {
        // C30 centre: UK Freeview.
        let w = television_warning(546e6).unwrap();
        assert!(w.contains("C30") && w.contains("UK Freeview"), "{w}");
        // C38 is a TV channel on the raster but not a UK one.
        let w = television_warning(610e6).unwrap();
        assert!(w.contains("C38") && !w.contains("Freeview"), "{w}");
        // C60 (not UK, still UHF TV).
        assert!(television_warning(786e6).unwrap().contains("C60"));
        // An ISM frequency is not.
        assert_eq!(television_warning(433.92e6), None);
        assert_eq!(television_warning(2.45e9), None);
    }

    #[test]
    fn the_arm_drops_after_five_seconds_out_of_focus() {
        assert!(!disarm_on_blur(None));
        assert!(!disarm_on_blur(Some(Duration::from_secs(4))));
        assert!(disarm_on_blur(Some(Duration::from_millis(5001))));
    }

    #[test]
    fn the_pluto_banner_and_attenuation_steps() {
        let mut s = TxStatus::idle(
            &decdvbt_tx::TxConfig::new("x.mp4".into()),
            &decdvbt_tx::Arm::new(),
        );
        s.radiating = true;
        s.atten_db = 40.0;
        assert_eq!(
            banner(&s, 2400.25e6, true).as_deref(),
            Some("TRANSMITTING - 2400.250 MHz - Pluto attenuation 40.00 dB")
        );
        // Less attenuation (more power) by at most 6 dB a step.
        assert_eq!(atten_step(89.75, 0.0), 83.75);
        assert_eq!(atten_step(10.0, 89.75), 16.0);
        assert_eq!(atten_step(3.0, 0.0), 0.0);
    }
}
