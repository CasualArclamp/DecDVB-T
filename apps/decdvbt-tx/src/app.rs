// made by CasualArclamp
//! The window: state strip, source / mode / channel / radio controls, the
//! signal view and meters, and the safety strip (docs/TX_DESIGN.md §7).
//!
//! The window owns no DSP. It builds a `TxConfig`, hands it to the session
//! thread ([`crate::host::Host`]) and draws the `TxStatus` and spectrum rows it
//! gets back. What may be pressed when is decided by the pure functions in
//! `safety.rs`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use decdvbt_core::channel::{UHF_FIRST, UHF_LAST, uhf_centre_hz, uhf_channel_at};
use decdvbt_core::dvbt::{Bandwidth, Rate};
use decdvbt_tx::modes::{
    T2_FFTS, T2_QAMS, T2_RATES, T2Config, baseband_filter_hz, qam_name, t2_guards, t2_mode,
    t2_mode_table, t2_rate,
};
use decdvbt_tx::encode::{self, CameraMode, Devices, Engine};
use decdvbt_tx::{
    AudioCodec, EncoderChoice, HackRfTxConfig, LICENCE_WARNING, LicenceAck, PlutoTxConfig,
    ServiceInfo, Source, TxConfig, TxRate, TxSink, TxState, TxStatus, VideoCodec, VideoProfile,
    VideoSettings, mode_table,
};
use eframe::egui::{self, Color32, RichText, vec2};

use crate::cli::{Options, SinkArg};
use crate::demo::{self, DemoState};
use crate::host::{Cmd, Host};
use crate::meters;
use crate::modeinfo;
use crate::safety::{self, STOP_FIRST, VGA_MAX};
use crate::settings::{Settings, SourceKind};
use crate::source::{self, FileInfo, ProbeJob, Probed};
use crate::{AMBER, GREEN, RED, freq_display};

/// A session that has not shown its first status after this long is given up.
const PREPARE_GRACE: Duration = Duration::from_secs(20);
/// Settings are written at most this often.
const SAVE_EVERY: Duration = Duration::from_secs(1);

/// Where the signal goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkKind {
    /// Generated and thrown away: the default on every start.
    Test,
    IqFile,
    HackRf,
    Pluto,
}

impl SinkKind {
    fn label(self) -> &'static str {
        match self {
            SinkKind::Test => "Test only (no output)",
            SinkKind::IqFile => "IQ file",
            SinkKind::HackRf => "HackRF (radio)",
            SinkKind::Pluto => "ADALM-PLUTO / Pluto+ (radio)",
        }
    }
}

/// A message in the red strip.
struct ErrorStrip {
    text: String,
    /// Extra advice under it.
    hint: Option<String>,
}

/// What the background probe finds: ffmpeg, the cameras and microphones,
/// each camera's modes, which encoders work.
struct DevProbe {
    devices: Devices,
    modes: Vec<(String, Vec<CameraMode>)>,
    working: Vec<(VideoCodec, Engine)>,
}

/// The `--start` hook's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auto {
    /// Not asked for (or finished).
    Off,
    WaitProbe,
    WaitReady(Instant),
    Running(Instant),
    /// Stop sent: close when the session is gone.
    Closing(Instant),
}

pub struct App {
    opts: Options,
    set: Settings,
    saved: Settings,
    saved_at: Instant,
    started: Instant,
    /// `None` in a demo: no session exists, so no button can reach the library.
    host: Option<Host>,
    demo: Option<DemoState>,
    demo_rows: u64,

    file: Option<PathBuf>,
    file_info: Option<FileInfo>,
    probe: Option<ProbeJob>,
    /// The capture mode chosen; `None`: picked for the picture.
    cam_mode: Option<CameraMode>,
    /// Found in the background: devices, modes, working encoders.
    dev: Option<Receiver<DevProbe>>,
    found: Option<DevProbe>,
    /// The input picture, and the serial of the one shown.
    cam_tex: Option<egui::TextureHandle>,

    sink: SinkKind,
    paced: bool,
    iq_path: String,
    iq_seconds: String,
    /// The TX VGA the slider asks for, and the last value sent to the radio.
    txvga: u16,
    vga_sent: u16,
    amp: bool,
    /// The Pluto: its link (`auto`, `usb`, `ip:ADDRESS`), and the attenuation
    /// the slider asks for and the last value sent.
    pluto_uri: String,
    atten: f32,
    atten_sent: f32,

    /// The session as last published (or an idle one).
    status: TxStatus,
    /// A session exists or is being made (the form is locked).
    session: bool,
    /// `Prepare` is sent and the first status has not come back.
    waiting: Option<Instant>,
    /// `Stop` is sent and the session is not gone yet.
    stopping: bool,
    /// The config the session was made from (for the banner's frequency).
    session_cfg: Option<TxConfig>,

    /// The text the licence area shows, and the tick under it.
    licence_shown: &'static str,
    read_ack: bool,
    was_armed: bool,
    unfocused_since: Option<Instant>,

    error: Option<ErrorStrip>,
    seen_status_error: Option<String>,

    smooth: Vec<f32>,
    /// The last row's span, Hz.
    span_hz: f64,

    show_modes: bool,
    title: &'static str,
    auto: Auto,
    exit_code: i32,
    shot_sent: bool,
    /// Running mean of the process CPU while transmitting: (sum, count).
    cpu_mean: (f64, u32),
    /// The UI thread's CPU seconds and the time, when the steady part of a
    /// `--start` run began; and the frames drawn since.
    gui_mark: Option<(f64, Instant, u64, u64, u64)>,
    frames: u64,
    /// Calls of `ui` (a frame can take more than one pass).
    passes: u64,
    /// Spectrum rows received.
    rows_seen: u64,
}

impl App {
    pub fn new(opts: Options, ctx: &egui::Context) -> App {
        let demo = opts.demo.then(|| opts.state.unwrap_or(DemoState::Ready));
        // A screenshot or demo run keeps nothing and reads nothing, so it comes
        // out the same each time; a real run starts from what was kept.
        let reproducible = opts.screenshot.is_some() || opts.demo;
        let mut set = if reproducible {
            Settings::default()
        } else {
            Settings::load()
        };
        if let Some(m) = opts.mode {
            set.mode = m;
            set.use_t2 = false;
        }
        if let Some(t2) = &opts.t2 {
            set.t2 = t2.clone();
            set.use_t2 = true;
        }
        if let Some(c) = opts.centre_hz {
            set.centre_hz = c;
        }
        if let Some(c) = opts.codec {
            set.video_codec = c;
        }
        if let Some((kind, rest)) = &opts.source {
            set.source = *kind;
            match kind {
                SourceKind::Camera if !rest.is_empty() => set.camera = rest.clone(),
                SourceKind::Udp => set.udp = rest.clone(),
                _ => {}
            }
        }
        if reproducible || opts.start {
            // A placeholder call sign (no one's), for test runs and pictures.
            set.callsign = "N0CALL".into();
            set.overlay = true;
        }
        // A start is always on the test output unless `--sink` says another
        // test output; the radio is never the start-up choice.
        let (sink, iq_path) = match &opts.sink {
            Some(SinkArg::IqFile(p)) => (SinkKind::IqFile, p.display().to_string()),
            _ => (SinkKind::Test, String::new()),
        };
        let sink = match demo {
            Some(d) if d.radio_output() => SinkKind::HackRf,
            _ => sink,
        };
        let host = (demo.is_none() && !opts.selftest).then(|| Host::spawn(ctx.clone()));
        let mut app = App {
            saved: set.clone(),
            set,
            saved_at: Instant::now(),
            started: Instant::now(),
            host,
            demo,
            demo_rows: 0,
            file: None,
            file_info: None,
            probe: None,
            cam_mode: None,
            dev: None,
            found: None,
            cam_tex: None,
            sink,
            paced: true,
            iq_path,
            iq_seconds: String::new(),
            txvga: 0,
            vga_sent: 0,
            pluto_uri: "auto".into(),
            atten: safety::ATTEN_MAX,
            atten_sent: safety::ATTEN_MAX,
            amp: false,
            status: TxStatus::idle(&TxConfig::new("none.mp4".into()), &decdvbt_tx::Arm::new()),
            session: false,
            waiting: None,
            stopping: false,
            session_cfg: None,
            licence_shown: LICENCE_WARNING,
            read_ack: false,
            was_armed: false,
            unfocused_since: None,
            error: None,
            seen_status_error: None,
            smooth: Vec::new(),
            span_hz: 0.0,
            show_modes: opts.show_modes,
            title: "DecDVB-T TX",
            auto: if opts.start {
                Auto::WaitProbe
            } else {
                Auto::Off
            },
            exit_code: 0,
            shot_sent: false,
            cpu_mean: (0.0, 0),
            gui_mark: None,
            frames: 0,
            passes: 0,
            rows_seen: 0,
            opts,
        };
        app.status = app.idle_status();
        if let Some((SourceKind::File | SourceKind::TsFile, path)) = app.opts.source.clone() {
            app.file = Some(PathBuf::from(path));
        }
        if let Some(f) = app.opts.file.clone() {
            app.set.source = SourceKind::File;
            app.file = Some(f);
        }
        // (A test would run ffmpeg for every codec for nothing.)
        if app.demo.is_none() && !app.opts.selftest && !cfg!(test) {
            app.start_dev_probe(ctx);
        }
        if let Some(d) = demo {
            // The demo pretends a file was opened.
            app.file = Some(PathBuf::from("promo clip.mp4"));
            app.file_info = Some(FileInfo::Done(Probed::Info(demo::video_info())));
            app.session = d.has_session();
        } else if app.set.source == SourceKind::File
            && let Some(f) = app.file.clone()
        {
            // The file fills in the form and nothing else.
            app.open_file(f, ctx);
        }
        app
    }

    /// Find the cameras and microphones, their modes and which encoders work,
    /// off the window's thread (a few seconds of ffmpeg runs).
    fn start_dev_probe(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ffmpeg = self.set.ffmpeg_path.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("tx-devices".into())
            .spawn(move || {
                let found = match decdvbt_tx::ffmpeg::find_ffmpeg(ffmpeg.as_deref()) {
                    Some(exe) => {
                        let devices = encode::list_devices(&exe);
                        let modes = devices
                            .cameras
                            .iter()
                            .map(|c| (c.clone(), encode::camera_modes(&exe, c)))
                            .collect();
                        DevProbe {
                            devices,
                            modes,
                            working: encode::working_encoders(&exe),
                        }
                    }
                    None => DevProbe {
                        devices: Devices::default(),
                        modes: Vec::new(),
                        working: Vec::new(),
                    },
                };
                let _ = tx.send(found);
                ctx.request_repaint_after(Duration::from_millis(1));
            })
            .ok();
        self.dev = Some(rx);
    }

