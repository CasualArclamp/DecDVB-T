// made by CasualArclamp
//! [`TxSession`]: what the GUI (and the zero-RF test example) drives.
//!
//! Lifecycle (docs/TX_DESIGN.md §2 and §6):
//!
//! ```text
//!   new(cfg) ──prepare()──> Preparing ──prefilled──> Ready ──transmit()──> Transmitting
//!    Idle                   ffmpeg up, modulator       │  (HackRf: needs arm())     │
//!                           warmed, rings filling      │                            ├─ file ends, no loop -> Finishing -> Finished
//!                                                      └──stop()──> Idle            └─ error / stop() -> Idle or Failed
//! ```
//!
//! - `prepare()` starts ffmpeg and the modulator thread but never the radio:
//!   it is safe with any sink. The rings fill to their prefill level
//!   (0.5 s of TS, a full IQ ring) before the state becomes `Ready`.
//! - `transmit()` starts the radio. For a `HackRf` sink it needs a live arm
//!   (consumed by the call); for `Null` and `IqFile` it needs none.
//! - `stop()` (and `Drop`) stops everything and drops the radio, which puts the
//!   HackRF into its off state. After a stop the arm is gone.
//!
//! Threads (all started by the session; none outlive it):
//! `tx-ts-read`, `tx-ff-log` (feeder), `tx-mod` (modulator -> IQ ring),
//! and the radio's thread (`hackrf-tx`, or `tx-virtual`).
//!
//! `tx-mod` does the whole producer side: it finds ffmpeg, probes the file,
//! picks the chain, starts the feeder, builds and warms up the modulator and
//! then makes blocks for the IQ ring until told to stop. `prepare()` therefore
//! returns at once (the NVENC test alone takes 0.7 s the first time). It owns
//! the [`Feeder`], so however it ends (stop, error, panic) ffmpeg is killed.
//! The state is not stored: [`TxSession::status`] derives it from the shared
//! flags each call, and checks the health of what runs (ffmpeg's exit, the
//! radio's thread, the prefill time-out) while it does.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_core::{Error, Result};

use crate::arm::{Arm, LicenceAck};
use crate::config::{HackRfTxConfig, TxConfig, TxSink, VideoCodec};
use crate::cpu::CpuGauge;
use crate::feeder::{Feeder, FeederShared};
use crate::ffmpeg::{self, Chain, VideoInfo};
use crate::iq::{IqGenerator, IqStats};
use crate::modes::VideoProfile;
use crate::preview::{PreviewQueue, PreviewRow};
use crate::radio::{self, Radio, RadioShared, UNDERRUN_ZERO_BYTES, VirtualRadio};
use crate::ring::{IqRing, TsRing};
use crate::si::{SiConfig, SiInjector};
use crate::ts_source::{FeederTs, TsSourceStats};

/// IQ ring: blocks of at least [`IQ_BLOCK_MIN_BYTES`], this many deep. 40
/// blocks of 128 KiB or more is about 0.3 s of signal at 9.14 MS/s.
pub const IQ_RING_BLOCKS: usize = 40;
/// A block is whole OFDM symbols adding up to at least this many bytes.
pub const IQ_BLOCK_MIN_BYTES: usize = 128 * 1024;
/// The TS ring holds this much programme (seconds at the mode's rate), at
/// least 4 MiB: 1.5 s at the top mode.
pub const TS_RING_SECONDS: f64 = 1.5;
/// `Ready` needs this much programme in the TS ring, seconds.
pub const TS_PREFILL_SECONDS: f64 = 0.5;
/// Preparing gives up after this long without reaching `Ready`.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
/// After the last block is handed over, how long the HackRF's USB transfers in
/// flight (three of 256 KiB, 85 ms at 9.14 MS/s) and the driver's pending
/// buffer get to play out before the radio is switched off.
const RADIO_DRAIN: Duration = Duration::from_millis(500);
/// A transmission is ended when more than this fraction of a second of its
/// samples were the zeros of an underrun.
const STARVED_FRACTION: f64 = 0.25;

/// Where the session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    /// Nothing running.
    Idle,
    /// ffmpeg starting, modulator warming up, rings filling.
    Preparing,
    /// Prefilled and waiting. The radio is **not** keyed.
    Ready,
    /// The sink is draining. With a `HackRf` sink the radio is keyed.
    Transmitting,
    /// The file ended (no loop): the last packets are going out.
    Finishing,
    /// Done; the radio is off. `stop()` returns to `Idle`.
    Finished,
    /// Something failed (see `TxStatus::error`); everything is stopped.
    Failed,
}

