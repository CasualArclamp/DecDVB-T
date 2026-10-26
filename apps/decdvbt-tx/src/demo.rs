// made by CasualArclamp
//! `--demo`: a made-up status and spectrum, so the window can be rendered (for
//! `--screenshot`) with no session, no file, no ffmpeg and no radio.
//!
//! **There is no session behind a demo.** The window creates no [`crate::host::Host`]
//! when it is on, so no button can reach the library; the "radiating" state
//! below is a drawn picture of the red banner and nothing more.

use decdvbt_tx::ffmpeg::VideoInfo;
use decdvbt_tx::{PreviewRow, TxConfig, TxState, TxStatus};

/// Which state `--state` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoState {
    Idle,
    Preparing,
    Ready,
    /// Ready, with the radio output armed (47 s left).
    Armed,
    /// The radio is keyed: the red frame, banner and title.
    Transmitting,
    /// A test output running: neutral, no banner.
    Running,
    /// A test output running with the encoder falling behind (amber meters).
    Stuffing,
    /// Everything wrong at once (red meters).
    Trouble,
    Failed,
    Finished,
}

impl DemoState {
    pub const NAMES: &'static str =
        "idle|preparing|ready|armed|transmitting|running|stuffing|trouble|failed|finished";

    pub fn parse(s: &str) -> Option<DemoState> {
        Some(match s {
            "idle" => DemoState::Idle,
            "preparing" => DemoState::Preparing,
            "ready" => DemoState::Ready,
            "armed" => DemoState::Armed,
            "transmitting" => DemoState::Transmitting,
            "running" => DemoState::Running,
            "stuffing" => DemoState::Stuffing,
            "trouble" => DemoState::Trouble,
            "failed" => DemoState::Failed,
            "finished" => DemoState::Finished,
            _ => return None,
        })
    }

    /// The demo draws the radio as the output for these.
    pub fn radio_output(self) -> bool {
        matches!(self, DemoState::Armed | DemoState::Transmitting)
    }

    /// Whether a session is pretended (the controls lock).
    pub fn has_session(self) -> bool {
        self != DemoState::Idle
    }
}

/// The file the demo pretends to have opened.
pub fn video_info() -> VideoInfo {
    VideoInfo {
        duration: Some(std::time::Duration::from_secs(192)),
        width: 1920,
        height: 1080,
        fps: 25.0,
        video_codec: "h264".into(),
        audio_codec: Some("aac".into()),
        audio_channels: 2,
        interlaced: false,
        bit_depth: 8,
        bitrate: Some(12_400_000),
    }
}

/// A status for `state` at `t` seconds into the demo.
pub fn status(state: DemoState, cfg: &TxConfig, t: f64) -> TxStatus {
    let mut s = TxStatus::idle(cfg, &decdvbt_tx::Arm::new());
    let wobble = |period: f64, amp: f64| (t * std::f64::consts::TAU / period).sin() * amp;
    s.sample_rate_hz = cfg.sample_rate_hz();
    s.ts_rate_bps = cfg.mode.bitrate_hp();
    s.chain = "NVENC (CUDA)".into();
    match state {
        DemoState::Idle => {
            s.chain.clear();
            return s;
        }
        DemoState::Preparing => {
            s.state = TxState::Preparing;
            let p = ((t * 0.35) % 1.0) as f32;
            s.ts_fill = 0.34 * p;
            s.iq_fill = p;
            s.ffmpeg_cpu_pct = 14.0;
            s.process_cpu_pct = 31.0;
            return s;
        }
        DemoState::Failed => {
            s.state = TxState::Failed;
            s.error = Some("ffmpeg exited with code 1".into());
            s.ffmpeg_log = vec![
                "[in#0 @ 000001] Error opening input: Invalid data found when processing input"
                    .into(),
                "Error opening input file broken.mp4.".into(),
            ];
            s.chain.clear();
            return s;
        }
        _ => {}
    }
    s.state = match state {
        DemoState::Ready | DemoState::Armed => TxState::Ready,
        DemoState::Finished => TxState::Finished,
        _ => TxState::Transmitting,
    };
    s.armed = state == DemoState::Armed;
    s.arm_seconds_left = s.armed.then_some(47.0);
    s.radiating = state == DemoState::Transmitting;
    s.ts_in_bps = s.ts_rate_bps;
    s.ts_fill = (0.93 + wobble(7.0, 0.04)) as f32;
    s.iq_fill = (0.97 + wobble(5.0, 0.02)) as f32;
    s.modulator_load_pct = (36.0 + wobble(4.0, 3.0)) as f32;
    s.modulator_speed_x = 100.0 / s.modulator_load_pct;
    s.ffmpeg_cpu_pct = (11.0 + wobble(6.0, 2.0)) as f32;
    s.ffmpeg_speed_x = 1.5;
    s.process_cpu_pct = (41.0 + wobble(3.0, 3.0)) as f32;
    s.level_rms_dbfs = -12.0;
    s.peak_dbfs = -2.3;
    s.clipped = 2;
    if matches!(state, DemoState::Transmitting | DemoState::Running) {
        s.txvga_db = if state == DemoState::Transmitting {
            12
        } else {
            0
        };
    }
    match state {
        DemoState::Ready | DemoState::Armed => {
            s.ts_fill = 0.34;
            s.iq_fill = 1.0;
            s.ts_in_bps = 0.0;
            s.modulator_load_pct = 0.0;
            s.process_cpu_pct = 9.0;
            s.ffmpeg_cpu_pct = 2.0;
        }
        DemoState::Transmitting | DemoState::Running => {
            s.sent_s = 161.0 + t;
            s.programme_s = 163.0 + t;
        }
        DemoState::Stuffing => {
            s.sent_s = 54.0 + t;
            s.programme_s = 55.0 + t;
            s.ts_fill = 0.17;
            s.ts_stuffed = 1432 + (t * 40.0) as u64;
            s.ffmpeg_cpu_pct = 96.0;
            s.ffmpeg_speed_x = 0.93;
            s.chain = "x264 veryfast".into();
        }
        DemoState::Trouble => {
            s.sent_s = 88.0 + t;
            s.programme_s = 89.0 + t;
            s.ts_fill = 0.12;
            s.ts_stuffed = 5120 + (t * 90.0) as u64;
            s.underruns = 3;
            s.dts_pcr_warnings = 41;
            s.modulator_load_pct = 94.0;
            s.clipped = 2 + (t * 300.0) as u64;
            s.chain = "x264 veryfast".into();
        }
        DemoState::Finished => {
            s.sent_s = 192.0;
            s.programme_s = 192.0;
            s.ts_fill = 0.0;
            s.iq_fill = 0.0;
        }
        _ => {}
    }
    s
}

