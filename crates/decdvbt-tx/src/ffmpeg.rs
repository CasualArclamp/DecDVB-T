// made by CasualArclamp
//! ffmpeg as the video side of the transmitter: where it is, what the input
//! file holds, which encoder chain works here, and the exact command line.
//!
//! The command lines are fixed in `docs/TX_DESIGN.md` §4; this module builds
//! them from a [`TxConfig`]. Every figure there was measured on this machine
//! (ffmpeg 9.0.2, Gyan full build).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use decdvbt_core::{Error, Result};

use crate::config::{AudioCodec, EncoderChoice, Source, TxConfig, VideoCodec};
use crate::encode::{AUTO_ORDER, Engine, encoder_name};
use crate::modes::VideoProfile;

/// What ffprobe says about the input.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VideoInfo {
    pub duration: Option<Duration>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_codec: String,
    pub audio_codec: Option<String>,
    pub audio_channels: u32,
    /// `field_order` is not "progressive": the software chain adds `yadif`
    /// and the GPU chain is not used.
    pub interlaced: bool,
    pub bit_depth: u32,
    /// Container bitrate, bit/s, if known.
    pub bitrate: Option<u64>,
}

/// A working encoder chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    /// CUDA decode, `scale_cuda`, `h264_nvenc`: 0.07 to 0.11 core per stream
    /// second, whatever the source (measured).
    NvencCuda,
    /// `libx264 -preset veryfast`, decode on one thread (or `-hwaccel cuda`
    /// with frames copied back for a 4K source): 0.5 to 1.2 core.
    X264,
    /// `mpeg2video` (software, 0.37 core at 720p).
    Mpeg2,
    /// Any other codec or engine (H.265, H.266, AV1, a card's encoder for a
    /// camera): software decode and filters, the encoder's own options
    /// ([`crate::encode::video_options`]).
    Encoder(VideoCodec, Engine),
}

impl Chain {
    /// For the GUI's details line.
    pub fn label(self) -> String {
        match self {
            Chain::NvencCuda => "NVENC (CUDA)".into(),
            Chain::X264 => "x264 veryfast".into(),
            Chain::Mpeg2 => "MPEG-2".into(),
            Chain::Encoder(c, e) => format!("{} on {}", c.label(), e.label()),
        }
    }
}

/// The program's file name here.
const EXE: &str = if cfg!(windows) {
    "ffmpeg.exe"
} else {
    "ffmpeg"
};
const PROBE_EXE: &str = if cfg!(windows) {
    "ffprobe.exe"
} else {
    "ffprobe"
};

/// Where ffmpeg is: `configured` if it exists; else beside this program, on
/// the PATH, or where winget puts it. Same search as
/// `decdvbt_video::ffmpeg::find` (copied: that crate drags in the audio
/// stack).
pub fn find_ffmpeg(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        dirs.push(d);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let winget = PathBuf::from(local).join("Microsoft").join("WinGet");
        dirs.push(winget.join("Links"));
        // Where `winget install Gyan.FFmpeg` unpacks it (Packages\Gyan.FFmpeg…\
        // ffmpeg-<version>-full_build\bin), for when it has made no link and
        // the PATH has not been refreshed yet.
        if let Ok(packages) = std::fs::read_dir(winget.join("Packages")) {
            for p in packages.flatten() {
                if !p.file_name().to_string_lossy().starts_with("Gyan.FFmpeg") {
                    continue;
                }
                if let Ok(inner) = std::fs::read_dir(p.path()) {
                    dirs.extend(inner.flatten().map(|d| d.path().join("bin")));
                }
            }
        }
    }
    for var in ["ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            dirs.push(PathBuf::from(pf).join("ffmpeg").join("bin"));
        }
    }
    dirs.push(PathBuf::from(r"C:\ffmpeg\bin"));
    dirs.into_iter().map(|d| d.join(EXE)).find(|p| p.is_file())
}

/// ffprobe beside ffmpeg (the Gyan build ships both in one `bin`).
pub fn find_ffprobe(ffmpeg: &Path) -> Option<PathBuf> {
    let beside = ffmpeg.with_file_name(PROBE_EXE);
    beside.is_file().then_some(beside)
}

/// Run a console program without a console window popping up (Windows).
#[cfg(windows)]
pub(crate) fn no_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn no_console(_: &mut Command) {}

