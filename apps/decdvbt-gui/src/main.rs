// made by CasualArclamp
//! DecDVB-T desktop GUI.
//!
//! A HackRF (or an IQ file) in, the spectrum and waterfall over the UHF band,
//! tuning by channel, an IQ recorder and the cost of it all in the status
//! bar; the receiver's services list, transport stream analyser, programme
//! guide and player; and, at the bottom, the constellation scopes and
//! channel measurements (or the channel sounder's fading and delay–Doppler
//! panels).

mod band;
mod card;
mod channel_view;
mod epg_view;
mod external;
mod player;
mod prefs;
mod scopes;
mod services;
mod source;
mod ts_viewer;
mod waterfall;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use decdvbt_core::channel::{parse_channel, uhf_centre_hz, uhf_channel_at, uk_channels};
use decdvbt_core::dvbt::Mode;
use decdvbt_engine::{Command, CpuGauge, Outputs, Row, Stats, TsView, spectral_cn_db};
use decdvbt_io::name_hint;
use decdvbt_rx::receiver::ReceiverStatus;
use decdvbt_rx::sounder::Geometry;
use decdvbt_ts::{Epg, TsTap};
use eframe::egui::{self, Color32, RichText};
use source::{Gains, Session, Spec};
use waterfall::{History, RingImage};

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const RED: Color32 = Color32::from_rgb(255, 90, 80);
const GREEN: Color32 = Color32::from_rgb(110, 220, 130);

/// Height of the channel panels (fading waterfall, delay-Doppler map) and of
/// the scopes.
const CHANNEL_PANEL_H: f32 = 270.0;
const SCOPES_PANEL_H: f32 = 250.0;
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
    /// What the bottom panel starts as.
    bottom: Bottom,
    /// Start with the receiver decoding (on unless `--no-decode`).
    decode: bool,
    /// Open a window at the start (for screenshots): "ts" or "epg".
    window: Option<String>,
    /// Select this service at the start (its number).
    service: Option<u16>,
    /// Play this service as soon as the receiver has read its tables.
    play: Option<u16>,
    /// With `--play`, draw the service's first subtitle stream.
    subs: bool,
    /// Outputs on from the start: an HTTP server and a UDP target
    /// (`127.0.0.1:8001`), and a `.ts` recording.
    http: Option<String>,
    udp: Option<String>,
    record: bool,
}

