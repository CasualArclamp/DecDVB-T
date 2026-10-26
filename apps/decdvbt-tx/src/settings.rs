// made by CasualArclamp
//! The settings that are kept from run to run, and how they are written.
//!
//! Kept: window size, last folder, mode, centre frequency, loop, level,
//! encoder, codecs, ffmpeg path, preview, radio rate.
//!
//! **Never kept** (docs/TX_DESIGN.md §6 and §7): the arm, TX VGA, the RF amp,
//! the output (a start always begins on "test only") and the file itself. There
//! is no field for any of them here, so no code path can write them.

use std::path::PathBuf;

use decdvbt_core::dvbt::{Mode, Modulation};
use decdvbt_tx::{AudioCodec, EncoderChoice, TxRate, VideoCodec};

use crate::prefs;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub window: Option<(u32, u32)>,
    pub folder: Option<PathBuf>,
    pub mode: Mode,
    pub centre_hz: f64,
    pub loop_input: bool,
    pub level_dbfs: f32,
    pub encoder: EncoderChoice,
    pub video_codec: VideoCodec,
    pub audio_codec: AudioCodec,
    pub ffmpeg_path: Option<PathBuf>,
    pub preview: bool,
    pub rate: TxRate,
}

impl Default for Settings {
    /// The UK mode on C30, native rate, -12 dBFS, looping.
    fn default() -> Self {
        Settings {
            window: None,
            folder: None,
            mode: crate::modeinfo::presets()[0].mode,
            centre_hz: 546_000_000.0,
            loop_input: true,
            level_dbfs: -12.0,
            encoder: EncoderChoice::Auto,
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::AacLatm,
            ffmpeg_path: None,
            preview: true,
            rate: TxRate::Native,
        }
    }
}

/// A mode as `Mode::parse` reads it: "64qam-2/3-8k-1/32-8mhz".
pub fn mode_text(m: &Mode) -> String {
    let c = match m.modulation {
        Modulation::Qpsk => "qpsk",
        Modulation::Qam16 => "16qam",
        Modulation::Qam64 => "64qam",
    };
    format!(
        "{c}-{}-{}-{}-{}mhz",
        m.rate_hp.name(),
        m.fft.name(),
        m.guard.name(),
        m.bandwidth.mhz() as u32
    )
}

pub fn rate_text(r: TxRate) -> String {
    match r {
        TxRate::Native => "native".into(),
        TxRate::Fixed(hz) => hz.to_string(),
    }
}

fn parse_rate(s: &str) -> Option<TxRate> {
    if s == "native" {
        return Some(TxRate::Native);
    }
    let hz: u32 = s.parse().ok()?;
    (2_000_000..=20_000_000)
        .contains(&hz)
        .then_some(TxRate::Fixed(hz))
}

pub fn encoder_text(e: EncoderChoice) -> &'static str {
    match e {
        EncoderChoice::Auto => "auto",
        EncoderChoice::Nvenc => "nvenc",
        EncoderChoice::X264 => "x264",
    }
}

fn parse_encoder(s: &str) -> Option<EncoderChoice> {
    [
        EncoderChoice::Auto,
        EncoderChoice::Nvenc,
        EncoderChoice::X264,
    ]
    .into_iter()
    .find(|e| encoder_text(*e) == s)
}

pub fn audio_text(a: AudioCodec) -> &'static str {
    match a {
        AudioCodec::AacLatm => "aac-latm",
        AudioCodec::AacAdts => "aac-adts",
        AudioCodec::Mp2 => "mp2",
    }
}

fn parse_audio(s: &str) -> Option<AudioCodec> {
    [AudioCodec::AacLatm, AudioCodec::AacAdts, AudioCodec::Mp2]
        .into_iter()
        .find(|a| audio_text(*a) == s)
}

pub fn video_text(v: VideoCodec) -> &'static str {
    match v {
        VideoCodec::H264 => "h264",
        VideoCodec::Mpeg2 => "mpeg2",
    }
}

fn parse_video(s: &str) -> Option<VideoCodec> {
    [VideoCodec::H264, VideoCodec::Mpeg2]
        .into_iter()
        .find(|v| video_text(*v) == s)
}

fn flag(text: &str, key: &str, default: bool) -> bool {
    prefs::lookup(text, key).map_or(default, |v| v == "1")
}

impl Settings {
    /// From the prefs file; whatever is missing or unreadable is the default.
    pub fn load() -> Settings {
        Settings::from_text(&prefs::read())
    }

