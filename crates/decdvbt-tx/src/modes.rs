// made by CasualArclamp
//! The DVB-T modes a transmission can use, the transport stream rate each
//! carries, and the video profile (picture size and bitrates) that fits it;
//! and the DVB-T2 modes the app offers at 1.7 MHz.
//!
//! The modulator takes packets at exactly the channel's useful rate and never
//! inserts nulls, so ffmpeg must be told to mux at that rate (`-muxrate`).
//! The rate is EN 300 744 Table 17's: data carriers x bits x code rate x
//! 188/204 / Ts. At 1 MHz (every clock of 8 MHz over 8) it is an eighth of
//! the 8 MHz figure: 0.62 to 3.96 Mbit/s. DVB-T2's is the PLP's: FEC blocks
//! a frame x (K_bch - 80) bits / the frame's duration.

use decdvbt_core::CodeRate;
use decdvbt_core::dvbt::{Bandwidth, FftSize, Guard, Mode, Modulation, Rate};
// The T2 types the app's controls need, so it can name them from here.
pub use decdvbt_fec::t2::qam::Qam;
pub use decdvbt_mod::t2::T2Config;
pub use decdvbt_ofdm::t2::{Fft as T2Fft, Guard as T2Guard, PilotPattern};

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
    /// The profile for a mux rate in bit/s: under 1.5 Mbit/s (the narrow
    /// channels' slower modes) CIF, 352x288 with 64 kbit/s audio; under 9.5
    /// Mbit/s standard definition (720x576, 96 kbit/s audio), under 17 Mbit/s
    /// 720p (128k), above that 1080p (192k).
    pub fn for_rate(rate_bps: u64) -> VideoProfile {
        let (width, height, audio_kbps) = if rate_bps < 1_500_000 {
            (352, 288, 64)
        } else if rate_bps < 9_500_000 {
            (720, 576, 96)
        } else if rate_bps < 17_000_000 {
            (1280, 720, 128)
        } else {
            (1920, 1080, 192)
        };
        let r_kbps = rate_bps as f64 / 1000.0;
        // The tables take a fixed rate whatever the mux's: PAT and PMT ten
        // times a second, SDT and NIT twice, TDT and TOT once, 26 packets or
        // 39 kbit/s. Lost in the margin above 1.5 Mbit/s; at 0.62 Mbit/s it
        // is 6 % and, not taken off, left 4.7 % of nulls (measured).
        let tables_kbps = if rate_bps < 1_500_000 { 40.0 } else { 0.0 };
        // Round down to 10 kbit/s: the figures are a budget, not a target.
        let floor10 = |v: f64| ((v / 10.0).floor() as u32) * 10;
        let maxrate_kbps = floor10(0.91 * r_kbps - audio_kbps as f64 - tables_kbps);
        VideoProfile {
            width,
            height,
            video_kbps: floor10(0.85 * r_kbps - audio_kbps as f64 - tables_kbps),
            maxrate_kbps,
            bufsize_kbps: maxrate_kbps / 2,
            audio_kbps,
        }
    }
}

/// The radio's baseband filter for a channel `bandwidth` wide, Hz: the
/// channel's width for 6 to 8 MHz (the firmware takes its nearest setting:
/// 8 MHz for 8 000 000), and 1.75 MHz, the narrowest the HackRF's MAX2837
/// has, for 1 and 1.7 MHz (1.54 MHz of carriers at 1.7 MHz fit inside it with
/// 0.1 MHz to spare each side).
pub fn baseband_filter_hz(bandwidth: Bandwidth) -> u32 {
    if bandwidth.narrow() {
        1_750_000
    } else {
        (bandwidth.mhz() * 1e6).round() as u32
    }
}

// ---- DVB-T2 at 1.7 MHz ------------------------------------------------------

