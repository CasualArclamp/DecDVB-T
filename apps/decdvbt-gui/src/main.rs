// made by CasualArclamp
// A release exe is a Windows GUI program: no console window opens with it
// (`attach_console` joins the one that started it, if there is one). Debug
// builds keep theirs, for the log.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
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
mod errormaps;
mod external;
mod freq_display;
mod heatmaps;
mod player;
mod prefs;
mod redbutton;
mod remember;
mod scopes;
mod sdr;
mod services;
mod source;
mod t2card;
mod ts_viewer;
mod video;
mod waterfall;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use decdvbt_core::channel::{parse_channel, uhf_centre_hz, uhf_channel_at, uk_channels};
use decdvbt_core::dvbt::{Bandwidth, Mode};
use decdvbt_engine::{
    Command, CpuGauge, FrontEndSettings, Outputs, Row, Stats, TsView, signal_block, spectral_cn_db,
};
use decdvbt_io::name_hint;
use decdvbt_rx::auto::Standard;
use decdvbt_rx::receiver::ReceiverStatus;
use decdvbt_rx::sounder::Geometry;
use decdvbt_rx::t2::receiver::T2Status;
use decdvbt_ts::{Epg, TsTap};
use eframe::egui::{self, Color32, RichText};
use source::{AirGains, Gains, Preset, Session, Spec};
use waterfall::{History, RingImage};

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const RED: Color32 = Color32::from_rgb(255, 90, 80);
const GREEN: Color32 = Color32::from_rgb(110, 220, 130);

/// Height of the channel panels (fading waterfall, delay-Doppler map) and of
/// the scopes.
const CHANNEL_PANEL_H: f32 = 270.0;
const SCOPES_PANEL_H: f32 = 250.0;
/// The display rate from which the heat maps take 60 rows a second (20
/// below it).
const SMOOTH_FPS: f64 = 45.0;
/// The channel plan's transmitter offsets, Hz: a carrier found within
/// [`SNAP_HZ`] of one is taken to be on it.
const PLAN_OFFSETS_HZ: [f64; 5] = [-333_333.0, -166_667.0, 0.0, 166_667.0, 333_333.0];
const SNAP_HZ: f64 = 40e3;
/// The display rates the list offers.
const FPS_CHOICES: [f64; 6] = [5.0, 10.0, 15.0, 20.0, 30.0, 60.0];
/// HackRF sample rates with the least clock jitter that hold a channel.
const RATES_MSPS: [f64; 4] = [10.0, 12.5, 16.0, 20.0];
/// The same for the narrow channels (1 MHz DVB-T, 1.7 MHz DVB-T2): the
/// radio's lowest, which holds them, and twice it.
const NARROW_RATES_MSPS: [f64; 2] = [2.0, 4.0];
/// Half an 8 MHz channel plus a margin: a channel is received without
/// retuning only if all of this (scaled to its width) is inside the span.
const CHANNEL_HALF_NEEDED_HZ: f64 = 4.1e6;

/// The rates the radio is offered for a channel `bw` wide.
fn rates_for(bw: Bandwidth) -> &'static [f64] {
    if bw.narrow() {
        &NARROW_RATES_MSPS
    } else {
        &RATES_MSPS
    }
}

struct Options {
    file: Option<PathBuf>,
    channel: Option<u8>,
    /// `--freq MHZ`: a free frequency instead of a channel.
    freq_hz: Option<f64>,
    /// `--bandwidth`: the channel's width (nothing in the signal says it).
    bandwidth: Option<Bandwidth>,
    rate_msps: Option<f64>,
    screenshot: Option<PathBuf>,
    after: f64,
    /// With `--screenshot`: record the window from `after` on for
    /// `video_secs` seconds to this file (see [`video`]).
    video: Option<PathBuf>,
    video_secs: f64,
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
    /// Run the gain optimiser once the receiver has locked (a test aid).
    optimise: bool,
    /// The radio to use (`--radio hackrf|airspy`), else the one last used.
    radio: Option<SourceKind>,
    /// Which view settings the command line gave: what it did not is what
    /// was kept from the last run (but a screenshot, which has to come out
    /// the same each time, keeps nothing).
    bottom_given: bool,
    fps_given: bool,
    decode_given: bool,
    /// What was kept from the last run.
    kept: remember::Saved,
}

/// What the bottom panel shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bottom {
    /// The constellation scopes and the channel measurements.
    Scopes,
    /// The sounder's fading waterfall and delay–Doppler map.
    Channel,
    /// The channel over time: gain and SNR across the band, and the echoes,
    /// as waterfalls.
    Heat,
    /// Maps with no time axis: the paths by delay and Doppler, the fading by
    /// carrier and Doppler, and the data cells' error as a cloud and per
    /// constellation point.
    Maps,
    Hidden,
}

impl Bottom {
    fn name(self) -> &'static str {
        match self {
            Bottom::Scopes => "scopes",
            Bottom::Channel => "channel",
            Bottom::Maps => "maps",
            Bottom::Heat => "history",
            Bottom::Hidden => "none",
        }
    }

    fn parse(s: &str) -> Option<Bottom> {
        Some(match s {
            "scopes" => Bottom::Scopes,
            "channel" => Bottom::Channel,
            "maps" => Bottom::Maps,
            "history" | "heat" => Bottom::Heat,
            "none" => Bottom::Hidden,
            _ => return None,
        })
    }
}