    fn poll_dev_probe(&mut self) {
        let Some(rx) = &self.dev else { return };
        let Ok(p) = rx.try_recv() else { return };
        self.dev = None;
        if !p.devices.cameras.contains(&self.set.camera) {
            self.set.camera = p.devices.cameras.first().cloned().unwrap_or_default();
        }
        if self
            .set
            .mic
            .as_ref()
            .is_none_or(|m| !p.devices.microphones.contains(m))
        {
            self.set.mic = p.devices.microphones.first().cloned();
        }
        self.found = Some(p);
    }

    /// The source has what it needs to start.
    fn source_ready(&self) -> bool {
        match self.set.source {
            SourceKind::Camera => !self.set.camera.is_empty(),
            SourceKind::File | SourceKind::TsFile => self.file.is_some(),
            SourceKind::Udp => self.set.udp.parse::<std::net::SocketAddr>().is_ok(),
            SourceKind::Test => true,
        }
    }

    /// What is missing, for the PREPARE button's hover text.
    fn source_missing(&self) -> &'static str {
        match self.set.source {
            SourceKind::Camera if self.dev.is_some() => "Looking for cameras...",
            SourceKind::Camera => "No camera found",
            SourceKind::File => "Choose a video file first",
            SourceKind::TsFile => "Choose a transport stream file first",
            SourceKind::Udp => "Listen on an address:port, like 127.0.0.1:1234",
            SourceKind::Test => "",
        }
    }

    /// The process exit code the run asks for.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    // ---- state helpers -------------------------------------------------

    fn sink_is_radio(&self) -> bool {
        matches!(self.sink, SinkKind::HackRf | SinkKind::Pluto)
    }

    /// The form is locked: a session exists (or a demo pretends one).
    fn locked(&self) -> bool {
        self.session || self.stopping
    }

    /// The DVB-T2 mode while 1.7 MHz is chosen; `None` is DVB-T.
    fn tx_t2(&self) -> Option<T2Config> {
        self.set.use_t2.then(|| self.set.t2.clone())
    }

    /// The channel's width the form says.
    fn bandwidth(&self) -> Bandwidth {
        if self.set.use_t2 {
            self.set.t2.bandwidth
        } else {
            self.set.mode.bandwidth
        }
    }

    fn idle_cfg(&self) -> TxConfig {
        let mut c = TxConfig::new(self.file.clone().unwrap_or_else(|| "none.mp4".into()));
        c.video.video_codec = self.set.video_codec;
        c.video.audio_codec = self.set.audio_codec;
        c.mode = self.set.mode;
        c.t2 = self.tx_t2();
        c.centre_hz = self.set.centre_hz;
        c.rate = self.set.rate;
        c
    }

    fn idle_status(&self) -> TxStatus {
        TxStatus::idle(&self.idle_cfg(), &decdvbt_tx::Arm::new())
    }

    /// The source the form describes.
    fn build_source(&self) -> Result<Source, String> {
        Ok(match self.set.source {
            SourceKind::Camera => {
                if self.set.camera.is_empty() {
                    return Err("no camera found".into());
                }
                Source::Camera {
                    video: self.set.camera.clone(),
                    audio: self.set.mic.clone(),
                    mode: self.cam_mode.clone().or_else(|| self.auto_mode()),
                }
            }
            SourceKind::File => Source::File,
            SourceKind::TsFile => Source::TsFile,
            SourceKind::Udp => Source::Udp(
                self.set
                    .udp
                    .trim()
                    .parse()
                    .map_err(|_| format!("{}: not an address:port", self.set.udp))?,
            ),
            SourceKind::Test => Source::Test,
        })
    }

    /// The config for the session, from the form.
    fn build_config(&self) -> Result<TxConfig, String> {
        let source = self.build_source()?;
        let file = match self.set.source {
            SourceKind::File | SourceKind::TsFile => {
                self.file.clone().ok_or_else(|| self.source_missing().to_string())?
            }
            _ => PathBuf::new(),
        };
        let mut c = TxConfig::new(file.clone());
        c.source = source;
        c.overlay = self.set.overlay;
        c.loop_input = self.set.loop_input;
        c.mode = self.set.mode;
        c.t2 = self.tx_t2();
        c.centre_hz = self.set.centre_hz;
        c.rate = self.set.rate;
        c.level_dbfs = self.set.level_dbfs;
        c.video = VideoSettings {
            encoder: self.set.encoder,
            video_codec: self.set.video_codec,
            audio_codec: self.set.audio_codec,
            ..VideoSettings::default()
        };
        let call = self.set.callsign.trim();
        c.service = ServiceInfo {
            name: if !call.is_empty() {
                call.to_string()
            } else if self.set.source == SourceKind::File {
                source::service_name(&file)
            } else {
                ServiceInfo::default().name
            },
            ..ServiceInfo::default()
        };
        c.ffmpeg_path = self.set.ffmpeg_path.clone();
        c.preview = self.set.preview;
        c.sink = match self.sink {
            SinkKind::Test => TxSink::Null { paced: self.paced },
            SinkKind::IqFile => {
                let path = if self.iq_path.trim().is_empty() {
                    PathBuf::from(source::iq_file_name(c.centre_hz, c.sample_rate_hz()))
                } else {
                    PathBuf::from(self.iq_path.trim())
                };
                TxSink::IqFile {
                    path,
                    paced: self.paced,
                    seconds: self
                        .iq_seconds
                        .trim()
                        .parse()
                        .ok()
                        .filter(|s: &f64| *s > 0.0),
                }
            }
            SinkKind::HackRf => TxSink::HackRf(HackRfTxConfig {
                txvga_db: self.txvga,
                amp: self.amp,
                // The filter follows the channel: 8 MHz for an 8 MHz channel,
                // the radio's narrowest (1.75 MHz) for 1 and 1.7 MHz.
                baseband_filter_hz: Some(baseband_filter_hz(self.bandwidth())),
            }),
            SinkKind::Pluto => TxSink::Pluto(PlutoTxConfig {
                uri: self.pluto_uri.trim().to_string(),
                attenuation_db: self.atten,
                // The AD9361's TX filter follows the channel too.
                bandwidth_hz: Some(baseband_filter_hz(self.bandwidth())),
            }),
        };
        Ok(c)
    }

    fn set_error(&mut self, text: String, from_radio_start: bool) {
        let hint =
            (from_radio_start && self.sink_is_radio() && !text.contains("not armed")).then(|| {
                if self.sink == SinkKind::Pluto {
                    "Is the Pluto plugged in, with ADI's USB driver (or reachable at \
                     192.168.2.1)? Over USB only one program can use it: close DecDVB-T's \
                     Pluto source, or use ip:192.168.2.1 in both."
                        .to_string()
                } else {
                    "Close DecDVB-T (or whatever holds the HackRF) and try again.".to_string()
                }
            });
        self.error = Some(ErrorStrip { text, hint });
    }

    // ---- actions -------------------------------------------------------

    fn open_file(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.locked() {
            self.set_error(format!("{STOP_FIRST} before choosing another file."), false);
            return;
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            self.set.folder = Some(dir.to_path_buf());
        }
        self.error = None;
        self.file = Some(path.clone());
        self.file_info = None;
        self.probe = None;
        // A transport stream is sent as it is: nothing to read from it.
        if self.set.source == SourceKind::File {
            self.file_info = Some(FileInfo::Probing);
            self.probe = Some(ProbeJob::start(
                path,
                self.set.ffmpeg_path.clone(),
                ctx.clone(),
            ));
        }
    }

    fn browse(&mut self, ctx: &egui::Context) {
        let ts = self.set.source == SourceKind::TsFile;
        let mut d = rfd::FileDialog::new()
            .set_title(if ts {
                "Choose a transport stream"
            } else {
                "Choose a video file"
            })
            .add_filter(
                if ts { "Transport stream" } else { "Video" },
                if ts {
                    &["ts", "m2ts", "mts"][..]
                } else {
                    &source::VIDEO_EXTENSIONS[..]
                },
            )
            .add_filter("All files", &["*"]);
        if let Some(f) = self.set.folder.as_ref().filter(|f| f.is_dir()) {
            d = d.set_directory(f);
        }
        if let Some(p) = d.pick_file() {
            self.open_file(p, ctx);
        }
    }

    fn prepare(&mut self) {
        let cfg = match self.build_config() {
            Ok(c) => c,
            Err(e) => {
                self.set_error(e, false);
                return;
            }
        };
        let Some(host) = &self.host else { return };
        self.error = None;
        self.seen_status_error = None;
        self.smooth.clear();
        self.read_ack = false;
        self.vga_sent = self.txvga;
        self.atten_sent = self.atten;
        self.session_cfg = Some(cfg.clone());
        self.session = true;
        self.waiting = Some(Instant::now());
        self.status = self.idle_status();
        self.status.state = TxState::Preparing;
        host.send(Cmd::Prepare(Box::new(cfg)));
    }

    /// STOP / cancel / reset: everything off and the arm gone.
    fn stop(&mut self) {
        self.read_ack = false;
        if let Some(host) = &self.host {
            host.send(Cmd::Stop);
            if self.session {
                self.stopping = true;
            }
        } else if self.demo.is_some() {
            // A demo has nothing to stop; the button just returns it to idle.
            self.demo = Some(DemoState::Idle);
            self.session = false;
        }
        self.auto = Auto::Off;
    }

    fn arm(&mut self) {
        // The only way to an arm: the exact text the window shows.
        if let (Some(host), Some(ack)) = (&self.host, LicenceAck::accept(self.licence_shown)) {
            host.send(Cmd::Arm(ack));
        }
    }

    fn transmit(&mut self) {
        if let Some(host) = &self.host {
            self.read_ack = false;
            host.send(Cmd::Transmit);
        }
    }

    // ---- per frame: logic ---------------------------------------------

    fn handle_dropped(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
        });
        if let Some(p) = dropped
            && self.demo.is_none()
        {
            // A transport stream is a TS source, anything else a video file.
            let ts = p
                .extension()
                .is_some_and(|e| ["ts", "m2ts", "mts"].iter().any(|x| e.eq_ignore_ascii_case(x)));
            if !self.locked() {
                self.set.source = if ts {
                    SourceKind::TsFile
                } else {
                    SourceKind::File
                };
            }
            self.open_file(p, ctx);
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (esc, space) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::Space),
            )
        });
        // Space is also a character in a text box: only when none is being typed in.
        if (esc || (space && !ctx.egui_wants_keyboard_input())) && self.locked() {
            self.stop();
        }
    }

    fn poll_probe(&mut self) {
        let Some(job) = &self.probe else { return };
        let Some(result) = job.poll() else { return };
        let name = source::file_label(&job.path);
        self.probe = None;
        match &result {
            Probed::NoFfmpeg(m) => {
                self.error = Some(ErrorStrip {
                    text: m.clone(),
                    hint: None,
                })
            }
            Probed::Bad(m) => {
                self.error = Some(ErrorStrip {
                    text: format!("{name}: {m}"),
                    hint: Some("This does not look like a video file ffmpeg can read.".into()),
                });
            }
            _ => {}
        }
        self.file_info = Some(FileInfo::Done(result));
    }

    /// The newest input picture from the session, as a texture.
    fn update_picture(&mut self, ctx: &egui::Context) {
        let Some(f) = self.host.as_ref().and_then(|h| h.take_picture()) else {
            return;
        };
        let img = egui::ColorImage::from_rgba_unmultiplied(
            [f.width as usize, f.height as usize],
            &f.rgba,
        );
        match &mut self.cam_tex {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => {
                self.cam_tex = Some(ctx.load_texture("camera", img, egui::TextureOptions::LINEAR));
            }
        }
    }

    fn poll_host(&mut self) {
        let Some(host) = &self.host else { return };
        let st = host.status();
        let problems = host.take_problems();
        let mut rows = Vec::new();
        host.take_rows(&mut rows);
        let dead = host.is_dead();
        let have = st.is_some();
        match st {
            Some(s) => {
                self.waiting = None;
                self.session = true;
                self.status = s;
            }
            None => {
                if self.waiting.is_some_and(|t| t.elapsed() > PREPARE_GRACE) {
                    self.waiting = None;
                }
                if self.waiting.is_none() {
                    self.session = false;
                    self.stopping = false;
                    if self.status.state != TxState::Idle || self.status.radiating {
                        self.status = self.idle_status();
                    }
                }
            }
        }
        for p in problems {
            if !have {
                // `prepare()` failed outright: there is no session to show.
                self.waiting = None;
            }
            self.set_error(p.text, p.from_radio_start);
        }
        if dead && self.session {
            self.session = false;
            self.waiting = None;
            self.set_error("the session thread stopped unexpectedly".into(), false);
        }
        for r in rows {
            self.push_row(r.bins, r.sample_rate_hz);
        }
        self.finish_status_effects();
    }

    /// A failure the session reports about itself goes in the red strip, once.
    fn note_status_error(&mut self) {
        if self.status.state != TxState::Failed && self.status.error.is_none() {
            return;
        }
        let e = self
            .status
            .error
            .clone()
            .unwrap_or_else(|| "the session failed".into());
        if self.seen_status_error.as_deref() != Some(e.as_str()) {
            self.seen_status_error = Some(e.clone());
            self.set_error(e, false);
        }
    }

    fn push_row(&mut self, bins: Vec<f32>, rate_hz: f64) {
        self.span_hz = rate_hz;
        self.rows_seen += 1;
        // The trace is smoothed a little so it does not flicker.
        if self.smooth.len() != bins.len() {
            self.smooth = bins;
        } else {
            for (s, b) in self.smooth.iter_mut().zip(&bins) {
                *s += 0.4 * (b - *s);
            }
        }
    }

    /// Things that follow from the status: the ack tick, the VGA.
    fn finish_status_effects(&mut self) {
        // An arm that ended (used, lapsed, dropped) takes the tick with it:
        // every arm starts from a fresh, deliberate tick.
        if self.was_armed && !self.status.armed {
            self.read_ack = false;
        }
        self.was_armed = self.status.armed;
        if self.status.state != TxState::Transmitting {
            self.vga_sent = self.status.txvga_db;
            if self.sink == SinkKind::Pluto {
                self.atten_sent = self.status.atten_db;
            }
        }
    }

    /// The window out of focus for 5 s drops the arm; the TX VGA walks toward
    /// the slider in steps of at most 6 dB.
    fn safety_tick(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if focused {
            self.unfocused_since = None;
        } else if self.unfocused_since.is_none() {
            self.unfocused_since = Some(Instant::now());
        }
        if self.status.armed {
            if safety::disarm_on_blur(self.unfocused_since.map(|t| t.elapsed()))
                && let Some(h) = &self.host
            {
                h.send(Cmd::Disarm);
            }
            // The countdown and the blur timer need frames.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        if self.sink_is_radio()
            && self.status.state == TxState::Transmitting
            && self.vga_sent != self.txvga
            && let Some(h) = &self.host
        {
            let next = safety::vga_step(self.vga_sent, self.txvga);
            h.send(Cmd::SetVga(next));
            self.vga_sent = next;
            ctx.request_repaint_after(Duration::from_millis(60));
        }
        if self.sink == SinkKind::Pluto
            && self.status.state == TxState::Transmitting
            && self.atten_sent != self.atten
            && let Some(h) = &self.host
        {
            let next = safety::atten_step(self.atten_sent, self.atten);
            h.send(Cmd::SetAtten(next));
            self.atten_sent = next;
            ctx.request_repaint_after(Duration::from_millis(60));
        }
    }

    fn feed_demo(&mut self) {
        let Some(state) = self.demo else { return };
        let t = self.started.elapsed().as_secs_f64();
        let mut cfg = self.idle_cfg();
        cfg.sink = if state.radio_output() {
            TxSink::HackRf(HackRfTxConfig::default())
        } else {
            cfg.sink
        };
        self.status = demo::status(state, &cfg, t);
        self.session = state.has_session();
        if state == DemoState::Transmitting {
            self.txvga = self.status.txvga_db;
        }
        // Spectrum rows: 20 a second.
        if matches!(state, DemoState::Idle | DemoState::Failed) {
            return;
        }
        let want = if self.demo_rows == 0 {
            1
        } else {
            ((t * 20.0) as u64).saturating_sub(self.demo_rows).min(60)
        };
        let occupied = self.idle_cfg().occupied_hz();
        let rate = self.status.sample_rate_hz as f64;
        let first = self.demo_rows;
        for k in 0..want {
            let row = demo::row(first + k, rate, occupied);
            self.push_row(row.bins, rate);
        }
        self.demo_rows += want.max(1);
        self.finish_status_effects();
    }

    fn drive_auto(&mut self, ctx: &egui::Context) {
        match self.auto {
            Auto::Off => {}
            Auto::Closing(since) => {
                if !self.session || since.elapsed().as_secs() >= 10 {
                    println!(
                        "stopped in {:.0} ms ({})",
                        since.elapsed().as_secs_f64() * 1e3,
                        if self.session {
                            "the session did not end"
                        } else {
                            "the session ended"
                        }
                    );
                    self.auto = Auto::Off;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Auto::WaitProbe => {
                let file_done = self.set.source != SourceKind::File
                    || matches!(self.file_info, Some(FileInfo::Done(_)));
                // The encoders are known once the device probe has finished.
                if file_done && self.dev.is_none() {
                    self.prepare();
                    self.auto = Auto::WaitReady(Instant::now());
                }
            }
            Auto::WaitReady(since) => {
                if self.status.state == TxState::Ready {
                    // Never the radio: the options refuse that, and this
                    // checks again before it transmits by itself.
                    if self.sink_is_radio() {
                        self.auto = Auto::Off;
                        return;
                    }
                    self.transmit();
                    self.auto = Auto::Running(Instant::now());
                } else if self.status.state == TxState::Failed || since.elapsed().as_secs() > 90 {
                    self.finish_run("never became ready");
                }
            }
            Auto::Running(since) => {
                match self.status.state {
                    TxState::Failed => return self.finish_run("failed"),
                    TxState::Finished => return self.finish_run("file ended"),
                    TxState::Transmitting if since.elapsed().as_secs_f64() > 3.0 => {
                        self.cpu_mean.0 += self.status.process_cpu_pct as f64;
                        self.cpu_mean.1 += 1;
                        if self.gui_mark.is_none() {
                            self.gui_mark = Some((
                                crate::thread_cpu::seconds(),
                                Instant::now(),
                                self.frames,
                                self.passes,
                                self.rows_seen,
                            ));
                        }
                    }
                    _ => {}
                }
                let secs = self.opts.run_secs.unwrap_or(30.0);
                if since.elapsed().as_secs_f64() >= secs {
                    self.finish_run("time is up");
                }
            }
        }
    }

    /// `--start` is over: print the counters and close the window.
    fn finish_run(&mut self, why: &str) {
        let s = &self.status;
        let mean = if self.cpu_mean.1 > 0 {
            self.cpu_mean.0 / self.cpu_mean.1 as f64
        } else {
            s.process_cpu_pct as f64
        };
        println!(
            "decdvbt-tx run ({why}): state {:?}, sent {:.1} s, programme {:.1} s, \
             chain {}, TS in {:.2} Mbit/s of {:.2}, this app {:.0} % CPU (mean), \
             modulator {:.0} %, ffmpeg {:.0} % speed {:.2}x, underruns {}, stuffed {}, \
             dts<pcr {}, clipped {}, rms {:.1} dBFS",
            s.state,
            s.sent_s,
            s.programme_s,
            if s.chain.is_empty() { "-" } else { &s.chain },
            s.ts_in_bps / 1e6,
            s.ts_rate_bps / 1e6,
            mean,
            s.modulator_load_pct,
            s.ffmpeg_cpu_pct,
            s.ffmpeg_speed_x,
            s.underruns,
            s.ts_stuffed,
            s.dts_pcr_warnings,
            s.clipped,
            s.level_rms_dbfs
        );
        if let Some((cpu0, t0, f0, p0, r0)) = self.gui_mark {
            let dt = t0.elapsed().as_secs_f64().max(1e-3);
            println!(
                "window: UI thread {:.1} % of a core, {:.1} frames a second ({:.1} ui passes), {:.1} spectrum rows a second",
                100.0 * (crate::thread_cpu::seconds() - cpu0) / dt,
                (self.frames - f0) as f64 / dt,
                (self.passes - p0) as f64 / dt,
                (self.rows_seen - r0) as f64 / dt
            );
        }
        if let Some(e) = &self.error {
            println!("error: {}", e.text);
        }
        let bad = s.state == TxState::Failed || self.error.is_some();
        self.exit_code = i32::from(bad);
        // Stop the session the way the STOP button does, and close the window
        // once it is gone (this exercises the stop path too).
        if let Some(host) = &self.host {
            host.send(Cmd::Stop);
        }
        self.stopping = self.session;
        self.auto = Auto::Closing(Instant::now());
    }

    fn remember(&mut self, ctx: &egui::Context) {
        if self.opts.screenshot.is_some() || self.demo.is_some() || self.opts.selftest {
            return;
        }
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            let size = (r.width() as u32, r.height() as u32);
            if size.0 >= 900 && size.1 >= 600 {
                self.set.window = Some(size);
            }
        }
        if self.set != self.saved && self.saved_at.elapsed() >= SAVE_EVERY {
            self.set.store();
            self.saved = self.set.clone();
            self.saved_at = Instant::now();
        }
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.opts.screenshot.clone() else {
            return;
        };
        let after = self
            .opts
            .after
            .unwrap_or(if self.opts.start { 10.0 } else { 1.5 });
        if !self.shot_sent && self.started.elapsed().as_secs_f64() >= after {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.shot_sent = true;
        }
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let (w, h) = (image.size[0] as u32, image.size[1] as u32);
        match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
            Ok(()) => eprintln!("screenshot saved to {} ({w}x{h})", path.display()),
            Err(e) => {
                eprintln!("cannot save screenshot {}: {e}", path.display());
                self.exit_code = 1;
            }
        }
        // A live run keeps going to its own end; a demo is done.
        if !self.opts.start {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    // ---- per frame: drawing -------------------------------------------

    fn state_view(&self) -> (String, Color32) {
        let st = &self.status;
        if self.stopping {
            return ("STOPPING".into(), AMBER);
        }
        match st.state {
            TxState::Idle => ("IDLE".into(), Color32::GRAY),
            TxState::Preparing => (
                format!("PREPARING {:.0} %", meters::prepare_progress(st) * 100.0),
                AMBER,
            ),
            TxState::Ready if st.armed => ("READY - ARMED".into(), AMBER),
            TxState::Ready => ("READY".into(), GREEN),
            TxState::Transmitting if st.radiating => ("TRANSMITTING".into(), RED),
            TxState::Transmitting => ("RUNNING - test output, no radio".into(), GREEN),
            TxState::Finishing => ("FINISHING".into(), AMBER),
            TxState::Finished => ("FINISHED".into(), GREEN),
            TxState::Failed => ("FAILED".into(), RED),
        }
    }

    fn error_strip(&mut self, ui: &mut egui::Ui) {
        let Some(e) = &self.error else { return };
        let (text, hint) = (e.text.clone(), e.hint.clone());
        let log: Vec<String> = self.status.ffmpeg_log.clone();
        let mut dismiss = false;
        let mut browse_ffmpeg = false;
        egui::Frame::new()
            .fill(Color32::from_rgb(70, 20, 20))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&text).color(RED).strong())
                                .selectable(true),
                        );
                        if let Some(h) = &hint {
                            ui.label(RichText::new(h).color(Color32::from_gray(210)));
                        }
                        for l in log.iter().rev().take(8).rev() {
                            ui.add(
                                egui::Label::new(RichText::new(l).monospace().weak())
                                    .selectable(true),
                            );
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui.button("Dismiss").clicked() {
                            dismiss = true;
                        }
                        if ffmpeg_missing(&text) && ui.button("Browse for ffmpeg.exe...").clicked()
                        {
                            browse_ffmpeg = true;
                        }
                    });
                });
            });
        if dismiss {
            self.error = None;
        }
        if browse_ffmpeg {
            self.browse_ffmpeg();
        }
    }

    fn browse_ffmpeg(&mut self) {
        if self.locked() {
            return;
        }
        if let Some(p) = rfd::FileDialog::new()
            .set_title("Find ffmpeg")
            .add_filter("ffmpeg", &["exe"])
            .pick_file()
        {
            self.set.ffmpeg_path = Some(p);
            self.error = None;
        }
    }

    fn mode_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "MODE");
        let locked = self.locked();
        let r = ui.add_enabled_ui(!locked, |ui| {
            let presets = modeinfo::presets();
            let current = modeinfo::preset_of(&self.set.mode, self.tx_t2().as_ref());
            ui.horizontal(|ui| {
                ui.label("Preset");
                egui::ComboBox::from_id_salt("preset")
                    .selected_text(current.map_or("Custom", |i| presets[i].name))
                    .width(210.0)
                    .show_ui(ui, |ui| {
                        for (i, p) in presets.iter().enumerate() {
                            if ui
                                .selectable_label(current == Some(i), p.name)
                                .on_hover_text(p.use_case)
                                .clicked()
                            {
                                self.set.mode = p.mode;
                                self.set.use_t2 = p.t2.is_some();
                                if let Some(t2) = &p.t2 {
                                    self.set.t2 = t2.clone();
                                }
                            }
                        }
                    });
            });
            if self.set.use_t2 {
                self.t2_controls(ui);
                return;
            }
            let mut m = self.set.mode;
            // Two lines: what the signal carries, then how it is laid out.
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("mod")
                    .selected_text(m.modulation.name())
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for c in modeinfo::MODULATIONS {
                            ui.selectable_value(&mut m.modulation, c, c.name());
                        }
                    })
                    .response
                    .on_hover_text("Constellation");
                egui::ComboBox::from_id_salt("rate")
                    .selected_text(m.rate_hp.name())
                    .width(40.0)
                    .show_ui(ui, |ui| {
                        for r in Rate::ALL {
                            ui.selectable_value(&mut m.rate_hp, r, r.name());
                        }
                    })
                    .response
                    .on_hover_text("Code rate");
                egui::ComboBox::from_id_salt("guard")
                    .selected_text(m.guard.name())
                    .width(40.0)
                    .show_ui(ui, |ui| {
                        for g in decdvbt_core::dvbt::Guard::ALL {
                            ui.selectable_value(&mut m.guard, g, g.name());
                        }
                    })
                    .response
                    .on_hover_text("Guard interval");
            });
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("fft")
                    .selected_text(m.fft.name())
                    .width(40.0)
                    .show_ui(ui, |ui| {
                        for f in modeinfo::FFTS {
                            ui.selectable_value(&mut m.fft, f, f.name());
                        }
                    })
                    .response
                    .on_hover_text("FFT size");
                if let Some(b) = self.bandwidth_box(ui, m.bandwidth) {
                    if b.dvbt() {
                        m.bandwidth = b;
                    } else {
                        self.set.use_t2 = true;
                    }
                }
            });
            // The low-priority code rate is not used (non-hierarchical).
            m.rate_lp = m.rate_hp;
            self.set.mode = m;
        });
        if locked {
            r.response.on_hover_text(STOP_FIRST);
        }
        let sum = match self.tx_t2() {
            Some(t2) => modeinfo::t2_summary(&t2),
            None => modeinfo::summary(&self.set.mode),
        };
        ui.label(RichText::new(format!("-> {}", sum.rate_line())).strong());
        ui.label(RichText::new(format!("   {}", sum.picture_line(25))).weak());
        if ui.small_button("All modes...").clicked() {
            self.show_modes = !self.show_modes;
        }
    }

    /// The Bandwidth box: every width, DVB-T's and DVB-T2's 1.7 MHz. The width
    /// picked, if one was.
    fn bandwidth_box(&self, ui: &mut egui::Ui, now: Bandwidth) -> Option<Bandwidth> {
        let mut pick = now;
        egui::ComboBox::from_id_salt("bw")
            .selected_text(now.name())
            .width(80.0)
            .show_ui(ui, |ui| {
                for b in modeinfo::BANDWIDTHS {
                    let label = match b {
                        Bandwidth::Mhz1 => "1 MHz (DVB-T / 8)",
                        Bandwidth::Mhz1_7 => "1.7 MHz (DVB-T2)",
                        _ => b.name(),
                    };
                    ui.selectable_value(&mut pick, b, label);
                }
            })
            .response
            .on_hover_text(
                "Channel bandwidth. 1 MHz is DVB-T with every clock of 8 MHz divided by 8 \
                 (not a broadcast standard; receivers must be told the width); 1.7 MHz is \
                 DVB-T2's narrow channel. Both run the radio at 2 MS/s.",
            );
        (pick != now).then_some(pick)
    }

    /// The DVB-T2 (1.7 MHz) mode's controls: constellation, rate, guard; FFT
    /// size, bandwidth. The pilot pattern follows the guard.
    fn t2_controls(&mut self, ui: &mut egui::Ui) {
        let c = self.set.t2.clone();
        let (mut fft, mut guard, mut qam, mut rate) =
            (c.mode.fft, c.mode.guard, c.qam, t2_rate(&c));
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("t2mod")
                .selected_text(qam_name(qam))
                .width(70.0)
                .show_ui(ui, |ui| {
                    for q in T2_QAMS {
                        ui.selectable_value(&mut qam, q, qam_name(q));
                    }
                })
                .response
                .on_hover_text("Constellation (rotated)");
            egui::ComboBox::from_id_salt("t2rate")
                .selected_text(format!("{}/{}", rate.0, rate.1))
                .width(40.0)
                .show_ui(ui, |ui| {
                    for r in T2_RATES {
                        ui.selectable_value(&mut rate, r, format!("{}/{}", r.0, r.1));
                    }
                })
                .response
                .on_hover_text("LDPC code rate (64800-bit frames)");
            egui::ComboBox::from_id_salt("t2guard")
                .selected_text(guard.name())
                .width(56.0)
                .show_ui(ui, |ui| {
                    for g in t2_guards(fft) {
                        ui.selectable_value(&mut guard, g, g.name());
                    }
                })
                .response
                .on_hover_text("Guard interval (the pilot pattern follows it)");
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("t2fft")
                .selected_text(fft.name())
                .width(40.0)
                .show_ui(ui, |ui| {
                    for f in T2_FFTS {
                        ui.selectable_value(&mut fft, f, f.name());
                    }
                })
                .response
                .on_hover_text("FFT size");
            ui.label(RichText::new(c.mode.pp.to_string()).weak())
                .on_hover_text("Scattered pilot pattern");
            if let Some(b) = self.bandwidth_box(ui, Bandwidth::Mhz1_7) {
                // A DVB-T width: back to DVB-T, at that width.
                self.set.use_t2 = false;
                self.set.mode.bandwidth = b;
            }
        });
        // A guard the new FFT size does not have: the nearest it has.
        let guards = t2_guards(fft);
        if !guards.contains(&guard) {
            guard = guards[guards.len() / 2];
        }
        if let Some(t2) = t2_mode(fft, guard, qam, rate) {
            self.set.t2 = t2;
        }
    }

    fn state_strip(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("DecDVB-T TX")
                    .strong()
                    .color(Color32::from_rgb(255, 138, 128)),
            );
            ui.label(RichText::new(env!("CARGO_PKG_VERSION")).weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let locked = self.locked();
                let before = self.sink;
                ui.add_enabled_ui(!locked, |ui| {
                    egui::ComboBox::from_id_salt("sink")
                        .selected_text(self.sink.label())
                        .show_ui(ui, |ui| {
                            for k in [
                                SinkKind::Test,
                                SinkKind::IqFile,
                                SinkKind::HackRf,
                                SinkKind::Pluto,
                            ] {
                                ui.selectable_value(&mut self.sink, k, k.label());
                            }
                        });
                });
                if self.sink != before {
                    // A new output starts disarmed and unticked.
                    self.read_ack = false;
                }
                ui.label("Output");
                let (text, colour) = self.state_view();
                ui.label(RichText::new(text).strong().color(colour));
            });
        });
        ui.add_space(2.0);
    }

    fn banner(&self, ui: &mut egui::Ui) {
        let centre = self
            .session_cfg
            .as_ref()
            .map_or(self.set.centre_hz, |c| c.centre_hz);
        let pluto = self
            .session_cfg
            .as_ref()
            .is_some_and(|c| matches!(c.sink, TxSink::Pluto(_)));
        let Some(text) = safety::banner(&self.status, centre, pluto) else {
            return;
        };
        egui::Frame::new()
            .fill(Color32::from_rgb(92, 26, 22))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("● {text}"))
                            .size(16.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("Esc or Space stops").color(Color32::WHITE));
                    });
                });
            });
    }

    fn section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(8.0);
        ui.label(RichText::new(title).strong().color(AMBER));
    }

    /// The left column: the form (locked while a session exists), then the
    /// output's controls, which stay live (the TX VGA moves while sending).
    fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let locked = self.locked();
                ui.add_enabled_ui(!locked, |ui| {
                    self.source_section(ui);
                    if self.set.source.encodes() {
                        self.encoder_section(ui);
                    }
                    self.mode_section(ui);
                    self.channel_section(ui);
                })
                .response
                .on_disabled_hover_text(STOP_FIRST);
                self.radio_section(ui);
                self.advanced_section(ui);
            });
    }

    /// The cameras, microphones, their modes and which encoders work, once
    /// the background probe has found them.
    fn found_cameras(&self) -> Vec<String> {
        self.found
            .as_ref()
            .map(|p| p.devices.cameras.clone())
            .unwrap_or_default()
    }

    fn camera_modes(&self) -> &[CameraMode] {
        self.found
            .as_ref()
            .and_then(|p| p.modes.iter().find(|(c, _)| *c == self.set.camera))
            .map_or(&[], |(_, m)| m)
    }

    fn working(&self) -> &[(VideoCodec, Engine)] {
        self.found.as_ref().map_or(&[], |p| &p.working)
    }

    /// The engine that will encode: the one asked for if it works, else the
    /// best that does (the CPU while the probe has not finished).
    fn engine_used(&self) -> Engine {
        encode::pick_engine(self.set.video_codec, self.set.encoder.engine(), self.working())
            .unwrap_or(Engine::Cpu)
    }

    fn source_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "SOURCE");
        let before = self.set.source;
        ui.horizontal_wrapped(|ui| {
            for k in SourceKind::ALL {
                ui.selectable_value(&mut self.set.source, k, k.label());
            }
        });
        if self.set.source != before {
            self.error = None;
            // A video file's details are read when that tab is chosen.
            if self.set.source == SourceKind::File
                && self.file_info.is_none()
                && let Some(f) = self.file.clone()
            {
                self.open_file(f, ui.ctx());
            }
        }
        egui::Grid::new("source")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                match self.set.source {
                    SourceKind::Camera => {
                        let cams = self.found_cameras();
                        ui.label("Camera");
                        let none = if self.dev.is_some() {
                            "looking..."
                        } else {
                            "none found"
                        };
                        egui::ComboBox::from_id_salt("camera")
                            .width(260.0)
                            .selected_text(if self.set.camera.is_empty() {
                                none
                            } else {
                                &self.set.camera
                            })
                            .show_ui(ui, |ui| {
                                for c in cams {
                                    if ui.selectable_label(self.set.camera == c, &c).clicked() {
                                        self.set.camera = c;
                                        self.cam_mode = None;
                                    }
                                }
                            });
                        ui.end_row();
                        ui.label("Captures");
                        let auto = self.auto_mode();
                        let modes = self.camera_modes().to_vec();
                        egui::ComboBox::from_id_salt("mode_cap")
                            .width(260.0)
                            .selected_text(match (&self.cam_mode, &auto) {
                                (Some(m), _) => m.describe(),
                                (None, Some(a)) => format!("auto: {}", a.describe()),
                                (None, None) => "auto".into(),
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.cam_mode, None, "auto (for the picture)");
                                for m in modes {
                                    let d = m.describe();
                                    ui.selectable_value(&mut self.cam_mode, Some(m), d);
                                }
                            });
                        ui.end_row();
                        let mics = self
                            .found
                            .as_ref()
                            .map(|p| p.devices.microphones.clone())
                            .unwrap_or_default();
                        ui.label("Microphone");
                        egui::ComboBox::from_id_salt("mic")
                            .width(260.0)
                            .selected_text(self.set.mic.clone().unwrap_or_else(|| "none".into()))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.set.mic, None, "none");
                                for m in mics {
                                    let l = m.clone();
                                    ui.selectable_value(&mut self.set.mic, Some(m), l);
                                }
                            });
                        ui.end_row();
                        ui.label("Mic level");
                        let db = self.status.audio_dbfs.unwrap_or(-90.0);
                        ui.horizontal(|ui| {
                            let fill = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                            let col = if db > -3.0 {
                                RED
                            } else if db > -12.0 {
                                AMBER
                            } else {
                                GREEN
                            };
                            ui.add(egui::ProgressBar::new(fill).desired_width(180.0).fill(col));
                            ui.label(if db > -89.0 {
                                format!("{db:.0} dBFS")
                            } else {
                                "-".into()
                            });
                        });
                        ui.end_row();
                    }
                    SourceKind::File | SourceKind::TsFile => {
                        ui.label("File");
                        ui.horizontal(|ui| {
                            if ui.button("Choose...").clicked() {
                                self.browse(ui.ctx());
                            }
                            let name = self
                                .file
                                .as_ref()
                                .map_or("none".to_string(), |p| source::file_label(p));
                            ui.add(egui::Label::new(name).truncate());
                        });
                        ui.end_row();
                        ui.label("");
                        ui.checkbox(&mut self.set.loop_input, "loop");
                        ui.end_row();
                        if self.set.source == SourceKind::File {
                            match &self.file_info {
                                Some(FileInfo::Probing) => {
                                    ui.label("");
                                    ui.label(RichText::new("reading the file...").weak());
                                    ui.end_row();
                                }
                                Some(FileInfo::Done(Probed::Info(i))) => {
                                    let (a, b) = source::describe(i);
                                    ui.label("");
                                    ui.vertical(|ui| {
                                        ui.label(a);
                                        if !b.is_empty() {
                                            ui.label(RichText::new(b).weak());
                                        }
                                        if i.interlaced || i.bit_depth > 8 {
                                            ui.label(
                                                RichText::new(
                                                    "interlaced or 10-bit: the CPU chain (more CPU)",
                                                )
                                                .color(AMBER),
                                            );
                                        } else if i.height > 1080 || i.fps > 30.5 {
                                            ui.label(
                                                RichText::new(
                                                    "a heavy source: more CPU to scale it down",
                                                )
                                                .color(AMBER),
                                            );
                                        }
                                    });
                                    ui.end_row();
                                }
                                Some(FileInfo::Done(Probed::NoProbe)) => {
                                    ui.label("");
                                    ui.label(RichText::new("(no ffprobe: info unavailable)").weak());
                                    ui.end_row();
                                }
                                Some(FileInfo::Done(Probed::NoFfmpeg(_))) => {
                                    ui.label("");
                                    ui.label(RichText::new("ffmpeg not found").color(RED));
                                    ui.end_row();
                                }
                                Some(FileInfo::Done(Probed::Bad(_))) => {
                                    ui.label("");
                                    ui.label(RichText::new("unreadable file").color(RED));
                                    ui.end_row();
                                }
                                None => {}
                            }
                        } else {
                            ui.label("");
                            ui.label(
                                RichText::new("sent as it is, remultiplexed to the channel's rate")
                                    .weak(),
                            );
                            ui.end_row();
                        }
                    }
                    SourceKind::Udp => {
                        ui.label("Listen on");
                        ui.text_edit_singleline(&mut self.set.udp);
                        ui.end_row();
                        ui.label("");
                        ui.label(
                            RichText::new("sent as it is, remultiplexed to the channel's rate")
                                .weak(),
                        );
                        ui.end_row();
                    }
                    SourceKind::Test => {
                        ui.label("");
                        ui.label(RichText::new("ffmpeg's test picture and a 1 kHz tone").weak());
                        ui.end_row();
                    }
                }
                if self.set.source.encodes() {
                    ui.label("Call sign");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.set.callsign).desired_width(100.0),
                        )
                        .on_hover_text("The service name receivers list (empty: the file's name)");
                        ui.checkbox(&mut self.set.overlay, "on the picture");
                    });
                    ui.end_row();
                }
            });
    }

    fn encoder_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "ENCODER");
        let working = self.working().to_vec();
        let probing = self.dev.is_some();
        egui::Grid::new("encoder")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label("Video");
                ui.horizontal_wrapped(|ui| {
                    for c in VideoCodec::ALL {
                        let ok = probing || working.iter().any(|(wc, _)| *wc == c);
                        let r = ui.add_enabled(
                            ok,
                            egui::Button::selectable(self.set.video_codec == c, c.label()),
                        );
                        if r.clicked() {
                            self.set.video_codec = c;
                        }
                    }
                });
                ui.end_row();
                ui.label("Engine");
                let used = self.engine_used();
                egui::ComboBox::from_id_salt("engine")
                    .selected_text(match self.set.encoder.engine() {
                        None => format!("auto: {}", used.label()),
                        Some(e) => e.label().to_string(),
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.set.encoder,
                            EncoderChoice::Auto,
                            "auto (graphics card first)",
                        );
                        for (choice, e) in [
                            (EncoderChoice::Cpu, Engine::Cpu),
                            (EncoderChoice::Nvenc, Engine::Nvenc),
                            (EncoderChoice::Amf, Engine::Amf),
                            (EncoderChoice::Qsv, Engine::Qsv),
                        ] {
                            let ok = working.contains(&(self.set.video_codec, e));
                            ui.add_enabled_ui(ok, |ui| {
                                ui.selectable_value(&mut self.set.encoder, choice, e.label());
                            });
                        }
                    });
                ui.end_row();
                let cfg = self.idle_cfg();
                let p = VideoProfile::for_rate(cfg.mux_rate_bps());
                ui.label("Picture");
                ui.label(format!(
                    "{}x{} {}p - {} kbit/s of {:.0}",
                    p.width,
                    p.height,
                    cfg.video.fps,
                    p.video_kbps,
                    cfg.mux_rate_bps() as f64 / 1e3
                ));
                ui.end_row();
                ui.label("Audio");
                egui::ComboBox::from_id_salt("acodec")
                    .selected_text(match self.set.audio_codec {
                        AudioCodec::AacLatm => "AAC (LATM)",
                        AudioCodec::AacAdts => "AAC (ADTS)",
                        AudioCodec::Mp2 => "MP2",
                        AudioCodec::None => "none",
                    })
                    .show_ui(ui, |ui| {
                        for (a, t) in [
                            (AudioCodec::AacLatm, "AAC (LATM)"),
                            (AudioCodec::AacAdts, "AAC (ADTS)"),
                            (AudioCodec::Mp2, "MP2"),
                            (AudioCodec::None, "none"),
                        ] {
                            ui.selectable_value(&mut self.set.audio_codec, a, t);
                        }
                    });
                ui.end_row();
            });
        match self.set.video_codec {
            VideoCodec::H265 => {
                ui.label(
                    RichText::new("H.265 plays on newer televisions (service type 0x1F).")
                        .small()
                        .color(AMBER),
                );
            }
            VideoCodec::H266 | VideoCodec::Av1 => {
                ui.label(
                    RichText::new(format!(
                        "{} is for software receivers (DecDVB-T, VLC): no television decodes it.",
                        self.set.video_codec.label()
                    ))
                    .small()
                    .color(AMBER),
                );
                if self.set.video_codec == VideoCodec::H266 {
                    ui.label(
                        RichText::new("H.266 (vvenc) is heavy: about two cores.")
                            .small()
                            .color(AMBER),
                    );
                }
            }
            _ => {}
        }
    }

    /// The capture mode "auto" would pick for the picture the mode carries.
    fn auto_mode(&self) -> Option<CameraMode> {
        let cfg = self.idle_cfg();
        let p = VideoProfile::for_rate(cfg.mux_rate_bps());
        encode::best_mode(self.camera_modes(), p.width, p.height, cfg.video.fps)
    }

    fn channel_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "CHANNEL");
        freq_display::show(ui, &mut self.set.centre_hz, 1e6, 6e9, 28.0);
        let cur = uhf_channel_at(self.set.centre_hz)
            .filter(|&n| uhf_centre_hz(n).is_some_and(|c| (c - self.set.centre_hz).abs() < 1.0));
        ui.label(match cur {
            Some(n) => format!("UHF channel C{n}"),
            None => "not on a UHF channel centre".to_string(),
        });
        ui.horizontal(|ui| {
            ui.label("Channel");
            egui::ComboBox::from_id_salt("uhf")
                .selected_text(cur.map_or("custom".to_string(), |n| format!("C{n}")))
                .width(80.0)
                .show_ui(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            for n in UHF_FIRST..=UHF_LAST {
                                let c = uhf_centre_hz(n).unwrap_or(0.0);
                                if ui
                                    .selectable_label(
                                        cur == Some(n),
                                        format!("C{n}  {:.0} MHz", c / 1e6),
                                    )
                                    .clicked()
                                {
                                    self.set.centre_hz = c;
                                }
                            }
                        });
                });
        });
        if let Some(w) = safety::television_warning(self.set.centre_hz) {
            ui.label(RichText::new(w).color(RED));
        }
    }

    /// OUTPUT: what the chosen output needs. Live while sending (the TX VGA
    /// and the Pluto's attenuation move in steps of at most 6 dB).
    fn radio_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "OUTPUT");
        let locked = self.locked();
        let st = self.status.state;
        let idle = !locked;
        let live = self.sink_is_radio() && st == TxState::Transmitting && !self.stopping;
        match self.sink {
            SinkKind::Test => {
                ui.label(RichText::new("Test only: the whole chain runs, nothing is sent.").weak());
                ui.add_enabled(
                    idle,
                    egui::Checkbox::new(&mut self.paced, "paced (real time)"),
                )
                .on_hover_text("Off: generate as fast as possible (a speed test)");
            }
            SinkKind::IqFile => {
                ui.add_enabled_ui(idle, |ui| {
                    let hint = source::iq_file_name(
                        self.set.centre_hz,
                        self.set.rate.hz(self.bandwidth()),
                    );
                    ui.horizontal(|ui| {
                        ui.label("File");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.iq_path)
                                .desired_width(260.0)
                                .hint_text(hint),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Seconds");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.iq_seconds)
                                .desired_width(50.0)
                                .hint_text("all"),
                        );
                        ui.checkbox(&mut self.paced, "paced");
                        ui.label(RichText::new("(empty: until Stop) .cs8").weak());
                    });
                });
            }
            SinkKind::Pluto => {
                egui::Grid::new("pluto")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Link");
                        ui.add_enabled(
                            idle,
                            egui::TextEdit::singleline(&mut self.pluto_uri)
                                .desired_width(140.0)
                                .hint_text("auto"),
                        )
                        .on_hover_text(
                            "auto (USB with ADI's driver, else 192.168.2.1), usb, or \
                             ip:ADDRESS. To receive in DecDVB-T at the same time, use \
                             ip:192.168.2.1 in both, at the same sample rate",
                        );
                        ui.end_row();
                        ui.label("Attenuation");
                        ui.add_enabled(
                            idle || live,
                            egui::Slider::new(&mut self.atten, safety::ATTEN_MAX..=0.0)
                                .step_by(0.25)
                                .suffix(" dB"),
                        )
                        .on_hover_text(
                            "89.75 dB at every start; 0 dB is the Pluto's full power, about \
                             +7 dBm. Moves in steps of at most 6 dB",
                        );
                        ui.end_row();
                        if live && self.atten != self.status.atten_db {
                            ui.label("");
                            ui.label(
                                RichText::new(format!(
                                    "now {:.2} dB, moving to {:.2}",
                                    self.atten_sent, self.atten
                                ))
                                .color(AMBER),
                            );
                            ui.end_row();
                        }
                        if self.status.link_short > 0 {
                            ui.label("Link");
                            ui.label(
                                RichText::new(format!(
                                    "late {} blocks: the Pluto sent gaps (a lower rate, or a \
                                     Pluto+ on Ethernet)",
                                    self.status.link_short
                                ))
                                .color(RED),
                            );
                            ui.end_row();
                        }
                        ui.label("Level");
                        ui.add_enabled(
                            idle,
                            egui::Slider::new(&mut self.set.level_dbfs, -30.0..=-6.0)
                                .suffix(" dBFS RMS")
                                .fixed_decimals(0),
                        );
                        ui.end_row();
                    });
            }
            SinkKind::HackRf => {
                egui::Grid::new("radio")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("TX VGA");
                        ui.add_enabled(
                            idle || live,
                            egui::Slider::new(&mut self.txvga, 0..=VGA_MAX).suffix(" dB"),
                        )
                        .on_hover_text("Moves in steps of at most 6 dB, also while transmitting");
                        ui.end_row();
                        if live && self.txvga != self.status.txvga_db {
                            ui.label("");
                            ui.label(
                                RichText::new(format!(
                                    "now {} dB, moving to {}",
                                    self.vga_sent, self.txvga
                                ))
                                .color(AMBER),
                            );
                            ui.end_row();
                        }
                        ui.label("RF amp");
                        ui.add_enabled(
                            idle,
                            egui::Checkbox::new(&mut self.amp, "+14 dB (off at every start)"),
                        );
                        ui.end_row();
                        ui.label("Antenna power");
                        ui.label(RichText::new("never").weak());
                        ui.end_row();
                        ui.label("Level");
                        ui.add_enabled(
                            idle,
                            egui::Slider::new(&mut self.set.level_dbfs, -30.0..=-6.0)
                                .suffix(" dBFS RMS")
                                .fixed_decimals(0),
                        );
                        ui.end_row();
                    });
            }
        }
    }

    fn advanced_section(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        let locked = self.locked();
        egui::CollapsingHeader::new(RichText::new("Advanced").strong().color(AMBER))
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!locked, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Radio sample rate");
                        let text = match self.set.rate {
                            // The narrow channels' own rates are under the
                            // radio's 2 MS/s: native is 2 MS/s for them.
                            TxRate::Native if self.bandwidth().narrow() => {
                                "native (2 MS/s)".to_string()
                            }
                            TxRate::Native => "native".to_string(),
                            TxRate::Fixed(hz) => format!("{} MS/s", hz / 1_000_000),
                        };
                        egui::ComboBox::from_id_salt("txrate")
                            .selected_text(text)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.set.rate, TxRate::Native, "native (cheapest)");
                                ui.selectable_value(&mut self.set.rate, TxRate::Fixed(10_000_000), "10 MS/s");
                                ui.selectable_value(&mut self.set.rate, TxRate::Fixed(20_000_000), "20 MS/s");
                            });
                    });
                    ui.label(
                        RichText::new(
                            "Native costs no CPU. 10 and 20 MS/s resample, which moves the DAC \
                             images away from the channel; look at your spectrum before paying for it.",
                        )
                        .weak()
                        .small(),
                    );
                    ui.checkbox(&mut self.set.preview, "Signal and picture preview")
                        .on_hover_text(
                            "One 2048-point FFT per 50 ms of signal, and the input picture at 10 fps",
                        );
                    ui.horizontal(|ui| {
                        ui.label("ffmpeg");
                        let name = self
                            .set
                            .ffmpeg_path
                            .as_ref()
                            .map_or("(automatic)".to_string(), |p| p.display().to_string());
                        ui.add(egui::Label::new(RichText::new(name).weak()).truncate());
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Browse for ffmpeg.exe...").clicked() {
                            self.browse_ffmpeg();
                        }
                        if self.set.ffmpeg_path.is_some() && ui.button("Automatic").clicked() {
                            self.set.ffmpeg_path = None;
                        }
                    });
                });
                if locked {
                    ui.label(RichText::new(STOP_FIRST).weak());
                }
            });
    }

    /// The right side: the input picture and the meters side by side, the
    /// signal under them.
    fn signal_and_meters(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("CAMERA").strong().color(AMBER));
                let size = vec2(384.0, 216.0);
                match &self.cam_tex {
                    Some(t) if self.locked() => {
                        ui.add(egui::Image::new(t).fit_to_exact_size(size));
                    }
                    _ => {
                        let (r, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        ui.painter()
                            .rect_filled(r, 0.0, Color32::from_rgb(34, 40, 48));
                        ui.painter().text(
                            r.center(),
                            egui::Align2::CENTER_CENTER,
                            if !self.set.source.encodes() {
                                "no picture for this source"
                            } else if self.locked() {
                                "no picture for this chain"
                            } else {
                                "the picture shows after Prepare"
                            },
                            egui::FontId::monospace(12.0),
                            Color32::GRAY,
                        );
                    }
                }
            });
            ui.vertical(|ui| {
                ui.label(RichText::new("METERS").strong().color(AMBER));
                self.meters(ui);
            });
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new("SIGNAL (what the radio is given)")
                .strong()
                .color(AMBER),
        );
        self.spectrum(ui);
    }

    fn meters(&self, ui: &mut egui::Ui) {
        let st = &self.status;
        let bar = |ui: &mut egui::Ui, label: &str, frac: f32, bad: bool, text: String| {
            ui.horizontal(|ui| {
                ui.label(format!("{label:<12}"));
                ui.add(
                    egui::ProgressBar::new(frac.clamp(0.0, 1.0))
                        .desired_width(140.0)
                        .fill(if bad { RED } else { GREEN }),
                );
                ui.label(text);
            });
        };
        let live = self.session && st.state == TxState::Transmitting;
        ui.label(format!(
            "TS          {:.0} kbit/s - stuffed {} - underruns {}",
            st.ts_in_bps / 1e3,
            st.ts_stuffed,
            st.underruns
        ));
        ui.horizontal(|ui| {
            ui.label(format!("{:<12}", "TS buffer"));
            ui.add(
                egui::ProgressBar::new(st.ts_fill.clamp(0.0, 1.0))
                    .desired_width(140.0)
                    .fill(if live && st.ts_fill < 0.05 { AMBER } else { GREEN }),
            );
            ui.label(format!("{:.0} %", st.ts_fill * 100.0));
        });
        bar(
            ui,
            "IQ buffer",
            st.iq_fill,
            false,
            format!("{:.0} %", st.iq_fill * 100.0),
        );
        bar(
            ui,
            "Encoder",
            st.ffmpeg_cpu_pct / 200.0,
            st.ffmpeg_cpu_pct > 200.0,
            format!(
                "{:5.1} %  {}",
                st.ffmpeg_cpu_pct,
                if st.ffmpeg_speed_x > 0.0 {
                    format!("{:.1}x", st.ffmpeg_speed_x)
                } else {
                    String::new()
                }
            ),
        );
        bar(
            ui,
            "Modulator",
            st.modulator_load_pct / 100.0,
            st.modulator_load_pct > 80.0,
            format!("{:5.1} %", st.modulator_load_pct),
        );
        bar(
            ui,
            "This app",
            st.process_cpu_pct / 150.0,
            st.process_cpu_pct > 150.0,
            format!("{:5.1} % of a core", st.process_cpu_pct),
        );
        let warn = |ui: &mut egui::Ui, bad: bool, text: String| {
            ui.colored_label(
                if bad {
                    AMBER
                } else {
                    ui.visuals().text_color()
                },
                text,
            );
        };
        warn(
            ui,
            st.dts_pcr_warnings > 0,
            format!("Over the rate (dts < pcr): {}", st.dts_pcr_warnings),
        );
        warn(
            ui,
            st.clipped > 0,
            format!("Peak {:.1} dBFS - clipped {}", st.peak_dbfs, st.clipped),
        );
        ui.label(format!(
            "Sent {:02}:{:02}  - programme {:02}:{:02}",
            (st.sent_s / 60.0) as u64,
            (st.sent_s % 60.0) as u64,
            (st.programme_s / 60.0) as u64,
            (st.programme_s % 60.0) as u64,
        ));
        if !st.chain.is_empty() {
            ui.label(format!("Encoder chain: {}", st.chain));
        }
    }

    /// The spectrum of what the radio is given, with the occupied band
    /// shaded and a line every 10 dB.
    fn spectrum(&self, ui: &mut egui::Ui) {
        let w = ui.available_width().max(200.0);
        let h = (ui.available_height() - 4.0).clamp(120.0, 320.0);
        let (r, _) = ui.allocate_exact_size(vec2(w, h), egui::Sense::hover());
        let p = ui.painter_at(r);
        p.rect_filled(r, 0.0, Color32::from_rgb(16, 20, 24));
        let fs = if self.span_hz > 0.0 {
            self.span_hz
        } else {
            self.status.sample_rate_hz as f64
        }
        .max(1.0);
        let occupied = self.idle_cfg().occupied_hz();
        let half = (occupied / 2.0 / fs) as f32;
        let band = egui::Rect::from_x_y_ranges(
            r.center().x - r.width() * half..=r.center().x + r.width() * half,
            r.y_range(),
        );
        p.rect_filled(band, 0.0, Color32::from_rgb(24, 40, 58));
        let y = |db: f32| r.top() + (-db / 100.0).clamp(0.0, 1.0) * r.height();
        for k in 1..10 {
            let yy = y(-10.0 * k as f32);
            p.line_segment(
                [egui::pos2(r.left(), yy), egui::pos2(r.right(), yy)],
                (1.0, Color32::from_rgb(34, 40, 46)),
            );
        }
        if self.session && self.smooth.len() > 1 {
            let n = self.smooth.len();
            let pts: Vec<egui::Pos2> = self
                .smooth
                .iter()
                .enumerate()
                .map(|(i, &db)| egui::pos2(r.left() + r.width() * i as f32 / (n - 1) as f32, y(db)))
                .collect();
            p.add(egui::Shape::line(pts, (1.0, Color32::from_rgb(111, 182, 255))));
        }
        p.text(
            r.left_top() + vec2(6.0, 4.0),
            egui::Align2::LEFT_TOP,
            if self.set.preview {
                format!(
                    "{:.3} MS/s - {:.2} MHz occupied - 10 dB a line",
                    fs / 1e6,
                    occupied / 1e6
                )
            } else {
                "the preview is off (Advanced)".to_string()
            },
            egui::FontId::monospace(11.0),
            Color32::GRAY,
        );
    }

    fn safety_panel(&mut self, ui: &mut egui::Ui) {
        let radio = self.sink_is_radio();
        let st = self.status.state;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_max_width((ui.available_width() - 420.0).max(300.0));
                // Always visible, never a popup, and the very text the arm checks.
                ui.label(
                    RichText::new(self.licence_shown)
                        .small()
                        .color(Color32::from_rgb(232, 201, 198)),
                );
                if let Some(w) = safety::television_warning(self.set.centre_hz) {
                    ui.label(RichText::new(w).small().color(RED).strong());
                }
                let r = ui.add_enabled(
                    radio && !self.status.armed,
                    egui::Checkbox::new(&mut self.read_ack, "I have read this"),
                );
                if !radio {
                    r.on_disabled_hover_text("Only a radio output needs an arm");
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let big = |t: &str| RichText::new(t).strong().size(15.0);
                let sending = st == TxState::Transmitting;
                let stop = egui::Button::new(big("STOP").color(if sending {
                    Color32::BLACK
                } else {
                    ui.visuals().text_color()
                }))
                .fill(if sending {
                    RED
                } else {
                    Color32::from_rgb(44, 44, 44)
                })
                .min_size(vec2(if sending { 140.0 } else { 80.0 }, 34.0));
                if ui
                    .add_enabled(self.locked(), stop)
                    .on_hover_text("Radio off, everything stopped (Esc or Space)")
                    .clicked()
                {
                    self.stop();
                }
                let can_tx =
                    safety::can_transmit(&self.status, self.status.armed, radio) && !self.stopping;
                let r = ui.add_enabled(
                    can_tx,
                    egui::Button::new(big("TRANSMIT")).min_size(vec2(110.0, 34.0)),
                );
                let r = if radio && st == TxState::Ready && !self.status.armed {
                    r.on_disabled_hover_text(
                        "ARM first (it lasts 60 s and is used up by one transmission)",
                    )
                } else if st != TxState::Ready {
                    r.on_disabled_hover_text("Available when the state is READY")
                } else {
                    r
                };
                if r.clicked() {
                    self.transmit();
                }
                if radio {
                    let armed = self.status.armed;
                    let label = match (armed, self.status.arm_seconds_left) {
                        (true, Some(s)) => format!("ARMED {:.0} s", s.ceil().max(0.0)),
                        (true, None) => "ARMED".to_string(),
                        _ => "ARM".to_string(),
                    };
                    let can = safety::can_arm(st, radio, self.read_ack);
                    let r = ui.add_enabled(
                        armed || can,
                        egui::Button::new(big(&label).color(AMBER)).min_size(vec2(100.0, 34.0)),
                    );
                    let r = if armed {
                        r.on_hover_text("Click to disarm")
                    } else {
                        r.on_disabled_hover_text("Prepare, and tick \"I have read this\"")
                    };
                    if r.clicked() {
                        if armed {
                            if let Some(h) = &self.host {
                                h.send(Cmd::Disarm);
                            }
                        } else {
                            self.arm();
                        }
                    }
                }
                let can_prepare = safety::can_prepare(self.locked(), self.source_ready());
                let r = ui.add_enabled(
                    can_prepare,
                    egui::Button::new(big("PREPARE")).min_size(vec2(100.0, 34.0)),
                );
                let r = if self.source_ready() {
                    r.on_hover_text("Start the source and the modulator; the radio stays off")
                } else {
                    r.on_disabled_hover_text(self.source_missing())
                };
                if r.clicked() {
                    self.prepare();
                }
            });
        });
    }

    fn mode_window(&mut self, ctx: &egui::Context) {
        if !self.show_modes {
            return;
        }
        let locked = self.locked();
        let mut open = true;
        let mut pick = None;
        let mut pick_t2 = None;
        egui::Window::new("All modes")
            .open(&mut open)
            .default_width(520.0)
            .default_height(480.0)
            .show(ctx, |ui| {
                if self.set.use_t2 {
                    let c = self.set.t2.clone();
                    ui.label(
                        RichText::new(format!(
                            "DVB-T2 {} {} {} at 1.7 MHz, rotated, 64800-bit LDPC. Click a row \
                             to use it.",
                            c.mode.fft, c.mode.guard, c.mode.pp
                        ))
                        .weak(),
                    );
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        egui::Grid::new("t2modes")
                            .striped(true)
                            .num_columns(4)
                            .spacing([14.0, 3.0])
                            .show(ui, |ui| {
                                for h in ["mode", "Mbit/s", "needs C/N", "picture"] {
                                    ui.label(RichText::new(h).strong());
                                }
                                ui.end_row();
                                for row in t2_mode_table(c.mode.fft, c.mode.guard) {
                                    let sel = row.config == c;
                                    let r = ui.add_enabled(
                                        !locked,
                                        egui::Button::selectable(sel, &row.label)
                                            .frame_when_inactive(false),
                                    );
                                    if r.clicked() {
                                        pick_t2 = Some(row.config.clone());
                                    }
                                    ui.label(format!("{:.2}", row.bitrate / 1e6));
                                    ui.label(format!("{:.1} dB", row.qef_cn_db));
                                    let s = modeinfo::t2_summary(&row.config);
                                    ui.label(format!("{}p", s.profile.height));
                                    ui.end_row();
                                }
                            });
                    });
                    return;
                }
                let m = self.set.mode;
                ui.label(
                    RichText::new(format!(
                        "{} {}, non-hierarchical. Click a row to use it.",
                        m.fft.name(),
                        m.bandwidth
                    ))
                    .weak(),
                );
                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("modes")
                        .striped(true)
                        .num_columns(4)
                        .spacing([14.0, 3.0])
                        .show(ui, |ui| {
                            for h in ["mode", "Mbit/s", "needs C/N", "picture"] {
                                ui.label(RichText::new(h).strong());
                            }
                            ui.end_row();
                            for row in mode_table(m.fft, m.bandwidth) {
                                let sel = row.mode == m;
                                let r = ui.add_enabled(
                                    !locked,
                                    egui::Button::selectable(sel, &row.label)
                                        .frame_when_inactive(false),
                                );
                                if r.clicked() {
                                    pick = Some(row.mode);
                                }
                                ui.label(format!("{:.2}", row.bitrate / 1e6));
                                ui.label(format!("{:.1} dB", row.qef_cn_db));
                                let s = modeinfo::summary(&row.mode);
                                ui.label(format!("{}p", s.profile.height));
                                ui.end_row();
                            }
                        });
                });
            });
        if let Some(m) = pick {
            self.set.mode = m;
        }
        if let Some(t2) = pick_t2 {
            self.set.t2 = t2;
        }
        self.show_modes = open;
    }

    /// The red frame round the window while the radio is keyed, and the hint
    /// while a file is dragged over it.
    fn overlays(&self, ctx: &egui::Context) {
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("tx-overlay"),
        ));
        let screen = ctx.content_rect();
        if self.status.radiating {
            painter.rect_stroke(
                screen.shrink(2.0),
                0.0,
                egui::Stroke::new(5.0, RED),
                egui::StrokeKind::Inside,
            );
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(160));
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "Drop a video file",
                egui::FontId::monospace(28.0),
                Color32::WHITE,
            );
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // The radio goes off before the window is gone: the session thread
        // stops the session and is joined.
        if let Some(mut h) = self.host.take() {
            h.shutdown();
        }
        if self.opts.screenshot.is_none() && self.demo.is_none() && !self.opts.selftest {
            self.set.store();
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.frames += 1;
        self.handle_dropped(ctx);
        self.handle_keys(ctx);
        self.poll_probe();
        self.poll_dev_probe();
        self.poll_host();
        self.update_picture(ctx);
        self.feed_demo();
        self.note_status_error();
        self.safety_tick(ctx);
        self.drive_auto(ctx);
        self.remember(ctx);
        self.handle_screenshot(ctx);
        // A live session repaints when the session thread says so (new rows,
        // a new state, the meters a few times a second); only the hooks that
        // have no session to say it keep a timer.
        if self.demo.is_some()
            || self.opts.screenshot.is_some()
            || self.probe.is_some()
            || self.dev.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        let title = safety::window_title(&self.status);
        if title != self.title {
            self.title = title;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.to_string()));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.passes += 1;
        egui::Panel::top("state").show(ui, |ui| self.state_strip(ui));
        if self.error.is_some() {
            egui::Panel::top("error")
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.error_strip(ui));
        }
        if self.status.radiating {
            egui::Panel::top("banner")
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.banner(ui));
        }
        egui::Panel::bottom("safety")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(36, 29, 29))
                    .inner_margin(8.0),
            )
            .show(ui, |ui| self.safety_panel(ui));
        egui::Panel::left("controls")
            .resizable(true)
            .default_size(420.0)
            .min_size(360.0)
            .show(ui, |ui| self.left_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.signal_and_meters(ui));
        let ctx = ui.ctx().clone();
        self.mode_window(&ctx);
        self.overlays(&ctx);
    }
}

