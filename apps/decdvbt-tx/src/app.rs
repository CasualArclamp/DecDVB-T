// made by CasualArclamp
//! The window: state strip, source / mode / channel / radio controls, the
//! signal view and meters, and the safety strip (docs/TX_DESIGN.md §7).
//!
//! The window owns no DSP. It builds a `TxConfig`, hands it to the session
//! thread ([`crate::host::Host`]) and draws the `TxStatus` and spectrum rows it
//! gets back. What may be pressed when is decided by the pure functions in
//! `safety.rs`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use decdvbt_core::channel::{UHF_FIRST, UHF_LAST, uhf_centre_hz, uhf_channel_at};
use decdvbt_core::dvbt::Rate;
use decdvbt_tx::{
    AudioCodec, EncoderChoice, HackRfTxConfig, LICENCE_WARNING, LicenceAck, ServiceInfo, TxConfig,
    TxRate, TxSink, TxState, TxStatus, VideoCodec, VideoSettings, mode_table,
};
use eframe::egui::{self, Color32, RichText, vec2};

use crate::cli::{Options, SinkArg};
use crate::demo::{self, DemoState};
use crate::host::{Cmd, Host};
use crate::meters::{self, Level, MetersView, Trend};
use crate::modeinfo;
use crate::safety::{self, STOP_FIRST, VGA_MAX};
use crate::settings::Settings;
use crate::signal_view::{self, Readout, SignalInput};
use crate::source::{self, FileInfo, ProbeJob, Probed};
use crate::waterfall::{History, RingImage};
use crate::{AMBER, GREEN, RED, freq_display};

/// The red behind the TRANSMITTING banner and the STOP button.
const DEEP_RED: Color32 = Color32::from_rgb(176, 28, 28);
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
}

impl SinkKind {
    fn label(self) -> &'static str {
        match self {
            SinkKind::Test => "Test only (no output)",
            SinkKind::IqFile => "IQ file",
            SinkKind::HackRf => "HackRF (radio)",
        }
    }
}

/// A message in the red strip.
struct ErrorStrip {
    text: String,
    /// Extra advice under it.
    hint: Option<String>,
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

    sink: SinkKind,
    paced: bool,
    iq_path: String,
    iq_seconds: String,
    /// The TX VGA the slider asks for, and the last value sent to the radio.
    txvga: u16,
    vga_sent: u16,
    amp: bool,

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