/// The T2 FFT sizes offered at 1.7 MHz (16K and 32K would make carriers 56
/// to 113 Hz apart, too close for a portable link's Doppler and phase noise).
pub const T2_FFTS: [T2Fft; 4] = [T2Fft::K1, T2Fft::K2, T2Fft::K4, T2Fft::K8];
pub const T2_QAMS: [Qam; 4] = Qam::ALL;
/// The PLP code rates (EN 302 755 Table 28 less the T2-Lite ones).
pub const T2_RATES: [(u16, u16); 6] = [(1, 2), (3, 5), (2, 3), (3, 4), (4, 5), (5, 6)];

/// The guard intervals an FFT size has (EN 302 755 Table 59 for SISO: 1/128,
/// 19/256 and 19/128 only from 8K; 1/32 not at 1K).
pub fn t2_guards(fft: T2Fft) -> Vec<T2Guard> {
    use T2Guard::*;
    match fft {
        T2Fft::K1 => vec![G1_16, G1_8, G1_4],
        T2Fft::K2 | T2Fft::K4 => vec![G1_32, G1_16, G1_8, G1_4],
        _ => vec![G1_128, G1_32, G1_16, G19_256, G1_8, G19_128, G1_4],
    }
}

/// The scattered pilot pattern for a guard interval: the densest the
/// implementation guidelines pair with it (TS 102 831 §10.1: the pattern's
/// D_x·D_y should match the echoes the guard allows): PP7 for 1/128, PP4 for
/// 1/32, 1/16 and 19/256, PP2 for 1/8 and 19/128, PP1 for 1/4.
pub fn t2_pilots(guard: T2Guard) -> PilotPattern {
    use T2Guard::*;
    match guard {
        G1_128 => PilotPattern::Pp7,
        G1_32 | G1_16 | G19_256 => PilotPattern::Pp4,
        G1_8 | G19_128 => PilotPattern::Pp2,
        G1_4 => PilotPattern::Pp1,
    }
}

/// "16qam", as a T2 description spells it.
fn qam_token(q: Qam) -> &'static str {
    match q {
        Qam::Qpsk => "qpsk",
        Qam::Qam16 => "16qam",
        Qam::Qam64 => "64qam",
        Qam::Qam256 => "256qam",
    }
}

/// "16-QAM".
pub fn qam_name(q: Qam) -> &'static str {
    match q {
        Qam::Qpsk => "QPSK",
        Qam::Qam16 => "16-QAM",
        Qam::Qam64 => "64-QAM",
        Qam::Qam256 => "256-QAM",
    }
}

/// A DVB-T2 transmission at 1.7 MHz: one data PLP, rotated constellation,
/// 64800-bit LDPC frames, normal carriers (no extended mode), frames of about
/// 217 ms holding as many FEC blocks as fit, the pilot pattern
/// [`t2_pilots`] picks. `None` for a combination with no frame.
pub fn t2_mode(fft: T2Fft, guard: T2Guard, qam: Qam, rate: (u16, u16)) -> Option<T2Config> {
    let spec = format!(
        "{}-{}-pp{}-{}-{}/{}-rot-1.7mhz",
        fft.name().to_ascii_lowercase(),
        guard.name(),
        t2_pilots(guard).number(),
        qam_token(qam),
        rate.0,
        rate.1
    );
    T2Config::parse(&spec).ok()
}

/// A T2 mode as a line: "16-QAM 3/4 8K 1/16 PP4".
pub fn t2_label(c: &T2Config) -> String {
    format!(
        "{} {} {} {} {}",
        qam_name(c.qam),
        c.rate,
        c.mode.fft,
        c.mode.guard,
        c.mode.pp
    )
}

/// The C/N a T2 mode needs for quasi error free reception, dB (the
/// receiver's table, `decdvbt_fec::t2::threshold`).
pub fn t2_qef_cn_db(c: &T2Config) -> Option<f32> {
    decdvbt_fec::t2::threshold::qef_cn_db(c.frame, c.rate, c.qam, c.rotated)
}