impl Options {
    fn from_args() -> Result<Options, String> {
        let mut o = Options {
            file: None,
            channel: None,
            freq_hz: None,
            bandwidth: None,
            rate_msps: None,
            screenshot: None,
            after: 4.0,
            video: None,
            video_secs: 6.0,
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
            optimise: false,
            radio: None,
            bottom_given: false,
            fps_given: false,
            decode_given: false,
            kept: remember::Saved::default(),
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
                "--freq" => {
                    let f: f64 = value("MHz")?.parse().map_err(|_| "bad --freq")?;
                    o.freq_hz = Some(f * 1e6);
                }
                "--bandwidth" => {
                    let v = value("MHz: 1, 1.7, 6, 7 or 8")?;
                    o.bandwidth =
                        Some(Bandwidth::parse(&v).ok_or(format!("not a bandwidth: {v}"))?);
                }
                "--screenshot" => o.screenshot = Some(value("a .png path")?.into()),
                "--assume" => {
                    let v = value("a mode, e.g. 64qam-2/3-8k-1/32")?;
                    o.assume = Some(Mode::parse(&v)?);
                }
                "--no-channel" => {
                    o.bottom = Bottom::Hidden;
                    o.bottom_given = true;
                }
                "--bottom" => {
                    let v = value("scopes, channel, maps, history or none")?;
                    o.bottom = Bottom::parse(&v).ok_or(format!(
                        "--bottom: not scopes, channel, maps, history or none: {v}"
                    ))?;
                    o.bottom_given = true;
                }
                "--window" => o.window = Some(value("ts, epg or sdr")?),
                "--http" => o.http = Some(value("an address, e.g. 127.0.0.1:8001")?),
                "--udp" => o.udp = Some(value("an address, e.g. 127.0.0.1:1234")?),
                "--record" => o.record = true,
                "--optimise" => o.optimise = true,
                "--radio" => {
                    o.radio = Some(match value("hackrf or airspy")?.as_str() {
                        "hackrf" => SourceKind::HackRf,
                        "airspy" => SourceKind::Airspy,
                        v => return Err(format!("--radio is hackrf or airspy, not {v}")),
                    });
                }
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
                "--decode" | "--no-decode" => {
                    o.decode = a == "--decode";
                    o.decode_given = true;
                }
                "--fps" => {
                    o.fps = value("frames a second")?.parse().map_err(|_| "bad --fps")?;
                    o.fps_given = true;
                }
                "--after" => o.after = value("seconds")?.parse().map_err(|_| "bad --after")?,
                "--video" => o.video = Some(value("a video file")?.into()),
                "--video-secs" => {
                    o.video_secs = value("seconds")?.parse().map_err(|_| "bad --video-secs")?;
                }
                f if !f.starts_with("--") => o.file = Some(f.into()),
                _ => return Err(format!("unknown option {a}")),
            }
        }
        let kept = remember::Saved::load();
        // The view as it was left, where the command line said nothing; a
        // screenshot, which has to come out the same each time, keeps none.
        if o.screenshot.is_none() {
            if !o.bottom_given
                && let Some(b) = kept.bottom.as_deref().and_then(Bottom::parse)
            {
                o.bottom = b;
            }
            if !o.fps_given
                && let Some(f) = kept.fps.filter(|f| FPS_CHOICES.contains(f))
            {
                o.fps = f;
            }
            if !o.decode_given
                && let Some(d) = kept.decode
            {
                o.decode = d;
            }
        }
        o.kept = kept;
        Ok(o)
    }
}

/// A program built for the Windows GUI subsystem starts with no console and
/// its output goes nowhere. Started from a terminal, join that terminal's
/// console (so `--version`, usage errors and the `--screenshot` report print
/// as they did) and point the standard handles at it unless they were
/// redirected; started from Explorer there is none to join and nothing happens.
#[cfg(all(windows, not(debug_assertions)))]
fn attach_console() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        SetStdHandle,
    };
    // SAFETY: plain Win32 calls with valid arguments; the handles given to
    // `SetStdHandle` are leaked on purpose, as the standard handles are for
    // the life of the process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(which);
            // No handle (-1 is INVALID_HANDLE_VALUE): not redirected, so
            // the console.
            if (h.is_null() || h as isize == -1)
                && let Ok(f) = std::fs::OpenOptions::new().write(true).open("CONOUT$")
            {
                SetStdHandle(which, f.into_raw_handle() as _);
            }
        }
    }
}

#[cfg(not(all(windows, not(debug_assertions))))]
fn attach_console() {}

/// The window's icon (the same picture as the exe's), from the PNG built in.
fn window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon-256.png")).ok()?;
    let rgba = img.to_rgba8();
    Some(egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// The offsets learned for the channels, from the prefs.
fn load_offsets() -> HashMap<u8, f64> {
    (21..=69u8)
        .filter_map(|c| {
            let o: f64 = prefs::get(&format!("offset_C{c}"))?.parse().ok()?;
            Some((c, o))
        })
        .collect()
}

fn main() -> eframe::Result {
    attach_console();
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
                "usage: decdvbt-gui [capture.cs8] [--channel C26 | --freq MHZ] \
                 [--bandwidth 8|7|6|1.7|1] [--rate MSPS] \
                 [--screenshot OUT.png] [--after SECS] [--fps N] [--assume 64qam-2/3-8k-1/32] \
                 [--bottom scopes|channel|heat|none] [--window ts|epg|sdr] [--service ID] [--play ID] [--subs] \
                 [--http ADDR] [--udp ADDR] [--record] [--no-decode]"
            );
            std::process::exit(2);
        }
    };
    let (w, h) = opts
        .kept
        .window
        .filter(|_| opts.screenshot.is_none())
        .unwrap_or((1400, 860));
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([w as f32, h as f32])
        .with_min_inner_size([900.0, 560.0])
        .with_title("DecDVB-T")
        .with_drag_and_drop(true);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
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
    /// The Airspy R2 (10 MS/s only; its own gain stages).
    Airspy,
    File,
}

impl SourceKind {
    /// A radio, as against a capture file.
    fn live(self) -> bool {
        self != SourceKind::File
    }
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
    /// Which standard the receiver has locked on, and with a T2 lock that
    /// receiver's own status (`dec` then holds the common figures).
    standard: Standard,
    dec2: Option<Box<T2Status>>,
    dec_packets: u64,
    dec_rate: Option<(u64, Instant, f64)>,
    started: Instant,
    shot_sent: bool,
    /// The window's recording, while `--video` runs, and whether it has
    /// been made (a frame drawn after it must not start another).
    video: Option<video::VideoTap>,
    video_done: bool,
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
    /// The channel's width (1 and 1.7 MHz are the narrow ones, off the UHF
    /// raster), and what the engine was last told of it.
    bandwidth: Bandwidth,
    bandwidth_sent: Option<Bandwidth>,
    /// The channel being received (the VFO), by number.
    channel: u8,
    channel_text: String,
    rate_msps: f64,
    gains: Gains,
    /// The Airspy's stages, the libairspy preset they came from (`None`: set
    /// by hand) and whether it uses the packed transfers.
    air: AirGains,
    air_preset: Option<Preset>,
    air_packed: bool,
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

    /// The frequency the radio is tuned to (before a source runs: to be
    /// tuned to), the dial's value while it is being edited, and when the
    /// last edit was (the radio follows once it has stopped).
    tune_hz: f64,
    dial_hz: f64,
    dial_edit: Option<Instant>,
    /// How far each channel's carrier was found from its place in the plan,
    /// Hz; when the tuning was last changed and whether the carrier has been
    /// looked for since.
    offsets: HashMap<u8, f64>,
    /// The same for the Airspy, kept for this run only: the learned offsets
    /// in the prefs include the HackRF's own clock error, which the Airspy
    /// does not share.
    air_offsets: HashMap<u8, f64>,
    tuned_at: Instant,
    centre_checked: bool,
    centre_tries: u8,
    /// The SDR settings, what was last sent to the radio and the engine, the
    /// window, the gain optimiser (while it runs) and what it last found.
    sdr: sdr::SdrSettings,
    sdr_sent: Option<sdr::SdrSettings>,
    sdr_open: bool,
    optimiser: Option<sdr::Optimiser>,
    opt_result: Option<String>,
    /// The display rate the engine was last told (it makes a waterfall row
    /// a frame, and paces what it hands over by it).
    fps_sent: Option<f64>,
    /// The waterfall's height on the screen, physical pixels: its rows, one
    /// a pixel, so it moves a whole pixel a frame and the frame rate is its
    /// speed (from DecDAB, same author).
    wf_rows: usize,
    /// Frames that put 0, 1 and 2 rows on the waterfall (for the screenshot
    /// report: an even scroll is all 1s), and the rows come but not yet
    /// shown: one goes on each frame.
    wf_steps: [u32; 3],
    rows_due: std::collections::VecDeque<Row>,
    heat: heatmaps::HeatView,
    errors: errormaps::ErrorMaps,
    heat_sent: Option<f64>,
    /// What is kept from run to run (none for a screenshot), the radio's
    /// channel and frequency as they were when the source last was the
    /// radio, and the window's size.
    keeper: Option<remember::Keeper>,
    radio: (u8, f64),
    window_size: (u32, u32),
}