/// A snapshot for the GUI; cheap to take, call it every frame.
#[derive(Debug, Clone, PartialEq)]
pub struct TxStatus {
    pub state: TxState,
    pub error: Option<String>,
    /// The arm is live (HackRf sinks only mean anything).
    pub armed: bool,
    /// Seconds before the arm lapses.
    pub arm_seconds_left: Option<f32>,
    /// The HackRF is keyed: it is open (`Transmitting` or `Finishing`) and the
    /// sink is `HackRf`. The GUI's big red TRANSMITTING banner is this flag,
    /// nothing else.
    pub radiating: bool,
    pub sample_rate_hz: u32,
    pub txvga_db: u16,
    pub amp: bool,
    /// Seconds of signal given to the radio.
    pub sent_s: f64,
    /// Seconds of programme ffmpeg has encoded.
    pub programme_s: f64,
    /// The mode's useful rate, bit/s.
    pub ts_rate_bps: f64,
    /// What ffmpeg delivered over the last second, bit/s (null padding
    /// included: it is a constant-rate stream).
    pub ts_in_bps: f64,
    /// TS ring fill, 0..1.
    pub ts_fill: f32,
    /// Null packets stuffed because the ring was empty (encoder underruns).
    pub ts_stuffed: u64,
    /// IQ ring fill, 0..1.
    pub iq_fill: f32,
    /// Radio underruns (zeros sent): must stay 0.
    pub underruns: u64,
    /// Modulator thread busy time over the signal time it made, %, over the
    /// last second. 100 means it only just keeps up.
    pub modulator_load_pct: f32,
    /// Signal made per second of wall time, for the unpaced sinks (x real time).
    pub modulator_speed_x: f32,
    /// This process's CPU (100 % = one core) and ffmpeg's.
    pub process_cpu_pct: f32,
    pub ffmpeg_cpu_pct: f32,
    /// ffmpeg's `speed=` (1.0 = real time; above that it is waiting on us).
    pub ffmpeg_speed_x: f32,
    /// ffmpeg said the stream was over budget this many times (should be 0).
    pub dts_pcr_warnings: u64,
    /// The encoder chain in use ("NVENC (CUDA)", "x264 veryfast", ...).
    pub chain: String,
    /// Output level: rms of |c| and the peak component, dBFS, and clipped
    /// samples so far.
    pub level_rms_dbfs: f32,
    pub peak_dbfs: f32,
    pub clipped: u64,
    /// The last lines ffmpeg said that were not progress.
    pub ffmpeg_log: Vec<String>,
}

impl TxStatus {
    /// What `status()` shows before `prepare()`.
    pub fn idle(cfg: &TxConfig, arm: &Arm) -> TxStatus {
        let (txvga_db, amp) = match &cfg.sink {
            TxSink::HackRf(h) => (h.txvga_db, h.amp),
            _ => (0, false),
        };
        TxStatus {
            state: TxState::Idle,
            error: None,
            armed: arm.is_armed(),
            arm_seconds_left: arm.remaining().map(|d| d.as_secs_f32()),
            radiating: false,
            sample_rate_hz: cfg.sample_rate_hz(),
            txvga_db,
            amp,
            sent_s: 0.0,
            programme_s: 0.0,
            ts_rate_bps: cfg.ts_rate_bps(),
            ts_in_bps: 0.0,
            ts_fill: 0.0,
            ts_stuffed: 0,
            iq_fill: 0.0,
            underruns: 0,
            modulator_load_pct: 0.0,
            modulator_speed_x: 0.0,
            process_cpu_pct: 0.0,
            ffmpeg_cpu_pct: 0.0,
            ffmpeg_speed_x: 0.0,
            dts_pcr_warnings: 0,
            chain: String::new(),
            level_rms_dbfs: f32::NEG_INFINITY,
            peak_dbfs: f32::NEG_INFINITY,
            clipped: 0,
            ffmpeg_log: Vec::new(),
        }
    }
}