/// A deterministic pseudo-random value in -1..1 for `(row, bin)`.
fn noise(row: u64, bin: u64) -> f32 {
    let mut x = row.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ bin.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    x ^= x >> 31;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 29;
    ((x >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32
}

/// One made-up spectrum row: a flat-topped band of `occupied_hz` at about
/// -45 dBFS a bin, shoulders falling away outside it, an 8-bit noise floor and
/// a little ripple, as a 64-QAM OFDM signal looks at -12 dBFS.
pub fn row(index: u64, sample_rate_hz: f64, occupied_hz: f64) -> PreviewRow {
    const BINS: usize = decdvbt_tx::preview::BINS;
    let half = occupied_hz / 2.0;
    let bins = (0..BINS)
        .map(|i| {
            let f = ((i as f64 + 0.5) / BINS as f64 - 0.5) * sample_rate_hz;
            let out = (f.abs() - half).max(0.0);
            // 40 dB per MHz of skirt, down to the floor.
            let body = -45.0 - 40.0 * (out / 1e6) as f32;
            let floor = -78.0;
            let level = body.max(floor);
            level + 1.2 * noise(index, i as u64) + 0.5 * noise(0, i as u64)
        })
        .collect();
    PreviewRow {
        bins,
        sample_rate_hz,
        at_sample: index * (sample_rate_hz / 20.0) as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_transmitting_demo_is_radiating() {
        let cfg = TxConfig::new("x.mp4".into());
        for st in [
            DemoState::Idle,
            DemoState::Preparing,
            DemoState::Ready,
            DemoState::Armed,
            DemoState::Running,
            DemoState::Stuffing,
            DemoState::Trouble,
            DemoState::Failed,
            DemoState::Finished,
        ] {
            assert!(!status(st, &cfg, 1.0).radiating, "{st:?}");
        }
        assert!(status(DemoState::Transmitting, &cfg, 1.0).radiating);
    }

    #[test]
    fn the_names_parse() {
        for n in DemoState::NAMES.split('|') {
            assert!(DemoState::parse(n).is_some(), "{n}");
        }
        assert!(DemoState::parse("hackrf").is_none());
    }

    #[test]
    fn the_spectrum_has_a_band_and_a_floor() {
        let r = row(3, 64e6 / 7.0, 7.61e6);
        assert_eq!(r.bins.len(), 1024);
        let mid = r.bins[512];
        let edge_out = r.bins[2];
        assert!(mid > -50.0 && mid < -40.0, "{mid}");
        assert!(edge_out < -65.0, "{edge_out}");
    }
}
