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
    /// Start recording to this file (cs8), or stop with `None`.
    Record(Option<PathBuf>),
}

struct Shared {
    rows: Mutex<VecDeque<Row>>,
    stats: Mutex<Stats>,
    stop: AtomicBool,
}

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
    let started = Instant::now();
    let set_error = |e: String| sh.stats.lock().unwrap().error = Some(e);

    while !sh.stop.load(Ordering::Relaxed) {
        loop {
            match cmds.try_recv() {
                Ok(Command::Retune(hz)) => match source.retune(hz) {
                    Ok(true) => {
                        tuning += 1;
                        sh.rows.lock().unwrap().clear();
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
                    continue;
                }
                _ => {
                    sh.stats.lock().unwrap().ended = true;
                    break;
                }
            }
        }
        total += n as u64;

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
}
