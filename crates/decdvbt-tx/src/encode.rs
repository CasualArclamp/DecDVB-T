// made by CasualArclamp
//! The encoder side of the transmitter's sources, shared with DecSAT's TX
//! (ported from `decsat-tx/src/encode.rs`): which codec runs on which engine,
//! cameras and their modes, the test pattern, the call-sign overlay, and the
//! encoder options for a constant bit rate.
//!
//! ffmpeg is a child process (see [`crate::ffmpeg`]), so every codec the
//! installed build has is there: libx264, libx265, libvvenc, SVT-AV1, NVENC,
//! AMF and Quick Sync, and nothing needs linking.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::VideoCodec;
use crate::ffmpeg::no_console;

/// Who encodes: the CPU, or a graphics card's encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Cpu,
    Nvenc,
    Amf,
    Qsv,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Cpu => "CPU",
            Engine::Nvenc => "NVENC",
            Engine::Amf => "AMF",
            Engine::Qsv => "Quick Sync",
        }
    }
}

/// The engines tried, best first, when the choice is "automatic": a graphics
/// card leaves the CPU free, the CPU when there is none.
pub const AUTO_ORDER: [Engine; 4] = [Engine::Nvenc, Engine::Amf, Engine::Qsv, Engine::Cpu];

/// ffmpeg's encoder for a codec on an engine, if there is one.
pub fn encoder_name(codec: VideoCodec, engine: Engine) -> Option<&'static str> {
    use Engine::*;
    use VideoCodec::*;
    Some(match (codec, engine) {
        (Mpeg2, Cpu) => "mpeg2video",
        (H264, Cpu) => "libx264",
        (H265, Cpu) => "libx265",
        (H266, Cpu) => "libvvenc",
        (Av1, Cpu) => "libsvtav1",
        (H264, Nvenc) => "h264_nvenc",
        (H265, Nvenc) => "hevc_nvenc",
        (Av1, Nvenc) => "av1_nvenc",
        (H264, Amf) => "h264_amf",
        (H265, Amf) => "hevc_amf",
        (Av1, Amf) => "av1_amf",
        (H264, Qsv) => "h264_qsv",
        (H265, Qsv) => "hevc_qsv",
        (Av1, Qsv) => "av1_qsv",
        _ => return None,
    })
}

/// How a camera can deliver pictures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// "mjpeg", or a raw pixel format ("yuyv422").
    pub format: String,
}

impl CameraMode {
    pub fn describe(&self) -> String {
        let f = if self.format == "mjpeg" {
            "MJPEG"
        } else {
            &self.format
        };
        format!("{}x{} {} fps {f}", self.width, self.height, self.fps)
    }
}

/// A command for ffmpeg with no console window of its own.
fn command(exe: &Path) -> Command {
    let mut c = Command::new(exe);
    no_console(&mut c);
    c
}

/// Escape text for a `drawtext` filter inside `-vf`: the filter graph's own
/// specials and the option's.
pub fn drawtext_escape(s: &str) -> String {
    let mut o = String::new();
    for ch in s.chars() {
        if matches!(ch, '\\' | ':' | '\'' | ',' | ';' | '[' | ']' | '%' | '=') {
            o.push('\\');
        }
        o.push(ch);
    }
    o
}

/// The font for the overlay: one every Windows has; elsewhere fontconfig's
/// default.
pub fn overlay_font() -> Option<String> {
    let f = Path::new(r"C:\Windows\Fonts\consolab.ttf");
    // The filter wants forward slashes and the drive's colon escaped.
    f.is_file()
        .then(|| f.to_string_lossy().replace('\\', "/").replace(':', "\\\\:"))
}

/// The `drawtext` filter that puts `text` (the call sign) low on the picture.
pub fn overlay_filter(text: &str, height: u32) -> String {
    let size = (height / 12).max(10);
    let font = overlay_font()
        .map(|f| format!("fontfile={f}:"))
        .unwrap_or_default();
    format!(
        "drawtext={font}text={}:fontcolor=white:fontsize={size}:box=1:boxcolor=black@0.5:x=10:y=h-th-10",
        drawtext_escape(text)
    )
}

