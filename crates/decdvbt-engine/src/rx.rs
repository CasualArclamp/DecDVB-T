// made by CasualArclamp
//! The receive thread: reads an [`IqSource`], builds waterfall rows and level
//! readings, runs the receiver and writes a recording when asked.
//!
//! One thread, one pass over each block, nothing allocated in the loop. The
//! GUI never touches the source: it sends [`Command`]s and takes rows and
//! [`Stats`] from [`Rx`]. The transport stream the receiver makes goes to the
//! [`TsHub`](crate::hub::TsHub), a thread of its own.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_core::{Iq, SampleFormat};
use decdvbt_io::{IqFileWriter, IqSource};

use decdvbt_rx::auto::{AutoReceiver, Standard};
use decdvbt_rx::delay_doppler::{DelayDoppler, FreqDoppler};
use decdvbt_rx::receiver::ReceiverStatus;
use decdvbt_rx::scope::{Measurements, Scopes};
use decdvbt_rx::sounder::{Sounder, SounderConfig, Sounding, Status as SounderStatus};
use decdvbt_rx::t2::receiver::T2Status;
use decdvbt_ts::Epg;

use crate::frontend::{FrontEnd, FrontEndReading, FrontEndSettings};
use crate::hub::{Feeder, Outputs, TsHub, TsView};
use crate::meter::{Stage, StageLoad, StageMeter};
use crate::waterfall::Waterfall;

/// How the spectrum is built.
#[derive(Debug, Clone, Copy)]
pub struct RxOptions {
    pub fft: usize,
    pub rows_per_s: f64,
    /// FFT segments averaged into each row: the waterfall's whole cost.
    pub segments_per_row: u32,
}

impl Default for RxOptions {
    fn default() -> Self {
        RxOptions {
            fft: 4096,
            rows_per_s: 20.0,
            segments_per_row: 24,
        }
    }
}

/// One line of the waterfall.
#[derive(Debug, Clone)]
pub struct Row {
    /// dBFS per bin, the most negative frequency first.
    pub db: Vec<f32>,
    pub center_hz: f64,
    pub sample_rate: f64,
    /// Counts retunes: a row from before one does not line up with the
    /// rows after it.
    pub tuning: u32,
}

/// What the GUI is told about, refreshed every second.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub describe: String,
    pub sample_rate: f64,
    pub center_hz: f64,
    pub samples: u64,
    /// RMS level of the input, dBFS (a full-scale complex sample is 0).
    pub rms_dbfs: f32,
    /// Highest |I| or |Q| in the second, as a fraction of full scale.
    pub peak: f32,
    /// Samples at the ADC's limit, percent: the receiver is overloaded.
    pub clip_percent: f32,
    /// Buffers the source had to drop because we fell behind.
    pub dropped: u64,
    /// What the front end's estimates read: the DC offset and the image.
    pub front_end: FrontEndReading,
    pub load: StageLoad,
    pub recording: Option<Recording>,
    pub error: Option<String>,
    /// The source is exhausted (a file played to its end).
    pub ended: bool,
}

#[derive(Debug, Clone)]
pub struct Recording {
    pub path: PathBuf,
    pub bytes: u64,
}

pub enum Command {
    Retune(f64),
    /// Run the channel sounder on the signal at this RF frequency, Hz, or
    /// stop it with `None`. It costs CPU only while it runs, so the GUI
    /// asks for it only while its panels are open.
    Sounder(Option<f64>),
    /// Run the receiver on the signal at this RF frequency, Hz (lock, equalise,
    /// decode to a transport stream), or stop it with `None`.
    Decode(Option<f64>),
    /// Make the constellation scopes' cells (ten times a second) while on:
    /// they cost nothing otherwise.
    Scopes(bool),
    /// Make the channel measurements (four times a second) while on.
    Measure(bool),
    /// Make a row of the channel (its gain and signal-to-noise ratio across
    /// the band, its impulse response) this many times a second while on, for
    /// the heat maps; `None` stops. A row costs a transform.
    Heat(Option<f64>),
    /// The display's frame rate: the waterfall makes one row a frame, so it
    /// scrolls a row a frame (the same transforms a second at any rate, a
    /// smaller share of them to each row); what the receiver has made, the
    /// scopes' cells and the measurements are handed over as often as the
    /// display can show them (every 33 to 100 ms).
    DisplayFps(f64),
    /// Which corrections are made to the radio's samples (the DC offset, the
    /// I/Q imbalance) before the spectrum, the receiver and the sounder see them.
    FrontEnd(FrontEndSettings),
    /// Start recording to this file (cs8), or stop with `None`.
    Record(Option<PathBuf>),
}

/// What the sounder has made, waiting for the GUI.
#[derive(Default)]
struct SounderOut {
    status: SounderStatus,
    fading: Vec<Vec<f32>>,
    map: Option<Arc<DelayDoppler>>,
    map_new: bool,
    freq_map: Option<Arc<FreqDoppler>>,
    freq_new: bool,
}

