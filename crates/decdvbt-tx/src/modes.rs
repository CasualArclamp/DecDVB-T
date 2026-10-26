// made by CasualArclamp
//! The DVB-T modes a transmission can use, the transport stream rate each
//! carries, and the video profile (picture size and bitrates) that fits it.
//!
//! The modulator takes packets at exactly the channel's useful rate and never
//! inserts nulls, so ffmpeg must be told to mux at that rate (`-muxrate`).
//! The rate is EN 300 744 Table 17's: data carriers x bits x code rate x
//! 188/204 / Ts.

use decdvbt_core::dvbt::{Bandwidth, FftSize, Guard, Mode, Modulation, Rate};

/// The useful transport stream rate for `mode`, whole bit/s, rounded **down**
/// so ffmpeg's stream is never faster than the radio drains it.
pub fn mux_rate_bps(mode: &Mode) -> u64 {
    mode.bitrate_hp().floor() as u64
}

/// One row of the mode table the GUI offers.
#[derive(Debug, Clone, PartialEq)]
pub struct ModeRow {
    pub mode: Mode,
    /// "64-QAM 2/3 1/32".
    pub label: String,
    /// Useful bitrate, bit/s (exact; `mux_rate_bps` is its floor).
    pub bitrate: f64,
    /// C/N needed for quasi error free reception, dB (EN 300 744 Table A.1).
    pub qef_cn_db: f32,
}

/// Every non-hierarchical mode at `fft`/`bandwidth`, in table order:
/// constellation, then code rate, then guard (1/32 first).
fn walk(fft: FftSize, bandwidth: Bandwidth) -> impl Iterator<Item = Mode> {
    [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64]
        .into_iter()
        .flat_map(move |m| {
            Rate::ALL.into_iter().flat_map(move |r| {
                Guard::ALL.into_iter().map(move |g| {
                    let mut mode = Mode::uk(m, r);
                    mode.fft = fft;
                    mode.guard = g;
                    mode.bandwidth = bandwidth;
                    mode
                })
            })
        })
}

/// All 60 modes of `fft`/`bandwidth`, with their bitrates. The GUI shows
/// the table; the bitrate drives the video profile.
pub fn mode_table(fft: FftSize, bandwidth: Bandwidth) -> Vec<ModeRow> {
    walk(fft, bandwidth)
        .map(|mode| ModeRow {
            mode,
            label: format!(
                "{} {} {}",
                mode.modulation.name(),
                mode.rate_hp.name(),
                mode.guard.name()
            ),
            bitrate: mode.bitrate_hp(),
            qef_cn_db: mode.qef_cn_db().unwrap_or(f32::NAN),
        })
        .collect()
}

/// Picture size and bitrates for a transport stream rate. Bitrates are in
/// kbit/s, as ffmpeg takes them.
///
/// The budget held on hard content (4K60 drone footage plus noise audio, both
/// encoders) with no "dts < pcr" warnings and about 10 % nulls left:
/// video 0.85 R less the audio, peak 0.91 R less the audio, buffer half the
/// peak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoProfile {
    pub width: u32,
    pub height: u32,
    pub video_kbps: u32,
    pub maxrate_kbps: u32,
    pub bufsize_kbps: u32,
    pub audio_kbps: u32,
}

impl VideoProfile {
    /// The profile for a mux rate in bit/s: under 9.5 Mbit/s standard
    /// definition (720x576, 96 kbit/s audio), under 17 Mbit/s 720p (128k),
    /// above that 1080p (192k).
    pub fn for_rate(rate_bps: u64) -> VideoProfile {
        let (width, height, audio_kbps) = if rate_bps < 9_500_000 {
            (720, 576, 96)
        } else if rate_bps < 17_000_000 {
            (1280, 720, 128)
        } else {
            (1920, 1080, 192)
        };
        let r_kbps = rate_bps as f64 / 1000.0;
        // Round down to 10 kbit/s: the figures are a budget, not a target.
        let floor10 = |v: f64| ((v / 10.0).floor() as u32) * 10;
        let maxrate_kbps = floor10(0.91 * r_kbps - audio_kbps as f64);
        VideoProfile {
            width,
            height,
            video_kbps: floor10(0.85 * r_kbps - audio_kbps as f64),
            maxrate_kbps,
            bufsize_kbps: maxrate_kbps / 2,
            audio_kbps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uk(m: Modulation, r: Rate, g: Guard) -> Mode {
        let mut mode = Mode::uk(m, r);
        mode.guard = g;
        mode
    }

    #[test]
    fn the_uk_mode_is_24_13_mbit() {
        let m = uk(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        // 4512000000 / 187 = 24 128 342.246
        assert_eq!(mux_rate_bps(&m), 24_128_342);
    }

    #[test]
    fn the_three_verified_targets() {
        // A, B, C of the video scout's runs.
        let a = uk(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        let b = uk(Modulation::Qam16, Rate::R1_2, Guard::G1_32);
        let c = uk(Modulation::Qpsk, Rate::R1_2, Guard::G1_8);
        assert_eq!(mux_rate_bps(&a), 24_128_342);
        assert_eq!(mux_rate_bps(&b), 12_064_171);
        assert_eq!(mux_rate_bps(&c), 5_529_411);
        assert_eq!(VideoProfile::for_rate(24_128_342).height, 1080);
        assert_eq!(VideoProfile::for_rate(12_064_171).height, 720);
        assert_eq!(VideoProfile::for_rate(5_529_411).height, 576);
    }

    #[test]
    fn six_and_seven_mhz_scale() {
        let mut m = uk(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        let r8 = m.bitrate_hp();
        m.bandwidth = Bandwidth::Mhz7;
        assert!((m.bitrate_hp() / r8 - 7.0 / 8.0).abs() < 1e-9);
    }

    #[test]
    fn the_table_has_sixty_rows_and_the_video_fits() {
        let t = mode_table(FftSize::K8, Bandwidth::Mhz8);
        assert_eq!(t.len(), 60);
        for row in &t {
            let p = VideoProfile::for_rate(mux_rate_bps(&row.mode));
            assert!(p.video_kbps > 0, "{}", row.label);
            assert!(
                (p.maxrate_kbps + p.audio_kbps) as f64 * 1000.0 < row.bitrate,
                "{}",
                row.label
            );
        }
    }
}