/// The encoder's options for constant-rate encoding at `k` kbit/s (peak
/// `maxrate_k`, decoder buffer `buf_k`) with a key frame every `gop` pictures.
/// For the engines that are not the measured NVENC/CUDA or x264 chains.
pub fn video_options(
    codec: VideoCodec,
    engine: Engine,
    k: u32,
    maxrate_k: u32,
    buf_k: u32,
    gop: u32,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-c:v".into(),
        encoder_name(codec, engine).unwrap_or("libx264").into(),
    ];
    let rate = |a: &mut Vec<String>| {
        a.extend([
            "-b:v".into(),
            format!("{k}k"),
            "-maxrate".into(),
            format!("{maxrate_k}k"),
            "-bufsize".into(),
            format!("{buf_k}k"),
        ]);
    };
    match (codec, engine) {
        (VideoCodec::H264, Engine::Cpu) => {
            a.extend(["-preset", "veryfast", "-profile:v", "high"].map(String::from));
            a.extend(["-sc_threshold".into(), "0".into()]);
            rate(&mut a);
        }
        (VideoCodec::H265, Engine::Cpu) => {
            a.extend(["-preset", "ultrafast"].map(String::from));
            a.extend([
                "-x265-params".into(),
                "log-level=error:repeat-headers=1".into(),
            ]);
            rate(&mut a);
        }
        (VideoCodec::H266, Engine::Cpu) => {
            // vvenc has no CBR mode: a target rate with its rate control
            // (two passes are impossible live). `faster` is its quickest
            // preset; `-period 1` a key frame about each second.
            a.extend(["-preset", "faster"].map(String::from));
            a.extend(["-b:v".into(), format!("{k}k")]);
            a.extend(["-period".into(), "1".into()]);
        }
        (VideoCodec::Av1, Engine::Cpu) => {
            // Preset 12 is SVT-AV1's fastest; pred-struct 1 its low-delay
            // structure, which its CBR mode (rc 2) needs.
            a.extend(["-preset", "12"].map(String::from));
            a.extend([
                "-svtav1-params".into(),
                format!("pred-struct=1:rc=2:tbr={k}:keyint={gop}"),
            ]);
            a.extend(["-b:v".into(), format!("{k}k")]);
        }
        (VideoCodec::Mpeg2, _) => rate(&mut a),
        (_, Engine::Nvenc) => {
            a.extend(["-preset", "p4", "-tune", "hq", "-rc", "cbr"].map(String::from));
            rate(&mut a);
        }
        (_, Engine::Amf) => {
            a.extend(["-usage", "transcoding", "-rc", "cbr"].map(String::from));
            rate(&mut a);
        }
        (_, Engine::Qsv) => {
            a.extend(["-preset", "veryfast", "-look_ahead", "0"].map(String::from));
            rate(&mut a);
        }
    }
    // SVT-AV1 takes its key frame period in its own params; every other
    // encoder needs `-g`, or it keeps ffmpeg's default of 250 pictures.
    if !(codec == VideoCodec::Av1 && engine == Engine::Cpu) {
        a.extend(["-g".into(), gop.to_string()]);
    }
    a
}

/// The cameras and microphones the system offers, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    pub cameras: Vec<String>,
    pub microphones: Vec<String>,
}

/// Read DirectShow's device list from ffmpeg's log
/// (`-list_devices true -f dshow -i dummy`): lines `"Name" (video)`.
pub fn parse_dshow_devices(log: &str) -> Devices {
    let mut d = Devices::default();
    for line in log.lines() {
        let Some(start) = line.find('"') else {
            continue;
        };
        let rest = &line[start + 1..];
        let Some(end) = rest.find('"') else { continue };
        let name = &rest[..end];
        let tail = &rest[end + 1..];
        if line.contains("Alternative name") {
            continue;
        }
        if tail.contains("(video)") {
            d.cameras.push(name.to_string());
        } else if tail.contains("(audio)") {
            d.microphones.push(name.to_string());
        }
    }
    d
}

/// The cameras and microphones, asked of ffmpeg (a second or so).
pub fn list_devices(exe: &Path) -> Devices {
    if cfg!(windows) {
        let out = command(exe)
            .args([
                "-hide_banner",
                "-list_devices",
                "true",
                "-f",
                "dshow",
                "-i",
                "dummy",
            ])
            .stdin(Stdio::null())
            .output();
        out.map(|o| parse_dshow_devices(&String::from_utf8_lossy(&o.stderr)))
            .unwrap_or_default()
    } else {
        let cameras = (0..8)
            .map(|i| format!("/dev/video{i}"))
            .filter(|p| Path::new(p).exists())
            .collect();
        Devices {
            cameras,
            microphones: vec!["default".into()],
        }
    }
}

/// Read a camera's modes from ffmpeg's log (`-list_options true`). Duplicates
/// dropped, MJPEG first, largest first.
pub fn parse_camera_modes(log: &str) -> Vec<CameraMode> {
    let mut v: Vec<CameraMode> = Vec::new();
    for line in log.lines() {
        let format = if let Some(i) = line.find("vcodec=") {
            line[i + 7..].split_whitespace().next()
        } else if let Some(i) = line.find("pixel_format=") {
            line[i + 13..].split_whitespace().next()
        } else {
            continue;
        };
        let Some(format) = format else { continue };
        // The "max" half: the largest size and rate the mode reaches.
        let Some(max) = line.rsplit_once("max ") else {
            continue;
        };
        let (mut w, mut h, mut fps) = (0, 0, 0);
        for tok in max.1.split_whitespace() {
            if let Some(sz) = tok.strip_prefix("s=")
                && let Some((a, b)) = sz.split_once('x')
            {
                w = a.parse().unwrap_or(0);
                h = b.parse().unwrap_or(0);
            } else if let Some(f) = tok.strip_prefix("fps=") {
                fps = f.parse::<f64>().map(|f| f.round() as u32).unwrap_or(0);
            }
        }
        if w == 0 || h == 0 || fps == 0 {
            continue;
        }
        let m = CameraMode {
            width: w,
            height: h,
            fps,
            format: format.to_string(),
        };
        if !v.contains(&m) {
            v.push(m);
        }
    }
    v.sort_by_key(|m| {
        (
            m.format != "mjpeg",
            std::cmp::Reverse(m.width * m.height),
            std::cmp::Reverse(m.fps),
        )
    });
    v
}