/// Run `ffprobe -v error -show_entries stream=...:format=duration,bit_rate
/// -of compact=p=0` on `input` and read the lines (no JSON crate). `Ok(None)`
/// if ffprobe is missing: transmission still works without the info.
pub fn probe(ffmpeg: &Path, input: &Path) -> Result<Option<VideoInfo>> {
    let Some(ffprobe) = find_ffprobe(ffmpeg) else {
        return Ok(None);
    };
    if !input.is_file() {
        return Err(Error::Other(format!("{} is not a file", input.display())));
    }
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "stream=codec_type,codec_name,width,height,r_frame_rate,field_order,channels,\
         bits_per_raw_sample,pix_fmt:format=duration,bit_rate",
        "-of",
        "compact=p=0",
    ])
    .arg(input)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    no_console(&mut cmd);
    let out = cmd.output().map_err(Error::Io)?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Other(format!(
            "ffprobe could not read {}: {}",
            input.display(),
            why.lines().next().unwrap_or("unknown error")
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_probe(&text)
        .map(Some)
        .ok_or_else(|| Error::Other(format!("{} holds no video stream", input.display())))
}

/// ffprobe's compact lines to a [`VideoInfo`]; `None` if there is no video
/// stream. A stream line has `codec_type=`, the format line has `duration=`.
pub fn parse_probe(text: &str) -> Option<VideoInfo> {
    let mut info = VideoInfo::default();
    let (mut have_video, mut have_audio) = (false, false);
    for line in text.lines() {
        let kv: Vec<(&str, &str)> = line.split('|').filter_map(|f| f.split_once('=')).collect();
        let get = |k: &str| kv.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
        match get("codec_type") {
            Some("video") if !have_video => {
                have_video = true;
                info.video_codec = get("codec_name").unwrap_or("").to_string();
                info.width = get("width").and_then(|v| v.parse().ok()).unwrap_or(0);
                info.height = get("height").and_then(|v| v.parse().ok()).unwrap_or(0);
                info.fps = get("r_frame_rate").and_then(parse_fraction).unwrap_or(0.0);
                info.interlaced = matches!(get("field_order"), Some("tt" | "bb" | "tb" | "bt"));
                let pix = get("pix_fmt").unwrap_or("");
                info.bit_depth = get("bits_per_raw_sample")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(if pix.contains("12") {
                        12
                    } else if pix.contains("10") {
                        10
                    } else {
                        8
                    });
            }
            Some("audio") if !have_audio => {
                have_audio = true;
                info.audio_codec = get("codec_name").map(str::to_string);
                info.audio_channels = get("channels").and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            Some(_) => {}
            None => {
                if let Some(d) = get("duration").and_then(|v| v.parse::<f64>().ok())
                    && d.is_finite()
                    && d > 0.0
                {
                    info.duration = Some(Duration::from_secs_f64(d));
                }
                info.bitrate = get("bit_rate").and_then(|v| v.parse().ok());
            }
        }
    }
    have_video.then_some(info)
}

/// "25/1" or "30000/1001" or "25".
fn parse_fraction(s: &str) -> Option<f64> {
    match s.split_once('/') {
        Some((n, d)) => {
            let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
            (d != 0.0).then_some(n / d)
        }
        None => s.parse().ok(),
    }
}

/// Whether the NVIDIA chain worked, found once per process.
static NVENC_WORKS: OnceLock<bool> = OnceLock::new();

/// A 0.2 s encode of a `color` source through the CUDA filters and NVENC. The
/// encoder list is not enough: `h264_qsv` is compiled into the Gyan build and
/// fails at run time, and NVENC without a card or driver does the same.
fn nvenc_works(ffmpeg: &Path) -> bool {
    *NVENC_WORKS.get_or_init(|| {
        let mut cmd = Command::new(ffmpeg);
        cmd.args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "error",
            "-init_hw_device",
            "cuda=cu",
            "-filter_hw_device",
            "cu",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=640x360:r=25:d=0.2",
            "-vf",
            "format=nv12,hwupload,scale_cuda=320:240",
            "-c:v",
            "h264_nvenc",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        no_console(&mut cmd);
        cmd.status().map(|s| s.success()).unwrap_or(false)
    })
}

/// Whether `codec` encodes on `engine` here: a 3-frame test encode, cached
/// for the process. The encoder list is not enough: `h264_qsv` is compiled
/// into the Gyan build and fails at run time, and NVENC without a card or
/// driver does the same.
fn encoder_works(ffmpeg: &Path, codec: VideoCodec, engine: Engine) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<(VideoCodec, Engine), bool>>> = OnceLock::new();
    let Some(name) = encoder_name(codec, engine) else {
        return false;
    };
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&ok) = cache.lock().unwrap().get(&(codec, engine)) {
        return ok;
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error", "-f", "lavfi", "-i"])
        .arg("testsrc2=size=320x180:rate=25")
        .args(["-frames:v", "3", "-c:v", name, "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    no_console(&mut cmd);
    let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
    cache.lock().unwrap().insert((codec, engine), ok);
    ok
}

/// Which chain to use. H.264 from a file on NVIDIA keeps the measured CUDA
/// chain (decode, scale and encode on the card, `NvencCuda`; only for a
/// progressive 8-bit source, and not with the call-sign overlay, which needs
/// the frames in memory). Everything else picks an engine: the one asked for
/// if a test encode works, else the first of [`AUTO_ORDER`] that does.
/// `h264`'s CPU engine is the `X264` chain, MPEG-2 always `Mpeg2`.
pub fn detect_chain(
    ffmpeg: &Path,
    choice: EncoderChoice,
    codec: VideoCodec,
    info: Option<&VideoInfo>,
    cuda_ok: bool,
) -> Result<Chain> {
    if codec == VideoCodec::Mpeg2 {
        return Ok(Chain::Mpeg2);
    }
    if codec == VideoCodec::H264
        && cuda_ok
        && matches!(choice, EncoderChoice::Auto | EncoderChoice::Nvenc)
    {
        let suits = choice == EncoderChoice::Nvenc
            || info.is_none_or(|i| !i.interlaced && i.bit_depth <= 8);
        if suits && nvenc_works(ffmpeg) {
            return Ok(Chain::NvencCuda);
        }
    }
    let engine = match choice.engine() {
        Some(e) if encoder_works(ffmpeg, codec, e) => e,
        Some(e) => {
            return Err(Error::Other(format!(
                "{} does not encode {} on this PC",
                e.label(),
                codec.label()
            )));
        }
        None => AUTO_ORDER
            .into_iter()
            .find(|&e| encoder_works(ffmpeg, codec, e))
            .ok_or_else(|| {
                Error::Other(format!(
                    "this ffmpeg has no working {} encoder",
                    codec.label()
                ))
            })?,
    };
    Ok(match (codec, engine) {
        (VideoCodec::H264, Engine::Cpu) => Chain::X264,
        (c, e) => Chain::Encoder(c, e),
    })
}

/// The DVB service_type (EN 300 468 table 87) for the video we send.
pub fn service_type(video: VideoCodec, profile: &VideoProfile) -> u8 {
    let hd = profile.height > 576;
    match (video, hd) {
        // HEVC digital television (0x1F); H.266 and AV1 have no value of
        // their own in EN 300 468, so they go out as advanced-codec services.
        (VideoCodec::H265, _) => 0x1F,
        (VideoCodec::H264 | VideoCodec::H266 | VideoCodec::Av1, true) => 0x19, // advanced codec HD
        (VideoCodec::H264 | VideoCodec::H266 | VideoCodec::Av1, false) => 0x16, // advanced codec SD
        (VideoCodec::Mpeg2, true) => 0x11, // MPEG-2 HD digital television
        (VideoCodec::Mpeg2, false) => 0x01, // digital television
    }
}

/// A service name a receiver's menu can show: ASCII, at most 24 characters.
pub fn sanitise_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect();
    // Runs of spaces (a replaced letter, a double space) become one.
    let t = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = t.chars().take(24).collect();
    let t = t.trim_end().to_string();
    if t.is_empty() {
        "DecDVB-T TX".into()
    } else {
        t
    }
}

/// The ffmpeg arguments (not including the program) for `cfg`: input, filter
/// chain, encoder, `-muxrate`, service ids, `pipe:1`.
///
/// Invariants (tests check each): `-muxrate` is [`TxConfig::mux_rate_bps`];
/// `-threads 1` before `-i`; `-maxrate` and `-bufsize` always present for an
/// encoded chain; `-pcr_period 20`; `-muxdelay 0.4`; `-mpegts_flags
/// +system_b+nit` only if `muxer_has_nit`; `-stream_loop -1` before `-i` iff
/// `cfg.loop_input` (a file source); `-progress pipe:2 -nostats`; output is
/// `pipe:1` (then, with a preview, the preview's own output); never `-re` for
/// a file.
pub fn command_line(
    cfg: &TxConfig,
    info: Option<&VideoInfo>,
    chain: Chain,
    profile: &VideoProfile,
    muxer_has_nit: bool,
) -> Vec<String> {
    command_line_with(cfg, info, chain, profile, muxer_has_nit, None)
}

/// [`command_line`] with a `preview` URL (ffmpeg connects to it): the
/// picture small (320x180, 10 fps, RGBA) and the sound (8 kHz mono float) in
/// Matroska, for the window. A chain on the card's frames (`NvencCuda`) and a
/// transport stream that is not re-encoded have no preview.
pub fn command_line_with(
    cfg: &TxConfig,
    info: Option<&VideoInfo>,
    chain: Chain,
    profile: &VideoProfile,
    muxer_has_nit: bool,
    preview: Option<&str>,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut add = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    add(&[
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "warning",
        "-progress",
        "pipe:2",
        "-nostats",
    ]);

    let source = &cfg.source;
    let encodes = source.encodes();
    let (w, h) = (profile.width, profile.height);
    let fps = cfg.video.fps;
    let audio_wanted = encodes && cfg.video.audio_codec != AudioCodec::None;
    let preview = preview.filter(|_| encodes && chain != Chain::NvencCuda);

    // Input. The decoder's own threads are never left at "auto": 16 threads
    // spent twice the CPU of one (TX_DESIGN.md §4.3).
    add(&["-threads", "1"]);
    // Where the sound comes from: input 0 (a file, DirectShow's camera and
    // microphone) or input 1 (a separate device, the test tone); empty when
    // there is none.
    let mut audio_in = if audio_wanted { "0:a:0?" } else { "" }.to_string();
    match source {
        Source::File => {
            match chain {
                Chain::NvencCuda => add(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]),
                _ => {
                    add(&["-filter_threads", "1"]);
                    // 4K or 50/60 fps: software decode cannot keep up on one
                    // thread; the card decodes and the frames come back.
                    if info.is_some_and(|i| i.height > 1080 || i.fps > 30.5) {
                        add(&["-hwaccel", "auto"]);
                    }
                }
            }
            if cfg.loop_input {
                add(&["-stream_loop", "-1"]);
            }
            let input = cfg.input.to_string_lossy().into_owned();
            // A name starting with '-' would be read as an option.
            let input = if input.starts_with('-') {
                format!("./{input}")
            } else {
                input
            };
            add(&["-i", &input]);
        }
        Source::TsFile => {
            if cfg.loop_input {
                add(&["-stream_loop", "-1"]);
            }
            let input = cfg.input.to_string_lossy().into_owned();
            let input = if input.starts_with('-') {
                format!("./{input}")
            } else {
                input
            };
            add(&["-i", &input]);
        }
        Source::Udp(addr) => {
            let url = format!("udp://{addr}?fifo_size=5000000&overrun_nonfatal=1");
            add(&["-i", &url]);
        }
        Source::Test => {
            add(&["-filter_threads", "1", "-re", "-f", "lavfi", "-i"]);
            add(&[&format!("testsrc2=size={w}x{h}:rate={fps}")]);
            if audio_wanted {
                add(&["-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000"]);
                audio_in = "1:a:0".into();
            }
        }
        Source::Camera { video, audio, mode } => {
            add(&["-filter_threads", "1"]);
            let mut have_audio = audio_wanted;
            if cfg!(windows) {
                add(&["-f", "dshow", "-rtbufsize", "64M"]);
                if let Some(m) = mode {
                    add(&[
                        "-video_size",
                        &format!("{}x{}", m.width, m.height),
                        "-framerate",
                        &m.fps.to_string(),
                    ]);
                    if m.format == "mjpeg" {
                        add(&["-vcodec", "mjpeg"]);
                    } else {
                        add(&["-pixel_format", &m.format]);
                    }
                }
                let mut spec = format!("video={video}");
                match audio {
                    Some(mic) if have_audio => spec.push_str(&format!(":audio={mic}")),
                    _ => have_audio = false,
                }
                add(&["-i", &spec]);
                audio_in = "0:a:0".into();
            } else {
                add(&["-f", "v4l2"]);
                if let Some(m) = mode {
                    add(&[
                        "-video_size",
                        &format!("{}x{}", m.width, m.height),
                        "-framerate",
                        &m.fps.to_string(),
                    ]);
                }
                add(&["-i", video]);
                match audio {
                    Some(mic) if have_audio => add(&["-f", "alsa", "-i", mic]),
                    _ => have_audio = false,
                }
                audio_in = "1:a:0".into();
            }
            if !have_audio {
                audio_in.clear();
            }
        }
    }
    let audio_on = !audio_in.is_empty();

    let mut flags = String::from("+system_b");
    if muxer_has_nit {
        flags.push_str("+nit");
    }
    let audio_codec = if chain == Chain::Mpeg2 {
        AudioCodec::Mp2
    } else {
        cfg.video.audio_codec
    };
    let video_codec = match chain {
        Chain::Mpeg2 => VideoCodec::Mpeg2,
        Chain::Encoder(c, _) => c,
        _ => VideoCodec::H264,
    };

    // The sound is split for the preview's meter (through the filter graph,
    // which then also carries its resampling), except a file's: it may have
    // none.
    let split_audio = preview.is_some() && audio_on && !matches!(source, Source::File);
    if !encodes {
        // A transport stream as it is: every stream copied, the muxer
        // restamping the PCRs to the channel's constant rate.
        add(&["-map", "0", "-c", "copy"]);
    } else {
        // Picture. SD is anamorphic: 720x576 (352x288 at the lowest rates)
        // with the sample aspect ratio that makes it 16:9 (or 4:3 for a 4:3
        // source). HD keeps the source's aspect ratio inside the frame, so
        // the size varies, which receivers accept.
        let scale_name = if chain == Chain::NvencCuda {
            "scale_cuda"
        } else {
            "scale"
        };
        let sd = h <= 576;
        let dar = info
            .filter(|i| i.height > 0)
            .map(|i| i.width as f64 / i.height as f64);
        // The pixels' shape that shows `w`x`h` at 16:9 (4:3 for a 4:3
        // source): 64/45 and 16/15 for 720x576, 16/11 and 12/11 for the low
        // rates' 352x288.
        let (dw, dh) = if dar.is_some_and(|d| d < 1.5) {
            (4, 3)
        } else {
            (16, 9)
        };
        let sar = {
            let (n, d) = (dw * h, dh * w);
            let g = gcd(n, d);
            format!("{}/{}", n / g, d / g)
        };
        let mut vf = String::new();
        if chain != Chain::NvencCuda && info.is_some_and(|i| i.interlaced) {
            vf.push_str("yadif=deint=interlaced,");
        }
        if sd {
            vf.push_str(&format!("{scale_name}={w}:{h},setsar={sar}"));
        } else {
            vf.push_str(&format!(
                "{scale_name}={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2"
            ));
        }
        vf.push_str(&format!(",fps={fps}"));
        if chain != Chain::NvencCuda {
            vf.push_str(",format=yuv420p");
            if cfg.overlay && !cfg.service.name.is_empty() {
                vf.push(',');
                vf.push_str(&crate::encode::overlay_filter(&cfg.service.name, h));
            }
        }

        let mut add = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
        if preview.is_some() {
            let mut graph = format!(
                "[0:v:0]{vf},split=2[enc][pv0];[pv0]fps=10,scale=320:180:force_original_aspect_ratio=decrease,\
                 pad=320:180:-1:-1,format=rgba[pv]"
            );
            if split_audio {
                graph.push_str(&format!(
                    ";[{audio_in}]aresample=48000,asplit=2[aenc][apv0];[apv0]aresample=8000,\
                     aformat=sample_fmts=flt:channel_layouts=mono[apv]"
                ));
                add(&[
                    "-filter_complex",
                    &graph,
                    "-map",
                    "[enc]",
                    "-map",
                    "[aenc]",
                ]);
            } else {
                add(&["-filter_complex", &graph, "-map", "[enc]"]);
                if audio_on {
                    add(&["-map", &audio_in]);
                }
            }
        } else {
            add(&["-vf", &vf, "-map", "0:v:0"]);
            if audio_on {
                add(&["-map", &audio_in]);
            }
        }

        // Video encoder.
        let gop = (fps * cfg.video.gop_seconds.max(1)).to_string();
        let (vb, maxrate) = (
            format!("{}k", profile.video_kbps),
            format!("{}k", profile.maxrate_kbps),
        );
        let mut bufsize_kbps = profile.bufsize_kbps;
        match chain {
            Chain::NvencCuda => add(&[
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p4",
                "-tune",
                "hq",
                "-profile:v",
                "high",
                "-rc",
                "cbr",
            ]),
            Chain::X264 => add(&[
                "-c:v",
                "libx264",
                "-threads",
                "4",
                "-preset",
                "veryfast",
                "-profile:v",
                "high",
                "-keyint_min",
                &gop,
                "-sc_threshold",
                "0",
            ]),
            Chain::Mpeg2 => {
                add(&["-c:v", "mpeg2video", "-threads", "4"]);
                // MP@ML's decoder buffer is 1 835 008 bits; bigger is invalid
                // for an SD receiver.
                if !(w > 720 || h > 576) {
                    bufsize_kbps = bufsize_kbps.min(1835);
                }
            }
            Chain::Encoder(..) => {}
        }
        if let Chain::Encoder(codec, engine) = chain {
            let o = crate::encode::video_options(
                codec,
                engine,
                profile.video_kbps,
                profile.maxrate_kbps,
                bufsize_kbps,
                fps * cfg.video.gop_seconds.max(1),
            );
            a.extend(o);
        } else {
            let bufsize = format!("{bufsize_kbps}k");
            a.extend(
                [
                    "-b:v", &vb, "-maxrate", &maxrate, "-bufsize", &bufsize, "-g", &gop, "-bf",
                    "2",
                ]
                .map(String::from),
            );
        }

        // Audio.
        if audio_on {
            let ab = format!("{}k", profile.audio_kbps);
            let channels = if matches!(source, Source::Camera { .. }) {
                "1"
            } else {
                "2"
            };
            a.extend(
                [
                    "-c:a",
                    if audio_codec == AudioCodec::Mp2 {
                        "mp2"
                    } else {
                        "aac"
                    },
                    "-b:a",
                    &ab,
                    "-ar",
                    "48000",
                    "-ac",
                    channels,
                ]
                .map(String::from),
            );
            // (`-af` cannot be used on a stream the filter graph makes.)
            if !split_audio {
                a.extend(["-af", "aresample=async=1:first_pts=0"].map(String::from));
            }
        }
    }
    if audio_codec == AudioCodec::AacLatm && audio_on {
        flags.push_str("+latm");
    }

    // Mux: constant rate, exact PCR, the service identity.
    // A live source (camera, test pattern, UDP) keeps its own time, so the
    // multiplex runs 0.2 % under the channel's rate: the modulator fills the
    // difference with nulls and the ring never fills.
    let mux = cfg.mux_rate_bps();
    let rate = if source.is_live() {
        (mux as f64 * 0.998).floor() as u64
    } else {
        mux
    }
    .to_string();
    let s = &cfg.service;
    let stype = service_type(video_codec, profile).to_string();
    let (sid, onid, tsid) = (
        s.service_id.to_string(),
        s.original_network_id.to_string(),
        s.transport_stream_id.to_string(),
    );
    let provider = format!("service_provider={}", sanitise_name(&s.provider));
    let name = format!("service_name={}", sanitise_name(&s.name));
    let mut add = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    add(&[
        "-muxdelay",
        "0.4",
        "-f",
        "mpegts",
        "-muxrate",
        &rate,
        "-mpegts_flags",
        &flags,
        "-pcr_period",
        "20",
        "-mpegts_service_id",
        &sid,
        "-mpegts_original_network_id",
        &onid,
        "-mpegts_transport_stream_id",
        &tsid,
        "-mpegts_service_type",
        &stype,
        "-metadata",
        &provider,
        "-metadata",
        &name,
        "pipe:1",
    ]);
    if let Some(url) = preview {
        add(&["-map", "[pv]", "-c:v", "rawvideo"]);
        if audio_on && !matches!(source, Source::File) {
            add(&["-map", "[apv]", "-c:a", "pcm_f32le"]);
        }
        add(&[
            "-f",
            "matroska",
            "-live",
            "1",
            "-cluster_time_limit",
            "100",
            "-flush_packets",
            "1",
            url,
        ]);
    }
    a
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}

/// Whether `ffmpeg -h muxer=mpegts` mentions `nit` (ffmpeg 9 does; older
/// ones may not know the flag).
pub fn muxer_has_nit(ffmpeg: &Path) -> bool {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-h", "muxer=mpegts"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    no_console(&mut cmd);
    match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.trim_start().starts_with("nit ")),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::mux_rate_bps;
    use decdvbt_core::dvbt::{Guard, Mode, Modulation, Rate};

    fn cfg(m: Modulation, r: Rate, g: Guard) -> TxConfig {
        let mut c = TxConfig::new("clip.mp4".into());
        c.mode = Mode::uk(m, r);
        c.mode.guard = g;
        c
    }

    fn lines(c: &TxConfig, chain: Chain, nit: bool) -> Vec<String> {
        let p = VideoProfile::for_rate(mux_rate_bps(&c.mode));
        command_line(c, None, chain, &p, nit)
    }

    fn pos(a: &[String], s: &str) -> usize {
        a.iter()
            .position(|x| x == s)
            .unwrap_or_else(|| panic!("{s} missing in {a:?}"))
    }

    fn after<'a>(a: &'a [String], s: &str) -> &'a str {
        &a[pos(a, s) + 1]
    }

    #[test]
    fn invariants_hold_for_every_chain_and_target() {
        let targets = [
            cfg(Modulation::Qam64, Rate::R2_3, Guard::G1_32),
            cfg(Modulation::Qam16, Rate::R1_2, Guard::G1_32),
            cfg(Modulation::Qpsk, Rate::R1_2, Guard::G1_8),
        ];
        for c in &targets {
            for chain in [Chain::NvencCuda, Chain::X264, Chain::Mpeg2] {
                let a = lines(c, chain, true);
                let rate = mux_rate_bps(&c.mode).to_string();
                assert_eq!(after(&a, "-muxrate"), rate);
                assert!(pos(&a, "-threads") < pos(&a, "-i"));
                assert_eq!(after(&a, "-threads"), "1");
                assert!(a.contains(&"-maxrate".to_string()));
                assert!(a.contains(&"-bufsize".to_string()));
                assert_eq!(after(&a, "-pcr_period"), "20");
                assert_eq!(after(&a, "-muxdelay"), "0.4");
                assert_eq!(after(&a, "-progress"), "pipe:2");
                assert!(a.contains(&"-nostats".to_string()));
                assert_eq!(a.last().unwrap(), "pipe:1");
                assert!(!a.contains(&"-re".to_string()));
                assert!(!after(&a, "-mpegts_flags").contains("resend_headers"));
                assert!(after(&a, "-mpegts_flags").contains("+system_b+nit"));
                // Looping is inside ffmpeg, before the input.
                assert!(pos(&a, "-stream_loop") < pos(&a, "-i"));
                assert_eq!(after(&a, "-stream_loop"), "-1");
            }
        }
    }

    #[test]
    fn looping_and_nit_are_conditional() {
        let mut c = cfg(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        c.loop_input = false;
        let a = lines(&c, Chain::X264, false);
        assert!(!a.contains(&"-stream_loop".to_string()));
        assert!(!after(&a, "-mpegts_flags").contains("nit"));
        assert!(after(&a, "-mpegts_flags").contains("system_b"));
    }

    #[test]
    fn the_gpu_chain_is_the_measured_one() {
        let c = cfg(Modulation::Qam16, Rate::R1_2, Guard::G1_32);
        let a = lines(&c, Chain::NvencCuda, true);
        assert_eq!(after(&a, "-hwaccel"), "cuda");
        assert_eq!(after(&a, "-hwaccel_output_format"), "cuda");
        assert_eq!(
            after(&a, "-vf"),
            "scale_cuda=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,fps=25"
        );
        assert_eq!(after(&a, "-c:v"), "h264_nvenc");
        assert_eq!(after(&a, "-rc"), "cbr");
        assert_eq!(after(&a, "-mpegts_flags"), "+system_b+nit+latm");
        assert_eq!(after(&a, "-mpegts_service_type"), "25");
    }

    #[test]
    fn sd_is_anamorphic_and_the_software_chain_converts() {
        let c = cfg(Modulation::Qpsk, Rate::R1_2, Guard::G1_8);
        let a = lines(&c, Chain::X264, true);
        assert_eq!(
            after(&a, "-vf"),
            "scale=720:576,setsar=64/45,fps=25,format=yuv420p"
        );
        assert_eq!(after(&a, "-c:v"), "libx264");
        assert_eq!(after(&a, "-mpegts_service_type"), "22");
        let g = lines(&c, Chain::NvencCuda, true);
        assert_eq!(after(&g, "-vf"), "scale_cuda=720:576,setsar=64/45,fps=25");
    }

    #[test]
    fn the_narrow_channels_mux_slowly_and_send_cif() {
        // 1 MHz QPSK 1/2 1/8: 5 529 411 / 8 bit/s, a 352x288 picture whose
        // pixels make it 16:9.
        let mut c = cfg(Modulation::Qpsk, Rate::R1_2, Guard::G1_8);
        c.mode.bandwidth = decdvbt_core::dvbt::Bandwidth::Mhz1;
        let a = lines(&c, Chain::X264, true);
        assert_eq!(after(&a, "-muxrate"), "691176");
        assert_eq!(
            after(&a, "-vf"),
            "scale=352:288,setsar=16/11,fps=25,format=yuv420p"
        );
        assert_eq!(after(&a, "-b:a"), "64k");
        // T2 at 1.7 MHz: the PLP's rate.
        c.t2 = crate::modes::t2_mode(
            decdvbt_ofdm::t2::Fft::K8,
            decdvbt_ofdm::t2::Guard::G1_16,
            decdvbt_fec::t2::qam::Qam::Qam16,
            (3, 4),
        );
        let p = VideoProfile::for_rate(c.mux_rate_bps());
        let a = command_line(&c, None, Chain::X264, &p, true);
        assert_eq!(after(&a, "-muxrate"), c.mux_rate_bps().to_string());
        assert!(c.mux_rate_bps() > 3_000_000 && c.mux_rate_bps() < 5_000_000);
    }

    #[test]
    fn a_four_by_three_source_gets_the_four_by_three_sar() {
        let c = cfg(Modulation::Qpsk, Rate::R1_2, Guard::G1_8);
        let info = VideoInfo {
            width: 640,
            height: 480,
            fps: 25.0,
            interlaced: true,
            ..Default::default()
        };
        let p = VideoProfile::for_rate(mux_rate_bps(&c.mode));
        let a = command_line(&c, Some(&info), Chain::X264, &p, true);
        assert_eq!(
            after(&a, "-vf"),
            "yadif=deint=interlaced,scale=720:576,setsar=16/15,fps=25,format=yuv420p"
        );
    }

    #[test]
    fn a_heavy_source_decodes_on_the_card_in_the_software_chain() {
        let c = cfg(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        let p = VideoProfile::for_rate(mux_rate_bps(&c.mode));
        let info = VideoInfo {
            width: 3840,
            height: 2160,
            fps: 60.0,
            ..Default::default()
        };
        let a = command_line(&c, Some(&info), Chain::X264, &p, true);
        assert_eq!(after(&a, "-hwaccel"), "auto");
        assert!(pos(&a, "-hwaccel") < pos(&a, "-i"));
        let light = VideoInfo {
            width: 1920,
            height: 1080,
            fps: 25.0,
            ..Default::default()
        };
        let a = command_line(&c, Some(&light), Chain::X264, &p, true);
        assert!(!a.contains(&"-hwaccel".to_string()));
    }

    #[test]
    fn audio_codecs_set_the_flags() {
        let mut c = cfg(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        c.video.audio_codec = AudioCodec::AacAdts;
        let a = lines(&c, Chain::X264, true);
        assert_eq!(after(&a, "-c:a"), "aac");
        assert!(!after(&a, "-mpegts_flags").contains("latm"));
        c.video.audio_codec = AudioCodec::Mp2;
        let a = lines(&c, Chain::X264, true);
        assert_eq!(after(&a, "-c:a"), "mp2");
        let a = lines(&c, Chain::Mpeg2, true);
        assert_eq!(after(&a, "-c:v"), "mpeg2video");
        assert_eq!(after(&a, "-c:a"), "mp2");
        assert_eq!(after(&a, "-mpegts_service_type"), "17");
    }

    fn with_source(source: Source) -> (TxConfig, VideoProfile) {
        let mut c = cfg(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        c.source = source;
        let p = VideoProfile::for_rate(c.mux_rate_bps());
        (c, p)
    }

    #[test]
    fn the_other_codecs_use_their_own_encoders_and_service_types() {
        use crate::encode::Engine;
        let (c, p) = with_source(Source::Test);
        for (codec, engine, name, stype) in [
            (VideoCodec::H265, Engine::Cpu, "libx265", "31"),
            (VideoCodec::H265, Engine::Nvenc, "hevc_nvenc", "31"),
            (VideoCodec::H266, Engine::Cpu, "libvvenc", "25"),
            (VideoCodec::Av1, Engine::Cpu, "libsvtav1", "25"),
            (VideoCodec::Av1, Engine::Nvenc, "av1_nvenc", "25"),
        ] {
            let a = command_line(&c, None, Chain::Encoder(codec, engine), &p, true);
            assert_eq!(after(&a, "-c:v"), name);
            assert_eq!(after(&a, "-mpegts_service_type"), stype, "{name}");
            assert_eq!(after(&a, "-muxdelay"), "0.4");
            assert_eq!(a.last().unwrap(), "pipe:1");
        }
        // SVT-AV1 takes its key frame period in its own params; the others `-g`.
        let a = command_line(&c, None, Chain::Encoder(VideoCodec::Av1, Engine::Cpu), &p, true);
        assert!(!a.contains(&"-g".to_string()));
        assert!(after(&a, "-svtav1-params").contains("keyint=25"));
        let a = command_line(&c, None, Chain::Encoder(VideoCodec::H265, Engine::Cpu), &p, true);
        assert_eq!(after(&a, "-g"), "25");
    }

    #[test]
    fn the_test_pattern_and_the_camera_are_live_and_run_under_the_rate() {
        let (c, p) = with_source(Source::Test);
        let a = command_line(&c, None, Chain::X264, &p, true);
        assert!(a.contains(&"lavfi".to_string()));
        assert!(a.iter().any(|x| x.starts_with("testsrc2=size=1920x1080:rate=25")));
        assert_eq!(after(&a, "-map"), "0:v:0");
        // Live: 0.2 % under the channel's rate, and no looping.
        let rate = (c.mux_rate_bps() as f64 * 0.998).floor() as u64;
        assert_eq!(after(&a, "-muxrate"), rate.to_string());
        assert!(!a.contains(&"-stream_loop".to_string()));
        assert!(a.contains(&"sine=frequency=1000:sample_rate=48000".to_string()));
        assert!(a.contains(&"1:a:0".to_string()));

        let camera = Source::Camera {
            video: "USB Cam".into(),
            audio: Some("Mic".into()),
            mode: Some(crate::encode::CameraMode {
                width: 1280,
                height: 720,
                fps: 30,
                format: "mjpeg".into(),
            }),
        };
        let (c, p) = with_source(camera);
        let a = command_line(&c, None, Chain::X264, &p, true);
        if cfg!(windows) {
            assert_eq!(after(&a, "-f"), "dshow");
            assert_eq!(after(&a, "-i"), "video=USB Cam:audio=Mic");
            assert_eq!(after(&a, "-video_size"), "1280x720");
            assert_eq!(after(&a, "-vcodec"), "mjpeg");
        }
        assert_eq!(after(&a, "-muxrate"), rate.to_string());
        // A camera is mono.
        assert_eq!(after(&a, "-ac"), "1");
        // No microphone: no audio at all.
        let mut c2 = c.clone();
        if let Source::Camera { audio, .. } = &mut c2.source {
            *audio = None;
        }
        let a = command_line(&c2, None, Chain::X264, &p, true);
        assert!(!a.contains(&"-c:a".to_string()));
        assert!(!after(&a, "-mpegts_flags").contains("latm"));
    }

    #[test]
    fn a_transport_stream_is_copied_not_encoded() {
        for source in [Source::TsFile, Source::Udp("127.0.0.1:1234".parse().unwrap())] {
            let (mut c, p) = with_source(source.clone());
            c.input = "in.ts".into();
            let a = command_line(&c, None, Chain::X264, &p, true);
            assert_eq!(after(&a, "-c"), "copy");
            assert!(!a.contains(&"-c:v".to_string()));
            assert!(!a.contains(&"-vf".to_string()));
            assert!(!a.contains(&"-maxrate".to_string()));
            assert_eq!(after(&a, "-map"), "0");
            assert_eq!(after(&a, "-f"), "mpegts");
            match source {
                Source::TsFile => {
                    assert_eq!(after(&a, "-i"), "in.ts");
                    assert!(pos(&a, "-stream_loop") < pos(&a, "-i"));
                    assert_eq!(after(&a, "-muxrate"), c.mux_rate_bps().to_string());
                }
                _ => {
                    assert!(after(&a, "-i").starts_with("udp://127.0.0.1:1234?"));
                    assert!(!a.contains(&"-stream_loop".to_string()));
                }
            }
        }
    }

    #[test]
    fn the_preview_is_a_second_output_and_the_sound_is_not_filtered_twice() {
        let (mut c, p) = with_source(Source::Test);
        c.overlay = true;
        c.service.name = "N0CALL".into();
        let url = "tcp://127.0.0.1:5555";
        let a = command_line_with(&c, None, Chain::X264, &p, true, Some(url));
        let g = after(&a, "-filter_complex");
        assert!(g.contains("split=2[enc][pv0]") && g.contains("drawtext="));
        assert!(g.contains("asplit=2[aenc][apv0]"));
        // The graph makes the audio: `-af` on the same stream is an error.
        assert!(!a.contains(&"-af".to_string()));
        assert!(!a.contains(&"-vf".to_string()));
        assert!(pos(&a, "pipe:1") < pos(&a, "[pv]"));
        assert_eq!(a.last().unwrap(), url);
        // Without a preview the one chain, with the audio filter.
        let a = command_line_with(&c, None, Chain::X264, &p, true, None);
        assert!(a.contains(&"-vf".to_string()) && a.contains(&"-af".to_string()));
        // The card's own frames and a copied stream have no preview.
        let a = command_line_with(&c, None, Chain::NvencCuda, &p, true, Some(url));
        assert!(!a.contains(&"-filter_complex".to_string()));
        assert_eq!(a.last().unwrap(), "pipe:1");
    }

    #[test]
    fn no_audio_means_no_audio() {
        let (mut c, p) = with_source(Source::File);
        c.video.audio_codec = AudioCodec::None;
        let a = command_line(&c, None, Chain::X264, &p, true);
        assert!(!a.contains(&"-c:a".to_string()));
        assert!(!a.contains(&"0:a:0?".to_string()));
    }

    #[test]
    fn the_service_name_is_ascii_and_short() {
        assert_eq!(
            sanitise_name("Caf\u{e9} - a very long file name here.mp4"),
            "Caf - a very long file n"
        );
        assert_eq!(sanitise_name("   "), "DecDVB-T TX");
        let c = cfg(Modulation::Qam64, Rate::R2_3, Guard::G1_32);
        let a = lines(&c, Chain::X264, true);
        assert!(a.contains(&"service_name=DecDVB-T TX".to_string()));
        assert!(a.contains(&"service_provider=DecDVB-T".to_string()));
        assert_eq!(after(&a, "-mpegts_original_network_id"), "65281");
    }

    #[test]
    fn ffprobe_lines_are_read() {
        let text = "codec_name=h264|codec_type=video|width=1920|height=1080|pix_fmt=yuv420p|\
                    r_frame_rate=30000/1001|field_order=progressive|bits_per_raw_sample=N/A\n\
                    codec_name=aac|codec_type=audio|channels=2\n\
                    duration=192.500000|bit_rate=9876543\n";
        let i = parse_probe(text).unwrap();
        assert_eq!((i.width, i.height, i.bit_depth), (1920, 1080, 8));
        assert!((i.fps - 29.97).abs() < 0.01);
        assert!(!i.interlaced);
        assert_eq!(i.audio_codec.as_deref(), Some("aac"));
        assert_eq!(i.audio_channels, 2);
        assert_eq!(i.duration, Some(Duration::from_secs_f64(192.5)));
        assert_eq!(i.bitrate, Some(9_876_543));
        let ten = "codec_type=video|codec_name=hevc|width=3840|height=2160|pix_fmt=yuv420p10le|\
                   r_frame_rate=50/1|field_order=tt\n";
        let i = parse_probe(ten).unwrap();
        assert_eq!(i.bit_depth, 10);
        assert!(i.interlaced);
        assert!(parse_probe("codec_type=audio|codec_name=mp3\n").is_none());
    }
}
