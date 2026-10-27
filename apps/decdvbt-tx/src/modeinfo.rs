// made by CasualArclamp
//! The mode presets and the lines the GUI shows about a mode: the bitrate it
//! carries, the C/N it needs and the picture the video profile will make.
//!
//! The numbers all come from the library (`decdvbt_tx::modes`, the modulator's
//! own `Mode`); nothing here knows a bitrate by heart.
//!
//! Two standards: DVB-T at 1, 6, 7 and 8 MHz (1 MHz is the 8 MHz mode with
//! every clock divided by 8, as narrowband amateur DATV does) and DVB-T2 at
//! 1.7 MHz (EN 302 755's narrow channel), picked by the Bandwidth box.

use decdvbt_core::dvbt::{Bandwidth, FftSize, Guard, Mode, Modulation, Rate};
use decdvbt_tx::modes::{Qam, T2Config, T2Fft, T2Guard, t2_mode, t2_qef_cn_db};
use decdvbt_tx::{VideoProfile, mux_rate_bps};

/// A named starting point: a DVB-T mode, or a DVB-T2 one (`t2`).
#[derive(Debug, Clone)]
pub struct Preset {
    pub name: &'static str,
    pub mode: Mode,
    pub t2: Option<T2Config>,
    /// What it is for, in a few words.
    pub use_case: &'static str,
}

/// The three presets of docs/TX_DESIGN.md §3, then the narrow channels'.
pub fn presets() -> Vec<Preset> {
    let mut very_robust = Mode::uk(Modulation::Qpsk, Rate::R1_2);
    very_robust.guard = Guard::G1_8;
    let mut narrow = Mode::uk(Modulation::Qpsk, Rate::R2_3);
    (narrow.fft, narrow.guard, narrow.bandwidth) = (FftSize::K2, Guard::G1_8, Bandwidth::Mhz1);
    vec![
        Preset {
            name: "UK  64-QAM 2/3",
            mode: Mode::uk(Modulation::Qam64, Rate::R2_3),
            t2: None,
            use_case: "the best picture; needs a clean link (C/N 18 dB or more)",
        },
        Preset {
            name: "Robust  16-QAM 1/2",
            mode: Mode::uk(Modulation::Qam16, Rate::R1_2),
            t2: None,
            use_case: "a marginal link",
        },
        Preset {
            name: "Very robust  QPSK 1/2",
            mode: very_robust,
            t2: None,
            use_case: "a weak link, standard definition (about 5.5 Mbit/s)",
        },
        Preset {
            name: "1 MHz  QPSK 2/3 2k",
            mode: narrow,
            t2: None,
            use_case: "a narrow channel (amateur DATV style), 0.92 Mbit/s, a small picture",
        },
        Preset {
            name: "1.7 MHz T2  16-QAM 3/4",
            mode: Mode::uk(Modulation::Qam64, Rate::R2_3),
            t2: t2_default(),
            use_case: "DVB-T2's narrow channel, 3.99 Mbit/s, standard definition",
        },
    ]
}

/// The T2 mode a 1.7 MHz channel starts on: 16-QAM 3/4, 8K, 1/16, PP4.
pub fn t2_default() -> Option<T2Config> {
    t2_mode(T2Fft::K8, T2Guard::G1_16, Qam::Qam16, (3, 4))
}

/// Which preset the settings are exactly, if any: `t2` is the T2 mode in use
/// (`None` for DVB-T in `mode`).
pub fn preset_of(mode: &Mode, t2: Option<&T2Config>) -> Option<usize> {
    presets().iter().position(|p| match (&p.t2, t2) {
        (Some(a), Some(b)) => a == b,
        (None, None) => p.mode == *mode,
        _ => false,
    })
}

/// The constellations, rates, guards, FFT sizes and bandwidths, in the order
/// the combo boxes list them.
pub const MODULATIONS: [Modulation; 3] = [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64];
pub const FFTS: [FftSize; 2] = [FftSize::K2, FftSize::K8];
pub const BANDWIDTHS: [Bandwidth; 5] = Bandwidth::ALL;

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

/// The same for a DVB-T2 mode.
pub fn t2_summary(c: &T2Config) -> ModeSummary {
    let bitrate = c.ts_rate_bps();
    ModeSummary {
        bitrate,
        qef_cn_db: t2_qef_cn_db(c).filter(|c| c.is_finite()),
        profile: VideoProfile::for_rate(bitrate.floor() as u64),
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
        assert_eq!(p.len(), 5);
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
            assert_eq!(preset_of(&p.mode, p.t2.as_ref()), Some(i));
        }
        let mut m = presets()[0].mode;
        m.guard = Guard::G1_4;
        assert_eq!(preset_of(&m, None), None);
    }

    #[test]
    fn the_lines_read_well() {
        let s = summary(&presets()[0].mode);
        let line = s.rate_line();
        assert!(line.starts_with("24.13 Mbit/s, needs "), "{line}");
        assert!(line.ends_with("dB C/N"), "{line}");
        assert!(s.picture_line(25).starts_with("1080p25, video "));
    }

    #[test]
    fn the_narrow_presets_say_what_they_carry() {
        let p = presets();
        let one = summary(&p[3].mode);
        assert!(
            one.rate_line().starts_with("0.92 Mbit/s"),
            "{}",
            one.rate_line()
        );
        assert_eq!(one.profile.height, 288);
        let t2 = t2_summary(p[4].t2.as_ref().unwrap());
        assert!(
            t2.rate_line().starts_with("3.99 Mbit/s"),
            "{}",
            t2.rate_line()
        );
        assert!(t2.qef_cn_db.is_some());
        assert_eq!(t2.profile.height, 576);
    }
}
