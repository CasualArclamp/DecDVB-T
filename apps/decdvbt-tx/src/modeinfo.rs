// made by CasualArclamp
//! The mode presets and the lines the GUI shows about a mode: the bitrate it
//! carries, the C/N it needs and the picture the video profile will make.
//!
//! The numbers all come from the library (`decdvbt_tx::modes`, the modulator's
//! own `Mode`); nothing here knows a bitrate by heart.

use decdvbt_core::dvbt::{Bandwidth, FftSize, Guard, Mode, Modulation, Rate};
use decdvbt_tx::{VideoProfile, mux_rate_bps};

/// A named starting point.
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub name: &'static str,
    pub mode: Mode,
    /// What it is for, in a few words.
    pub use_case: &'static str,
}

/// The three presets of docs/TX_DESIGN.md §3.
pub fn presets() -> [Preset; 3] {
    let mut very_robust = Mode::uk(Modulation::Qpsk, Rate::R1_2);
    very_robust.guard = Guard::G1_8;
    [
        Preset {
            name: "UK  64-QAM 2/3",
            mode: Mode::uk(Modulation::Qam64, Rate::R2_3),
            use_case: "the best picture; needs a clean link (C/N 18 dB or more)",
        },
        Preset {
            name: "Robust  16-QAM 1/2",
            mode: Mode::uk(Modulation::Qam16, Rate::R1_2),
            use_case: "a marginal link",
        },
        Preset {
            name: "Very robust  QPSK 1/2",
            mode: very_robust,
            use_case: "a weak link, standard definition (about 5.5 Mbit/s)",
        },
    ]
}

/// Which preset `mode` is exactly, if any.
pub fn preset_of(mode: &Mode) -> Option<usize> {
    presets().iter().position(|p| p.mode == *mode)
}

/// The constellations, rates, guards, FFT sizes and bandwidths, in the order
/// the combo boxes list them.
pub const MODULATIONS: [Modulation; 3] = [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64];
pub const FFTS: [FftSize; 2] = [FftSize::K2, FftSize::K8];
pub const BANDWIDTHS: [Bandwidth; 3] = [Bandwidth::Mhz6, Bandwidth::Mhz7, Bandwidth::Mhz8];

/// What a mode gives, for the line under the mode controls.
#[derive(Debug, Clone, Copy)]
pub struct ModeSummary {
    /// Useful bitrate, bit/s.
    pub bitrate: f64,
    /// C/N for quasi error free reception, dB.
    pub qef_cn_db: Option<f32>,
    pub profile: VideoProfile,
}

pub fn summary(mode: &Mode) -> ModeSummary {
    ModeSummary {
        bitrate: mode.bitrate_hp(),
        qef_cn_db: mode.qef_cn_db().filter(|c| c.is_finite()),
        profile: VideoProfile::for_rate(mux_rate_bps(mode)),
    }
}

impl ModeSummary {
    /// "24.13 Mbit/s, needs 18.4 dB C/N".
    pub fn rate_line(&self) -> String {
        let need = self
            .qef_cn_db
            .map_or(String::new(), |c| format!(", needs {c:.1} dB C/N"));
        format!("{:.2} Mbit/s{need}", self.bitrate / 1e6)
    }

    /// "1080p25, video 20.3 Mbit/s, audio 192k".
    pub fn picture_line(&self, fps: u32) -> String {
        format!(
            "{}p{fps}, video {:.1} Mbit/s, audio {}k",
            self.profile.height,
            self.profile.video_kbps as f64 / 1000.0,
            self.profile.audio_kbps
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_presets_are_the_three_targets() {
        let p = presets();
        assert_eq!(mux_rate_bps(&p[0].mode), 24_128_342);
        assert_eq!(mux_rate_bps(&p[1].mode), 12_064_171);
        assert_eq!(mux_rate_bps(&p[2].mode), 5_529_411);
        assert_eq!(summary(&p[0].mode).profile.height, 1080);
        assert_eq!(summary(&p[1].mode).profile.height, 720);
        assert_eq!(summary(&p[2].mode).profile.height, 576);
    }

    #[test]
    fn a_preset_is_found_again() {
        for (i, p) in presets().iter().enumerate() {
            assert_eq!(preset_of(&p.mode), Some(i));
        }
        let mut m = presets()[0].mode;
        m.guard = Guard::G1_4;
        assert_eq!(preset_of(&m), None);
    }

    #[test]
    fn the_lines_read_well() {
        let s = summary(&presets()[0].mode);
        let line = s.rate_line();
        assert!(line.starts_with("24.13 Mbit/s, needs "), "{line}");
        assert!(line.ends_with("dB C/N"), "{line}");
        assert!(s.picture_line(25).starts_with("1080p25, video "));
    }
}
