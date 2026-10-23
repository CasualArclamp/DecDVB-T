// made by CasualArclamp
//! DecDVB-T desktop GUI.
//!
//! M1: a HackRF (or an IQ file) in, the spectrum and waterfall over the UHF
//! band, tuning by channel, an IQ recorder, and the cost of it all in the
//! status bar. The receiver, the constellation scopes and the services
//! panel's contents arrive with M3/M4; the right-hand panel and the bottom
//! one are laid out now so they do not move later.

mod band;
mod card;
mod source;
mod waterfall;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use decdvbt_core::channel::{parse_channel, uhf_centre_hz, uhf_channel_at, uk_channels};
use decdvbt_core::dvbt::Mode;
use decdvbt_engine::{Command, CpuGauge, Row, Stats, spectral_cn_db};
use decdvbt_io::name_hint;
use eframe::egui::{self, Color32, RichText};
use source::{Gains, Session, Spec};
use waterfall::{History, RingImage};

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const RED: Color32 = Color32::from_rgb(255, 90, 80);
const GREEN: Color32 = Color32::from_rgb(110, 220, 130);

/// HackRF sample rates with the least clock jitter that hold a channel.
const RATES_MSPS: [f64; 4] = [10.0, 12.5, 16.0, 20.0];
/// Half a channel plus a margin: a channel is received without retuning only
/// if all of this is inside the span.
const CHANNEL_HALF_NEEDED_HZ: f64 = 4.1e6;

struct Options {
    file: Option<PathBuf>,
    channel: Option<u8>,
    rate_msps: Option<f64>,
    screenshot: Option<PathBuf>,
    after: f64,
    /// Frames a second while a source runs; the CPU it costs is most of the
    /// GUI's.
    fps: f64,
    /// A mode to hold the C/N against, until the receiver reads the TPS.
    assume: Option<Mode>,
}

impl Options {
    fn from_args() -> Result<Options, String> {
        let mut o = Options {
            file: None,
            channel: None,
            rate_msps: None,
            screenshot: None,
            after: 4.0,
            fps: 15.0,
            assume: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            let mut value = |what: &str| args.next().ok_or(format!("{a} needs {what}"));
            match a.as_str() {
                "--channel" => {
                    let v = value("a channel, e.g. C26")?;
                    o.channel = Some(parse_channel(&v).ok_or(format!("not a UHF channel: {v}"))?);
                }
                "--rate" => {
                    o.rate_msps = Some(value("MS/s")?.parse().map_err(|_| "bad --rate")?);
                }
                "--screenshot" => o.screenshot = Some(value("a .png path")?.into()),
                "--assume" => {
                    let v = value("a mode, e.g. 64qam-2/3-8k-1/32")?;
                    o.assume = Some(Mode::parse(&v)?);
                }
                "--fps" => o.fps = value("frames a second")?.parse().map_err(|_| "bad --fps")?,
                "--after" => o.after = value("seconds")?.parse().map_err(|_| "bad --after")?,
                f if !f.starts_with("--") => o.file = Some(f.into()),
                _ => return Err(format!("unknown option {a}")),
            }
        }
        Ok(o)
    }
}

fn main() -> eframe::Result {
    // `--version` alone: print it and stop (the release workflow checks it).
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("decdvbt-gui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let opts = match Options::from_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("decdvbt-gui: {e}");
            eprintln!(
                "usage: decdvbt-gui [capture.cs8] [--channel C26] [--rate MSPS] \
                 [--screenshot OUT.png] [--after SECS] [--fps N] [--assume 64qam-2/3-8k-1/32]"
            );
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("DecDVB-T")
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "DecDVB-T",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // One monospace face throughout: columns of numbers line up.
            cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
                s.override_text_style = Some(egui::TextStyle::Monospace);
            });
            Ok(Box::new(App::new(opts)))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    HackRf,
    File,
}

