// made by CasualArclamp
//! The receive thread: reads an [`IqSource`], builds waterfall rows and level
//! readings, and writes a recording when asked.
//!
//! One thread, one pass over each block, nothing allocated in the loop. The
//! GUI never touches the source: it sends [`Command`]s and takes rows and
//! [`Stats`] from [`Rx`]. M3 adds the OFDM receiver to this loop; the
//! spectrum then comes from its own samples at no extra read.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_core::{Iq, SampleFormat};
use decdvbt_io::{IqFileWriter, IqSource};

use decdvbt_rx::delay_doppler::DelayDoppler;
use decdvbt_rx::receiver::{Receiver as OfdmReceiver, ReceiverConfig, ReceiverStatus};
use decdvbt_rx::sounder::{Sounder, SounderConfig, Status as SounderStatus};

use crate::meter::{Stage, StageLoad, StageMeter};
use crate::spectrum::RowBuilder;

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
}

/// What [`Rx::take_sounder`] hands over.
#[derive(Default)]
pub struct SounderUpdate {
    pub status: SounderStatus,
    /// Fading rows (|H|² in dB at each pilot) made since the last call.
    pub fading: Vec<Vec<f32>>,
    /// The delay–Doppler map, if a new one has been made.
    pub map: Option<Arc<DelayDoppler>>,
}

/// What the receiver has made, waiting for the GUI.
#[derive(Default)]
struct DecoderOut {
    status: ReceiverStatus,
    /// Transport packets (188 bytes each) not yet taken.
    ts: Vec<u8>,
    /// Packets made since the receiver was started, taken or not.
    packets: u64,
}

/// What [`Rx::take_decoder`] hands over.
#[derive(Default)]
pub struct DecoderUpdate {
    pub status: ReceiverStatus,
    /// Transport packets decoded since the last call (188 bytes each).
    pub ts: Vec<u8>,
    /// Packets decoded since the receiver was started.
    pub packets: u64,
}

struct Shared {
    rows: Mutex<VecDeque<Row>>,
    stats: Mutex<Stats>,
    sounder: Mutex<SounderOut>,
    decoder: Mutex<DecoderOut>,
    stop: AtomicBool,
}

/// Transport stream kept for a consumer that is slow to collect it: about
/// 2 MB, a second of a UK mux. What the consumer does not take in time is
/// dropped, oldest first, and counted by the packet continuity it breaks.
const MAX_QUEUED_TS: usize = 188 * 11_000;

/// Rows kept for a GUI that is slow to collect them.
const MAX_QUEUED_ROWS: usize = 100;

