// made by CasualArclamp
//! The video file: probing it off the UI thread, how its details are shown,
//! and the names derived from it (the service name the receiver lists, the
//! IQ file's name).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use decdvbt_tx::ffmpeg::{VideoInfo, find_ffmpeg, probe};
use eframe::egui;

/// What probing a file found.
#[derive(Debug, Clone, PartialEq)]
pub enum Probed {
    /// ffprobe's answer.
    Info(VideoInfo),
    /// ffmpeg is there but ffprobe is not: transmission still works.
    NoProbe,
    /// ffmpeg itself was not found (the message says where it looked).
    NoFfmpeg(String),
    /// The file could not be read as a video.
    Bad(String),
}

/// Where the file's details are, in the window.
#[derive(Debug, Clone, PartialEq)]
pub enum FileInfo {
    Probing,
    Done(Probed),
}

/// A probe running in its own thread: ffprobe takes tens to hundreds of
/// milliseconds and must never stall a frame.
pub struct ProbeJob {
    rx: Receiver<Probed>,
    pub path: PathBuf,
}

impl ProbeJob {
    pub fn start(path: PathBuf, ffmpeg: Option<PathBuf>, ctx: egui::Context) -> ProbeJob {
        let (tx, rx) = channel();
        let p = path.clone();
        std::thread::Builder::new()
            .name("tx-probe".into())
            .spawn(move || {
                let result = probe_file(&p, ffmpeg.as_deref());
                let _ = tx.send(result);
                ctx.request_repaint_after(std::time::Duration::from_millis(1));
            })
            .ok();
        ProbeJob { rx, path }
    }

    /// The answer, once there is one. A probe thread that died (a panic in
    /// the library) is reported as a failure, not waited for.
    pub fn poll(&self) -> Option<Probed> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some(Probed::Bad("the probe stopped unexpectedly".into()))
            }
        }
    }
}

fn probe_file(path: &Path, configured: Option<&Path>) -> Probed {
    let Some(ffmpeg) = find_ffmpeg(configured) else {
        return Probed::NoFfmpeg(
            "ffmpeg was not found: put it on the PATH, beside this program, \
             or choose ffmpeg.exe under Advanced"
                .into(),
        );
    };
    match probe(&ffmpeg, path) {
        Ok(Some(info)) => Probed::Info(info),
        Ok(None) => Probed::NoProbe,
        Err(e) => Probed::Bad(e.to_string()),
    }
}

/// "H.264", "HEVC", "AAC"... for ffprobe's codec names.
pub fn codec_label(name: &str) -> String {
    match name {
        "h264" => "H.264".into(),
        "hevc" => "HEVC".into(),
        "mpeg2video" => "MPEG-2".into(),
        "mpeg4" => "MPEG-4".into(),
        "vp9" => "VP9".into(),
        "av1" => "AV1".into(),
        "aac" => "AAC".into(),
        "mp2" => "MP2".into(),
        "mp3" => "MP3".into(),
        "ac3" => "AC-3".into(),
        "eac3" => "E-AC-3".into(),
        "opus" => "Opus".into(),
        "vorbis" => "Vorbis".into(),
        "flac" => "FLAC".into(),
        other => other.to_uppercase(),
    }
}

/// "25" or "29.97".
fn fps_text(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.005 {
        format!("{:.0}", fps)
    } else {
        format!("{fps:.2}")
    }
}

/// The two lines under the file name: "1920x1080 25p H.264 + AAC" and
/// "duration 0:03:12, 8-bit".
pub fn describe(info: &VideoInfo) -> (String, String) {
    let scan = if info.interlaced { "i" } else { "p" };
    let mut a = format!(
        "{}x{} {}{scan} {}",
        info.width,
        info.height,
        fps_text(info.fps),
        codec_label(&info.video_codec)
    );
    match &info.audio_codec {
        Some(c) => a.push_str(&format!(" + {}", codec_label(c))),
        None => a.push_str(" (no audio)"),
    }
    let mut b = Vec::new();
    if let Some(d) = info.duration {
        let s = d.as_secs();
        b.push(format!(
            "duration {}:{:02}:{:02}",
            s / 3600,
            s % 3600 / 60,
            s % 60
        ));
    }
    if info.bit_depth > 8 {
        b.push(format!("{}-bit", info.bit_depth));
    }
    if info.interlaced {
        b.push("interlaced".into());
    }
    if let Some(r) = info.bitrate {
        b.push(format!("{:.1} Mbit/s", r as f64 / 1e6));
    }
    (a, b.join(", "))
}

/// The service name a receiver lists: the file's name without its extension,
/// as ASCII, 24 characters at most (the DVB service name's own limit is 255
/// bytes but receivers show far fewer).
pub fn service_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ascii: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name: String = ascii.trim().chars().take(24).collect();
    if name.trim_matches('_').is_empty() {
        "DecDVB-T TX".into()
    } else {
        name.trim().to_string()
    }
}

/// A name the receiver can read the tuning from: `tx_546.000MHz_9.142857Msps.cs8`.
pub fn iq_file_name(centre_hz: f64, rate_hz: u32) -> String {
    format!(
        "tx_{:.3}MHz_{:.6}Msps.cs8",
        centre_hz / 1e6,
        rate_hz as f64 / 1e6
    )
}

/// The file's name for the title line.
pub fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// File types the Browse dialog offers first.
pub const VIDEO_EXTENSIONS: [&str; 14] = [
    "mp4", "mkv", "mov", "avi", "webm", "ts", "m2ts", "mts", "mpg", "mpeg", "wmv", "flv", "m4v",
    "3gp",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_file_description_reads_well() {
        let info = VideoInfo {
            duration: Some(Duration::from_secs(192)),
            width: 1920,
            height: 1080,
            fps: 25.0,
            video_codec: "h264".into(),
            audio_codec: Some("aac".into()),
            audio_channels: 2,
            interlaced: false,
            bit_depth: 8,
            bitrate: None,
        };
        let (a, b) = describe(&info);
        assert_eq!(a, "1920x1080 25p H.264 + AAC");
        assert_eq!(b, "duration 0:03:12");
    }

    #[test]
    fn interlaced_ten_bit_without_audio() {
        let info = VideoInfo {
            duration: None,
            width: 1920,
            height: 1080,
            fps: 29.97,
            video_codec: "hevc".into(),
            audio_codec: None,
            audio_channels: 0,
            interlaced: true,
            bit_depth: 10,
            bitrate: Some(18_500_000),
        };
        let (a, b) = describe(&info);
        assert_eq!(a, "1920x1080 29.97i HEVC (no audio)");
        assert_eq!(b, "10-bit, interlaced, 18.5 Mbit/s");
    }

    #[test]
    fn the_service_name_is_ascii_and_short() {
        assert_eq!(service_name(Path::new("C:/v/clip.mp4")), "clip");
        assert_eq!(
            service_name(Path::new("a_very_long_file_name_indeed_yes.mkv")),
            "a_very_long_file_name_in"
        );
        assert_eq!(service_name(Path::new("caf\u{e9} noir.mp4")), "caf_ noir");
        // Nothing usable left: the default.
        assert_eq!(
            service_name(Path::new("\u{4e2d}\u{6587}.mp4")),
            "DecDVB-T TX"
        );
        assert_eq!(service_name(Path::new("")), "DecDVB-T TX");
    }

    #[test]
    fn the_iq_file_name_carries_the_tuning() {
        assert_eq!(
            iq_file_name(546e6, 9_142_857),
            "tx_546.000MHz_9.142857Msps.cs8"
        );
    }
}