struct App {
    opts: Options,
    started: Instant,
    shot_sent: bool,
    /// For `--screenshot`: process CPU and time at 2 s, to average over the
    /// run without the screenshot's own cost.
    cpu_mark: Option<(f64, Instant)>,
    cpu_avg: Option<f32>,
    /// Time spent in `logic` and in `ui` over the run, and the frames
    /// counted, for the average cost per frame.
    frame_cost: [Duration; 2],
    frames: u32,
    /// Frame pacing: when the last frame began, and the delay asked for
    /// the next (see [`App::pace`]).
    last_frame: Option<Instant>,
    delay_ms: f64,

    session: Option<Session>,
    kind: SourceKind,
    /// The channel being received (the VFO), by number.
    channel: u8,
    channel_text: String,
    rate_msps: f64,
    gains: Gains,
    /// A file source: its path, centre (MHz) and rate (MS/s), editable
    /// because a raw file has no header.
    file: Option<PathBuf>,
    file_centre_mhz: f64,
    file_rate_msps: f64,

    /// The view: from the newest row.
    centre_hz: f64,
    span_hz: f64,
    vfo_hz: Option<f64>,
    tuning: u32,
    smooth: Vec<f32>,
    history: History,
    ring: RingImage,

    /// The C/N estimated from the spectrum, smoothed; and rows in a row
    /// that had none (it is dropped after a few, not at the first).
    cn_db: Option<f32>,
    cn_misses: u32,
    stats: Stats,
    stats_at: Instant,
    cpu: CpuGauge,
    cpu_percent: f32,
    note: String,
}

impl App {
    fn new(opts: Options) -> App {
        let channel = opts.channel.unwrap_or(26);
        let mut app = App {
            started: Instant::now(),
            shot_sent: false,
            cpu_mark: None,
            cpu_avg: None,
            frame_cost: [Duration::ZERO; 2],
            frames: 0,
            last_frame: None,
            delay_ms: 1000.0 / 15.0,
            session: None,
            kind: SourceKind::HackRf,
            channel,
            channel_text: format!("C{channel}"),
            rate_msps: opts.rate_msps.unwrap_or(10.0),
            gains: Gains::default(),
            file: None,
            file_centre_mhz: 0.0,
            file_rate_msps: 10.0,
            centre_hz: 0.0,
            span_hz: 0.0,
            vfo_hz: uhf_centre_hz(channel),
            tuning: 0,
            smooth: Vec::new(),
            history: History::default(),
            ring: RingImage::default(),
            cn_db: None,
            cn_misses: 0,
            stats: Stats::default(),
            stats_at: Instant::now(),
            cpu: CpuGauge::default(),
            cpu_percent: 0.0,
            note: String::new(),
            opts,
        };
        // Started from the command line: begin at once.
        if let Some(f) = app.opts.file.clone() {
            app.open_file(f);
        } else if app.opts.channel.is_some() {
            app.start();
        }
        app
    }

    // ---- sources --------------------------------------------------------

    fn start(&mut self) {
        self.stop();
        match self.kind {
            SourceKind::HackRf => {
                let Some(fc) = uhf_centre_hz(self.channel) else {
                    return;
                };
                let spec = Spec::HackRf {
                    center_hz: fc,
                    rate_hz: self.rate_msps * 1e6,
                    gains: self.gains,
                };
                self.begin(spec, Some(fc));
            }
            SourceKind::File => {
                let Some(path) = self.file.clone() else {
                    self.open_file_dialog();
                    return;
                };
                let spec = Spec::File {
                    path,
                    center_hz: self.file_centre_mhz * 1e6,
                    rate_hz: self.file_rate_msps * 1e6,
                };
                let vfo = uhf_channel_at(self.file_centre_mhz * 1e6)
                    .and_then(uhf_centre_hz)
                    .or((self.file_centre_mhz > 0.0).then_some(self.file_centre_mhz * 1e6));
                self.begin(spec, vfo);
            }
        }
    }