/// A running receive thread.
pub struct Rx {
    cmd: Sender<Command>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

/// Reads the source's own drop counter (a HackRF's USB overflows).
pub type DropProbe = Box<dyn Fn() -> u64 + Send>;

impl Rx {
    pub fn start(source: Box<dyn IqSource>, opts: RxOptions, drops: Option<DropProbe>) -> Rx {
        let shared = Arc::new(Shared {
            rows: Mutex::new(VecDeque::new()),
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
        let join = std::thread::Builder::new()
            .name("decdvbt-rx".into())
            .spawn(move || run(source, opts, drops, cmd_rx, sh))
            .expect("cannot start the receive thread");
        Rx {
            cmd,
            shared,
            join: Some(join),
        }
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
        }
    }

    /// What the receiver has made since the last call.
    pub fn take_decoder(&self) -> DecoderUpdate {
        let mut o = self.shared.decoder.lock().unwrap();
        DecoderUpdate {
            status: o.status.clone(),
            ts: std::mem::take(&mut o.ts),
            packets: o.packets,
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
) {
    let rate = source.sample_rate();
    let live = source.is_live();
    let mut builder = RowBuilder::new(opts.fft, rate, opts.rows_per_s, opts.segments_per_row);
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
    let mut decoder: Option<(OfdmReceiver, f64)> = None;
    let mut decoder_pub = Instant::now();
    let mut ts_scratch: Vec<u8> = Vec::new();
    let mut map_at = Instant::now();
    let started = Instant::now();
    let set_error = |e: String| sh.stats.lock().unwrap().error = Some(e);
    // A sounder for the signal at `vfo_hz`, for a source tuned to `centre`.
    let new_sounder = |vfo_hz: f64, centre: f64, rate: f64| {
        // A file without a centre in its name has none to measure from.
        let offset = if centre > 0.0 { vfo_hz - centre } else { 0.0 };
        Sounder::new(SounderConfig::new(rate, offset))
    };
    let new_decoder = |vfo_hz: f64, centre: f64, rate: f64| {
        let offset = if centre > 0.0 { vfo_hz - centre } else { 0.0 };
        OfdmReceiver::new(ReceiverConfig::new(rate, offset))
    };

    while !sh.stop.load(Ordering::Relaxed) {
        loop {
            match cmds.try_recv() {
                Ok(Command::Sounder(vfo)) => {
                    sounder = vfo.map(|v| (new_sounder(v, source.center_freq(), rate), v));
                    *sh.sounder.lock().unwrap() = SounderOut::default();
                }
                Ok(Command::Decode(vfo)) => {
                    decoder = vfo.map(|v| (new_decoder(v, source.center_freq(), rate), v));
                    *sh.decoder.lock().unwrap() = DecoderOut::default();
                }
                Ok(Command::Retune(hz)) => match source.retune(hz) {
                    Ok(true) => {
                        tuning += 1;
                        sh.rows.lock().unwrap().clear();
                        if let Some((s, v)) = &mut sounder {
                            *s = new_sounder(*v, source.center_freq(), rate);
                        }
                        if let Some((d, v)) = &mut decoder {
                            *d = new_decoder(*v, source.center_freq(), rate);
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
        let at = total;
        total += n as u64;

        if let Some((s, _)) = &mut sounder {
            meter.time(Stage::Sounder, || s.push(&block, at));
            if sounder_pub.elapsed() >= Duration::from_millis(100) {
                sounder_pub = Instant::now();
                // The map is made once a second, and only while locked.
                let map = (map_at.elapsed() >= Duration::from_secs(1)).then(|| {
                    map_at = Instant::now();
                    meter.time(Stage::Sounder, || s.delay_doppler().cloned())
                });
                let mut o = sh.sounder.lock().unwrap();
                o.status = s.status();
                s.take_fading(&mut o.fading);
                // A GUI that stops collecting does not make this grow.
                let extra = o.fading.len().saturating_sub(600);
                o.fading.drain(..extra);
                if let Some(Some(m)) = map {
                    o.map = Some(Arc::new(m));
                    o.map_new = true;
                }
            }
        }

        if let Some((d, _)) = &mut decoder {
            meter.time(Stage::Decoder, || {
                d.push(&block, at);
                d.take_ts(&mut ts_scratch);
            });
            if decoder_pub.elapsed() >= Duration::from_millis(100) {
                decoder_pub = Instant::now();
                let mut o = sh.decoder.lock().unwrap();
                o.status = d.status().clone();
                o.packets += (ts_scratch.len() / 188) as u64;
                o.ts.append(&mut ts_scratch);
                // A consumer that stops collecting does not make this grow.
                let extra = o.ts.len().saturating_sub(MAX_QUEUED_TS) / 188 * 188;
                o.ts.drain(..extra);
            }
        }

        if let Some((w, path, bytes)) = &mut writer {
            let r = meter.time(Stage::Record, || w.write(&block));
            *bytes += (n * SampleFormat::Cs8.bytes_per_sample()) as u64;
            if let Err(e) = r {
                set_error(format!("recording to {}: {e}", path.display()));
                writer = None;
            }
        }

        let row = meter.time(Stage::Spectrum, || {
            level.add(&block);
            if builder.push(&block) {
                let mut db = Vec::with_capacity(builder.fft_size());
                builder.take_row(&mut db).map(|_| Row {
                    db,
                    center_hz: source.center_freq(),
                    sample_rate: rate,
                    tuning,
                })
            } else {
                None
            }
        });
        if let Some(row) = row {
            let mut q = sh.rows.lock().unwrap();
            if q.len() == MAX_QUEUED_ROWS {
                q.pop_front();
            }
            q.push_back(row);
        }

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
        while t0.elapsed() < Duration::from_secs(7) && map.is_none() {
            std::thread::sleep(Duration::from_millis(100));
            let u = rx.take_sounder();
            status = u.status;
            rows += u.fading.len();
            map = u.map;
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
        rx.send(Command::Decode(Some(546e6)));
        let t0 = Instant::now();
        let (mut ts, mut status) = (Vec::new(), None);
        // Until a few thousand packets are in, before the file ends and the
        // engine plays it again from the start (the counters would restart).
        while t0.elapsed() < Duration::from_secs(5) && ts.len() < 188 * 3000 {
            std::thread::sleep(Duration::from_millis(50));
            let u = rx.take_decoder();
            ts.extend(u.ts);
            status = Some(u.status);
        }
        let u = rx.take_decoder();
        ts.extend(u.ts);
        let packets = u.packets;
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
    }
}