/// One row of the T2 mode table.
#[derive(Debug, Clone, PartialEq)]
pub struct T2ModeRow {
    pub config: T2Config,
    pub label: String,
    pub bitrate: f64,
    pub qef_cn_db: f32,
}

/// Every constellation and rate at `fft`/`guard`, 1.7 MHz, in table order.
pub fn t2_mode_table(fft: T2Fft, guard: T2Guard) -> Vec<T2ModeRow> {
    T2_QAMS
        .iter()
        .flat_map(|&q| T2_RATES.iter().map(move |&r| (q, r)))
        .filter_map(|(q, r)| t2_mode(fft, guard, q, r))
        .map(|c| T2ModeRow {
            label: format!("{} {}", qam_name(c.qam), c.rate),
            bitrate: c.ts_rate_bps(),
            qef_cn_db: t2_qef_cn_db(&c).unwrap_or(f32::NAN),
            config: c,
        })
        .collect()
}

/// The code rate of a T2 config as the table's pair.
pub fn t2_rate(c: &T2Config) -> (u16, u16) {
    let CodeRate { num, den } = c.rate;
    (num, den)
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

    #[test]
    fn one_mhz_is_an_eighth_and_the_video_still_fits() {
        let t = mode_table(FftSize::K8, Bandwidth::Mhz1);
        assert_eq!(t.len(), 60);
        let (lo, hi) = t.iter().fold((f64::MAX, 0f64), |(a, b), r| {
            (a.min(r.bitrate), b.max(r.bitrate))
        });
        assert!((lo / 1e6 - 4.976 / 8.0).abs() < 0.001, "{lo}");
        assert!((hi / 1e6 - 31.67 / 8.0).abs() < 0.001, "{hi}");
        for row in &t {
            let p = VideoProfile::for_rate(mux_rate_bps(&row.mode));
            assert!(p.video_kbps >= 300, "{}: {p:?}", row.label);
            assert!(
                (p.maxrate_kbps + p.audio_kbps) as f64 * 1000.0 < 0.95 * row.bitrate,
                "{}",
                row.label
            );
        }
        assert_eq!(VideoProfile::for_rate(622_000).height, 288);
        assert_eq!(baseband_filter_hz(Bandwidth::Mhz1), 1_750_000);
        assert_eq!(baseband_filter_hz(Bandwidth::Mhz1_7), 1_750_000);
        assert_eq!(baseband_filter_hz(Bandwidth::Mhz8), 8_000_000);
    }

    #[test]
    fn the_t2_modes_at_1_7_mhz_make_frames_and_rates() {
        let mut rows = 0;
        for fft in T2_FFTS {
            for guard in t2_guards(fft) {
                let t = t2_mode_table(fft, guard);
                // Every combination makes a frame.
                assert_eq!(t.len(), 24, "{fft} {guard}");
                for r in &t {
                    assert!(r.config.mode.allowed(), "{}", r.label);
                    // 0.6 to 9.5 Mbit/s; frames of about 217 ms.
                    assert!(
                        r.bitrate > 0.5e6 && r.bitrate < 9.6e6,
                        "{} {}",
                        r.label,
                        r.bitrate
                    );
                    assert!((r.config.frame_s() - 0.217).abs() < 0.01);
                    assert!(r.qef_cn_db.is_finite());
                    let p = VideoProfile::for_rate(r.bitrate as u64);
                    assert!(p.video_kbps >= 300, "{}: {p:?}", r.label);
                    rows += 1;
                }
            }
        }
        assert_eq!(rows, 24 * (3 + 4 + 4 + 7));
        let c = t2_mode(T2Fft::K8, T2Guard::G1_16, Qam::Qam16, (3, 4)).unwrap();
        assert_eq!(t2_label(&c), "16-QAM 3/4 8K 1/16 PP4");
        assert_eq!(c.bandwidth, Bandwidth::Mhz1_7);
        assert!(c.rotated && !c.mode.extended);
    }
}