    fn begin(&mut self, spec: Spec, vfo: Option<f64>) {
        match Session::open(&spec) {
            Ok(s) => {
                self.session = Some(s);
                self.vfo_hz = vfo;
                self.reset_view();
                self.note.clear();
            }
            Err(e) => self.note = e,
        }
    }

    fn stop(&mut self) {
        // Dropping the session stops the thread, which closes the radio.
        self.session = None;
    }

    fn open_file_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("IQ capture", &["cs8", "cu8", "cs16", "cf32", "wav"])
            .pick_file()
        {
            self.open_file(p);
        }
    }

    fn open_file(&mut self, path: PathBuf) {
        let hint = name_hint(&path);
        self.kind = SourceKind::File;
        self.file_centre_mhz = hint.center_hz.map_or(0.0, |h| h / 1e6);
        self.file_rate_msps = hint.sample_rate.map_or(10.0, |r| r / 1e6);
        self.file = Some(path);
        self.start();
    }

    fn reset_view(&mut self) {
        self.history.reset();
        self.smooth.clear();
        self.span_hz = 0.0;
    }

    // ---- tuning ---------------------------------------------------------

    /// Receive channel `n`: move the VFO if the span already holds the whole
    /// channel, retune the radio if not.
    fn tune_channel(&mut self, n: u8) {
        let Some(fc) = uhf_centre_hz(n) else { return };
        self.channel = n;
        self.channel_text = format!("C{n}");
        let fits = self.span_hz > 0.0
            && (fc - self.centre_hz).abs() + CHANNEL_HALF_NEEDED_HZ <= self.span_hz / 2.0;
        match &self.session {
            Some(s) if s.live && !fits => {
                s.rx.send(Command::Retune(fc));
                self.centre_hz = fc;
                self.reset_view();
                self.vfo_hz = Some(fc);
            }
            Some(s) if !s.live && !fits => {
                self.note = format!("C{n} is outside this capture");
            }
            _ => self.vfo_hz = Some(fc),
        }
    }

    fn step_channel(&mut self, up: bool) {
        let list: Vec<u8> = uk_channels().collect();
        let at = list.iter().position(|&c| c >= self.channel).unwrap_or(0);
        let next = if up {
            list[(at + usize::from(list[at] == self.channel)).min(list.len() - 1)]
        } else {
            list[at.saturating_sub(1)]
        };
        self.tune_channel(next);
    }

    fn change_gains(&mut self) {
        if let Some(s) = &self.session
            && let Err(e) = s.set_gains(self.gains)
        {
            self.note = e;
        }
    }

    // ---- recording ------------------------------------------------------

    fn recording(&self) -> bool {
        self.stats.recording.is_some()
    }

    fn toggle_record(&mut self) {
        let Some(s) = &self.session else { return };
        if self.recording() {
            s.rx.send(Command::Record(None));
            self.stats.recording = None;
            return;
        }
        let dir = PathBuf::from("captures");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.note = format!("cannot make {}: {e}", dir.display());
            return;
        }
        let name = format!(
            "c{}_{:.3}MHz_{}Msps_{}.cs8",
            uhf_channel_at(self.centre_hz).unwrap_or(0),
            self.centre_hz / 1e6,
            self.stats.sample_rate / 1e6,
            utc_stamp()
        );
        let path = dir.join(name);
        self.note = format!("recording to {}", path.display());
        s.rx.send(Command::Record(Some(path)));
    }

    // ---- per-frame work -------------------------------------------------

    fn take_rows(&mut self) {
        let Some(s) = &self.session else { return };
        let rows: Vec<Row> = s.rx.take_rows();
        // Polled a few times a second: the stats carry strings, and cloning
        // them every frame would be waste.
        let new_stats =
            (self.stats_at.elapsed() >= Duration::from_millis(250)).then(|| s.rx.stats());
        for row in rows {
            if row.tuning != self.tuning {
                self.tuning = row.tuning;
                self.reset_view();
            }
            self.centre_hz = row.center_hz;
            self.span_hz = row.sample_rate;
            // A calm trace: each row pulled a third of the way in (in dB).
            if self.smooth.len() != row.db.len() {
                self.smooth.clone_from(&row.db);
            } else {
                for (s, v) in self.smooth.iter_mut().zip(&row.db) {
                    *s += 0.35 * (v - *s);
                }
            }
            self.estimate_cn(&row);
            self.history.push(row.db);
        }
        if let Some(stats) = new_stats {
            self.stats = stats;
            self.stats_at = Instant::now();
            if let Some(e) = &self.stats.error {
                self.note.clone_from(e);
            }
        }
    }

    /// The wait to ask egui for before the next frame. egui's repaint timer
    /// does not tick at the interval it is given (a 50 ms request came back
    /// as 34 ms frames, on this machine and with vsync off), so the delay is
    /// steered by the frame gaps actually seen, to hold the display rate.
    fn pace(&mut self) -> Duration {
        let target = 1000.0 / self.opts.fps.clamp(1.0, 60.0);
        let now = Instant::now();
        if let Some(prev) = self.last_frame.replace(now) {
            let gap = now.duration_since(prev).as_secs_f64() * 1e3;
            // A long gap is a pause (nothing was running), not a measurement.
            if gap < 4.0 * target {
                self.delay_ms += 0.5 * (target - gap);
            }
        }
        self.delay_ms = self.delay_ms.clamp(1.0, 4.0 * target);
        Duration::from_secs_f64(self.delay_ms / 1e3)
    }

    /// Fold one row's spectral C/N estimate into the smoothed reading.
    fn estimate_cn(&mut self, row: &Row) {
        let est = self
            .vfo_hz
            .and_then(|v| spectral_cn_db(&row.db, row.center_hz, row.sample_rate, v));
        match est {
            Some(e) => {
                self.cn_misses = 0;
                self.cn_db = Some(self.cn_db.map_or(e, |c| c + 0.15 * (e - c)));
            }
            None => {
                self.cn_misses += 1;
                if self.cn_misses > 10 {
                    self.cn_db = None;
                }
            }
        }
    }

    fn card(&self) -> card::Card {
        let mode = self.opts.assume;
        card::Card {
            standard: mode.map(|_| card::Standard::DvbT),
            assumed: mode.is_some(),
            name: mode.map(|m| format!("{} {}", m.modulation.name(), m.rate_hp.name())),
            qef_db: mode.and_then(|m| m.qef_cn_db()),
            details: mode.map(|m| mode_details(&m)).unwrap_or_default(),
            cn_db: self.cn_db,
            cn_source: "spectrum",
            channel: self.vfo_hz.and_then(uhf_channel_at),
            freq_hz: self.vfo_hz,
            active: self.session.is_some(),
        }
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.opts.screenshot.clone() else {
            return;
        };
        if self.cpu_mark.is_none() && self.started.elapsed().as_secs_f64() >= 2.0 {
            self.cpu_mark = Some((decdvbt_engine::process_cpu_seconds(), Instant::now()));
        }
        if !self.shot_sent && self.started.elapsed().as_secs_f64() >= self.opts.after {
            if let Some((c0, t0)) = self.cpu_mark {
                let dt = t0.elapsed().as_secs_f64();
                self.cpu_avg =
                    Some((100.0 * (decdvbt_engine::process_cpu_seconds() - c0) / dt) as f32);
            }
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
            Ok(()) => eprintln!(
                "screenshot saved to {} (CPU {:.1} % average; per frame: logic {:.2} ms, ui {:.2} ms, {} frames; {})",
                path.display(),
                self.cpu_avg.unwrap_or(self.cpu_percent),
                self.frame_cost[0].as_secs_f64() * 1e3 / self.frames.max(1) as f64,
                self.frame_cost[1].as_secs_f64() * 1e3 / self.frames.max(1) as f64,
                self.frames,
                self.stats.describe
            ),
            Err(e) => eprintln!("cannot save screenshot {}: {e}", path.display()),
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ---- panels ---------------------------------------------------------

    fn control_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let running = self.session.is_some();
            let live = self.kind == SourceKind::HackRf;

            let before = self.kind;
            egui::ComboBox::from_id_salt("source")
                .selected_text(if live { "HackRF" } else { "IQ file" })
                .width(80.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.kind, SourceKind::HackRf, "HackRF");
                    ui.selectable_value(&mut self.kind, SourceKind::File, "IQ file");
                });
            if before != self.kind && running {
                self.stop();
            }

            if self.kind == SourceKind::HackRf {
                ui.label("channel");
                if ui.small_button("◀").clicked() {
                    self.step_channel(false);
                }
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.channel_text)
                        .desired_width(44.0)
                        .hint_text("C26"),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    match parse_channel(&self.channel_text) {
                        Some(n) => self.tune_channel(n),
                        None => {
                            self.note = format!("not a UHF channel (21–69): {}", self.channel_text)
                        }
                    }
                    self.channel_text = format!("C{}", self.channel);
                }
                if ui.small_button("▶").clicked() {
                    self.step_channel(true);
                }
                ui.label(format!(
                    "{:.3} MHz",
                    uhf_centre_hz(self.channel).unwrap_or(0.0) / 1e6
                ));

                ui.separator();
                ui.label("rate");
                let before = self.rate_msps;
                egui::ComboBox::from_id_salt("rate")
                    .selected_text(format!("{} MS/s", self.rate_msps))
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for r in RATES_MSPS {
                            ui.selectable_value(&mut self.rate_msps, r, format!("{r} MS/s"));
                        }
                    });
                if before != self.rate_msps && running {
                    self.start();
                }

                ui.separator();
                let mut g = self.gains;
                ui.label("LNA");
                ui.add(egui::DragValue::new(&mut g.lna).range(0..=40).speed(0.2));
                ui.label("VGA");
                ui.add(egui::DragValue::new(&mut g.vga).range(0..=62).speed(0.2));
                ui.checkbox(&mut g.amp, "amp");
                g.lna = g.lna / 8 * 8;
                g.vga = g.vga / 2 * 2;
                if g != self.gains {
                    self.gains = g;
                    self.change_gains();
                }
            } else {
                match &self.file {
                    Some(p) => ui.label(
                        p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    ),
                    None => ui.label(RichText::new("no file").weak()),
                };
                ui.label("centre");
                ui.add(
                    egui::DragValue::new(&mut self.file_centre_mhz)
                        .speed(0.1)
                        .suffix(" MHz"),
                );
                ui.label("rate");
                ui.add(
                    egui::DragValue::new(&mut self.file_rate_msps)
                        .speed(0.05)
                        .suffix(" MS/s"),
                );
                if ui.button("Open…").clicked() {
                    self.open_file_dialog();
                }
            }

            ui.separator();
            if running {
                if ui.button(RichText::new("■ Stop").color(RED)).clicked() {
                    self.stop();
                }
            } else if ui.button(RichText::new("▶ Start").color(GREEN)).clicked() {
                self.start();
            }
            if running && live {
                let label = if self.recording() {
                    RichText::new("● Recording").color(RED)
                } else {
                    RichText::new("● Record")
                };
                if ui.button(label).clicked() {
                    self.toggle_record();
                }
            }
        });
        ui.horizontal(|ui| {
            let s = &self.stats;
            let running = self.session.is_some();
            // Level meter: -60 dBFS to 0.
            let level = ((s.rms_dbfs + 60.0) / 60.0).clamp(0.0, 1.0);
            ui.add(
                egui::ProgressBar::new(if running { level } else { 0.0 })
                    .desired_width(180.0)
                    .text(if running {
                        format!("{:.1} dBFS", s.rms_dbfs)
                    } else {
                        "level".into()
                    }),
            );
            if running && s.clip_percent > 0.05 {
                ui.label(RichText::new("OVERLOAD: lower the gain").color(RED));
            } else if running && s.rms_dbfs < -45.0 {
                ui.label(RichText::new("weak: raise the gain").color(AMBER));
            }
            ui.separator();
            ui.label("display");
            egui::ComboBox::from_id_salt("fps")
                .selected_text(format!("{} fps", self.opts.fps))
                .width(70.0)
                .show_ui(ui, |ui| {
                    for f in [5.0, 10.0, 15.0, 20.0, 30.0] {
                        ui.selectable_value(&mut self.opts.fps, f, format!("{f} fps"));
                    }
                });
            if !self.note.is_empty() {
                ui.label(RichText::new(&self.note).color(AMBER));
            }
        });
        ui.add_space(2.0);
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        let s = &self.stats;
        let running = self.session.is_some();
        ui.add_space(4.0);
        ui.horizontal_top(|ui| {
            egui::Grid::new("receiver")
                .num_columns(2)
                .spacing([14.0, 2.0])
                .show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, k: &str, v: String| {
                        ui.label(RichText::new(k).weak());
                        ui.label(v);
                        ui.end_row();
                    };
                    row(
                        ui,
                        "state",
                        match (&self.session, s.ended) {
                            (None, _) => "stopped".into(),
                            (Some(_), true) => "end of file".into(),
                            _ => "running".into(),
                        },
                    );
                    row(ui, "source", s.describe.clone());
                    row(
                        ui,
                        "centre / span",
                        format!(
                            "{:.3} MHz / {:.1} MHz",
                            s.center_hz / 1e6,
                            s.sample_rate / 1e6
                        ),
                    );
                    row(
                        ui,
                        "channel",
                        self.vfo_hz
                            .and_then(uhf_channel_at)
                            .map_or("—".into(), |c| format!("C{c}")),
                    );
                });
            ui.add_space(30.0);
            egui::Grid::new("level")
                .num_columns(2)
                .spacing([14.0, 2.0])
                .show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, k: &str, v: String| {
                        ui.label(RichText::new(k).weak());
                        ui.label(v);
                        ui.end_row();
                    };
                    row(ui, "input rms", format!("{:.1} dBFS", s.rms_dbfs));
                    row(ui, "input peak", format!("{:.0} %", s.peak * 100.0));
                    row(ui, "clipping", format!("{:.2} %", s.clip_percent));
                    row(ui, "dropped buffers", s.dropped.to_string());
                });
            ui.add_space(30.0);
            egui::Grid::new("cpu")
                .num_columns(2)
                .spacing([14.0, 2.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("CPU, whole app").weak());
                    ui.label(format!("{:.0} %", self.cpu_percent));
                    ui.end_row();
                    // The stages, as a share of one core's time. "source" on
                    // a live radio is mostly waiting for the USB buffer.
                    for (stage, p) in decdvbt_engine::Stage::ALL.iter().zip(s.load.percent) {
                        if *stage == decdvbt_engine::Stage::Record && s.recording.is_none() {
                            continue;
                        }
                        ui.label(RichText::new(format!("  {}", stage.name())).weak());
                        ui.label(if *stage == decdvbt_engine::Stage::Source && running {
                            format!("{p:.0} % (waiting)")
                        } else {
                            format!("{p:.1} %")
                        });
                        ui.end_row();
                    }
                    if let Some(r) = &s.recording {
                        ui.label(RichText::new("recording").weak());
                        ui.label(format!("{:.0} MB", r.bytes as f64 / 1e6));
                        ui.end_row();
                    }
                });
        });
        ui.add_space(4.0);
    }

    fn services_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        card::show(ui, &self.card());
        ui.add_space(10.0);
        ui.label(RichText::new("Services").strong());
        ui.separator();
        ui.label(
            RichText::new(
                "No transport stream yet. The DVB-T receiver (M3) fills this list with the \
                 mux's services, their bitrates and now/next, and plays the one you click.",
            )
            .weak(),
        );
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        // A file dropped on the window is opened.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
        });
        if let Some(p) = dropped {
            self.open_file(p);
        }
        self.take_rows();
        self.cpu_percent = self.cpu.poll();
        self.handle_screenshot(ctx);
        if self.session.is_some() || self.opts.screenshot.is_some() {
            // Rows come 20 times a second; no need to draw faster than that.
            ctx.request_repaint_after(self.pace());
        }
        self.frame_cost[0] += t0.elapsed();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let t0 = Instant::now();
        self.frames += 1;
        egui::Panel::top("controls").show(ui, |ui| self.control_bar(ui));
        egui::Panel::bottom("receiver").show(ui, |ui| self.status_panel(ui));
        egui::Panel::right("services")
            .resizable(true)
            .default_size(380.0)
            .min_size(300.0)
            .show(ui, |ui| self.services_panel(ui));

        let mut tune = None;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                let tex = self.ring.update(ui.ctx(), &self.history);
                let inp = band::BandInput {
                    spectrum_db: &self.smooth,
                    center_hz: self.centre_hz,
                    span_hz: self.span_hz,
                    levels: self.history.levels(),
                    waterfall: tex,
                    vfo_hz: self.vfo_hz,
                };
                if let Some(band::Action::Tune(n)) = band::show(ui, &inp) {
                    tune = Some(n);
                }
            });
        if let Some(n) = tune {
            if self.session.is_none() && self.kind == SourceKind::HackRf {
                self.channel = n;
                self.channel_text = format!("C{n}");
                self.vfo_hz = uhf_centre_hz(n);
            } else {
                self.tune_channel(n);
            }
        }
        self.frame_cost[1] += t0.elapsed();
    }
}