    trend: Trend,
    history: History,
    ring: RingImage,
    smooth: Vec<f32>,
    /// The last row's span, Hz.
    span_hz: f64,
    wf_rows: usize,

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
        }
        if let Some(c) = opts.centre_hz {
            set.centre_hz = c;
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
            sink,
            paced: true,
            iq_path,
            iq_seconds: String::new(),
            txvga: 0,
            vga_sent: 0,
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
            trend: Trend::default(),
            history: History::default(),
            ring: RingImage::default(),
            smooth: Vec::new(),
            span_hz: 0.0,
            wf_rows: 0,
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
        if let Some(d) = demo {
            // The demo pretends a file was opened.
            app.file = Some(PathBuf::from("promo clip.mp4"));
            app.file_info = Some(FileInfo::Done(Probed::Info(demo::video_info())));
            app.session = d.has_session();
        } else if let Some(f) = app.opts.file.clone() {
            // The file fills in the form and nothing else.
            app.open_file(f, ctx);
        }
        app
    }

    /// The process exit code the run asks for.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    // ---- state helpers -------------------------------------------------

    fn sink_is_radio(&self) -> bool {
        self.sink == SinkKind::HackRf
    }

    /// The form is locked: a session exists (or a demo pretends one).
    fn locked(&self) -> bool {
        self.session || self.stopping
    }

    fn idle_cfg(&self) -> TxConfig {
        let mut c = TxConfig::new(self.file.clone().unwrap_or_else(|| "none.mp4".into()));
        c.mode = self.set.mode;
        c.centre_hz = self.set.centre_hz;
        c.rate = self.set.rate;
        c
    }

    fn idle_status(&self) -> TxStatus {
        TxStatus::idle(&self.idle_cfg(), &decdvbt_tx::Arm::new())
    }

    /// The config for the session, from the form. `None` without a file.
    fn build_config(&self) -> Option<TxConfig> {
        let file = self.file.clone()?;
        let mut c = TxConfig::new(file.clone());
        c.loop_input = self.set.loop_input;
        c.mode = self.set.mode;
        c.centre_hz = self.set.centre_hz;
        c.rate = self.set.rate;
        c.level_dbfs = self.set.level_dbfs;
        c.video = VideoSettings {
            encoder: self.set.encoder,
            video_codec: self.set.video_codec,
            audio_codec: self.set.audio_codec,
            ..VideoSettings::default()
        };
        c.service = ServiceInfo {
            name: source::service_name(&file),
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
                // The filter follows the channel: 8 MHz for an 8 MHz channel.
                baseband_filter_hz: Some((self.set.mode.bandwidth.mhz() * 1e6) as u32),
            }),
        };
        Some(c)
    }

    fn set_error(&mut self, text: String, from_radio_start: bool) {
        let hint = (from_radio_start && self.sink_is_radio() && !text.contains("not armed"))
            .then(|| "Close DecDVB-T (or whatever holds the HackRF) and try again.".to_string());
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
        self.file_info = Some(FileInfo::Probing);
        self.probe = Some(ProbeJob::start(
            path,
            self.set.ffmpeg_path.clone(),
            ctx.clone(),
        ));
    }

    fn browse(&mut self, ctx: &egui::Context) {
        let mut d = rfd::FileDialog::new()
            .set_title("Choose a video file")
            .add_filter("Video", &source::VIDEO_EXTENSIONS)
            .add_filter("All files", &["*"]);
        if let Some(f) = self.set.folder.as_ref().filter(|f| f.is_dir()) {
            d = d.set_directory(f);
        }
        if let Some(p) = d.pick_file() {
            self.open_file(p, ctx);
        }
    }

    fn prepare(&mut self) {
        let Some(cfg) = self.build_config() else {
            return;
        };
        let Some(host) = &self.host else { return };
        self.error = None;
        self.seen_status_error = None;
        self.history.reset();
        self.smooth.clear();
        self.trend.clear();
        self.read_ack = false;
        self.vga_sent = self.txvga;
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
        // The trace is smoothed a little so it does not flicker; the waterfall
        // keeps every row as it arrived.
        if self.smooth.len() != bins.len() {
            self.smooth = bins.clone();
        } else {
            for (s, b) in self.smooth.iter_mut().zip(&bins) {
                *s += 0.4 * (b - *s);
            }
        }
        self.history.push(bins);
    }

    /// Things that follow from the status: the trend, the ack tick, the VGA.
    fn finish_status_effects(&mut self) {
        let t = self.started.elapsed().as_secs_f64();
        self.trend
            .push(t, self.status.ts_stuffed, self.status.clipped);
        // An arm that ended (used, lapsed, dropped) takes the tick with it:
        // every arm starts from a fresh, deliberate tick.
        if self.was_armed && !self.status.armed {
            self.read_ack = false;
        }
        self.was_armed = self.status.armed;
        if self.status.state != TxState::Transmitting {
            self.vga_sent = self.status.txvga_db;
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
        // Rows: 20 a second, and a full waterfall at the start.
        if self.wf_rows == 0 || matches!(state, DemoState::Idle | DemoState::Failed) {
            return;
        }
        if self.history.is_empty() {
            self.demo_rows = 0;
        }
        let want = if self.demo_rows == 0 {
            self.history.capacity() as u64
        } else {
            ((t * 20.0) as u64).saturating_sub(self.demo_rows).min(60)
        };
        let occupied = self.set.mode.bandwidth.mhz() * 1e6 * 0.951;
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
                if matches!(self.file_info, Some(FileInfo::Done(_))) {
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

    fn state_strip(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let (text, colour) = self.state_view();
            ui.label(RichText::new(text).size(20.0).strong().color(colour));
            if self.status.state == TxState::Preparing && !self.stopping {
                ui.add(
                    egui::ProgressBar::new(meters::prepare_progress(&self.status))
                        .desired_width(160.0)
                        .fill(AMBER),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.main_action(ui);
            });
        });
        ui.add_space(2.0);
    }

    /// Prepare / Cancel / Reset: the action that is not "transmit".
    fn main_action(&mut self, ui: &mut egui::Ui) {
        let st = self.status.state;
        if self.stopping {
            return;
        }
        if !self.locked() {
            let ok = safety::can_prepare(self.session, self.file.is_some());
            let b = egui::Button::new(RichText::new("Prepare").size(16.0).strong())
                .min_size(vec2(120.0, 28.0));
            let r = ui.add_enabled(ok, b);
            let r = if self.file.is_none() {
                r.on_disabled_hover_text("Choose a video file first")
            } else {
                r.on_hover_text("Start ffmpeg and the modulator; the radio stays off")
            };
            if r.clicked() {
                self.prepare();
            }
            return;
        }
        let label = match st {
            TxState::Preparing | TxState::Ready => "Cancel",
            TxState::Finished | TxState::Failed => "Reset",
            _ => return,
        };
        if ui
            .add(egui::Button::new(RichText::new(label).size(16.0)).min_size(vec2(120.0, 28.0)))
            .clicked()
        {
            self.stop();
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

    fn banner(&self, ui: &mut egui::Ui) {
        let centre = self
            .session_cfg
            .as_ref()
            .map_or(self.set.centre_hz, |c| c.centre_hz);
        let Some(text) = safety::banner(&self.status, centre) else {
            return;
        };
        egui::Frame::new()
            .fill(DEEP_RED)
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(text)
                            .size(24.0)
                            .strong()
                            .color(Color32::WHITE),
                    );
                });
            });
    }

    fn section(ui: &mut egui::Ui, title: &str) {
        ui.add_space(8.0);
        ui.label(RichText::new(title).strong().color(Color32::from_gray(150)));
        ui.separator();
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.source_section(ui);
                self.mode_section(ui);
                self.channel_section(ui);
                self.radio_section(ui);
                self.advanced_section(ui);
            });
    }

    fn source_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "SOURCE");
        let locked = self.locked();
        ui.horizontal(|ui| {
            let r = ui.add_enabled(!locked, egui::Button::new("Browse..."));
            let r = if locked {
                r.on_disabled_hover_text(STOP_FIRST)
            } else {
                r
            };
            if r.clicked() {
                self.browse(ui.ctx());
            }
            let name = self
                .file
                .as_ref()
                .map_or("(no file)".to_string(), |p| source::file_label(p));
            ui.add(egui::Label::new(RichText::new(name).strong()).truncate());
        });
        ui.label(RichText::new("(drop a file anywhere)").weak());
        match &self.file_info {
            Some(FileInfo::Probing) => {
                ui.label(RichText::new("reading the file...").weak());
            }
            Some(FileInfo::Done(Probed::Info(i))) => {
                let (a, b) = source::describe(i);
                ui.label(a);
                if !b.is_empty() {
                    ui.label(RichText::new(b).weak());
                }
                if i.interlaced || i.bit_depth > 8 {
                    ui.label(
                        RichText::new("interlaced or 10-bit: the x264 chain (more CPU)")
                            .color(AMBER),
                    );
                } else if i.height > 1080 || i.fps > 30.5 {
                    ui.label(
                        RichText::new("a heavy source: more CPU to scale it down").color(AMBER),
                    );
                }
            }
            Some(FileInfo::Done(Probed::NoProbe)) => {
                ui.label(RichText::new("(no ffprobe: info unavailable)").weak());
            }
            Some(FileInfo::Done(Probed::NoFfmpeg(_))) => {
                ui.label(RichText::new("ffmpeg not found").color(RED));
            }
            Some(FileInfo::Done(Probed::Bad(_))) => {
                ui.label(RichText::new("unreadable file").color(RED));
            }
            None => {}
        }
        ui.add_enabled_ui(!locked, |ui| {
            ui.checkbox(&mut self.set.loop_input, "Loop")
                .on_hover_text("Start again at the end of the file (inside ffmpeg: no breaks)");
        });
    }

    fn mode_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "MODE");
        let locked = self.locked();
        let r = ui.add_enabled_ui(!locked, |ui| {
            let presets = modeinfo::presets();
            let current = modeinfo::preset_of(&self.set.mode);
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
                            }
                        }
                    });
            });
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
                egui::ComboBox::from_id_salt("bw")
                    .selected_text(format!("{} MHz", m.bandwidth.mhz()))
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for b in modeinfo::BANDWIDTHS {
                            ui.selectable_value(&mut m.bandwidth, b, format!("{} MHz", b.mhz()));
                        }
                    })
                    .response
                    .on_hover_text("Channel bandwidth");
            });
            // The low-priority code rate is not used (non-hierarchical).
            m.rate_lp = m.rate_hp;
            self.set.mode = m;
        });
        if locked {
            r.response.on_hover_text(STOP_FIRST);
        }
        let sum = modeinfo::summary(&self.set.mode);
        ui.label(RichText::new(format!("-> {}", sum.rate_line())).strong());
        ui.label(RichText::new(format!("   {}", sum.picture_line(25))).weak());
        if ui.small_button("All modes...").clicked() {
            self.show_modes = !self.show_modes;
        }
    }

    fn channel_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "CHANNEL");
        let locked = self.locked();
        let r = ui.add_enabled_ui(!locked, |ui| {
            ui.horizontal(|ui| {
                freq_display::show(ui, &mut self.set.centre_hz, 1e6, 6e9, 28.0);
                ui.label("Hz");
            });
            ui.horizontal(|ui| {
                ui.label("UHF channel");
                let cur = uhf_channel_at(self.set.centre_hz).filter(|&n| {
                    uhf_centre_hz(n).is_some_and(|c| (c - self.set.centre_hz).abs() < 1.0)
                });
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
                ui.label(RichText::new(format!("{:.3} MHz", self.set.centre_hz / 1e6)).weak());
            });
        });
        if locked {
            r.response.on_hover_text(STOP_FIRST);
        }
        if let Some(w) = safety::television_warning(self.set.centre_hz) {
            ui.label(RichText::new(w).color(RED));
        }
    }

    fn radio_section(&mut self, ui: &mut egui::Ui) {
        Self::section(ui, "RADIO");
        let radio = self.sink_is_radio();
        let st = self.status.state;
        // The amp is set before Prepare; the VGA is also live while transmitting.
        let idle = !self.locked();
        let live = radio && st == TxState::Transmitting && !self.stopping;
        ui.add_enabled_ui(radio, |ui| {
            ui.horizontal(|ui| {
                ui.label("TX VGA");
                let r = ui.add_enabled(
                    idle || live,
                    egui::Slider::new(&mut self.txvga, 0..=VGA_MAX).suffix(" dB"),
                );
                r.on_hover_text(
                    "Set before Prepare, or live while transmitting (it moves in steps of at most 6 dB)",
                );
            });
            if radio && self.txvga != self.status.txvga_db && live {
                ui.label(
                    RichText::new(format!("   now {} dB, moving to {}", self.vga_sent, self.txvga))
                        .color(AMBER),
                );
            }
            ui.add_enabled(
                idle,
                egui::Checkbox::new(&mut self.amp, "RF amp (+14 dB)"),
            )
            .on_hover_text("Off at every start. Only while stopped.");
            if self.amp {
                ui.label(RichText::new("   the RF amp is on").color(AMBER));
            }
        });
        if !radio {
            ui.label(RichText::new("(the HackRF output is not selected)").weak());
        }
        ui.label(
            RichText::new(format!(
                "Level {:.0} dBFS (Advanced...)",
                self.set.level_dbfs
            ))
            .weak(),
        );
    }

    fn advanced_section(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        let locked = self.locked();
        egui::CollapsingHeader::new(RichText::new("Advanced").strong().color(Color32::from_gray(150)))
            .default_open(false)
            .show(ui, |ui| {
                ui.add_enabled_ui(!locked, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Level");
                        ui.add(
                            egui::Slider::new(&mut self.set.level_dbfs, -30.0..=-6.0)
                                .suffix(" dBFS")
                                .fixed_decimals(0),
                        )
                        .on_hover_text(
                            "The signal's rms before the radio's gain. -12 keeps 38 dB of \
                             8-bit quantisation SNR without clipping; the power is set by TX VGA.",
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Radio sample rate");
                        let text = match self.set.rate {
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
                    ui.horizontal(|ui| {
                        ui.label("Encoder");
                        egui::ComboBox::from_id_salt("encoder")
                            .selected_text(match self.set.encoder {
                                EncoderChoice::Auto => "auto",
                                EncoderChoice::Nvenc => "NVENC",
                                EncoderChoice::X264 => "x264",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.set.encoder, EncoderChoice::Auto, "auto");
                                ui.selectable_value(&mut self.set.encoder, EncoderChoice::Nvenc, "NVENC");
                                ui.selectable_value(&mut self.set.encoder, EncoderChoice::X264, "x264");
                            });
                        ui.label("Video");
                        egui::ComboBox::from_id_salt("vcodec")
                            .selected_text(match self.set.video_codec {
                                VideoCodec::H264 => "H.264",
                                VideoCodec::Mpeg2 => "MPEG-2",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.set.video_codec, VideoCodec::H264, "H.264");
                                ui.selectable_value(&mut self.set.video_codec, VideoCodec::Mpeg2, "MPEG-2");
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Audio");
                        egui::ComboBox::from_id_salt("acodec")
                            .selected_text(match self.set.audio_codec {
                                AudioCodec::AacLatm => "AAC (LATM)",
                                AudioCodec::AacAdts => "AAC (ADTS)",
                                AudioCodec::Mp2 => "MP2",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.set.audio_codec, AudioCodec::AacLatm, "AAC (LATM)");
                                ui.selectable_value(&mut self.set.audio_codec, AudioCodec::AacAdts, "AAC (ADTS)");
                                ui.selectable_value(&mut self.set.audio_codec, AudioCodec::Mp2, "MP2");
                            });
                    });
                    ui.checkbox(&mut self.set.preview, "Signal preview")
                        .on_hover_text("One 2048-point FFT per 50 ms of signal");
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

    fn signal_and_meters(&mut self, ui: &mut egui::Ui) {
        let total = ui.available_height();
        let sig_h = (total * 0.56).max(240.0).min(total);
        let ppp = ui.ctx().pixels_per_point();
        self.wf_rows = signal_view::waterfall_rows(sig_h, ppp).max(64);
        self.history.set_capacity(self.wf_rows);
        let tex = self.ring.update(ui.ctx(), &self.history);
        let live = self.session && self.status.state != TxState::Idle;
        let empty = if !self.set.preview {
            "the preview is off (Advanced)"
        } else if self.status.state == TxState::Failed {
            "no signal: the session failed (see the message above)"
        } else if live {
            "waiting for the first rows of the signal..."
        } else {
            "no signal yet: choose a file, press Prepare, then Transmit"
        };
        let centre = self
            .session_cfg
            .as_ref()
            .map_or(self.set.centre_hz, |c| c.centre_hz);
        let now = self.started.elapsed().as_secs_f64();
        let levels = meters::classify(&self.status, &self.trend, now);
        let readout = (live && self.status.level_rms_dbfs.is_finite()).then(|| Readout {
            rms_dbfs: self.status.level_rms_dbfs,
            peak_dbfs: self.status.peak_dbfs,
            clipped: self.status.clipped,
            clipping: levels.clipping == Level::Red,
        });
        let span = if self.span_hz > 0.0 {
            self.span_hz
        } else {
            self.set.rate.hz(&self.set.mode) as f64
        };
        ui.allocate_ui(vec2(ui.available_width(), sig_h), |ui| {
            signal_view::show(
                ui,
                &SignalInput {
                    spectrum_db: &self.smooth,
                    centre_hz: centre,
                    span_hz: span,
                    occupied_hz: self.set.mode.bandwidth.mhz() * 1e6 * 0.951,
                    levels: self.history.levels(),
                    waterfall: tex,
                    readout,
                    empty_text: empty,
                },
            );
        });
        ui.add_space(6.0);
        ui.label(
            RichText::new("METERS")
                .strong()
                .color(Color32::from_gray(150)),
        );
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let paced = match &self.session_cfg {
                    Some(c) => match &c.sink {
                        TxSink::Null { paced } | TxSink::IqFile { paced, .. } => *paced,
                        TxSink::HackRf(_) => true,
                    },
                    None => true,
                };
                meters::show(
                    ui,
                    &MetersView {
                        status: &self.status,
                        levels,
                        paced,
                        live,
                    },
                );
            });
    }

    fn safety_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(
            RichText::new("SAFETY")
                .strong()
                .color(Color32::from_gray(150)),
        );
        // Always visible, never a popup, and the very text the arm checks.
        ui.add(
            egui::Label::new(
                RichText::new(self.licence_shown).color(Color32::from_rgb(255, 214, 140)),
            )
            .wrap(),
        );
        if let Some(w) = safety::television_warning(self.set.centre_hz) {
            ui.label(RichText::new(w).color(RED).strong());
        }
        ui.add_space(4.0);
        let radio = self.sink_is_radio();
        let st = self.status.state;
        ui.horizontal_wrapped(|ui| {
            let r = ui.add_enabled(
                radio && !self.status.armed,
                egui::Checkbox::new(&mut self.read_ack, "I have read this"),
            );
            if !radio {
                r.on_disabled_hover_text("Only the HackRF output needs an arm");
            }
            let armed = self.status.armed;
            let can = safety::can_arm(st, radio, self.read_ack);
            let label = if armed { "DISARM" } else { "ARM" };
            let b = egui::Button::new(RichText::new(label).size(18.0).strong())
                .min_size(vec2(100.0, 40.0));
            let r = ui.add_enabled(armed || can, b);
            let r = if !radio {
                r.on_disabled_hover_text("A test output needs no arm")
            } else if st != TxState::Ready && !armed {
                r.on_disabled_hover_text(
                    "Press Prepare first: ARM is available when the state is READY",
                )
            } else if !self.read_ack && !armed {
                r.on_disabled_hover_text("Tick \"I have read this\" first")
            } else {
                r
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
            if radio {
                let text = safety::arm_text(&self.status);
                ui.label(RichText::new(text).color(if self.status.armed {
                    AMBER
                } else {
                    Color32::GRAY
                }));
            }

            ui.add_space(12.0);
            let can_tx =
                safety::can_transmit(&self.status, self.status.armed, radio) && !self.stopping;
            let fill = if radio {
                DEEP_RED
            } else {
                Color32::from_rgb(34, 110, 60)
            };
            let b = egui::Button::new(
                RichText::new(if radio { "TRANSMIT" } else { "START" })
                    .size(18.0)
                    .strong()
                    .color(Color32::WHITE),
            )
            .fill(fill)
            .min_size(vec2(130.0, 40.0));
            let r = ui.add_enabled(can_tx, b);
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

            ui.add_space(12.0);
            let stop = egui::Button::new(
                RichText::new("STOP")
                    .size(26.0)
                    .strong()
                    .color(Color32::WHITE),
            )
            .fill(DEEP_RED)
            .min_size(vec2(170.0, 52.0));
            if ui
                .add_enabled(self.locked(), stop)
                .on_hover_text("Radio off, everything stopped (Esc or Space)")
                .clicked()
            {
                self.stop();
            }
        });
        ui.add_space(4.0);
        self.output_row(ui);
        ui.add_space(4.0);
    }

    fn output_row(&mut self, ui: &mut egui::Ui) {
        let locked = self.locked();
        ui.horizontal_wrapped(|ui| {
            ui.label("Output:");
            let before = self.sink;
            ui.add_enabled_ui(!locked, |ui| {
                egui::ComboBox::from_id_salt("sink")
                    .selected_text(self.sink.label())
                    .show_ui(ui, |ui| {
                        for k in [SinkKind::Test, SinkKind::IqFile, SinkKind::HackRf] {
                            ui.selectable_value(&mut self.sink, k, k.label());
                        }
                    });
            });
            if self.sink != before {
                // A new output starts disarmed and unticked.
                self.read_ack = false;
            }
            match self.sink {
                SinkKind::Test => {
                    ui.add_enabled(
                        !locked,
                        egui::Checkbox::new(&mut self.paced, "paced (real time)"),
                    )
                    .on_hover_text("Off: generate as fast as possible (a speed test)");
                    ui.label(RichText::new("Test output - no radio").color(GREEN));
                }
                SinkKind::IqFile => {
                    ui.add_enabled_ui(!locked, |ui| {
                        let hint = source::iq_file_name(
                            self.set.centre_hz,
                            self.set.rate.hz(&self.set.mode),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.iq_path)
                                .desired_width(260.0)
                                .hint_text(hint),
                        );
                        ui.label("seconds");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.iq_seconds)
                                .desired_width(50.0)
                                .hint_text("all"),
                        );
                        ui.checkbox(&mut self.paced, "paced");
                    });
                    ui.label(RichText::new("IQ file - no radio").color(GREEN));
                }
                SinkKind::HackRf => {
                    ui.label(
                        RichText::new("HackRF: this radiates when TRANSMIT is pressed")
                            .color(AMBER),
                    );
                }
            }
        });
    }

    fn mode_window(&mut self, ctx: &egui::Context) {
        if !self.show_modes {
            return;
        }
        let locked = self.locked();
        let mut open = true;
        let mut pick = None;
        egui::Window::new("All modes")
            .open(&mut open)
            .default_width(520.0)
            .default_height(480.0)
            .show(ctx, |ui| {
                let m = self.set.mode;
                ui.label(
                    RichText::new(format!(
                        "{} {} MHz, non-hierarchical. Click a row to use it.",
                        m.fft.name(),
                        m.bandwidth.mhz()
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
        self.poll_host();
        self.feed_demo();
        self.note_status_error();
        self.safety_tick(ctx);
        self.drive_auto(ctx);
        self.remember(ctx);
        self.handle_screenshot(ctx);
        // A live session repaints when the session thread says so (new rows,
        // a new state, the meters a few times a second); only the hooks that
        // have no session to say it keep a timer.
        if self.demo.is_some() || self.opts.screenshot.is_some() || self.probe.is_some() {
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
        egui::Panel::bottom("safety").show(ui, |ui| self.safety_panel(ui));
        egui::Panel::left("controls")
            .resizable(true)
            .default_size(380.0)
            .min_size(330.0)
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
    }
}