/// What [`Rx::take_sounder`] hands over.
#[derive(Default)]
pub struct SounderUpdate {
    pub status: SounderStatus,
    /// Fading rows (|H|² in dB at each pilot) made since the last call.
    pub fading: Vec<Vec<f32>>,
    /// The delay–Doppler map, if a new one has been made.
    pub map: Option<Arc<DelayDoppler>>,
    /// The Doppler spectrum at each carrier, likewise.
    pub freq_map: Option<Arc<FreqDoppler>>,
}

/// What the receiver has made, waiting for the GUI.
#[derive(Default)]
struct DecoderOut {
    status: ReceiverStatus,
    /// Which standard has the lock, and the T2 receiver's status with it.
    standard: Standard,
    t2: Option<Box<T2Status>>,
    /// Packets made since the receiver was started.
    packets: u64,
    scopes: Option<Arc<Scopes>>,
    measurements: Option<Arc<Measurements>>,
    /// Heat rows not yet taken (the oldest go past [`MAX_HEAT`]).
    heat: Vec<Arc<Measurements>>,
}

/// Heat rows kept for a GUI that is slow to collect them.
const MAX_HEAT: usize = 400;

/// How often the thread hands things over, as the display is paced.
#[derive(Clone, Copy)]
struct Pace {
    /// The receiver's and the sounder's results to the GUI.
    publish: Duration,
    scopes: Duration,
    measure: Duration,
    /// The waterfall's rows a second, and the transforms in each.
    rows_per_s: f64,
    segments: u32,
}

impl Pace {
    fn normal(opts: &RxOptions) -> Pace {
        Pace {
            publish: Duration::from_millis(100),
            scopes: Duration::from_millis(100),
            measure: Duration::from_millis(250),
            rows_per_s: opts.rows_per_s,
            segments: opts.segments_per_row,
        }
    }

    /// For a display of `fps` frames a second: a waterfall row a frame, with
    /// the transforms a row scaled so the waterfall's cost a second stays
    /// `opts`'s; the results handed over once a frame, but no more often
    /// than 30 times a second and no less than 10.
    fn for_fps(opts: &RxOptions, fps: f64) -> Pace {
        let fps = fps.clamp(1.0, 120.0);
        let publish = Duration::from_secs_f64((1.0 / fps).clamp(1.0 / 30.0, 0.1));
        Pace {
            publish,
            scopes: publish,
            measure: (publish * 5 / 2).max(Duration::from_millis(80)),
            rows_per_s: fps,
            segments: ((f64::from(opts.segments_per_row) * opts.rows_per_s / fps).round() as u32)
                .max(1),
        }
    }
}

/// What [`Rx::take_decoder`] hands over.
#[derive(Default)]
pub struct DecoderUpdate {
    /// The status in the DVB-T receiver's terms (see [`AutoReceiver::status`]).
    pub status: ReceiverStatus,
    /// The standard locked on, and, if it is T2, that receiver's own status.
    pub standard: Standard,
    pub t2: Option<Box<T2Status>>,
    /// Transport packets decoded since the receiver was started.
    pub packets: u64,
    /// The scopes' cells and the measurements, if new ones have been made
    /// since the last call.
    pub scopes: Option<Arc<Scopes>>,
    pub measurements: Option<Arc<Measurements>>,
    /// Rows of the channel made since the last call, oldest first.
    pub heat: Vec<Arc<Measurements>>,
}

struct Shared {
    /// Filled by the waterfall's thread.
    rows: Arc<Mutex<VecDeque<Row>>>,
    stats: Mutex<Stats>,
    sounder: Mutex<SounderOut>,
    decoder: Mutex<DecoderOut>,
    stop: AtomicBool,
}

/// Signal thrown away after a retune, seconds: a live source has already
/// flushed what it had of the old channel, and this covers what is left of
/// the old samples and the radio's own settling. The receiver and the
/// sounder begin on the new channel's own signal.
const SETTLE_S: f64 = 0.05;

/// The samples still to throw away after a retune.
#[derive(Default)]
struct Settle {
    left: u64,
}

impl Settle {
    fn start(&mut self, rate: f64) {
        self.left = (SETTLE_S * rate) as u64;
    }

    /// Cut the stale samples off the front of `block`; how many went.
    fn trim(&mut self, block: &mut Vec<Iq>) -> usize {
        let cut = (self.left as usize).min(block.len());
        self.left -= cut as u64;
        block.drain(..cut);
        cut
    }
}

/// A running receive thread.
pub struct Rx {
    cmd: Sender<Command>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
    /// Dropped after the thread has ended (it holds a feeder).
    hub: TsHub,
}

/// Reads the source's own drop counter (a HackRF's USB overflows).
pub type DropProbe = Box<dyn Fn() -> u64 + Send>;