/// The message says ffmpeg itself could not be found (so a button to point at
/// it is useful), as opposed to ffmpeg failing on a file.
fn ffmpeg_missing(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("ffmpeg")
        && (t.contains("not found") || t.contains("could not find") || t.contains("no ffmpeg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ffmpeg_button_is_for_a_missing_ffmpeg() {
        assert!(ffmpeg_missing("ffmpeg was not found: put it on the PATH"));
        assert!(ffmpeg_missing("could not find ffmpeg"));
        assert!(!ffmpeg_missing("ffmpeg exited with code 1"));
        assert!(!ffmpeg_missing("the HackRF was not found"));
    }

    /// The form is built into a config that never names the radio unless the
    /// output was chosen, and never starts the radio at a raised gain.
    #[test]
    fn the_config_follows_the_form_and_defaults_safe() {
        let ctx = egui::Context::default();
        let opts = Options::default();
        let mut app = App::new(opts, &ctx);
        app.set.source = SourceKind::File;
        // (Not whatever mode the user's own prefs file holds.)
        app.set.mode = modeinfo::presets()[0].mode;
        app.set.use_t2 = false;
        app.file = Some(PathBuf::from("C:/v/My Clip.mp4"));
        let c = app.build_config().unwrap();
        assert!(matches!(c.sink, TxSink::Null { paced: true }));
        assert_eq!(c.service.name, "My Clip");
        assert_eq!(c.level_dbfs, -12.0);
        assert!(c.loop_input);
        app.sink = SinkKind::HackRf;
        let c = app.build_config().unwrap();
        match c.sink {
            TxSink::HackRf(h) => {
                assert_eq!((h.txvga_db, h.amp), (0, false));
                assert_eq!(h.baseband_filter_hz, Some(8_000_000));
            }
            _ => panic!("expected the radio sink"),
        }
        // Not armed, and not transmitting.
        assert!(!app.status.armed && !app.status.radiating);
    }

    #[test]
    fn a_new_window_starts_on_the_test_output_and_disarmed() {
        let ctx = egui::Context::default();
        let app = App::new(Options::default(), &ctx);
        assert_eq!(app.sink, SinkKind::Test);
        assert!(!app.read_ack);
        assert!(!app.status.armed);
        assert_eq!((app.txvga, app.amp), (0, false));
        assert_eq!(app.atten, 89.75);
    }

    #[test]
    fn the_pluto_output_starts_at_the_most_attenuation() {
        let ctx = egui::Context::default();
        let mut app = App::new(Options::default(), &ctx);
        app.set.source = SourceKind::File;
        app.set.mode = modeinfo::presets()[0].mode;
        app.set.use_t2 = false;
        app.file = Some(PathBuf::from("C:/v/clip.mp4"));
        app.sink = SinkKind::Pluto;
        assert!(app.sink_is_radio());
        let c = app.build_config().unwrap();
        match c.sink {
            TxSink::Pluto(p) => {
                assert_eq!(p.attenuation_db, 89.75);
                assert_eq!(p.uri, "auto");
                assert_eq!(p.bandwidth_hz, Some(8_000_000));
            }
            _ => panic!("expected the Pluto sink"),
        }
        assert!(!app.status.armed && !app.status.radiating);
    }

    /// Each source tab builds the source the library expects, and the
    /// codec and call sign reach the config.
    #[test]
    fn every_source_builds_its_config() {
        let ctx = egui::Context::default();
        let mut app = App::new(Options::default(), &ctx);
        // No camera yet (the probe has not run): refused, not a panic.
        app.set.source = SourceKind::Camera;
        app.set.camera.clear();
        assert!(app.build_config().is_err());
        assert!(!app.source_ready());
        app.set.camera = "USB Cam".into();
        app.set.mic = Some("Mic".into());
        let c = app.build_config().unwrap();
        assert!(matches!(&c.source, Source::Camera { video, audio, .. }
            if video == "USB Cam" && audio.as_deref() == Some("Mic")));

        app.set.source = SourceKind::Test;
        app.set.video_codec = VideoCodec::Av1;
        app.set.callsign = "N0CALL".into();
        app.set.overlay = true;
        let c = app.build_config().unwrap();
        assert_eq!(c.source, Source::Test);
        assert_eq!(c.video.video_codec, VideoCodec::Av1);
        assert_eq!(c.service.name, "N0CALL");
        assert!(c.overlay);

        app.set.source = SourceKind::Udp;
        app.set.udp = "127.0.0.1:1234".into();
        assert!(matches!(app.build_config().unwrap().source, Source::Udp(_)));
        app.set.udp = "nonsense".into();
        assert!(app.build_config().is_err());
        assert!(!app.source_ready());

        app.set.source = SourceKind::TsFile;
        assert!(app.build_config().is_err());
        app.file = Some(PathBuf::from("x.ts"));
        let c = app.build_config().unwrap();
        assert_eq!(c.source, Source::TsFile);
        assert_eq!(c.input, PathBuf::from("x.ts"));
    }
}