impl App {
    fn new(mut opts: Options) -> App {
        let kept = std::mem::take(&mut opts.kept);
        // Where the radio was left, unless the command line says where to go.
        let by_hand = opts.channel.is_some() || opts.file.is_some() || opts.freq_hz.is_some();
        let bandwidth = opts.bandwidth.or(kept.bandwidth).unwrap_or(Bandwidth::Mhz8);
        let mut channel = opts.channel.or(kept.channel).unwrap_or(26);
        let sdr = sdr::SdrSettings::load();
        let offsets = load_offsets();
        let mut tune_hz = uhf_centre_hz(channel).map_or(0.0, |c| {
            c + if sdr.auto_centre {
                offsets.get(&channel).copied().unwrap_or(0.0)
            } else {
                0.0
            }
        });
        if !by_hand && let Some(h) = kept.tune_hz {
            tune_hz = h;
            channel = uhf_channel_at(h).unwrap_or(channel);
        }
        if let Some(f) = opts.freq_hz {
            tune_hz = f;
            channel = uhf_channel_at(f).unwrap_or(channel);
        }
        // The capture last opened (offered again, not opened).
        let file = kept.file.clone().filter(|f| f.exists());
        let kind = if !by_hand && kept.file_source && file.is_some() {
            SourceKind::File
        } else if let Some(k) = opts.radio {
            k
        } else if kept.airspy {
            SourceKind::Airspy
        } else {
            SourceKind::HackRf
        };
        let mut app = App {
            started: Instant::now(),
            shot_sent: false,
            video: None,
            video_done: false,
            cpu_mark: None,
            cpu_avg: None,
            frame_cost: [Duration::ZERO; 2],
            frames: 0,
            last_frame: None,
            delay_ms: 1000.0 / 15.0,
            session: None,
            kind,
            bandwidth,
            bandwidth_sent: None,
            channel,
            channel_text: format!("C{channel}"),
            rate_msps: opts
                .rate_msps
                .or(kept.rate_msps.filter(|r| rates_for(bandwidth).contains(r)))
                .unwrap_or(rates_for(bandwidth)[0]),
            gains: kept.gains.unwrap_or_default(),
            air: kept.air.unwrap_or_default(),
            // Old prefs have no preset: the default gains are its.
            air_preset: if kept.air.is_some() {
                kept.air_preset
            } else {
                Some(Preset::DEFAULT)
            },
            air_packed: kept.air_packed.unwrap_or(false),
            file,
            file_centre_mhz: kept.file_centre_mhz.unwrap_or(0.0),
            file_rate_msps: kept.file_rate_msps.unwrap_or(10.0),
            centre_hz: 0.0,
            span_hz: 0.0,
            vfo_hz: Some(tune_hz).filter(|h| *h > 0.0),
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
            epg_window: {
                let mut v = epg_view::EpgView::new(opts.window.as_deref() == Some("epg"));
                if let Some(sid) = opts.service {
                    v.select_now(sid);
                }
                v
            },
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
            standard: Standard::Searching,
            dec2: None,
            dec_packets: 0,
            dec_rate: None,
            stats: Stats::default(),
            stats_at: Instant::now(),
            cpu: CpuGauge::default(),
            cpu_percent: 0.0,
            note: String::new(),
            tune_hz,
            dial_hz: tune_hz,
            dial_edit: None,
            offsets,
            air_offsets: HashMap::new(),
            tuned_at: Instant::now(),
            centre_checked: false,
            centre_tries: 0,
            sdr,
            sdr_sent: None,
            sdr_open: opts.window.as_deref() == Some("sdr"),
            optimiser: None,
            opt_result: None,
            fps_sent: None,
            wf_rows: 0,
            wf_steps: [0; 3],
            rows_due: std::collections::VecDeque::new(),
            heat: heatmaps::HeatView::default(),
            errors: errormaps::ErrorMaps::default(),
            heat_sent: None,
            keeper: None,
            radio: (channel, tune_hz),
            window_size: kept.window.unwrap_or((1400, 860)),
            opts,
        };
        if app.opts.screenshot.is_none()
            && let Some(d) = kept
                .doppler_hz
                .filter(|d| channel_view::DOPPLER_VIEWS.contains(d))
        {
            app.chan_view.doppler_view = d;
        }
        // Started from the command line: begin at once.
        if let Some(f) = app.opts.file.clone() {
            app.open_file(f);
        } else if app.opts.channel.is_some() || app.opts.freq_hz.is_some() {
            app.start();
        }
        // From here what changes is kept (not by a screenshot: a test run
        // must leave the next real one as it found it).
        if app.opts.screenshot.is_none() {
            app.keeper = Some(remember::Keeper::new());
        }
        app
    }

    /// What is kept from run to run, as the app is now.
    fn snapshot(&self) -> remember::Snapshot {
        let (channel, tune_hz) = if self.kind.live() {
            (self.channel, self.tune_hz)
        } else {
            self.radio
        };
        remember::Snapshot {
            channel,
            tune_hz: tune_hz.round() as i64,
            file_source: self.kind == SourceKind::File,
            airspy: self.kind == SourceKind::Airspy,
            rate_khz: (self.rate_msps * 1e3).round() as i64,
            gains: self.gains,
            air: self.air,
            air_preset: self.air_preset,
            air_packed: self.air_packed,
            file: self
                .file
                .as_ref()
                .map_or(String::new(), |p| p.display().to_string()),
            file_centre_hz: (self.file_centre_mhz * 1e6).round() as i64,
            file_rate_khz: (self.file_rate_msps * 1e3).round() as i64,
            decode: self.decode,
            bottom: self.bottom.name().to_string(),
            fps: self.opts.fps as u32,
            doppler_hz: self.chan_view.doppler_view as u32,
            window: self.window_size,
            bandwidth: self.bandwidth.token().to_string(),
        }
    }