impl Rx {
    pub fn start(source: Box<dyn IqSource>, opts: RxOptions, drops: Option<DropProbe>) -> Rx {
        let shared = Arc::new(Shared {
            rows: Arc::new(Mutex::new(VecDeque::new())),
            stats: Mutex::new(Stats {
                describe: source.describe(),
                sample_rate: source.sample_rate(),
                center_hz: source.center_freq(),
                ..Default::default()
            }),
            sounder: Mutex::new(SounderOut::default()),
            decoder: Mutex::new(DecoderOut::default()),
            stop: AtomicBool::new(false),
        });
        let (cmd, cmd_rx) = mpsc::channel();
        let sh = Arc::clone(&shared);
        let hub = TsHub::start();
        let feeder = hub.feeder();
        let join = std::thread::Builder::new()
            .name("decdvbt-rx".into())
            .spawn(move || run(source, opts, drops, cmd_rx, sh, feeder))
            .expect("cannot start the receive thread");
        Rx {
            cmd,
            shared,
            join: Some(join),
            hub,
        }
    }

    /// Where the transport stream goes (a file, UDP, HTTP, the player).
    pub fn set_outputs(&self, o: Outputs) {
        self.hub.set_outputs(o);
    }

    /// What the transport stream carries and where it goes.
    pub fn ts_view(&self) -> TsView {
        self.hub.view()
    }

    /// The programme guide.
    pub fn epg(&self) -> Arc<Epg> {
        self.hub.epg()
    }

    pub fn send(&self, c: Command) {
        // The thread may have ended (end of file, error); nothing to tell.
        let _ = self.cmd.send(c);
    }

    /// Rows made since the last call, oldest first.
    pub fn take_rows(&self) -> Vec<Row> {
        self.shared.rows.lock().unwrap().drain(..).collect()
    }

    pub fn stats(&self) -> Stats {
        self.shared.stats.lock().unwrap().clone()
    }

    /// What the sounder has made since the last call.
    pub fn take_sounder(&self) -> SounderUpdate {
        let mut o = self.shared.sounder.lock().unwrap();
        SounderUpdate {
            status: o.status,
            fading: std::mem::take(&mut o.fading),
            map: if std::mem::take(&mut o.map_new) {
                o.map.clone()
            } else {
                None
            },
            freq_map: if std::mem::take(&mut o.freq_new) {
                o.freq_map.clone()
            } else {
                None
            },
        }
    }

    /// What the receiver has made since the last call.
    pub fn take_decoder(&self) -> DecoderUpdate {
        let mut o = self.shared.decoder.lock().unwrap();
        DecoderUpdate {
            status: o.status.clone(),
            standard: o.standard,
            t2: o.t2.clone(),
            packets: o.packets,
            scopes: o.scopes.take(),
            measurements: o.measurements.take(),
            heat: std::mem::take(&mut o.heat),
        }
    }

    /// Whether the thread is still running.
    pub fn running(&self) -> bool {
        self.join.as_ref().is_some_and(|j| !j.is_finished())
    }
}

impl Drop for Rx {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Level of a block, from every 8th sample: the cost of reading it is a
/// small fraction of the cost of producing it.
#[derive(Default)]
struct Level {
    power: f64,
    n: u64,
    peak: f32,
    clipped: u64,
}

impl Level {
    const STRIDE: usize = 8;
    /// |I| or |Q| at or over this is at the converter's limit (cs8's ±127/128).
    const CLIP: f32 = 0.99;