    pub fn from_text(text: &str) -> Settings {
        let d = Settings::default();
        let get = |k: &str| prefs::lookup(text, k);
        let window = get("window").and_then(|v| {
            let (w, h) = v.split_once('x')?;
            let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
            // A window smaller than the minimum, or absurdly large, is a
            // stale or damaged value.
            ((900..=8000).contains(&w) && (600..=5000).contains(&h)).then_some((w, h))
        });
        Settings {
            window,
            folder: get("folder").filter(|s| !s.is_empty()).map(PathBuf::from),
            mode: get("mode")
                .and_then(|s| Mode::parse(&s).ok())
                .filter(|m| !m.hierarchical())
                .unwrap_or(d.mode),
            centre_hz: get("centre_hz")
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|f| (1e6..=6e9).contains(f))
                .unwrap_or(d.centre_hz),
            loop_input: flag(text, "loop", d.loop_input),
            level_dbfs: get("level")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|l| (-30.0..=-6.0).contains(l))
                .unwrap_or(d.level_dbfs),
            encoder: get("encoder")
                .and_then(|s| parse_encoder(&s))
                .unwrap_or(d.encoder),
            video_codec: get("video")
                .and_then(|s| parse_video(&s))
                .unwrap_or(d.video_codec),
            audio_codec: get("audio")
                .and_then(|s| parse_audio(&s))
                .unwrap_or(d.audio_codec),
            ffmpeg_path: get("ffmpeg").filter(|s| !s.is_empty()).map(PathBuf::from),
            preview: flag(text, "preview", d.preview),
            rate: get("rate").and_then(|s| parse_rate(&s)).unwrap_or(d.rate),
        }
    }

    /// The lines to write (an empty value removes a key).
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let on = |b: bool| if b { "1" } else { "0" }.to_string();
        vec![
            (
                "window",
                self.window
                    .map_or(String::new(), |(w, h)| format!("{w}x{h}")),
            ),
            (
                "folder",
                self.folder
                    .as_ref()
                    .map_or(String::new(), |p| p.display().to_string()),
            ),
            ("mode", mode_text(&self.mode)),
            ("centre_hz", format!("{:.0}", self.centre_hz)),
            ("loop", on(self.loop_input)),
            ("level", format!("{:.1}", self.level_dbfs)),
            ("encoder", encoder_text(self.encoder).into()),
            ("video", video_text(self.video_codec).into()),
            ("audio", audio_text(self.audio_codec).into()),
            (
                "ffmpeg",
                self.ffmpeg_path
                    .as_ref()
                    .map_or(String::new(), |p| p.display().to_string()),
            ),
            ("preview", on(self.preview)),
            ("rate", rate_text(self.rate)),
        ]
    }

    pub fn store(&self) {
        let pairs = self.pairs();
        let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        prefs::set_many(&refs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(s: &Settings) -> String {
        let pairs = s.pairs();
        let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        prefs::merged("", &refs)
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(Settings::from_text(""), Settings::default());
    }

    #[test]
    fn everything_round_trips() {
        let s = Settings {
            window: Some((1200, 800)),
            folder: Some(PathBuf::from("C:/Videos")),
            mode: Mode::parse("16qam-3/4-2k-1/16-7mhz").unwrap(),
            centre_hz: 618_000_000.0,
            loop_input: false,
            level_dbfs: -9.5,
            encoder: EncoderChoice::X264,
            video_codec: VideoCodec::Mpeg2,
            audio_codec: AudioCodec::Mp2,
            ffmpeg_path: Some(PathBuf::from("D:/tools/ffmpeg.exe")),
            preview: false,
            rate: TxRate::Fixed(10_000_000),
        };
        assert_eq!(Settings::from_text(&text_of(&s)), s);
    }

    #[test]
    fn damaged_values_fall_back_to_the_defaults() {
        let s = Settings::from_text(
            "window=10x10\nmode=nonsense\ncentre_hz=3\nlevel=5\nrate=12\nencoder=gpu\n",
        );
        assert_eq!(s, Settings::default());
    }

    /// The safety state has no field here: nothing about the arm, the gains
    /// or the output can be written (docs/TX_DESIGN.md §6.5, §7).
    #[test]
    fn nothing_dangerous_is_kept() {
        let keys: Vec<&str> = Settings::default().pairs().iter().map(|p| p.0).collect();
        for banned in [
            "arm", "armed", "vga", "txvga", "amp", "sink", "output", "file",
        ] {
            assert!(!keys.contains(&banned), "{banned} must not be persisted");
        }
    }

    #[test]
    fn mode_text_is_what_mode_parse_reads() {
        for p in crate::modeinfo::presets() {
            assert_eq!(Mode::parse(&mode_text(&p.mode)).unwrap(), p.mode);
        }
    }
}