/// A camera's modes, asked of ffmpeg.
pub fn camera_modes(exe: &Path, camera: &str) -> Vec<CameraMode> {
    if !cfg!(windows) {
        return Vec::new();
    }
    command(exe)
        .args(["-hide_banner", "-f", "dshow", "-list_options", "true", "-i"])
        .arg(format!("video={camera}"))
        .stdin(Stdio::null())
        .output()
        .map(|o| parse_camera_modes(&String::from_utf8_lossy(&o.stderr)))
        .unwrap_or_default()
}

/// The mode to capture for a picture of `w`x`h` at `fps`: the smallest MJPEG
/// mode that covers it (cheap to decode; cameras give their full rate only as
/// MJPEG), else the camera's largest.
pub fn best_mode(modes: &[CameraMode], w: u32, h: u32, fps: u32) -> Option<CameraMode> {
    modes
        .iter()
        .filter(|m| m.width >= w && m.height >= h && m.fps >= fps)
        .min_by_key(|m| (m.format != "mjpeg", m.width * m.height))
        .or_else(|| modes.first())
        .cloned()
}

/// Which encoders work on this machine: each tried for a few small frames (a
/// graphics card's encoder is only listed by ffmpeg, not known to work, until
/// it has encoded something). Takes a few seconds; run it off the UI.
pub fn working_encoders(exe: &Path) -> Vec<(VideoCodec, Engine)> {
    let mut v = Vec::new();
    for codec in VideoCodec::ALL {
        for engine in [Engine::Cpu, Engine::Nvenc, Engine::Amf, Engine::Qsv] {
            let Some(name) = encoder_name(codec, engine) else {
                continue;
            };
            let ok = command(exe)
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=size=320x180:rate=25")
                .args(["-frames:v", "3", "-c:v", name, "-f", "null", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                v.push((codec, engine));
            }
        }
    }
    v
}

/// The engine for `codec` among `working`: `wanted` if it works, else the
/// first of [`AUTO_ORDER`] that does.
pub fn pick_engine(
    codec: VideoCodec,
    wanted: Option<Engine>,
    working: &[(VideoCodec, Engine)],
) -> Option<Engine> {
    let ok = |e| working.contains(&(codec, e));
    match wanted {
        Some(e) if ok(e) => Some(e),
        _ => AUTO_ORDER.into_iter().find(|&e| ok(e)),
    }
}

/// Where ffmpeg is, for callers that only have the settings' path.
pub fn ffmpeg_or_search(configured: Option<&Path>) -> Option<PathBuf> {
    crate::ffmpeg::find_ffmpeg(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_dshow_devices() {
        let log = r#"[in#0 @ 0000] "USB2.0 HD UVC WebCam" (video)
[in#0 @ 0000]   Alternative name "@device_pnp_\\?\usb#vid"
[in#0 @ 0000] "Microphone Array (Realtek(R) Audio)" (audio)
Error opening input file dummy."#;
        let d = parse_dshow_devices(log);
        assert_eq!(d.cameras, ["USB2.0 HD UVC WebCam"]);
        assert_eq!(d.microphones, ["Microphone Array (Realtek(R) Audio)"]);
    }

    #[test]
    fn reads_camera_modes_and_picks_one() {
        let log = "\
[in#0 @ 0]   pixel_format=yuyv422  min s=1280x720 fps=10 max s=1280x720 fps=10
[in#0 @ 0]   vcodec=mjpeg  min s=1280x720 fps=30 max s=1280x720 fps=30
[in#0 @ 0]   vcodec=mjpeg  min s=640x480 fps=30 max s=640x480 fps=30
[in#0 @ 0]   vcodec=mjpeg  min s=640x480 fps=30 max s=640x480 fps=30";
        let m = parse_camera_modes(log);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].describe(), "1280x720 30 fps MJPEG");
        assert_eq!(best_mode(&m, 640, 360, 25).unwrap().width, 640);
        assert_eq!(best_mode(&m, 1280, 720, 25).unwrap().width, 1280);
    }

    #[test]
    fn every_listed_codec_has_a_cpu_encoder() {
        for c in VideoCodec::ALL {
            assert!(encoder_name(c, Engine::Cpu).is_some(), "{c:?}");
        }
        assert!(encoder_name(VideoCodec::H266, Engine::Nvenc).is_none());
    }

    #[test]
    fn engines_are_picked_best_first() {
        let w = [
            (VideoCodec::H265, Engine::Cpu),
            (VideoCodec::H265, Engine::Nvenc),
        ];
        assert_eq!(pick_engine(VideoCodec::H265, None, &w), Some(Engine::Nvenc));
        assert_eq!(
            pick_engine(VideoCodec::H265, Some(Engine::Cpu), &w),
            Some(Engine::Cpu)
        );
        assert_eq!(pick_engine(VideoCodec::H266, None, &w), None);
    }
}