    fn add(&mut self, block: &[Iq]) {
        for s in block.iter().step_by(Self::STRIDE) {
            self.power += s.norm_sqr() as f64;
            let m = s.re.abs().max(s.im.abs());
            self.peak = self.peak.max(m);
            self.clipped += (m >= Self::CLIP) as u64;
            self.n += 1;
        }
    }
}

fn run(
    mut source: Box<dyn IqSource>,
    opts: RxOptions,
    drops: Option<DropProbe>,
    cmds: Receiver<Command>,
    sh: Arc<Shared>,
    feeder: Feeder,
) {
    let rate = source.sample_rate();
    let live = source.is_live();
    let mut pace = Pace::normal(&opts);
    // The waterfall's rows are made and paced on a thread of their own.
    let mut wf = Waterfall::start(
        opts.fft,
        rate,
        pace.rows_per_s,
        pace.segments,
        Arc::clone(&sh.rows),
    );
    let mut meter = StageMeter::default();
    let mut block: Vec<Iq> = Vec::new();
    let mut level = Level::default();
    let mut total = 0u64;
    let mut tuning = 0u32;
    let mut writer: Option<(IqFileWriter, PathBuf, u64)> = None;
    // The sounder, with the RF frequency of the signal it was asked for.
    let mut sounder: Option<(Sounder, f64)> = None;
    let mut sounder_pub = Instant::now();
    // The receiver, with the RF frequency of the signal it was asked for.
    let mut decoder: Option<(AutoReceiver, f64)> = None;
    let mut decoder_pub = Instant::now();
    let mut settle = Settle::default();
    let mut front = FrontEnd::new(FrontEndSettings::default());
    let mut heat_scratch: Vec<Measurements> = Vec::new();
    let mut ts_scratch: Vec<u8> = Vec::new();
    // The display's scopes and measurements, and when each was last made.
    let (mut scopes_on, mut measure_on) = (false, false);
    let mut heat_rate: Option<f64> = None;
    let (mut scopes_at, mut measure_at) = (Instant::now(), Instant::now());
    let mut map_at = Instant::now();
    let started = Instant::now();
    let set_error = |e: String| sh.stats.lock().unwrap().error = Some(e);
    // A sounder for the signal at `vfo_hz`, for a source tuned to `centre`.
    let new_sounder = |vfo_hz: f64, centre: f64, rate: f64| {
        // A file without a centre in its name has none to measure from.
        let offset = if centre > 0.0 { vfo_hz - centre } else { 0.0 };
        Sounder::new(SounderConfig::new(rate, offset))
    };
    let new_decoder = |vfo_hz: f64, centre: f64, rate: f64, heat: Option<f64>, sound: bool| {
        let offset = if centre > 0.0 { vfo_hz - centre } else { 0.0 };
        let mut d = AutoReceiver::new(rate, offset);
        // A T2 lock's sounder is the receiver's own (DVB-T's cannot read it).
        d.set_sounding(sound);
        // A file is held to real time here too.
        d.set_realtime(true);
        d.set_heat(heat.unwrap_or(0.0));
        d
    };

    while !sh.stop.load(Ordering::Relaxed) {
        loop {
            match cmds.try_recv() {
                Ok(Command::Sounder(vfo)) => {
                    sounder = vfo.map(|v| (new_sounder(v, source.center_freq(), rate), v));
                    *sh.sounder.lock().unwrap() = SounderOut::default();
                    if let Some((d, _)) = &mut decoder {
                        d.set_sounding(sounder.is_some());
                    }
                }
                Ok(Command::Decode(vfo)) => {
                    decoder = vfo.map(|v| {
                        let d = new_decoder(
                            v,
                            source.center_freq(),
                            rate,
                            heat_rate,
                            sounder.is_some(),
                        );
                        (d, v)
                    });
                    *sh.decoder.lock().unwrap() = DecoderOut::default();
                    ts_scratch.clear();
                    feeder.reset();
                }
                Ok(Command::Scopes(on)) => scopes_on = on,
                Ok(Command::Measure(on)) => measure_on = on,
                Ok(Command::Heat(rows)) => {
                    heat_rate = rows;
                    if let Some((d, _)) = &mut decoder {
                        d.set_heat(rows.unwrap_or(0.0));
                    }
                }
                Ok(Command::FrontEnd(f)) => front.settings = f,
                Ok(Command::DisplayFps(fps)) => {
                    pace = Pace::for_fps(&opts, fps);
                    wf.set_rate(pace.rows_per_s, pace.segments);
                }
                Ok(Command::Retune(hz)) => match source.retune(hz) {
                    Ok(true) => {
                        tuning += 1;
                        settle.start(rate);
                        sh.rows.lock().unwrap().clear();
                        wf.clear();
                        if let Some((s, v)) = &mut sounder {
                            *s = new_sounder(*v, source.center_freq(), rate);
                        }
                        if let Some((d, v)) = &mut decoder {
                            *d = new_decoder(
                                *v,
                                source.center_freq(),
                                rate,
                                heat_rate,
                                sounder.is_some(),
                            );
                            ts_scratch.clear();
                            feeder.reset();
                        }
                    }
                    Ok(false) => {}
                    Err(e) => set_error(e.to_string()),
                },
                Ok(Command::Record(path)) => {
                    if let Some((w, ..)) = writer.take()
                        && let Err(e) = w.finish()
                    {
                        set_error(format!("recording: {e}"));
                    }
                    if let Some(p) = path {
                        match IqFileWriter::create(&p, SampleFormat::Cs8) {
                            Ok(w) => writer = Some((w, p, 0)),
                            Err(e) => set_error(format!("cannot record to {}: {e}", p.display())),
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        let n = match meter.time(Stage::Source, || source.read(&mut block)) {
            Ok(n) => n,
            Err(e) => {
                set_error(e.to_string());
                break;
            }
        };
        if n == 0 {
            // A file at its end starts again; a source that cannot is done.
            match source.rewind() {
                Ok(true) => {
                    // The signal starts over: so does the lock.
                    if let Some((s, _)) = &mut sounder {
                        s.reset();
                    }
                    if let Some((d, _)) = &mut decoder {
                        d.reset();
                    }
                    continue;
                }
                _ => {
                    sh.stats.lock().unwrap().ended = true;
                    break;
                }
            }
        }
        // Samples from before a retune took effect are not the new
        // channel's: leave them out (but keep counting them).
        total += settle.trim(&mut block) as u64;
        if block.is_empty() {
            continue;
        }
        let n = block.len();
        let at = total;
        total += n as u64;

        // What the radio gave, as it gave it: its level and clipping, and
        // the recording.
        level.add(&block);
        if let Some((w, path, bytes)) = &mut writer {
            let r = meter.time(Stage::Record, || w.write(&block));
            *bytes += (n * SampleFormat::Cs8.bytes_per_sample()) as u64;
            if let Err(e) = r {
                set_error(format!("recording to {}: {e}", path.display()));
                writer = None;
            }
        }
        meter.time(Stage::FrontEnd, || front.apply(&mut block));

        // While DVB-T2 has the lock its receiver sounds the channel from its
        // own estimates; DVB-T's sounder would only fail to lock, so it rests.
        let t2_locked = decoder
            .as_ref()
            .is_some_and(|(d, _)| d.standard() == Standard::Dvbt2);
        if let Some((s, _)) = &mut sounder {
            if !t2_locked {
                meter.time(Stage::Sounder, || s.push(&block, at));
            }
            if sounder_pub.elapsed() >= pace.publish {
                sounder_pub = Instant::now();
                let s: &mut dyn Sounding = match decoder
                    .as_mut()
                    .filter(|_| t2_locked)
                    .and_then(|(d, _)| d.t2_sounder())
                {
                    Some(t2) => t2,
                    None => s,
                };
                // The map is made once a second, and only while locked.
                let map = (map_at.elapsed() >= Duration::from_secs(1)).then(|| {
                    map_at = Instant::now();
                    meter.time(Stage::Sounder, || {
                        (s.delay_doppler().cloned(), s.freq_doppler().cloned())
                    })
                });
                let mut o = sh.sounder.lock().unwrap();
                o.status = s.status();
                s.take_fading(&mut o.fading);
                // A GUI that stops collecting does not make this grow.
                let extra = o.fading.len().saturating_sub(600);
                o.fading.drain(..extra);
                if let Some((dd, fd)) = map {
                    if let Some(m) = dd {
                        o.map = Some(Arc::new(m));
                        o.map_new = true;
                    }
                    if let Some(m) = fd {
                        o.freq_map = Some(Arc::new(m));
                        o.freq_new = true;
                    }
                }
            }
        }

        if let Some((d, _)) = &mut decoder {
            meter.time(Stage::Decoder, || {
                if scopes_on && scopes_at.elapsed() >= pace.scopes {
                    scopes_at = Instant::now();
                    d.request_scopes();
                }
                d.push(&block, at);
                d.take_ts(&mut ts_scratch);
            });
            if decoder_pub.elapsed() >= pace.publish {
                decoder_pub = Instant::now();
                let packets = (ts_scratch.len() / 188) as u64;
                feeder.feed(std::mem::take(&mut ts_scratch));
                let measured = (measure_on && measure_at.elapsed() >= pace.measure).then(|| {
                    measure_at = Instant::now();
                    meter.time(Stage::Decoder, || d.measure())
                });
                let mut o = sh.decoder.lock().unwrap();
                o.status = d.status();
                o.standard = d.standard();
                o.t2 = (d.standard() == Standard::Dvbt2).then(|| Box::new(d.dvbt2().clone()));
                o.packets += packets;
                if let Some(s) = d.take_scopes() {
                    o.scopes = Some(Arc::new(s));
                }
                if let Some(Some(m)) = measured {
                    o.measurements = Some(Arc::new(m));
                }
                d.take_heat(&mut heat_scratch);
                o.heat.extend(heat_scratch.drain(..).map(Arc::new));
                let extra = o.heat.len().saturating_sub(MAX_HEAT);
                o.heat.drain(..extra);
            }
        }

        // The waterfall: the windows its rows need go to its thread.
        let centre = source.center_freq();
        meter.time(Stage::Spectrum, || wf.push(&block, centre, tuning));

        if meter.tick() {
            let mut s = sh.stats.lock().unwrap();
            s.describe = source.describe();
            s.sample_rate = rate;
            s.center_hz = source.center_freq();
            s.samples = total;
            if level.n > 0 {
                s.rms_dbfs = (10.0 * (level.power / level.n as f64).max(1e-20).log10()) as f32;
                s.peak = level.peak;
                s.clip_percent = (100.0 * level.clipped as f64 / level.n as f64) as f32;
            }
            level = Level::default();
            s.dropped = drops.as_ref().map_or(0, |d| d());
            s.front_end = front.reading();
            s.load = meter.last;
            s.recording = writer.as_ref().map(|(_, p, b)| Recording {
                path: p.clone(),
                bytes: *b,
            });
        }

        // A file is read as fast as the disk allows; hold it to real time.
        if !live {
            let due = Duration::from_secs_f64(total as f64 / rate);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
    }
    if let Some((w, ..)) = writer {
        let _ = w.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::Result;

    /// A tone, delivered as fast as asked, like a file.
    struct ToneSource {
        k: u64,
        left: usize,
    }

    impl IqSource for ToneSource {
        fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
            out.clear();
            let n = self.left.min(16_384);
            for _ in 0..n {
                let ph = std::f32::consts::TAU * 0.1 * (self.k % 10) as f32;
                out.push(Iq::new(0.5 * ph.cos(), 0.5 * ph.sin()));
                self.k += 1;
            }
            self.left -= n;
            Ok(n)
        }
        fn sample_rate(&self) -> f64 {
            1e6
        }
        fn describe(&self) -> String {
            "tone".into()
        }
    }

    #[test]
    fn samples_after_a_retune_are_cut_off_the_front_until_the_settling_is_over() {
        let mut s = Settle::default();
        s.start(1000.0); // 0.05 s: 50 samples
        let block = |n| vec![Iq::new(1.0, 0.0); n];
        let mut b = block(30);
        assert_eq!(s.trim(&mut b), 30);
        assert!(b.is_empty());
        let mut b = block(30);
        assert_eq!(s.trim(&mut b), 20);
        assert_eq!(b.len(), 10);
        let mut b = block(30);
        assert_eq!(s.trim(&mut b), 0);
        assert_eq!(b.len(), 30);
    }

    #[test]
    fn a_source_yields_rows_and_levels_then_ends() {
        let rx = Rx::start(
            Box::new(ToneSource {
                k: 0,
                left: 600_000,
            }),
            RxOptions {
                fft: 512,
                rows_per_s: 20.0,
                segments_per_row: 4,
            },
            None,
        );
        let t0 = Instant::now();
        let mut rows = Vec::new();
        while rx.running() && t0.elapsed() < Duration::from_secs(5) {
            rows.extend(rx.take_rows());
            std::thread::sleep(Duration::from_millis(20));
        }
        rows.extend(rx.take_rows());
        // 0.6 s of signal at 20 rows/s.
        assert!(rows.len() >= 10, "{} rows", rows.len());
        assert!(rows.iter().all(|r| r.db.len() == 512));
        // The tone is at +0.1 fs: the strongest bin sits there.
        let r = &rows[3];
        let peak = (0..512)
            .max_by(|&a, &b| r.db[a].partial_cmp(&r.db[b]).unwrap())
            .unwrap();
        assert!(peak.abs_diff(256 + 51) <= 1, "peak bin {peak}");
        assert!(rx.stats().ended);
    }

    /// At 60 frames a second the waterfall makes a row a frame: three times
    /// the default's rows.
    #[test]
    fn a_60_fps_display_triples_the_waterfalls_rows() {
        let rows_in_a_while = |smooth: bool| {
            let rx = Rx::start(
                Box::new(ToneSource {
                    k: 0,
                    left: 600_000,
                }),
                RxOptions::default(),
                None,
            );
            if smooth {
                rx.send(Command::DisplayFps(60.0));
            }
            let t0 = Instant::now();
            let mut n = 0;
            while rx.running() && t0.elapsed() < Duration::from_secs(5) {
                n += rx.take_rows().len();
                std::thread::sleep(Duration::from_millis(20));
            }
            n + rx.take_rows().len()
        };
        let (normal, smooth) = (rows_in_a_while(false), rows_in_a_while(true));
        // 0.6 s at 20 and at 60 rows a second.
        assert!((8..=16).contains(&normal), "{normal} rows");
        assert!(smooth > 2 * normal, "{smooth} rows against {normal}");
    }

    #[test]
    fn recording_writes_the_samples_as_cs8() {
        let path = std::env::temp_dir().join("decdvbt-engine-rec.cs8");
        let _ = std::fs::remove_file(&path);
        let rx = Rx::start(
            Box::new(ToneSource {
                k: 0,
                left: 300_000,
            }),
            RxOptions::default(),
            None,
        );
        rx.send(Command::Record(Some(path.clone())));
        let t0 = Instant::now();
        while rx.running() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(rx);
        let len = std::fs::metadata(&path).unwrap().len();
        // The command may land a block or two in; the rest is all there.
        assert!(len > 2 * 250_000 && len <= 2 * 300_000, "{len} bytes");
        let _ = std::fs::remove_file(&path);
    }

    /// A DVB-T signal played through the thread with the sounder on: it
    /// locks, makes fading rows and, after a couple of seconds, a map.
    #[test]
    fn the_sounder_runs_in_the_thread() {
        use decdvbt_core::dvbt::Mode;
        use decdvbt_io::IqFileReader;
        use decdvbt_mod::channel::{Channel, Path as ChPath};
        use decdvbt_mod::sim::{SimOptions, generate};

        let opts = SimOptions {
            mode: Mode::parse("64qam-2/3-8k-1/32").unwrap(),
            cell_id: None,
            ts_file: None,
            seconds: 4.2,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: Channel {
                paths: vec![ChPath::echo(0.0, 0.0), ChPath::echo(3.0, -6.0)],
                cfo_hz: 90_000.0,
                cn_db: Some(28.0),
                ..Default::default()
            },
        };
        let path = std::env::temp_dir().join("decdvbt-engine-sounder.cs8");
        let mut w = decdvbt_io::IqFileWriter::create(&path, SampleFormat::Cs8).unwrap();
        generate(&opts, |b| w.write(b).map_err(|e| e.to_string())).unwrap();
        w.finish().unwrap();

        let src = IqFileReader::open(&path, SampleFormat::Cs8, 10e6, 1 << 17)
            .unwrap()
            .with_center_freq(546e6);
        let rx = Rx::start(Box::new(src), RxOptions::default(), None);
        rx.send(Command::Sounder(Some(546e6)));
        let t0 = Instant::now();
        let (mut rows, mut map, mut status) = (0usize, None, SounderStatus::default());
        let mut fmap = None;
        while t0.elapsed() < Duration::from_secs(7) && map.is_none() {
            std::thread::sleep(Duration::from_millis(100));
            let u = rx.take_sounder();
            status = u.status;
            rows += u.fading.len();
            map = u.map;
            fmap = u.freq_map.or(fmap);
        }
        drop(rx);
        let _ = std::fs::remove_file(&path);
        assert!(status.tracking, "{status:?}");
        assert!(
            (status.cfo_hz - 90_000.0).abs() < 300.0,
            "{}",
            status.cfo_hz
        );
        assert!(rows > 20, "{rows} fading rows");
        // The Doppler spectrum at each carrier came with it.
        let f = fmap.expect("a frequency-Doppler map");
        assert!(f.cols > 100 && f.dopplers > 10);
        let m = map.expect("a map");
        // The strongest path near 0 us and its echo 3 us later.
        let c0 = ((0.0 - m.delay_start_us) / m.delay_step_us).round() as usize;
        let c3 = ((3.0 - m.delay_start_us) / m.delay_step_us).round() as usize;
        let at = |c: usize| {
            (0..m.dopplers)
                .flat_map(|r| (c.saturating_sub(5)..=c + 5).map(move |c| (r, c)))
                .map(|(r, c)| m.at(r, c))
                .fold(f32::MIN, f32::max)
        };
        assert!(
            at(c0) > -1.0 && (at(c3) + 6.0).abs() < 3.0,
            "{} {}",
            at(c0),
            at(c3)
        );
    }

    /// A DVB-T2 signal through the thread: the receiver finds out for itself
    /// that it is T2, reads L1, hands over the packets that went in, and the
    /// scopes and the channel come out too.
    #[test]
    fn a_t2_signal_is_recognised_and_decoded_in_the_thread() {
        use decdvbt_io::IqFileReader;
        use decdvbt_mod::channel::{Channel, Path as ChPath};
        use decdvbt_mod::sim::{T2SimOptions, generate_t2};
        use decdvbt_mod::t2::T2Config;
        use decdvbt_mod::ts::check_test_payload;

        let opts = T2SimOptions {
            cfg: T2Config::parse("8k-1/16-pp4-16qam-3/4-short-ext").unwrap(),
            ts_file: None,
            frames: 14,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: Channel {
                paths: vec![ChPath::echo(0.0, 0.0), ChPath::echo(2.0, -9.0)],
                cfo_hz: 40_000.0,
                cn_db: Some(30.0),
                ..Default::default()
            },
        };
        let path = std::env::temp_dir().join("decdvbt-engine-t2.cs8");
        let mut w = decdvbt_io::IqFileWriter::create(&path, SampleFormat::Cs8).unwrap();
        generate_t2(&opts, |b| w.write(b).map_err(|e| e.to_string())).unwrap();
        w.finish().unwrap();

        let src = IqFileReader::open(&path, SampleFormat::Cs8, 10e6, 1 << 17)
            .unwrap()
            .with_center_freq(546e6);
        let rx = Rx::start(Box::new(src), RxOptions::default(), None);
        let (tap, tap_rx) = decdvbt_ts::TsTap::new(4096);
        rx.set_outputs(Outputs {
            tap: Some(tap),
            ..Default::default()
        });
        rx.send(Command::Decode(Some(546e6)));
        rx.send(Command::Scopes(true));
        rx.send(Command::Measure(true));
        let t0 = Instant::now();
        let (mut ts, mut last) = (Vec::new(), None);
        let (mut scopes, mut measurements) = (None, None);
        // Until a couple of thousand packets are in, before the file ends and
        // the engine plays it again from the start.
        while t0.elapsed() < Duration::from_secs(20) && ts.len() < 188 * 2000 {
            std::thread::sleep(Duration::from_millis(50));
            let u = rx.take_decoder();
            for batch in tap_rx.try_iter() {
                for p in &batch {
                    ts.extend_from_slice(p);
                }
            }
            scopes = u.scopes.or(scopes);
            measurements = u.measurements.or(measurements);
            last = Some((u.standard, u.t2, u.status));
        }
        drop(rx);
        let _ = std::fs::remove_file(&path);
        let (standard, t2, status) = last.expect("a status");
        assert_eq!(standard, Standard::Dvbt2);
        let t2 = t2.expect("T2's status");
        assert!(t2.l1_ok && t2.decoding, "{t2:?}");
        assert!(status.tracking && status.tps_synced && status.decoding);
        let good = ts
            .as_chunks::<188>()
            .0
            .iter()
            .filter(|p| check_test_payload(&p[..]).is_some())
            .count();
        assert!(good > 1000, "{good} good packets of {}", ts.len() / 188);
        let scopes = scopes.expect("scopes");
        assert_eq!(scopes.t2_qam, Some((4, false)));
        assert!(!scopes.data.is_empty() && !scopes.pilots.is_empty());
        let m = measurements.expect("measurements");
        assert!(m.mean_snr_db > 15.0, "{}", m.mean_snr_db);
        assert!(
            m.echoes
                .iter()
                .any(|e| (e.delay_us.abs() - 2.0).abs() < 0.5)
        );
    }

    /// The same kind of signal with the receiver on: it locks, reads the TPS
    /// and hands over transport packets that are the stream that went in.
    #[test]
    fn the_receiver_runs_in_the_thread() {
        use decdvbt_core::dvbt::Mode;
        use decdvbt_io::IqFileReader;
        use decdvbt_mod::channel::{Channel, Path as ChPath};
        use decdvbt_mod::sim::{SimOptions, generate};
        use decdvbt_mod::ts::check_test_payload;

        let opts = SimOptions {
            mode: Mode::parse("64qam-2/3-8k-1/32").unwrap(),
            cell_id: None,
            ts_file: None,
            seconds: 2.4,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: Channel {
                paths: vec![ChPath::echo(0.0, 0.0), ChPath::echo(3.0, -9.0)],
                cfo_hz: 90_000.0,
                cn_db: Some(30.0),
                ..Default::default()
            },
        };
        let path = std::env::temp_dir().join("decdvbt-engine-decoder.cs8");
        let mut w = decdvbt_io::IqFileWriter::create(&path, SampleFormat::Cs8).unwrap();
        generate(&opts, |b| w.write(b).map_err(|e| e.to_string())).unwrap();
        w.finish().unwrap();

        let src = IqFileReader::open(&path, SampleFormat::Cs8, 10e6, 1 << 17)
            .unwrap()
            .with_center_freq(546e6);
        let rx = Rx::start(Box::new(src), RxOptions::default(), None);
        let (tap, tap_rx) = decdvbt_ts::TsTap::new(4096);
        rx.set_outputs(Outputs {
            tap: Some(tap),
            ..Default::default()
        });
        rx.send(Command::Decode(Some(546e6)));
        rx.send(Command::Scopes(true));
        rx.send(Command::Measure(true));
        rx.send(Command::Heat(Some(20.0)));
        let t0 = Instant::now();
        let (mut ts, mut status) = (Vec::new(), None);
        let (mut scopes, mut measurements) = (None, None);
        let mut heat = Vec::new();
        // Until a few thousand packets are in, before the file ends and the
        // engine plays it again from the start (the counters would restart).
        while t0.elapsed() < Duration::from_secs(5) && ts.len() < 188 * 3000 {
            std::thread::sleep(Duration::from_millis(50));
            let u = rx.take_decoder();
            for batch in tap_rx.try_iter() {
                for p in &batch {
                    ts.extend_from_slice(p);
                }
            }
            status = Some(u.status);
            scopes = u.scopes.or(scopes);
            measurements = u.measurements.or(measurements);
            heat.extend(u.heat);
        }
        let packets = rx.take_decoder().packets;
        let view = rx.ts_view();
        drop(rx);
        let _ = std::fs::remove_file(&path);
        let st = status.expect("a status");
        assert!(st.tracking && st.decoding, "{st:?}");
        assert_eq!(
            st.tps.unwrap().modulation,
            decdvbt_core::dvbt::Modulation::Qam64
        );
        let counters: Vec<u32> = ts
            .as_chunks::<188>()
            .0
            .iter()
            .filter_map(|p| check_test_payload(p))
            .collect();
        assert!(
            counters.len() > 500,
            "{} data packets of {packets}",
            counters.len()
        );
        assert!(counters.windows(2).all(|w| w[1] == w[0] + 1));
        // The scopes and the measurements came too, and the hub counted the
        // packets.
        let scopes = scopes.expect("scopes");
        assert!(!scopes.data.is_empty() && !scopes.pilots.is_empty());
        let m = measurements.expect("measurements");
        assert!(m.mean_snr_db > 15.0, "{}", m.mean_snr_db);
        let e = m.echoes.iter().find(|e| e.db > -12.0).expect("an echo");
        assert!((e.delay_us.abs() - 3.0).abs() < 0.5, "{e:?}");
        assert!(view.packets > 0 && view.dropped == 0, "{view:?}");
        // Rows of the channel came too, with the echo in them.
        assert!(heat.len() >= 4, "{} heat rows", heat.len());
        let row = heat.last().unwrap();
        assert!(
            row.echoes
                .iter()
                .any(|e| (e.delay_us.abs() - 3.0).abs() < 0.5 && e.db > -13.0)
        );
    }
}