/// Everything the threads share. All the flags are monotonic (set once).
struct Core {
    /// Everything is to stop: the producer leaves its loop.
    stop: AtomicBool,
    failed: AtomicBool,
    finished: AtomicBool,
    /// The programme is over and the tail is going out (no loop).
    eof: AtomicBool,
    /// The modulator is warmed up and making blocks.
    gen_ready: AtomicBool,
    /// Prefilled (sticky).
    ready: AtomicBool,
    error: Mutex<Option<String>>,
    ts_ring: Arc<TsRing>,
    iq_ring: Arc<IqRing>,
    radio_shared: Arc<RadioShared>,
    /// The radio while it runs. Dropping it switches it off.
    radio: Mutex<Option<Radio>>,
    ts_stats: Arc<TsSourceStats>,
    iq_stats: Arc<IqStats>,
    preview: PreviewQueue,
    feeder: Mutex<Option<Arc<FeederShared>>>,
    info: Mutex<Option<VideoInfo>>,
    chain: Mutex<String>,
    /// ffmpeg's CPU %, as `f32` bits, renewed by the producer.
    ffmpeg_cpu_bits: AtomicU32,
    /// Programme in the TS ring that makes the session `Ready`.
    prefill_packets: usize,
    started: Instant,
}

impl Core {
    /// Record why it failed (the first reason wins), switch the radio off and
    /// stop the producer.
    fn fail(&self, msg: String) {
        {
            let mut e = self.error.lock().unwrap();
            if e.is_none() {
                *e = Some(msg);
            }
        }
        self.failed.store(true, Ordering::Release);
        self.shut_down();
    }

    /// The radio off first, then the producer.
    fn shut_down(&self) {
        // Out of the lock before the drop: dropping joins the radio thread.
        let radio = self.radio.lock().unwrap().take();
        drop(radio);
        self.stop.store(true, Ordering::Release);
        self.ts_ring.abort();
        self.iq_ring.abort();
    }

    /// The end of the transmission: radio off, and `Finished` unless the
    /// radio's own thread said it failed.
    fn finish_radio(&self) {
        let radio = self.radio.lock().unwrap().take();
        let err = radio.as_ref().and_then(|r| r.error(&self.radio_shared));
        drop(radio);
        match err {
            Some(e) => self.fail(e),
            None => {
                if !self.failed.load(Ordering::Acquire) {
                    self.finished.store(true, Ordering::Release);
                }
                self.shut_down();
            }
        }
    }

    /// Notice what ended by itself: ffmpeg dying, the radio's thread stopping.
    fn check_health(&self) {
        if self.failed.load(Ordering::Acquire) || self.finished.load(Ordering::Acquire) {
            return;
        }
        let feeder = self.feeder.lock().unwrap().clone();
        if let Some(e) = feeder.and_then(|f| f.error.lock().unwrap().clone()) {
            self.fail(e);
            return;
        }
        let radio_stopped = self
            .radio
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|r| r.has_stopped(&self.radio_shared));
        if radio_stopped {
            // By its error, or because it reached the end of its input or its
            // `seconds`.
            self.finish_radio();
            return;
        }
        if !self.ready.load(Ordering::Acquire) {
            let fed = feeder_ended(&self.feeder);
            if self.gen_ready.load(Ordering::Acquire)
                && self.iq_ring.len_blocks() >= IQ_RING_BLOCKS
                && (self.ts_ring.len_packets() >= self.prefill_packets || fed)
            {
                self.ready.store(true, Ordering::Release);
            } else if self.started.elapsed() > PREPARE_TIMEOUT {
                let log = feeder_log_tail(&self.feeder);
                self.fail(format!(
                    "not ready after {} s: no stream from ffmpeg{}",
                    PREPARE_TIMEOUT.as_secs(),
                    log
                ));
            }
        }
    }
}

fn feeder_ended(f: &Mutex<Option<Arc<FeederShared>>>) -> bool {
    f.lock()
        .unwrap()
        .as_ref()
        .is_some_and(|f| f.ended.load(Ordering::Acquire))
}

fn feeder_log_tail(f: &Mutex<Option<Arc<FeederShared>>>) -> String {
    match f.lock().unwrap().as_ref() {
        Some(f) => {
            let log = f.log.lock().unwrap();
            if log.is_empty() {
                String::new()
            } else {
                format!(" ({})", log.last().unwrap())
            }
        }
        None => String::new(),
    }
}

/// The PC's offset from UTC, seconds (for the TOT).
#[cfg(windows)]
fn local_utc_offset_s() -> i32 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY: a plain struct of integers; the call fills it in.
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `tz` is a valid out-pointer for the call.
    let id = unsafe { GetTimeZoneInformation(&mut tz) };
    // 1 = standard time, 2 = daylight saving time; Bias is minutes west.
    let bias = tz.Bias
        + if id == 2 {
            tz.DaylightBias
        } else {
            tz.StandardBias
        };
    -bias * 60
}

#[cfg(not(windows))]
fn local_utc_offset_s() -> i32 {
    0
}