    /// Once a frame: note what is to be kept (written when it has settled).
    fn remember(&mut self, ctx: &egui::Context) {
        if self.keeper.is_none() {
            return;
        }
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            self.window_size = (r.width().round() as u32, r.height().round() as u32);
        }
        if self.kind.live() {
            self.radio = (self.channel, self.tune_hz);
        }
        let now = self.snapshot();
        if let Some(k) = self.keeper.as_mut()
            && let Some(after) = k.look(now, Instant::now())
        {
            ctx.request_repaint_after(after);
        }
    }

    // ---- sources --------------------------------------------------------

    fn start(&mut self) {
        self.stop();
        match self.kind {
            SourceKind::HackRf => {
                let fc = self.tune_hz;
                if fc <= 0.0 {
                    return;
                }
                let spec = Spec::HackRf {
                    center_hz: fc,
                    rate_hz: self.rate_msps * 1e6,
                    gains: self.gains,
                };
                self.begin(spec, Some(fc));
            }
            SourceKind::Airspy => {
                let fc = self.tune_hz;
                if fc <= 0.0 {
                    return;
                }
                let spec = Spec::Airspy {
                    center_hz: fc,
                    gains: self.air,
                    packed: self.air_packed,
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
                // A capture's signal is in its channel; a narrow one is
                // taken at the capture's centre (it is off the UHF raster).
                let vfo = uhf_channel_at(self.file_centre_mhz * 1e6)
                    .filter(|_| !self.bandwidth.narrow())
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
                self.bandwidth_sent = None;
                self.sounder_sent = None;
                self.decoder_sent = None;
                self.dec = ReceiverStatus::default();
                (self.standard, self.dec2) = (Standard::Searching, None);
                self.dec_packets = 0;
                self.dec_rate = None;
                self.reset_stream();
                self.outputs_sent = None;
                self.vfo_hz = vfo;
                self.reset_view();
                self.note.clear();
                // A new engine knows nothing of what the last was told.
                (
                    self.scopes_sent,
                    self.sdr_sent,
                    self.fps_sent,
                    self.heat_sent,
                ) = (false, None, None, None);
                self.optimiser = None;
                self.tuned_at = Instant::now();
                self.centre_checked = false;
                self.centre_tries = 0;
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
        self.rows_due.clear();
        self.smooth.clear();
        self.span_hz = 0.0;
        self.chan_view.reset();
        self.scope.reset();
        self.heat.reset();
        self.errors.reset();
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

    /// The sounder runs for the panels that show its estimates.
    fn show_channel(&self) -> bool {
        matches!(self.bottom, Bottom::Channel | Bottom::Maps)
    }

    /// Ask the engine to run the sounder on the signal under the VFO while
    /// the channel panels are open, and to stop when they are not (it costs
    /// CPU only while it runs).
    fn sync_sounder(&mut self) {
        let Some(s) = &self.session else { return };
        // On a DVB-T2 lock the engine sounds the channel from the T2
        // receiver's own estimates (the decoder has to be on for that).
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

    /// Tell the engine the channel's width when it changes (it restarts the
    /// receiver and the sounder for it).
    fn sync_bandwidth(&mut self) {
        let Some(s) = &self.session else { return };
        if self.bandwidth_sent != Some(self.bandwidth) {
            s.rx.send(Command::Bandwidth(self.bandwidth));
            self.bandwidth_sent = Some(self.bandwidth);
        }
    }

    /// A new width: the receiver starts again, the radio's rate follows (2
    /// MS/s for the narrow channels, 10 for the others) and the services go.
    fn set_bandwidth(&mut self, bw: Bandwidth) {
        if bw == self.bandwidth {
            return;
        }
        self.bandwidth = bw;
        self.reset_stream();
        self.dec = ReceiverStatus::default();
        (self.standard, self.dec2) = (Standard::Searching, None);
        let rates = rates_for(bw);
        if !rates.contains(&self.rate_msps) {
            self.rate_msps = rates[0];
            if self.session.is_some() && self.kind == SourceKind::HackRf {
                self.start();
            }
        }
    }

    /// Half the band a channel of this width needs inside the span, Hz.
    fn channel_half_needed_hz(&self) -> f64 {
        CHANNEL_HALF_NEEDED_HZ * self.bandwidth.scale()
    }

    /// The name outputs and recordings give the channel: "C26", or the
    /// frequency of a narrow one ("433.250MHz").
    fn channel_name(&self) -> String {
        match self.vfo_hz {
            Some(v) if self.bandwidth.narrow() => format!("{:.3}MHz", v / 1e6),
            v => v
                .and_then(uhf_channel_at)
                .map_or_else(|| format!("C{}", self.channel), |c| format!("C{c}")),
        }
    }

    /// The UHF channel of the VFO for the card (none for a narrow signal).
    fn card_channel(&self) -> Option<u8> {
        self.vfo_hz
            .and_then(uhf_channel_at)
            .filter(|_| !self.bandwidth.narrow())
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
                (self.standard, self.dec2) = (Standard::Searching, None);
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
        let want = matches!(self.bottom, Bottom::Scopes | Bottom::Maps) && self.decode;
        if want != self.scopes_sent {
            s.rx.send(Command::Scopes(want));
            s.rx.send(Command::Measure(want));
            self.scopes_sent = want;
            if !want {
                self.scope.reset();
                self.errors.reset();
            }
        }
    }

    /// Tell the hub where the stream goes, when that has changed.
    fn sync_outputs(&mut self) {
        let Some(s) = &self.session else { return };
        let name = self.channel_name();
        let mut o = self
            .outputs_ui
            .outputs(&name, self.tap.clone(), self.pending_vlc.is_some());
        o.carousel = self.tv.red.link();
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
        let sel = player::selection(p, audio, subs);
        if sel.video.is_none() && sel.audio.is_none() {
            self.note = format!(
                "{} has no picture or sound on air right now",
                p.name.as_deref().unwrap_or("that service")
            );
            return;
        }
        self.note.clear();
        let (tap, rx) = TsTap::new(2048);
        self.tap = Some(tap.for_video(player::video_pid(p)));
        self.selected = Some(sid);
        self.tv.start(p, audio, subs, rx);
        self.sync_outputs();
    }

    /// A service picked in the list or the guide: play it, unless it is
    /// already playing (the window is brought up instead).
    fn watch(&mut self, sid: u16) {
        self.selected = Some(sid);
        if self.tv.is_playing(sid) {
            self.tv.open = true;
        } else {
            self.play(sid, None, None);
        }
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
            services::Action::Play(sid) => self.watch(sid),
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
        let Some(fc) = self.channel_hz(n) else { return };
        self.cancel_optimiser();
        self.channel = n;
        self.channel_text = format!("C{n}");
        self.tune_hz = fc;
        self.tuned_at = Instant::now();
        self.centre_checked = false;
        self.centre_tries = 0;
        let fits = self.span_hz > 0.0
            && (fc - self.centre_hz).abs() + self.channel_half_needed_hz() <= self.span_hz / 2.0;
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

    /// The learned carrier offsets of the radio in use.
    fn cur_offsets(&self) -> &HashMap<u8, f64> {
        if self.kind == SourceKind::Airspy {
            &self.air_offsets
        } else {
            &self.offsets
        }
    }

    /// Where channel `n` is tuned: its place in the plan, and, with centring
    /// on, the offset found for it.
    fn channel_hz(&self, n: u8) -> Option<f64> {
        let nominal = uhf_centre_hz(n)?;
        let off = if self.sdr.auto_centre {
            self.cur_offsets().get(&n).copied().unwrap_or(0.0)
        } else {
            0.0
        };
        Some(nominal + off)
    }

    /// Tune to `hz`: retune a live radio, move the VFO within a capture.
    fn set_frequency(&mut self, hz: f64) {
        self.cancel_optimiser();
        self.tune_hz = hz;
        self.tuned_at = Instant::now();
        self.centre_checked = true;
        if let Some(ch) = uhf_channel_at(hz) {
            self.channel = ch;
            self.channel_text = format!("C{ch}");
        }
        match &self.session {
            Some(s) if s.live => {
                s.rx.send(Command::Retune(hz));
                self.centre_hz = hz;
                self.reset_view();
                self.reset_stream();
                self.vfo_hz = Some(hz);
            }
            Some(_) => {
                let fits = self.span_hz > 0.0
                    && (hz - self.centre_hz).abs() + self.channel_half_needed_hz()
                        <= self.span_hz / 2.0;
                if fits {
                    self.vfo_hz = Some(hz);
                    self.reset_stream();
                } else {
                    self.note = format!("{:.3} MHz is outside this capture", hz / 1e6);
                }
            }
            None => self.vfo_hz = Some(hz),
        }
    }

    /// The big frequency digits: scroll or click a digit to tune. The radio
    /// follows when the editing has paused.
    fn freq_dial(&mut self, ui: &mut egui::Ui) {
        if self.dial_edit.is_none() {
            self.dial_hz = self.vfo_hz.unwrap_or(self.tune_hz);
        }
        if freq_display::show(ui, &mut self.dial_hz, 1e6, 6e9, 34.0) {
            self.dial_edit = Some(Instant::now());
        }
    }

    fn apply_dial(&mut self, ctx: &egui::Context) {
        if let Some(t) = self.dial_edit {
            if t.elapsed() >= Duration::from_millis(150) {
                self.dial_edit = None;
                self.set_frequency(self.dial_hz);
            } else {
                ctx.request_repaint_after(Duration::from_millis(160));
            }
        }
    }

    /// A few seconds after tuning, find where the carrier really is: UK muxes
    /// are transmitted up to a third of a megahertz off their channel's centre.
    /// If the receiver has locked it knows to the hertz (its carrier offset),
    /// else the edges of the block in the spectrum say, to a few tens of
    /// kilohertz: good enough to tell which of the plan's offsets it is, not
    /// to retune by, so that is all that is taken from them. The offset found
    /// is remembered for the channel, and the radio retuned to the carrier.
    fn auto_centre(&mut self) {
        if !self.sdr.auto_centre || self.centre_checked || self.session.is_none() {
            return;
        }
        // The plan's offsets are the UHF raster's: a narrow signal is where
        // it is tuned.
        if self.bandwidth.narrow() {
            self.centre_checked = true;
            return;
        }
        if self.tuned_at.elapsed() < Duration::from_secs(2) || self.span_hz <= 0.0 {
            return;
        }
        let Some(nominal) = uhf_centre_hz(self.channel) else {
            self.centre_checked = true;
            return;
        };
        let locked = self.decode && self.dec.tracking && self.dec.tps_synced;
        let snap = |off: f64| {
            PLAN_OFFSETS_HZ
                .iter()
                .find(|p| (off - **p).abs() < SNAP_HZ)
                .copied()
        };
        let found = if locked {
            // The carrier's offset from the radio's centre, to the hertz; the
            // radio's own clock error is in it, which the plan's offsets are
            // not: snap to them if one is near.
            let off = self.centre_hz + self.dec.cfo_hz - nominal;
            Some(snap(off).unwrap_or(off))
        } else {
            signal_block(
                &self.smooth,
                self.centre_hz,
                self.span_hz,
                nominal,
                self.bandwidth,
            )
            .and_then(|(centre, _)| snap(centre - nominal))
        };
        let Some(off) = found.filter(|o| o.abs() < 600e3) else {
            // Nothing to go on yet: look again, a few times.
            self.centre_tries += 1;
            self.tuned_at = Instant::now() - Duration::from_secs(1);
            self.centre_checked = self.centre_tries >= 4;
            return;
        };
        let ch = self.channel;
        if self.kind == SourceKind::Airspy {
            self.air_offsets.insert(ch, off);
        } else {
            self.offsets.insert(ch, off);
            prefs::set(&format!("offset_C{ch}"), &format!("{off:.0}"));
        }
        let target = nominal + off;
        if (target - self.tune_hz).abs() > 25e3 {
            self.set_frequency(target);
            self.note = format!("C{ch} is {:+.0} kHz off: retuned", off / 1e3);
        }
        self.centre_checked = true;
    }

    /// Send the SDR settings to the radio and the engine when they change.
    fn sync_sdr(&mut self) {
        let Some(s) = &self.session else { return };
        if self.sdr_sent == Some(self.sdr) {
            return;
        }
        let first = self.sdr_sent.is_none();
        s.rx.send(Command::FrontEnd(FrontEndSettings {
            dc_removal: self.sdr.dc_removal,
            iq_balance: self.sdr.iq_balance,
        }));
        let mut errors = Vec::new();
        if (!first || self.sdr.filter_mhz.is_some())
            && let Err(e) = s.set_filter_mhz(self.sdr.filter_mhz)
        {
            errors.push(e);
        }
        if (!first || self.sdr.ppm != 0.0)
            && let Err(e) = s.set_ppm(self.sdr.ppm)
        {
            errors.push(e);
        }
        if let Some(e) = errors.pop() {
            self.note = e;
        }
        self.sdr_sent = Some(self.sdr);
        self.sdr.save();
    }

    /// The waterfall's row rate follows the display rate: a row a frame.
    fn sync_fps(&mut self) {
        let fps = self.opts.fps;
        let Some(s) = &self.session else { return };
        if self.fps_sent != Some(fps) {
            s.rx.send(Command::DisplayFps(fps));
            self.fps_sent = Some(fps);
        }
        // Exactly a row a physical pixel of the waterfall's height: a row a
        // frame then moves it one whole pixel a frame, 60 pixels a second at
        // 60 fps, 15 at 15 (the time on the screen is the height over the
        // rate). Any other count would draw rows across fractions of pixels,
        // which shimmers as they move. A resize or a change of scaling starts
        // the waterfall afresh.
        if self.wf_rows > 0 && self.history.capacity() != self.wf_rows {
            self.history.set_capacity(self.wf_rows);
        }
    }

    /// Rows of the channel while the heat maps are shown.
    fn sync_heat(&mut self) {
        let Some(s) = &self.session else { return };
        let want = (self.bottom == Bottom::Heat && self.decode).then_some(
            if self.opts.fps >= SMOOTH_FPS {
                60.0
            } else {
                20.0
            },
        );
        if self.heat_sent != want {
            s.rx.send(Command::Heat(want));
            self.heat_sent = want;
            if want.is_none() {
                self.heat.reset();
            }
        }
    }

    fn start_optimiser(&mut self) {
        if !self.session.as_ref().is_some_and(|s| s.live) {
            self.note = "Optimising the gain needs a live radio".into();
            return;
        }
        if self.kind == SourceKind::Airspy {
            self.note =
                "The gain optimiser is for the HackRF; set the Airspy's gain by hand".into();
            return;
        }
        self.opt_result = None;
        self.optimiser = Some(sdr::Optimiser::new(self.gains));
    }

    /// Stop the optimiser and put back the gains it began with.
    fn cancel_optimiser(&mut self) {
        if let Some(o) = self.optimiser.take() {
            self.gains = o.original;
            self.change_gains();
            self.opt_result = Some("cancelled".into());
        }
    }

    /// One step of the gain optimiser each frame.
    fn optimise_tick(&mut self) {
        let Some(o) = self.optimiser.as_mut() else {
            return;
        };
        if self.session.is_none() {
            self.optimiser = None;
            return;
        }
        let reading = sdr::Reading {
            now: Instant::now(),
            mer: (self.dec.decoding && self.dec.snr_db.is_finite()).then_some(self.dec.snr_db),
            cn: self.cn_db,
            rms_dbfs: self.stats.rms_dbfs,
            clip_percent: self.stats.clip_percent,
        };
        match o.tick(&reading) {
            sdr::Action::Wait => {}
            sdr::Action::Apply(g) => {
                self.gains = g;
                self.change_gains();
            }
            sdr::Action::Finish(best) => {
                let o = self.optimiser.take().expect("running");
                let found = best.zip(o.best());
                self.gains = best.unwrap_or(o.original);
                self.change_gains();
                self.opt_result = Some(match found {
                    Some((g, p)) => format!(
                        "best: LNA {} VGA {}, {:.1} dB {} (level {:.0} dBFS), {} settings tried",
                        g.lna,
                        g.vga,
                        p.mer.or(p.cn).unwrap_or(0.0),
                        if p.mer.is_some() { "MER" } else { "C/N" },
                        p.rms_dbfs,
                        o.done()
                    ),
                    None => "nothing to measure (no signal, or clipping at every setting)".into(),
                });
            }
        }
    }

    fn change_gains(&mut self) {
        let Some(s) = &self.session else { return };
        let r = match self.kind {
            SourceKind::Airspy => s.set_air_gains(self.air),
            _ => s.set_gains(self.gains),
        };
        if let Err(e) = r {
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
            uhf_channel_at(self.centre_hz)
                .filter(|_| !self.bandwidth.narrow())
                .unwrap_or(0),
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
        if self.show_channel() {
            self.chan_view.update(s.rx.take_sounder());
        }
        if self.decode {
            // The stream itself is the hub's; what is kept here is its rate,
            // the receiver's status and the scopes' cells.
            let u = s.rx.take_decoder();
            self.dec = u.status;
            self.standard = u.standard;
            self.dec2 = u.t2;
            self.dec_packets = u.packets;
            if self.bottom == Bottom::Maps {
                if let Some(c) = &u.scopes {
                    self.errors.add(c);
                }
            } else {
                self.scope.update(u.scopes, u.measurements);
            }
            if self.bottom == Bottom::Heat {
                self.heat.update(u.heat);
            }
            let now = Instant::now();
            // A T2 stream arrives a frame (217 ms) at a time, so a longer
            // window keeps the figure from jumping about.
            let window = Duration::from_secs(if self.standard == Standard::Dvbt2 {
                3
            } else {
                1
            });
            match &mut self.dec_rate {
                Some((at, t, rate)) if t.elapsed() >= window => {
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
        // The rows wait in a queue and one goes on each frame, so the
        // waterfall moves a row a frame even when the engine's rows and the
        // frames do not fall in step; a queue that grows (the display slower
        // than asked) is caught up two rows a frame, and one that has run far
        // behind is cut.
        self.rows_due.extend(rows);
        let backlog = self.rows_due.len();
        if backlog > (self.opts.fps as usize).max(10) {
            self.rows_due.drain(..backlog - 2);
        }
        let take = match self.rows_due.len() {
            0 => 0,
            n if n > 3 => 2,
            _ => 1,
        };
        if self.span_hz > 0.0 {
            self.wf_steps[take.min(2)] += 1;
        }
        // A calm trace: a time constant of about 0.12 s, whatever the row
        // rate.
        let pull = 1.0 - (-1.0 / (0.12 * self.opts.fps.max(1.0)) as f32).exp();
        for row in self.rows_due.drain(..take).collect::<Vec<_>>() {
            if row.tuning != self.tuning {
                self.tuning = row.tuning;
                self.reset_view();
            }
            self.centre_hz = row.center_hz;
            self.span_hz = row.sample_rate;
            if self.smooth.len() != row.db.len() {
                self.smooth.clone_from(&row.db);
            } else {
                for (s, v) in self.smooth.iter_mut().zip(&row.db) {
                    *s += pull * (v - *s);
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
        let est = self.vfo_hz.and_then(|v| {
            spectral_cn_db(&row.db, row.center_hz, row.sample_rate, v, self.bandwidth)
        });
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
            // The TPS does not say the width: it is the one the receiver runs at.
            m.bandwidth = if self.bandwidth.dvbt() {
                self.bandwidth
            } else {
                Bandwidth::Mhz8
            };
            m.fft = t.fft;
            m.guard = t.guard;
            m.rate_lp = t.rate_lp;
            m.alpha = t.alpha;
            m
        });
        let mut mode = read.or(self.opts.assume);
        if read.is_none()
            && let (Some(m), Some((fft, guard))) = (&mut mode, locked.and_then(|g| g.dvbt))
        {
            m.fft = fft;
            m.guard = guard;
        }
        if let (Standard::Dvbt2, Some(t2), true) = (self.standard, &self.dec2, self.decode) {
            return self.t2_card(t2);
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
        // The effective SNR replaces the spectrum's C/N once decoding: the
        // band's mean SNR flatters a channel with notches, whose faded
        // carriers are where the errors come from (see
        // `decdvbt_rx::capacity`). The mean is kept as a detail.
        let (cn_db, cn_source) = if decoding && self.dec.effective_snr_db.is_finite() {
            details.push(kv_mer(self.dec.snr_db));
            (Some(self.dec.effective_snr_db), "effective")
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
            channel: self.card_channel(),
            freq_hz: self.vfo_hz,
            active: self.session.is_some(),
        }
    }

    /// The card for a DVB-T2 lock.
    fn t2_card(&self, t2: &T2Status) -> card::Card {
        let mut c = t2card::card(t2, self.ts_mbps());
        // Measured as soon as L1 is read and the frames are equalised, packets
        // or not: a signal under the code's threshold must show its negative
        // margin, not the spectrum's flattering guess.
        let measured = t2.l1_ok && t2.effective_snr_db.is_finite() && t2.snr_db.is_finite();
        if measured {
            c.details.push(kv_mer(t2.snr_db));
        }
        card::Card {
            standard: Some(card::Standard::DvbT2),
            assumed: false,
            name: c.name,
            qef_db: t2.plp.and_then(|p| p.qef_cn_db()),
            details: c.details,
            cn_db: if measured {
                Some(t2.effective_snr_db)
            } else {
                self.cn_db
            },
            cn_source: if measured { "effective" } else { "spectrum" },
            channel: self.card_channel(),
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
        if self.opts.video.is_some() && self.started.elapsed().as_secs_f64() >= self.opts.after {
            self.record_video(ctx, &path);
            return;
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
                "screenshot saved to {} (CPU {:.1} % average; per frame: logic {:.2} ms, ui {:.2} ms, {} frames; waterfall rows a frame 0/1/2: {:?}; {})",
                path.display(),
                self.cpu_avg.unwrap_or(self.cpu_percent),
                self.frame_cost[0].as_secs_f64() * 1e3 / self.frames.max(1) as f64,
                self.frame_cost[1].as_secs_f64() * 1e3 / self.frames.max(1) as f64,
                self.frames,
                self.wf_steps,
                self.stats.describe
            ),
            Err(e) => eprintln!("cannot save screenshot {}: {e}", path.display()),
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// `--video`: every frame read back and handed to the recording; at the
    /// end the last frame is the screenshot and the window closes.
    fn record_video(&mut self, ctx: &egui::Context, shot: &std::path::Path) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        ctx.request_repaint();
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        if self.video_done {
            return;
        }
        if self.video.is_none() {
            let out = self.opts.video.clone().expect("--video");
            let tap = decdvbt_video::find_ffmpeg(prefs::ffmpeg_path().as_deref())
                .ok_or_else(|| "ffmpeg not found".to_string())
                .and_then(|ff| {
                    video::VideoTap::start(&ff, &out, image.size).map_err(|e| e.to_string())
                });
            match tap {
                Ok(t) => self.video = Some(t),
                Err(e) => {
                    eprintln!("cannot record {}: {e}", out.display());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
            }
        }
        let Some(tap) = &mut self.video else { return };
        if let Err(e) = tap.push(&image) {
            eprintln!("recording: {e}");
        }
        if tap.elapsed() < self.opts.video_secs {
            return;
        }
        let tap = self.video.take().expect("recording");
        self.video_done = true;
        let frames = tap.finish().unwrap_or(0);
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let (w, h) = (image.size[0] as u32, image.size[1] as u32);
        let _ = image::save_buffer(shot, &rgba, w, h, image::ColorType::Rgba8);
        eprintln!(
            "video saved to {} ({frames} frames)",
            self.opts.video.as_ref().expect("--video").display()
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ---- panels ---------------------------------------------------------

    fn control_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            self.freq_dial(ui);
            ui.add_space(10.0);
            ui.vertical(|ui| self.control_rows(ui));
        });
        ui.add_space(2.0);
    }

    /// The controls beside the dial: the source, the channel and the radio on
    /// the first line, the level, the panels and the display on the second.
    fn control_rows(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let running = self.session.is_some();
            let live = self.kind.live();
            let before = self.kind;
            egui::ComboBox::from_id_salt("source")
                .selected_text(match self.kind {
                    SourceKind::HackRf => "HackRF",
                    SourceKind::Airspy => "Airspy R2",
                    SourceKind::File => "IQ file",
                })
                .width(90.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.kind, SourceKind::HackRf, "HackRF");
                    ui.selectable_value(&mut self.kind, SourceKind::Airspy, "Airspy R2");
                    ui.selectable_value(&mut self.kind, SourceKind::File, "IQ file");
                });
            if before != self.kind && running {
                self.stop();
            }
            // The Airspy's lowest rate is 10 MS/s, and the narrow channels
            // need the radio at 2 MS/s: it offers only the wide ones.
            if self.kind == SourceKind::Airspy && self.bandwidth.narrow() {
                self.set_bandwidth(Bandwidth::Mhz8);
            }

            // The channel's width: what the receiver looks for (both
            // sources).
            let mut bw = self.bandwidth;
            egui::ComboBox::from_id_salt("bandwidth")
                .selected_text(bw.name())
                .width(70.0)
                .show_ui(ui, |ui| {
                    for b in Bandwidth::ALL {
                        if self.kind == SourceKind::Airspy && b.narrow() {
                            continue;
                        }
                        let label = match b {
                            Bandwidth::Mhz1 => "1 MHz (DVB-T)",
                            Bandwidth::Mhz1_7 => "1.7 MHz (T2)",
                            _ => b.name(),
                        };
                        ui.selectable_value(&mut bw, b, label);
                    }
                })
                .response
                .on_hover_text(
                    "The channel's width: the receiver must be told it. 1 MHz is DVB-T with \
                     every clock of 8 MHz divided by 8 (DecDVB-T's narrow mode); 1.7 MHz is \
                     DVB-T2's narrow channel. Both run the radio at 2 MS/s.",
                );
            self.set_bandwidth(bw);

            if self.kind.live() {
                ui.label("channel");
                if ui.small_button("◀").clicked() {
                    self.step_channel(false);
                }
                let edit = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.channel_text)
                            .desired_width(56.0)
                            .hint_text("C26 / MHz"),
                    )
                    .on_hover_text("A UHF channel (C26), or any frequency in MHz (433.25)");
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let text = self.channel_text.trim().to_string();
                    match (parse_channel(&text), text.parse::<f64>()) {
                        (Some(n), _) if !text.contains('.') => self.tune_channel(n),
                        (_, Ok(mhz)) if (1.0..=6000.0).contains(&mhz) => {
                            self.set_frequency(mhz * 1e6)
                        }
                        _ => {
                            self.note =
                                format!("not a UHF channel (21–69) or a frequency in MHz: {text}")
                        }
                    }
                    self.channel_text = match self.vfo_hz {
                        Some(v) if uhf_channel_at(v).is_none() || self.bandwidth.narrow() => {
                            format!("{:.3}", v / 1e6)
                        }
                        _ => format!("C{}", self.channel),
                    };
                }
                if ui.small_button("▶").clicked() {
                    self.step_channel(true);
                }
                ui.label(format!(
                    "{:.3} MHz",
                    self.vfo_hz.or(uhf_centre_hz(self.channel)).unwrap_or(0.0) / 1e6
                ));

                ui.separator();
                ui.label("rate");
                if self.kind == SourceKind::Airspy {
                    // The R2 has one rate here (its ADC runs at 20 MS/s and
                    // the host makes 10 MS/s IQ).
                    ui.label("10 MS/s");
                } else {
                    let before = self.rate_msps;
                    egui::ComboBox::from_id_salt("rate")
                        .selected_text(format!("{} MS/s", self.rate_msps))
                        .width(90.0)
                        .show_ui(ui, |ui| {
                            for &r in rates_for(self.bandwidth) {
                                ui.selectable_value(&mut self.rate_msps, r, format!("{r} MS/s"));
                            }
                        });
                    if before != self.rate_msps && running {
                        self.start();
                    }
                }

                ui.separator();
                if self.kind == SourceKind::Airspy {
                    if sdr::airspy_gain_row(ui, &mut self.air, &mut self.air_preset) {
                        self.change_gains();
                    }
                } else {
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
            ui.separator();
            if ui
                .selectable_label(self.sdr_open, "⚙ SDR")
                .on_hover_text(
                    "DC removal, I/Q balance, filter, frequency correction, gain optimiser",
                )
                .clicked()
            {
                self.sdr_open = !self.sdr_open;
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
            ui.selectable_value(&mut self.bottom, Bottom::Maps, "maps");
            ui.selectable_value(&mut self.bottom, Bottom::Heat, "history");
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
                    // The waterfall scrolls a row a frame: the rate is also
                    // its speed.
                    for f in FPS_CHOICES {
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
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.kind.live() {
            self.radio = (self.channel, self.tune_hz);
        }
        let now = self.snapshot();
        if let Some(k) = self.keeper.as_mut() {
            k.flush(now);
        }
    }

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
        self.sync_bandwidth();
        self.sync_sounder();
        self.sync_decoder();
        self.sync_scopes();
        self.sync_sdr();
        self.sync_fps();
        self.sync_heat();
        self.poll_stream();
        self.sync_outputs();
        self.auto_centre();
        if self.opts.optimise && self.dec.decoding {
            self.opts.optimise = false;
            self.start_optimiser();
        }
        self.optimise_tick();
        self.apply_dial(ctx);
        self.remember(ctx);
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
            Bottom::Maps => {
                egui::Panel::bottom("maps").resizable(false).show(ui, |ui| {
                    ui.set_min_height(CHANNEL_PANEL_H);
                    self.chan_view.show_maps(ui, &mut self.errors);
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
            Bottom::Heat => {
                egui::Panel::bottom("heat").resizable(false).show(ui, |ui| {
                    ui.set_min_height(CHANNEL_PANEL_H);
                    // Rows a second: one for each frame of a T2 signal.
                    let t2_rate = self
                        .dec2
                        .as_ref()
                        .filter(|_| self.standard == Standard::Dvbt2)
                        .and_then(|t| t.frame_period_s())
                        .map(|p| (1.0 / p) as f32);
                    let rate = t2_rate.unwrap_or(self.heat_sent.unwrap_or(20.0) as f32);
                    self.heat.show(ui, rate);
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
                self.wf_rows =
                    band::waterfall_rows(ui.available_height(), ui.ctx().pixels_per_point())
                        .max(64);
                let tex = self.ring.update(ui.ctx(), &self.history);
                let inp = band::BandInput {
                    spectrum_db: &self.smooth,
                    center_hz: self.centre_hz,
                    span_hz: self.span_hz,
                    levels: self.history.levels(),
                    waterfall: tex,
                    vfo_hz: self.vfo_hz,
                    occupied_half_hz: band::OCCUPIED_HALF_HZ * self.bandwidth.scale(),
                    raster: !self.bandwidth.narrow(),
                };
                if let Some(band::Action::Tune(n)) = band::show(ui, &inp) {
                    tune = Some(n);
                }
            });
        let ctx = ui.ctx().clone();
        self.ts_window.show(&ctx, &self.ts);
        if self.sdr_open {
            let live = self.session.as_ref().is_some_and(|s| s.live);
            let asks = sdr::show(
                &ctx,
                &mut self.sdr_open,
                sdr::SdrView {
                    settings: &mut self.sdr,
                    gains: &mut self.gains,
                    air: &mut self.air,
                    air_preset: &mut self.air_preset,
                    air_packed: &mut self.air_packed,
                    airspy: self.kind == SourceKind::Airspy,
                    live,
                    front_end: self.stats.front_end,
                    optimiser: self.optimiser.as_ref(),
                    last_result: self.opt_result.as_deref(),
                    offsets: if self.kind == SourceKind::Airspy {
                        &self.air_offsets
                    } else {
                        &self.offsets
                    },
                },
            );
            if asks.gains_changed {
                self.change_gains();
            }
            if asks.restart && self.session.is_some() {
                // The transfer format is fixed when the stream starts.
                self.start();
            }
            if asks.optimise {
                self.start_optimiser();
            }
            if asks.cancel {
                self.cancel_optimiser();
            }
            if asks.clear_offsets {
                for c in 21..=69u8 {
                    prefs::set(&format!("offset_C{c}"), "");
                }
                self.offsets.clear();
            }
            if asks.reset {
                self.sdr = sdr::SdrSettings::default();
                self.gains = Gains::default();
                self.air = AirGains::default();
                self.air_preset = Some(Preset::DEFAULT);
                self.change_gains();
            }
        }
        let report = Arc::clone(&self.ts.report);
        if let Some(sid) = self.epg_window.show(&ctx, &self.epg, &report) {
            self.watch(sid);
        }
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
            if self.session.is_none() && self.kind.live() {
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

/// The card's row for the band's mean SNR (the MER the pilots measure).
fn kv_mer(snr_db: f32) -> (String, String) {
    ("MER".to_string(), format!("{snr_db:.1} dB mean"))
}

/// What the sounder knows of a signal it has locked to, for the signal card:
/// the FFT size and the guard interval, and what follows from them.
fn lock_details(g: &Geometry, cfo_hz: f64) -> Vec<(String, String)> {
    let kv = |k: &str, v: String| (k.to_string(), v);
    let useful_us = 1e6 / g.carrier_spacing_hz;
    vec![
        kv("FFT size", g.fft_name.to_string()),
        kv("guard", format!("{} ({:.0} µs)", g.guard_name, g.guard_us)),
        kv("symbol", format!("{:.0} µs", g.symbol_s * 1e6)),
        kv("useful", format!("{useful_us:.0} µs")),
        kv(
            "carriers",
            format!("{} ({} data)", g.active_carriers, g.data_carriers),
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
        kv("bandwidth", m.bandwidth.name().to_string()),
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