/// What the bottom panel shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bottom {
    /// The constellation scopes and the channel measurements.
    Scopes,
    /// The sounder's fading waterfall and delay–Doppler map.
    Channel,
    Hidden,
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
            bottom: Bottom::Scopes,
            decode: true,
            window: None,
            service: None,
            play: None,
            subs: false,
            http: None,
            udp: None,
            record: false,
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
                "--no-channel" => o.bottom = Bottom::Hidden,
                "--bottom" => {
                    o.bottom = match value("scopes, channel or none")?.as_str() {
                        "scopes" => Bottom::Scopes,
                        "channel" => Bottom::Channel,
                        "none" => Bottom::Hidden,
                        v => return Err(format!("--bottom: not scopes, channel or none: {v}")),
                    };
                }
                "--window" => o.window = Some(value("ts or epg")?),
                "--http" => o.http = Some(value("an address, e.g. 127.0.0.1:8001")?),
                "--udp" => o.udp = Some(value("an address, e.g. 127.0.0.1:1234")?),
                "--record" => o.record = true,
                "--subs" => o.subs = true,
                "--play" => {
                    o.play = Some(value("a service id")?.parse().map_err(|_| "bad --play")?)
                }
                "--service" => {
                    o.service = Some(
                        value("a service id")?
                            .parse()
                            .map_err(|_| "bad --service")?,
                    )
                }
                "--decode" => o.decode = true,
                "--no-decode" => o.decode = false,
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
                 [--screenshot OUT.png] [--after SECS] [--fps N] [--assume 64qam-2/3-8k-1/32] \
                 [--bottom scopes|channel|none] [--window ts|epg] [--service ID] [--play ID] [--subs] \
                 [--http ADDR] [--udp ADDR] [--record] [--no-decode]"
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
    /// The bottom panel. The sounder runs only while it shows the channel
    /// panels, and the scopes and measurements are made only while it shows
    /// them.
    bottom: Bottom,
    chan_view: channel_view::ChannelView,
    scope: scopes::ScopeView,
    /// What was last sent for the scopes and measurements.
    scopes_sent: bool,
    /// The transport stream as the hub reports it (polled four times a
    /// second), the programme guide (once a second, while its window is
    /// open), and the service picked in the list.
    ts: TsView,
    ts_at: Instant,
    epg: Arc<Epg>,
    epg_at: Instant,
    selected: Option<u16>,
    ts_window: ts_viewer::TsViewer,
    epg_window: epg_view::EpgView,
    tv: player::TvWindow,
    /// The player's tap, and the outputs as last sent to the hub.
    tap: Option<TsTap>,
    outputs_ui: services::OutputsUi,
    outputs_sent: Option<Outputs>,
    /// VLC is to be opened on this service once the HTTP output is up.
    pending_vlc: Option<u16>,
    /// The RF frequency the sounder was last asked to look at (`Some(None)`:
    /// asked to stop); `None`: not asked since the source began.
    sounder_sent: Option<Option<f64>>,
    /// The receiver: on or off, the frequency it was last asked to decode
    /// (as for the sounder), what it reports, and the packets it has made
    /// with when they were counted, for the transport stream's rate.
    decode: bool,
    decoder_sent: Option<Option<f64>>,
    dec: ReceiverStatus,
    dec_packets: u64,
    dec_rate: Option<(u64, Instant, f64)>,
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
            bottom: opts.bottom,
            chan_view: channel_view::ChannelView::default(),
            scope: scopes::ScopeView::default(),
            scopes_sent: false,
            ts: TsView::default(),
            ts_at: Instant::now(),
            epg: Arc::new(Epg::new()),
            epg_at: Instant::now(),
            selected: opts.service,
            ts_window: ts_viewer::TsViewer::new(opts.window.as_deref() == Some("ts")),
            epg_window: epg_view::EpgView::new(opts.window.as_deref() == Some("epg")),
            tv: player::TvWindow::default(),
            tap: None,
            outputs_ui: {
                let mut o = services::OutputsUi::default();
                if let Some(a) = &opts.http {
                    (o.http_on, o.http) = (true, a.clone());
                }
                if let Some(a) = &opts.udp {
                    (o.udp_on, o.udp) = (true, a.clone());
                }
                o.record = opts.record;
                o
            },
            outputs_sent: None,
            pending_vlc: None,
            sounder_sent: None,
            decode: opts.decode,
            decoder_sent: None,
            dec: ReceiverStatus::default(),
            dec_packets: 0,
            dec_rate: None,
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
                self.sounder_sent = None;
                self.decoder_sent = None;
                self.dec = ReceiverStatus::default();
                self.dec_packets = 0;
                self.dec_rate = None;
                self.reset_stream();
                self.outputs_sent = None;
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
        self.chan_view.reset();
        self.scope.reset();
    }

    /// A different multiplex (or none): the services, the guide and the
    /// player's stream are no longer this one's.
    fn reset_stream(&mut self) {
        self.ts = TsView::default();
        self.epg = Arc::new(Epg::new());
        self.selected = None;
        self.pending_vlc = None;
        self.tv.close();
        self.tap = None;
    }

    fn show_channel(&self) -> bool {
        self.bottom == Bottom::Channel
    }

    /// Ask the engine to run the sounder on the signal under the VFO while
    /// the channel panels are open, and to stop when they are not (it costs
    /// CPU only while it runs).
    fn sync_sounder(&mut self) {
        let Some(s) = &self.session else { return };
        let want = self
            .show_channel()
            .then(|| {
                self.vfo_hz
                    .or((self.centre_hz > 0.0).then_some(self.centre_hz))
            })
            .flatten();
        if self.sounder_sent != Some(want) && (want.is_some() || self.sounder_sent.is_some()) {
            s.rx.send(Command::Sounder(want));
            self.sounder_sent = Some(want);
            if want.is_none() {
                self.chan_view.reset();
            }
        }
    }

    /// Ask the engine to decode the signal under the VFO while the decode
    /// switch is on, and to stop when it is not.
    fn sync_decoder(&mut self) {
        let Some(s) = &self.session else { return };
        let want = self
            .decode
            .then(|| {
                self.vfo_hz
                    .or((self.centre_hz > 0.0).then_some(self.centre_hz))
            })
            .flatten();
        if self.decoder_sent != Some(want) && (want.is_some() || self.decoder_sent.is_some()) {
            s.rx.send(Command::Decode(want));
            self.decoder_sent = Some(want);
            if want.is_none() {
                self.dec = ReceiverStatus::default();
                self.dec_packets = 0;
                self.dec_rate = None;
                self.reset_stream();
            }
        }
    }

    /// Ask the engine for the scopes' cells and the measurements while the
    /// scopes are shown and the receiver is decoding, and to stop when not.
    fn sync_scopes(&mut self) {
        let Some(s) = &self.session else { return };
        let want = self.bottom == Bottom::Scopes && self.decode;
        if want != self.scopes_sent {
            s.rx.send(Command::Scopes(want));
            s.rx.send(Command::Measure(want));
            self.scopes_sent = want;
            if !want {
                self.scope.reset();
            }
        }
    }

    /// Tell the hub where the stream goes, when that has changed.
    fn sync_outputs(&mut self) {
        let Some(s) = &self.session else { return };
        let name = self
            .vfo_hz
            .and_then(uhf_channel_at)
            .map_or_else(|| format!("C{}", self.channel), |c| format!("C{c}"));
        let o = self
            .outputs_ui
            .outputs(&name, self.tap.clone(), self.pending_vlc.is_some());
        if self.outputs_sent.as_ref() != Some(&o) {
            s.rx.set_outputs(o.clone());
            self.outputs_sent = Some(o);
        }
    }

    /// The transport stream's view and the guide, from the hub.
    fn poll_stream(&mut self) {
        let Some(s) = &self.session else { return };
        if !self.decode {
            return;
        }
        if self.ts_at.elapsed() >= Duration::from_millis(250) {
            self.ts_at = Instant::now();
            self.ts = s.rx.ts_view();
        }
        if self.epg_window.open && self.epg_at.elapsed() >= Duration::from_secs(1) {
            self.epg_at = Instant::now();
            self.epg = s.rx.epg();
        }
        // A service asked for on the command line plays once its streams are
        // known.
        if let Some(sid) = self.opts.play
            && self
                .ts
                .report
                .programmes
                .iter()
                .any(|p| p.number == sid && !p.streams.is_empty())
        {
            self.opts.play = None;
            let subs = self
                .ts
                .report
                .programmes
                .iter()
                .find(|p| p.number == sid)
                .and_then(|p| player::subtitle_streams(p).next())
                .map(|e| e.pid)
                .filter(|_| self.opts.subs);
            self.play(sid, None, subs);
        }
        // VLC waits for the HTTP server to be up.
        if let Some(sid) = self.pending_vlc
            && let Some((addr, _)) = &self.ts.tcp
        {
            self.pending_vlc = None;
            if let Err(e) = external::vlc(&external::http_url(*addr), sid) {
                self.note = e;
            }
        }
    }

    /// Play service `sid` in the app, with this sound and these subtitles.
    fn play(&mut self, sid: u16, audio: Option<u16>, subs: Option<u16>) {
        let report = Arc::clone(&self.ts.report);
        let Some(p) = report.programmes.iter().find(|p| p.number == sid) else {
            return;
        };
        let (tap, rx) = TsTap::new(2048);
        self.tap = Some(tap.for_video(player::video_pid(p)));
        self.selected = Some(sid);
        self.tv.start(p, audio, subs, rx);
        self.sync_outputs();
    }

    fn open_vlc(&mut self, sid: u16) {
        self.selected = Some(sid);
        self.outputs_ui.http_on = true;
        self.pending_vlc = Some(sid);
        // If the server is already up, the next poll opens it.
        if let Some((addr, _)) = &self.ts.tcp {
            self.pending_vlc = None;
            if let Err(e) = external::vlc(&external::http_url(*addr), sid) {
                self.note = e;
            }
        }
    }

    fn do_action(&mut self, a: services::Action) {
        match a {
            services::Action::Play(sid) => self.play(sid, None, None),
            services::Action::Vlc(sid) => self.open_vlc(sid),
            services::Action::OpenTs => self.ts_window.open = true,
            services::Action::OpenEpg => {
                self.epg_window.open = true;
                self.epg_at = Instant::now() - Duration::from_secs(2);
            }
        }
    }

    /// The transport stream's rate, Mbit/s, from the packets counted over the
    /// last second or so.
    fn ts_mbps(&self) -> Option<f64> {
        self.dec_rate.map(|(_, _, r)| r).filter(|r| *r > 0.0)
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
                self.reset_stream();
                self.vfo_hz = Some(fc);
            }
            Some(s) if !s.live && !fits => {
                self.note = format!("C{n} is outside this capture");
            }
            _ => {
                self.vfo_hz = Some(fc);
                self.reset_stream();
            }
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
        if self.bottom == Bottom::Channel {
            self.chan_view.update(s.rx.take_sounder());
        }
        if self.decode {
            // The stream itself is the hub's; what is kept here is its rate,
            // the receiver's status and the scopes' cells.
            let u = s.rx.take_decoder();
            self.dec = u.status;
            self.dec_packets = u.packets;
            self.scope.update(u.scopes, u.measurements);
            let now = Instant::now();
            match &mut self.dec_rate {
                Some((at, t, rate)) if t.elapsed() >= Duration::from_secs(1) => {
                    *rate = (u.packets.saturating_sub(*at)) as f64 * 188.0 * 8.0
                        / t.elapsed().as_secs_f64()
                        / 1e6;
                    *at = u.packets;
                    *t = now;
                }
                None => self.dec_rate = Some((u.packets, now, 0.0)),
                _ => {}
            }
        }
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
        // The player's pictures set a floor under the display rate.
        let fps = self.opts.fps.max(self.tv.wants_fps().unwrap_or(0.0));
        let target = 1000.0 / fps.clamp(1.0, 60.0);
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
        // What the receiver has read from the TPS wins, then what the sounder
        // has read of the signal (the FFT size and the guard interval), then
        // an assumption.
        let locked: Option<Geometry> = self
            .chan_view
            .status
            .geometry
            .filter(|_| self.chan_view.status.tracking);
        let tps = self.dec.tps.filter(|_| self.dec.tps_synced && self.decode);
        let read: Option<Mode> = tps.map(|t| {
            let mut m = Mode::uk(t.modulation, t.rate_hp);
            m.fft = t.fft;
            m.guard = t.guard;
            m.rate_lp = t.rate_lp;
            m.alpha = t.alpha;
            m
        });
        let mut mode = read.or(self.opts.assume);
        if read.is_none()
            && let (Some(m), Some(g)) = (&mut mode, locked)
        {
            m.fft = g.fft;
            m.guard = g.guard;
        }
        let mut details = match (mode, locked) {
            (Some(m), _) => mode_details(&m),
            (None, Some(g)) => lock_details(&g, self.chan_view.status.cfo_hz),
            _ => Vec::new(),
        };
        let decoding = self.decode && self.dec.decoding;
        if decoding {
            let kv = |k: &str, v: String| (k.to_string(), v);
            let o = &self.dec.outer;
            if let Some(r) = self.ts_mbps() {
                details.push(kv("stream", format!("{r:.2} Mbit/s")));
            }
            details.push(kv(
                "RS blocks",
                format!(
                    "{} fixed · {} lost",
                    o.corrected + o.uncorrectable,
                    o.uncorrectable
                ),
            ));
            details.push(kv("before RS", format!("{:.1e} BER", o.pre_rs_ber())));
        }
        // The pilots' signal-to-noise ratio replaces the spectrum's C/N once
        // decoding: it is the figure the code sees.
        let (cn_db, cn_source) = if decoding && self.dec.snr_db.is_finite() {
            (Some(self.dec.snr_db), "MER")
        } else {
            (self.cn_db, "spectrum")
        };
        card::Card {
            standard: (mode.is_some() || locked.is_some()).then_some(card::Standard::DvbT),
            assumed: read.is_none() && mode.is_some(),
            name: mode.map(|m| format!("{} {}", m.modulation.name(), m.rate_hp.name())),
            qef_db: mode.and_then(|m| m.qef_cn_db()),
            details,
            cn_db,
            cn_source,
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
            ui.checkbox(&mut self.decode, "decode");
            ui.label("panel");
            ui.selectable_value(&mut self.bottom, Bottom::Scopes, "scopes");
            ui.selectable_value(&mut self.bottom, Bottom::Channel, "channel");
            ui.selectable_value(&mut self.bottom, Bottom::Hidden, "none");
            if self.show_channel() {
                egui::ComboBox::from_id_salt("doppler")
                    .selected_text(format!("±{} Hz", self.chan_view.doppler_view))
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        for d in channel_view::DOPPLER_VIEWS {
                            ui.selectable_value(
                                &mut self.chan_view.doppler_view,
                                d,
                                format!("±{d} Hz"),
                            );
                        }
                    });
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
                        if (*stage == decdvbt_engine::Stage::Record && s.recording.is_none())
                            || (*stage == decdvbt_engine::Stage::Sounder && !self.show_channel())
                            || (*stage == decdvbt_engine::Stage::Decoder && !self.decode)
                        {
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
                    if self.decode && self.ts.packets > 0 {
                        ui.label(RichText::new("  transport stream").weak());
                        ui.label(format!("{:.1} %", self.ts.load_percent));
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
        ui.horizontal(|ui| {
            ui.label(RichText::new("Services").strong());
            if self.ts.packets > 0 {
                ui.label(
                    RichText::new(format!(
                        "{} · {:.2} Mbit/s",
                        self.ts.report.programmes.len(),
                        self.ts.rate_bps / 1e6
                    ))
                    .weak(),
                );
            }
        });
        ui.separator();
        let mut action = services::list(ui, &self.ts, &mut self.selected);
        ui.add_space(4.0);
        action = services::buttons(ui, self.selected).or(action);
        ui.add_space(4.0);
        egui::CollapsingHeader::new("Outputs")
            .default_open(false)
            .show(ui, |ui| {
                self.outputs_ui.show(ui, &self.ts);
            });
        if let Some(a) = action {
            self.do_action(a);
        }
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
        self.sync_sounder();
        self.sync_decoder();
        self.sync_scopes();
        self.poll_stream();
        self.sync_outputs();
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
        match self.bottom {
            Bottom::Channel => {
                egui::Panel::bottom("channel")
                    .resizable(false)
                    .show(ui, |ui| {
                        // The panel is as tall as its content asks.
                        ui.set_min_height(CHANNEL_PANEL_H);
                        self.chan_view.show(ui);
                    });
            }
            Bottom::Scopes => {
                egui::Panel::bottom("scopes")
                    .resizable(false)
                    .show(ui, |ui| {
                        ui.set_min_height(SCOPES_PANEL_H);
                        self.scope.show(ui, &self.dec);
                    });
            }
            Bottom::Hidden => {}
        }
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
        let ctx = ui.ctx().clone();
        self.ts_window.show(&ctx, &self.ts);
        self.epg_window.show(&ctx, &self.epg, &self.ts.report);
        let report = Arc::clone(&self.ts.report);
        let list = services::ordered(&report);
        match self.tv.show(&ctx, &list, self.ts.tap_dropped) {
            Some(player::Request::Play(sid, audio, subs)) => self.play(sid, audio, subs),
            Some(player::Request::Close) => {
                self.tap = None;
            }
            None => {}
        }
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

/// What the sounder knows of a signal it has locked to, for the signal card:
/// the FFT size and the guard interval, and what follows from them.
fn lock_details(g: &Geometry, cfo_hz: f64) -> Vec<(String, String)> {
    let kv = |k: &str, v: String| (k.to_string(), v);
    let useful_us = g.fft.n() as f64 / (g.carrier_spacing_hz * g.fft.n() as f64) * 1e6;
    vec![
        kv("FFT size", g.fft.name().to_string()),
        kv(
            "guard",
            format!("{} ({:.0} µs)", g.guard.name(), g.guard_us),
        ),
        kv("symbol", format!("{:.0} µs", g.symbol_s * 1e6)),
        kv("useful", format!("{useful_us:.0} µs")),
        kv(
            "carriers",
            format!(
                "{} ({} data)",
                g.fft.active_carriers(),
                g.fft.data_carriers()
            ),
        ),
        kv("carrier offset", format!("{:+.2} kHz", cfo_hz / 1e3)),
    ]
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