/// The producer thread: ffmpeg, the modulator, then blocks into the IQ ring
/// until stopped or the programme ends.
fn producer(core: Arc<Core>, cfg: TxConfig, dump: Option<PathBuf>) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        produce(&core, &cfg, dump.as_deref())
    }));
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) => core.fail(e.to_string()),
        Err(_) => core.fail("internal error in the modulator thread".into()),
    }
}

fn produce(core: &Arc<Core>, cfg: &TxConfig, dump: Option<&Path>) -> Result<()> {
    let ffmpeg_path = ffmpeg::find_ffmpeg(cfg.ffmpeg_path.as_deref()).ok_or_else(|| {
        Error::Other(
            "ffmpeg was not found: install it (winget install Gyan.FFmpeg), put it beside \
             this program, or choose ffmpeg.exe"
                .into(),
        )
    })?;
    let info = ffmpeg::probe(&ffmpeg_path, &cfg.input)?;
    *core.info.lock().unwrap() = info.clone();
    let chain = if cfg.video.video_codec == VideoCodec::Mpeg2 {
        Chain::Mpeg2
    } else {
        ffmpeg::detect_chain(&ffmpeg_path, cfg.video.encoder, info.as_ref())?
    };
    *core.chain.lock().unwrap() = chain.label().to_string();
    if core.stop.load(Ordering::Acquire) {
        return Ok(());
    }

    let rate_bps = cfg.mux_rate_bps();
    let mut profile = VideoProfile::for_rate(rate_bps);
    if let Some((w, h)) = cfg.video.size {
        profile.width = w;
        profile.height = h;
    }
    let args = ffmpeg::command_line(
        cfg,
        info.as_ref(),
        chain,
        &profile,
        ffmpeg::muxer_has_nit(&ffmpeg_path),
    );
    tracing::debug!("ffmpeg {}", args.join(" "));
    // ffmpeg first: it takes 0.3 to 0.9 s to deliver, the modulator's
    // warm-up runs meanwhile.
    let mut feeder = Feeder::spawn(ffmpeg_path, args, core.ts_ring.clone())?;
    *core.feeder.lock().unwrap() = Some(feeder.shared.clone());

    let pps = cfg.ts_rate_bps() / (188.0 * 8.0);
    let si = SiInjector::new(
        SiConfig {
            service: cfg.service.clone(),
            mode: cfg.mode,
            t2: cfg.t2.clone(),
            centre_hz: cfg.centre_hz,
            utc_offset_s: local_utc_offset_s(),
        },
        ffmpeg::service_type(
            if chain == Chain::Mpeg2 {
                VideoCodec::Mpeg2
            } else {
                VideoCodec::H264
            },
            &profile,
        ),
    );
    // The lead-in must outlast what the IQ ring holds, or the programme would
    // be taken into blocks before the session is `Ready` (and be stuffed if the
    // encoder is not there yet).
    let ring_span_s = (IQ_RING_BLOCKS + 2) as f64 * IQ_BLOCK_MIN_BYTES as f64
        / 2.0
        / f64::from(cfg.sample_rate_hz());
    let lead_in_s = (f64::from(cfg.lead_in_ms) / 1000.0).max(ring_span_s + 0.1);
    let feeder_ts = FeederTs::new(
        core.ts_ring.clone(),
        core.ts_stats.clone(),
        Some(si),
        (lead_in_s * pps) as u64,
    )
    .with_rate(pps);
    let feeder_ts = match dump {
        Some(p) => {
            let f = std::fs::File::create(p)
                .map_err(|e| Error::Other(format!("cannot create {}: {e}", p.display())))?;
            feeder_ts.with_dump(std::io::BufWriter::with_capacity(1 << 20, f))
        }
        None => feeder_ts,
    };
    let priming = feeder_ts.priming.clone();
    let mut generator = IqGenerator::new(
        cfg,
        Box::new(feeder_ts),
        priming,
        core.iq_stats.clone(),
        cfg.preview.then(|| core.preview.clone()),
    )?;
    generator.warm_up();
    core.gen_ready.store(true, Ordering::Release);

    let mut last_cpu = Instant::now();
    let mut tail_blocks = 0;
    while !core.stop.load(Ordering::Acquire) {
        let mut block = core.iq_ring.get_block();
        generator.fill(&mut block, IQ_BLOCK_MIN_BYTES);
        if !core.iq_ring.push_blocking(block) {
            break;
        }
        if last_cpu.elapsed() >= Duration::from_secs(1) {
            last_cpu = Instant::now();
            core.ffmpeg_cpu_bits
                .store(feeder.ffmpeg_cpu_pct().to_bits(), Ordering::Relaxed);
        }
        if core.ts_stats.finished.load(Ordering::Acquire) {
            // The ring ran dry because the stream ended. ffmpeg storing an
            // error comes before the ring closes, so look now.
            if let Some(e) = feeder.shared.error.lock().unwrap().clone() {
                return Err(Error::Other(e));
            }
            // The modulator holds the last packets in its interleavers: a few
            // blocks of nulls push them out.
            core.eof.store(true, Ordering::Release);
            tail_blocks += 1;
            if tail_blocks >= 3 {
                break;
            }
        }
    }
    if core.stop.load(Ordering::Acquire) {
        return Ok(()); // `feeder` drops here: ffmpeg is killed
    }

    // End of the programme: let the radio play what is queued, then off.
    core.radio_shared
        .producer_done
        .store(true, Ordering::Release);
    let mut empty_since: Option<Instant> = None;
    while !core.stop.load(Ordering::Acquire) {
        let (present, stopped) = {
            let g = core.radio.lock().unwrap();
            (
                g.is_some(),
                g.as_ref()
                    .is_some_and(|r| r.has_stopped(&core.radio_shared)),
            )
        };
        if present && core.iq_ring.len_blocks() == 0 {
            let t = *empty_since.get_or_insert_with(Instant::now);
            if stopped || t.elapsed() >= RADIO_DRAIN {
                core.finish_radio();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// The running parts. Private: the session is the only way to touch them.
struct Pipeline {
    core: Arc<Core>,
    thread: Option<JoinHandle<()>>,
    window: Window,
}

impl Pipeline {
    /// Radio off, then the producer, then ffmpeg (the producer owns it).
    fn shutdown(&mut self) {
        self.core.shut_down();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Whether `underruns` in `dt` seconds (each a transfer of zeros) were more than
/// [`STARVED_FRACTION`] of the signal at `rate_hz`.
fn is_starving(underruns: u64, rate_hz: f64, dt: f64) -> bool {
    underruns as f64 * (UNDERRUN_ZERO_BYTES as f64 / 2.0) > STARVED_FRACTION * rate_hz * dt
}

/// Rates over the last second or so, renewed as `status()` is called.
struct Window {
    at: Instant,
    bytes_in: u64,
    busy_ns: u64,
    samples: u64,
    ts_in_bps: f64,
    load_pct: f32,
    speed_x: f32,
    underruns: u64,
    /// The radio starved for more than a quarter of the last second.
    starving: bool,
}

impl Window {
    fn new() -> Window {
        Window {
            at: Instant::now(),
            bytes_in: 0,
            busy_ns: 0,
            samples: 0,
            ts_in_bps: 0.0,
            load_pct: 0.0,
            speed_x: 0.0,
            underruns: 0,
            starving: false,
        }
    }

    fn update(&mut self, core: &Core, rate_hz: f64) {
        let dt = self.at.elapsed().as_secs_f64();
        if dt < 1.0 {
            return;
        }
        let bytes_in = core
            .feeder
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |f| f.bytes_in.load(Ordering::Relaxed));
        let busy = core.iq_stats.busy_ns.load(Ordering::Relaxed);
        let samples = core.iq_stats.samples.load(Ordering::Relaxed);
        self.ts_in_bps = (bytes_in - self.bytes_in) as f64 * 8.0 / dt;
        let made_s = (samples - self.samples) as f64 / rate_hz;
        self.speed_x = (made_s / dt) as f32;
        self.load_pct = if made_s > 0.0 {
            (100.0 * (busy - self.busy_ns) as f64 * 1e-9 / made_s) as f32
        } else {
            0.0
        };
        let underruns = core.radio_shared.stats.underruns.load(Ordering::Relaxed);
        self.starving = is_starving(underruns - self.underruns, rate_hz, dt);
        self.underruns = underruns;
        self.at = Instant::now();
        self.bytes_in = bytes_in;
        self.busy_ns = busy;
        self.samples = samples;
    }
}

/// One transmission, from a video file to a sink.
pub struct TxSession {
    config: TxConfig,
    arm: Arm,
    pipeline: Option<Pipeline>,
    /// The TX VGA now (it can change while transmitting).
    live_txvga: u16,
    cpu: CpuGauge,
    ts_dump: Option<PathBuf>,
}

impl TxSession {
    /// Check the config. Touches no process, file or radio.
    pub fn new(config: TxConfig) -> Result<TxSession> {
        config.validate()?;
        let live_txvga = match &config.sink {
            TxSink::HackRf(h) => h.txvga_db,
            _ => 0,
        };
        Ok(TxSession {
            config,
            arm: Arm::new(),
            pipeline: None,
            live_txvga,
            cpu: CpuGauge::new(),
            ts_dump: None,
        })
    }

    /// For the next `prepare()`: write every packet the modulator is given
    /// (after the warm-up) to `path`. A test tool's hook; costs a file write.
    pub fn set_ts_dump(&mut self, path: Option<PathBuf>) {
        self.ts_dump = path;
    }

    pub fn config(&self) -> &TxConfig {
        &self.config
    }

    /// What ffprobe said about the input, once `prepare()` has got that far.
    pub fn video_info(&self) -> Option<VideoInfo> {
        self.pipeline
            .as_ref()
            .and_then(|p| p.core.info.lock().unwrap().clone())
    }

    /// The process id of the running ffmpeg, if there is one (for tests that
    /// check nothing is left behind).
    pub fn ffmpeg_pid(&self) -> Option<u32> {
        let p = self.pipeline.as_ref()?;
        let f = p.core.feeder.lock().unwrap();
        f.as_ref().map(|f| f.pid.load(Ordering::Relaxed))
    }

    /// Start ffmpeg and the modulator; fill the rings. Returns at once: poll
    /// [`Self::status`] for `Ready` (or `Failed`). Never keys the radio.
    pub fn prepare(&mut self) -> Result<()> {
        if self.pipeline.is_some() {
            return Err(Error::Other("already running".into()));
        }
        self.config.validate()?;
        let pps = self.config.ts_rate_bps() / (188.0 * 8.0);
        let capacity = ((4 << 20) / 188).max((TS_RING_SECONDS * pps) as usize);
        let prefill = ((TS_PREFILL_SECONDS * pps) as usize).min(capacity / 2);
        let iq_ring = Arc::new(IqRing::new(IQ_RING_BLOCKS));
        let core = Arc::new(Core {
            stop: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            gen_ready: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            error: Mutex::new(None),
            ts_ring: Arc::new(TsRing::new(capacity)),
            radio_shared: Arc::new(RadioShared::new(iq_ring.clone())),
            iq_ring,
            radio: Mutex::new(None),
            ts_stats: Arc::new(TsSourceStats::default()),
            iq_stats: Arc::new(IqStats::default()),
            preview: PreviewQueue::default(),
            feeder: Mutex::new(None),
            info: Mutex::new(None),
            chain: Mutex::new(String::new()),
            ffmpeg_cpu_bits: AtomicU32::new(0),
            prefill_packets: prefill,
            started: Instant::now(),
        });
        let thread = {
            let (core, cfg, dump) = (core.clone(), self.config.clone(), self.ts_dump.clone());
            std::thread::Builder::new()
                .name("tx-mod".into())
                .spawn(move || producer(core, cfg, dump))
                .map_err(|e| Error::Other(format!("cannot start the modulator thread: {e}")))?
        };
        self.pipeline = Some(Pipeline {
            core,
            thread: Some(thread),
            window: Window::new(),
        });
        Ok(())
    }

    /// Arm the radio for one transmission: lapses after
    /// [`crate::arm::ARM_TIMEOUT`], is used up by [`Self::transmit`], and is
    /// cleared by `stop()` and by an error. `ack` comes from
    /// [`LicenceAck::accept`] with the text the user saw.
    pub fn arm(&mut self, ack: LicenceAck) {
        self.arm.arm(ack);
    }

    pub fn disarm(&mut self) {
        self.arm.disarm();
    }

    pub fn is_armed(&self) -> bool {
        self.arm.is_armed()
    }

    /// Start the sink. `Ready` only. With a `HackRf` sink this is the one path
    /// to the radio: it fails with "not armed" unless [`Self::arm`] was
    /// called in the last minute, and the arm is used up either way.
    pub fn transmit(&mut self) -> Result<()> {
        let needs_arm = self.config.sink.is_radio();
        let proof = self.arm.take();
        if needs_arm && proof.is_none() {
            return Err(Error::Other("not armed".into()));
        }
        let state = self.status().state;
        if state != TxState::Ready {
            return Err(Error::Other(format!("not ready to transmit ({state:?})")));
        }
        let Some(p) = &self.pipeline else {
            return Err(Error::Other("not prepared".into()));
        };
        let shared = p.core.radio_shared.clone();
        let radio = match &self.config.sink {
            TxSink::HackRf(h) => {
                let Some(proof) = proof else {
                    return Err(Error::Other("not armed".into()));
                };
                let h = HackRfTxConfig {
                    txvga_db: self.live_txvga,
                    ..*h
                };
                radio::open_hackrf(proof, &self.config, &h, shared)?
            }
            sink => Radio::Virtual(VirtualRadio::start(
                shared,
                sink,
                self.config.sample_rate_hz(),
            )?),
        };
        *p.core.radio.lock().unwrap() = Some(radio);
        Ok(())
    }

    /// Stop everything: radio off first, then the modulator, then ffmpeg.
    /// Clears the arm. Safe at any time, any number of times.
    pub fn stop(&mut self) {
        self.arm.disarm();
        if let Some(mut p) = self.pipeline.take() {
            p.shutdown();
        }
    }

    /// TX VGA while transmitting (HackRf sink; 0 to 47 dB). The only setting
    /// that changes live; everything else needs a stop.
    pub fn set_txvga(&mut self, db: u16) -> Result<()> {
        let db = db.min(47);
        if let Some(p) = &self.pipeline
            && let Some(r) = p.core.radio.lock().unwrap().as_ref()
        {
            r.set_txvga(db)?;
        }
        // Before the radio is open this sets where it will start.
        self.live_txvga = db;
        Ok(())
    }

    /// A snapshot.
    pub fn status(&mut self) -> TxStatus {
        let Some(p) = &mut self.pipeline else {
            return TxStatus::idle(&self.config, &self.arm);
        };
        let core = &*p.core;
        core.check_health();
        let rate_hz = f64::from(self.config.sample_rate_hz());
        p.window.update(core, rate_hz);
        if p.window.starving && core.radio.lock().unwrap().is_some() {
            // Zeros are going out for much of the time: the modulator is not
            // keeping up (or has hung). A keyed radio sending silence is no
            // use to anyone, so end it, and say why.
            core.fail(
                "the modulator could not keep up with the radio (it sent silence for more                  than a quarter of a second): the PC is too busy, or the mode too heavy"
                    .into(),
            );
        }

        let (radio_open, bytes_out) = {
            let g = core.radio.lock().unwrap();
            (
                g.is_some(),
                g.as_ref().map(|r| r.bytes_out(&core.radio_shared)),
            )
        };
        // After the radio is gone (finished) the virtual radio's count stays.
        let bytes_out =
            bytes_out.unwrap_or_else(|| core.radio_shared.stats.bytes_out.load(Ordering::Relaxed));
        let state = if core.failed.load(Ordering::Acquire) {
            TxState::Failed
        } else if core.finished.load(Ordering::Acquire) {
            TxState::Finished
        } else if radio_open {
            if core.eof.load(Ordering::Acquire) {
                TxState::Finishing
            } else {
                TxState::Transmitting
            }
        } else if core.ready.load(Ordering::Acquire) {
            TxState::Ready
        } else {
            TxState::Preparing
        };

        let (feeder_log, programme_us, speed_x100, dts) = match core.feeder.lock().unwrap().as_ref()
        {
            Some(f) => (
                f.log.lock().unwrap().clone(),
                f.out_time_us.load(Ordering::Relaxed),
                f.speed_x100.load(Ordering::Relaxed),
                f.dts_pcr_warnings.load(Ordering::Relaxed),
            ),
            None => (Vec::new(), 0, 0, 0),
        };
        let (rms, peak) = core.iq_stats.level();
        let (txvga_db, amp) = match &self.config.sink {
            TxSink::HackRf(h) => (self.live_txvga, h.amp),
            _ => (0, false),
        };
        TxStatus {
            state,
            error: core.error.lock().unwrap().clone(),
            armed: self.arm.is_armed(),
            arm_seconds_left: self.arm.remaining().map(|d| d.as_secs_f32()),
            radiating: radio_open && self.config.sink.is_radio(),
            sample_rate_hz: self.config.sample_rate_hz(),
            txvga_db,
            amp,
            sent_s: bytes_out as f64 / 2.0 / rate_hz,
            programme_s: programme_us as f64 * 1e-6,
            ts_rate_bps: self.config.ts_rate_bps(),
            ts_in_bps: p.window.ts_in_bps,
            ts_fill: core.ts_ring.fill(),
            ts_stuffed: core.ts_stats.stuffed.load(Ordering::Relaxed),
            iq_fill: core.iq_ring.fill(),
            underruns: core.radio_shared.stats.underruns.load(Ordering::Relaxed),
            modulator_load_pct: p.window.load_pct,
            modulator_speed_x: p.window.speed_x,
            process_cpu_pct: self.cpu.update(),
            ffmpeg_cpu_pct: f32::from_bits(core.ffmpeg_cpu_bits.load(Ordering::Relaxed)),
            ffmpeg_speed_x: speed_x100 as f32 / 100.0,
            dts_pcr_warnings: dts,
            chain: core.chain.lock().unwrap().clone(),
            level_rms_dbfs: rms,
            peak_dbfs: if peak > 0.0 {
                20.0 * peak.log10()
            } else {
                f32::NEG_INFINITY
            },
            clipped: core.iq_stats.clipped.load(Ordering::Relaxed),
            ffmpeg_log: feeder_log,
        }
    }

    /// Spectrum rows made since the last call (at most 64), oldest first.
    pub fn take_preview_rows(&mut self, out: &mut Vec<PreviewRow>) {
        if let Some(p) = &self.pipeline {
            out.extend(p.core.preview.lock().unwrap().drain(..));
        }
    }
}

impl Drop for TxSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_radio_session_cannot_transmit_unarmed() {
        let mut cfg = TxConfig::new("x.mp4".into());
        cfg.sink = TxSink::HackRf(Default::default());
        let mut s = TxSession::new(cfg).unwrap();
        let e = s.transmit().unwrap_err();
        assert!(e.to_string().contains("not armed"));
    }

    #[test]
    fn a_new_session_is_idle_and_disarmed() {
        let mut s = TxSession::new(TxConfig::new("x.mp4".into())).unwrap();
        let st = s.status();
        assert_eq!(st.state, TxState::Idle);
        assert!(!st.armed);
        assert!(!st.radiating);
    }

    #[test]
    fn transmit_before_prepare_is_an_error() {
        let mut s = TxSession::new(TxConfig::new("x.mp4".into())).unwrap();
        assert!(s.transmit().is_err());
    }

    fn wait_for(s: &mut TxSession, want: impl Fn(TxState) -> bool) -> TxStatus {
        let t = Instant::now();
        loop {
            let st = s.status();
            if want(st.state) {
                return st;
            }
            assert!(
                t.elapsed() < Duration::from_secs(60),
                "stuck in {:?}",
                st.state
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_missing_input_fails_with_a_message_and_leaves_no_thread() {
        let mut cfg = TxConfig::new("definitely-not-here.mp4".into());
        cfg.sink = TxSink::Null { paced: false };
        let mut s = TxSession::new(cfg).unwrap();
        s.prepare().unwrap();
        assert!(s.prepare().is_err(), "a second prepare is refused");
        let st = wait_for(&mut s, |st| st == TxState::Failed);
        let msg = st.error.unwrap();
        assert!(!msg.is_empty());
        s.stop();
        assert_eq!(s.status().state, TxState::Idle);
        // And it can be prepared again.
        s.prepare().unwrap();
        s.stop();
    }

    #[test]
    fn the_radio_cannot_open_in_a_test_even_armed() {
        // T12: armed, ready or not, `open_hackrf` refuses in a test build.
        let mut cfg = TxConfig::new("definitely-not-here.mp4".into());
        cfg.sink = TxSink::HackRf(Default::default());
        let mut s = TxSession::new(cfg).unwrap();
        s.arm(LicenceAck::accept(crate::arm::LICENCE_WARNING).unwrap());
        assert!(s.is_armed());
        let e = s.transmit().unwrap_err();
        // Not prepared, so it is refused before the radio; either way no USB.
        assert!(!e.to_string().is_empty());
        assert!(!s.is_armed(), "the arm is used up by the attempt");
        assert!(!s.status().radiating);
    }

    #[test]
    fn a_radio_that_is_mostly_silence_is_starving() {
        // A transfer of zeros is 131 072 samples, 14 ms at 9.14 MS/s.
        assert!(!is_starving(0, 9_142_857.0, 1.0));
        assert!(!is_starving(3, 9_142_857.0, 1.0)); // a hiccup
        assert!(is_starving(30, 9_142_857.0, 1.0)); // 0.43 s of the second
        assert!(!is_starving(30, 9_142_857.0, 2.0)); // 0.21 s of two
        assert!(!is_starving(10, 20_000_000.0, 2.0));
    }

    #[test]
    fn stop_clears_the_arm() {
        let mut s = TxSession::new(TxConfig::new("x.mp4".into())).unwrap();
        s.arm(LicenceAck::accept(crate::arm::LICENCE_WARNING).unwrap());
        s.stop();
        assert!(!s.is_armed());
    }
}