/// A mode's parameters for the signal card, as label / value pairs.
fn mode_details(m: &Mode) -> Vec<(String, String)> {
    let kv = |k: &str, v: String| (k.to_string(), v);
    let mut d = vec![
        kv("FFT size", m.fft.name().to_string()),
        kv(
            "guard",
            format!("{} ({:.0} µs)", m.guard.name(), m.guard_s() * 1e6),
        ),
        kv("bandwidth", format!("{} MHz", m.bandwidth.mhz())),
        kv("symbol", format!("{:.0} µs", m.symbol_s() * 1e6)),
        kv(
            "carriers",
            format!(
                "{} ({} data)",
                m.fft.active_carriers(),
                m.fft.data_carriers()
            ),
        ),
        kv("hierarchy", m.alpha.name().to_string()),
    ];
    match m.bitrate_lp() {
        Some(lp) => {
            d.push(kv(
                "HP rate",
                format!("{} · {:.2} Mbit/s", m.rate_hp.name(), m.bitrate_hp() / 1e6),
            ));
            d.push(kv(
                "LP rate",
                format!("{} · {:.2} Mbit/s", m.rate_lp.name(), lp / 1e6),
            ));
        }
        None => {
            d.push(kv(
                "bit rate",
                format!("{:.2} Mbit/s", m.bitrate_hp() / 1e6),
            ));
        }
    }
    d
}

/// "20261023-094512" (UTC) for file names, without a date library: the
/// civil-from-days conversion is Howard Hinnant's.
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_utc_stamp_is_well_formed() {
        let s = utc_stamp();
        assert_eq!(s.len(), 15, "{s}");
        assert_eq!(&s[8..9], "-");
        assert!(s[..4].parse::<u32>().unwrap() >= 2026);
    }
}
