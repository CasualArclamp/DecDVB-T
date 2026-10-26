// made by CasualArclamp
//! The receiver: a DVB-T signal in the radio's samples to a transport stream.
//!
//! It is fed blocks of the radio's stream in order and takes from them only
//! the stretch it asks for next, as the sounder does ([`crate::window`]).
//! Per OFDM symbol:
//!
//! 1. [`SymbolReader`](crate::symbol::SymbolReader): the symbol's samples
//!    read at the signal's own rate and clock, the carrier offset taken out,
//!    transformed: the K active carriers.
//! 2. [`Equalizer`](crate::chest::Equalizer): the common phase and timing
//!    slope from the continual pilots, the channel on a 3-carrier grid from
//!    the scattered ones, every carrier divided by its estimate. The timing
//!    slope steers the reader.
//! 3. [`TpsReceiver`](crate::tps_rx::TpsReceiver): the frame found from the
//!    TPS sync word; the symbol's place in it and the transmission's
//!    parameters.
//! 4. With those: [`Demapper`](crate::demap::Demapper) (soft bits, deinter-
//!    leaved), [`Viterbi`](decdvbt_fec::viterbi::Viterbi) (the inner code) and
//!    [`OuterDecoder`](crate::outer::OuterDecoder) (byte sync, byte deinter-
//!    leaving, Reed–Solomon, descrambling): transport packets.

use std::time::{Duration, Instant};

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{Alpha, Bandwidth, FftSize, Guard, Modulation, Rate, qef_cn_db};
use decdvbt_fec::viterbi::Viterbi;
use decdvbt_ofdm::pilots::{SymbolPlan, prbs};
use decdvbt_ofdm::tps::TpsFrame;

use crate::chest::Equalizer;
use crate::demap::Demapper;
use crate::interp::Interpolator;
use crate::lock::{ACQ_NATIVE, Failure, lock_on};
use crate::notch::{CarrierWatch, NotchStat};
use crate::outer::{OuterDecoder, OuterStats};
use crate::scope::{self, MeasureState, Measurements, Scopes};
use crate::symbol::{PAD, SymbolReader};
use crate::tone::{ToneCanceller, ToneStat};
use crate::tps_rx::TpsReceiver;
use crate::window::Window;

/// The wait between attempts to acquire, seconds.
const RETRY_S: f64 = 0.2;
/// Symbols with no coherence in a row that end the lock.
const LOST_AFTER: u32 = 24;
const MIN_COHERENCE: f32 = 0.2;
/// The lock's health is looked at every this many symbols (half a second),
/// and a lock that has done no good for this many looks in a row is dropped
/// to be found again.
const HEALTH_SYMBOLS: u64 = 512;
const STALLED_LOOKS: u32 = 4;
/// The signal-to-noise ratio is looked at every this many symbols (an eighth
/// of a second). A lock whose ratio has been under its floor for
/// [`POOR_LOOKS`] looks in a row is dropped. The floor is [`UNDECODABLE_DB`]
/// under what the mode needs for quasi error free reception (and never under
/// [`MIN_SNR_DB`]): nothing comes out that far below it, and a lock that has
/// gone onto samples that are not the signal (the old channel's, as a retune
/// leaves them ahead of the new one's) wanders between about −4 and 6 dB while
/// the pilots still seem to line up, which is why this is not left to
/// coherence.
const QUALITY_SYMBOLS: u64 = 128;
/// Heat rows kept for a consumer that is slow to take them.
const HEAT_QUEUE: usize = 256;
const MIN_SNR_DB: f32 = 2.0;
const UNDECODABLE_DB: f32 = 8.0;
const POOR_LOOKS: u32 = 4;
/// The delay-domain filter on the channel estimate goes on when the effective
/// SNR is under what the mode needs plus the first figure and at least the
/// share given of the Reed–Solomon blocks needed mending in the last look; off
/// again above the second figure, or after the given looks in a row (4 s) with
/// under the last share needing it.
const DENOISE_ON_MARGIN_DB: f32 = 4.0;
const DENOISE_OFF_MARGIN_DB: f32 = 6.0;
const DENOISE_BUSY_FRAC: f32 = 0.02;
const DENOISE_IDLE_FRAC: f32 = 0.001;
const DENOISE_IDLE_LOOKS: u32 = 32;

/// Is the signal there? Counts looks at the signal-to-noise ratio under the
/// floor in a row.
#[derive(Debug, Default)]
struct Quality {
    poor: u32,
}

impl Quality {
    /// One look at `snr_db` against `floor_db`: true when it has been under
    /// for [`POOR_LOOKS`] looks in a row.
    fn look(&mut self, snr_db: f32, floor_db: f32) -> bool {
        self.poor = if snr_db < floor_db { self.poor + 1 } else { 0 };
        self.poor >= POOR_LOOKS
    }

    /// The floor for a lock whose TPS says `tps` (none yet: the least).
    fn floor(tps: Option<&TpsFrame>) -> f32 {
        tps.map_or(MIN_SNR_DB, |i| {
            (qef_cn_db(i.modulation, i.rate_hp) - UNDECODABLE_DB).max(MIN_SNR_DB)
        })
    }
}

/// Is the lock doing any good? A lock that is coherent (the pilots are
/// there) can still be useless: wrong parameters, a channel estimate that went
/// bad. The signs are the TPS not syncing, or syncing and decoding, but not
/// one Reed–Solomon block coming out clean or mended.
#[derive(Debug, Default)]
struct Health {
    good: u64,
    stalled: u32,
}

impl Health {
    /// One look: whether the TPS is `synced`, whether the chain is `decoding`
    /// and how many blocks have come out right (`good`, counted since the
    /// chain began). True when the lock has been no use for [`STALLED_LOOKS`]
    /// looks.
    fn look(&mut self, synced: bool, decoding: bool, good: u64) -> bool {
        let working = synced && (!decoding || good > self.good);
        self.good = good;
        self.stalled = if working { 0 } else { self.stalled + 1 };
        self.stalled >= STALLED_LOOKS
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReceiverConfig {
    /// The radio's sample rate.
    pub fs_in: f64,
    /// How far above the radio's centre the signal's centre is.
    pub vfo_offset_hz: f64,
    pub bandwidth: Bandwidth,
}

impl ReceiverConfig {
    pub fn new(fs_in: f64, vfo_offset_hz: f64) -> ReceiverConfig {
        ReceiverConfig {
            fs_in,
            vfo_offset_hz,
            bandwidth: Bandwidth::Mhz8,
        }
    }
}

/// Wall time spent in each stage of the receiver, since the start: where
/// the CPU goes.
#[derive(Debug, Clone, Copy, Default)]
pub struct Profile {
    /// Acquisition attempts.
    pub acquire: Duration,
    /// Reading the symbols from the samples: interpolation and the transform.
    pub read: Duration,
    /// The channel estimate and the equalised cells.
    pub equalise: Duration,
    /// The TPS carriers.
    pub tps: Duration,
    /// Soft bits, deinterleaved.
    pub demap: Duration,
    /// The inner code.
    pub viterbi: Duration,
    /// Byte sync, byte deinterleaver, Reed–Solomon, descrambler.
    pub outer: Duration,
}

/// What the receiver knows, for the display and the logs.
#[derive(Debug, Clone, Default)]
pub struct ReceiverStatus {
    /// A signal is locked on and being read.
    pub tracking: bool,
    /// The TPS frame is found: the position in the frame is known.
    pub tps_synced: bool,
    /// The inner and outer decoders are running.
    pub decoding: bool,
    /// The TPS says the signal is hierarchical, which is not decoded yet.
    pub hierarchical: bool,
    pub fft: Option<FftSize>,
    pub guard: Option<Guard>,
    pub tps: Option<TpsFrame>,
    /// The signal's offset from the radio's centre, Hz, and the radio's
    /// clock against the signal's, ppm.
    pub cfo_hz: f64,
    pub clock_ppm: f64,
    /// Signal-to-noise ratio of the data cells, dB: the mean channel power
    /// over the mean noise power the pilots show (once decoding).
    pub snr_db: f32,
    /// The SNR a Gaussian channel would need to carry what the cells carry,
    /// dB: below `snr_db` on a channel with notches, and the figure to set
    /// against the mode's QEF C/N (see [`crate::capacity`]).
    pub effective_snr_db: f32,
    pub coherence: f32,
    /// The channel's maximum Doppler, Hz, as the continual pilots' correlation
    /// in time fits it (`None` until a quarter second has been seen; a steady
    /// channel reads 0), and how the channel estimate is being made: the time
    /// filter for a moving channel instead of the IIR (`mobile`), and the
    /// delay-domain filter for a signal near its threshold (`denoise`).
    pub doppler_hz: Option<f32>,
    pub mobile: bool,
    pub denoise: bool,
    /// Symbols read since the lock, and locks since the start.
    pub symbols: u64,
    pub locks: u32,
    /// Acquisition attempts since the last lock, and the best cyclic-prefix
    /// contrast so far.
    pub attempts: u32,
    pub contrast: f32,
    pub tps_frames_ok: u64,
    pub tps_frames_bad: u64,
    /// Acquisition attempts that found no cyclic prefix, and ones that found
    /// one but not DVB-T's pilots, since the receiver began.
    pub acq_no_prefix: u32,
    pub acq_not_dvbt: u32,
    /// Locks given up as no use (no data coming out, see [`Health`], or no
    /// signal-to-noise ratio, see [`Quality`]), since the receiver began.
    pub stalled_locks: u32,
    pub outer: OuterStats,
    pub profile: Profile,
    /// Tones found in the channel and erased (see [`crate::notch`]).
    pub notch: NotchStat,
    /// Strong tones taken out of the samples (see [`crate::tone`]).
    pub tone: ToneStat,
}

/// The inner and outer decoders for one set of TPS parameters.
struct Chain {
    modulation: Modulation,
    rate: Rate,
    demap: Demapper,
    viterbi: Viterbi,
    outer: OuterDecoder,
    soft: Vec<i8>,
    bytes: Vec<u8>,
}

impl Chain {
    fn new(fft: FftSize, modulation: Modulation, rate: Rate) -> Chain {
        Chain {
            modulation,
            rate,
            demap: Demapper::new(fft, modulation),
            viterbi: Viterbi::new(rate.into()),
            outer: OuterDecoder::new(),
            soft: Vec::new(),
            bytes: Vec::new(),
        }
    }
}

struct Track {
    reader: SymbolReader,
    eq: Equalizer,
    tps: TpsReceiver,
    /// The next symbol's scattered-pilot phase (its index in the frame mod 4,
    /// as the lock found it).
    phase: usize,
    guard: Guard,
    /// 3/4 · (1 − 2 w_k) per carrier: the reciprocal of a pilot's value.
    inv: Vec<f32>,
    chain: Option<Chain>,
    bad: u32,
    symbols: u64,
    health: Health,
    quality: Quality,
    /// Narrowband interference: tones in the channel, erased in the demapper.
    watch: CarrierWatch,
    denoise: DenoiseGate,
}

/// When the delay-domain filter on the channel estimate is worth its CPU: the
/// signal is near what the mode needs *and* the Reed–Solomon code is at work
/// (a static signal that comes out clean, however small its margin, does
/// without it). Looked at every [`QUALITY_SYMBOLS`].
#[derive(Debug, Default)]
struct DenoiseGate {
    /// The Reed–Solomon counts at the last look: blocks, and blocks that
    /// needed mending or could not be mended.
    blocks: u64,
    busy: u64,
    /// Looks in a row with the code idle.
    idle: u32,
}

impl DenoiseGate {
    /// One look, with the margin of the effective SNR over the mode's QEF C/N
    /// (dB) and the code's counts; whether the filter is wanted, given that it
    /// is `on` now.
    fn look(&mut self, on: bool, margin_db: f32, o: &OuterStats) -> bool {
        let busy = o.corrected + o.uncorrectable;
        // Counts that went back (a new chain) start the reckoning afresh.
        let (db, dbusy) = if o.blocks >= self.blocks && busy >= self.busy {
            (o.blocks - self.blocks, busy - self.busy)
        } else {
            (0, 0)
        };
        (self.blocks, self.busy) = (o.blocks, busy);
        let frac = if db > 0 {
            dbusy as f32 / db as f32
        } else {
            0.0
        };
        self.idle = if frac < DENOISE_IDLE_FRAC {
            self.idle + 1
        } else {
            0
        };
        if on {
            margin_db < DENOISE_OFF_MARGIN_DB && self.idle < DENOISE_IDLE_LOOKS
        } else {
            margin_db < DENOISE_ON_MARGIN_DB && frac >= DENOISE_BUSY_FRAC
        }
    }
}

pub struct Receiver {
    cfg: ReceiverConfig,
    rho: f64,
    fs_native: f64,
    interp: Interpolator,
    win: Window,
    track: Option<Track>,
    tone: ToneCanceller,
    status: ReceiverStatus,
    next_attempt: u64,
    ts: Vec<u8>,
    carriers: Vec<Iq>,
    /// The display's asks: the next symbol's cells wanted, the ones made,
    /// the carriers as read (kept when wanted) and what making them needs.
    scope_wanted: bool,
    scopes: Option<Scopes>,
    raw: Vec<Iq>,
    plan: Option<SymbolPlan>,
    measure_state: MeasureState,
    /// Rows of the channel's gain, signal-to-noise ratio and impulse response
    /// wanted a second (0: none), and the ones made and not yet taken.
    heat_rate: f64,
    heat: Vec<Measurements>,
    /// Diagnostics, for comparing on a capture: env DECDVBT_DENOISE=0|1 and
    /// DECDVBT_MOBILE=0|1 force the channel estimate's delay-domain filter and
    /// its time filter off or on instead of leaving them to the signal.
    dev_denoise: Option<bool>,
    dev_mobile: Option<bool>,
}

impl Receiver {
    pub fn new(cfg: ReceiverConfig) -> Receiver {
        let fs_native = cfg.bandwidth.sample_rate_hz();
        Receiver {
            cfg,
            rho: cfg.fs_in / fs_native,
            fs_native,
            interp: Interpolator::new(),
            win: Window::new(),
            track: None,
            tone: ToneCanceller::new(cfg.fs_in, cfg.vfo_offset_hz),
            status: ReceiverStatus::default(),
            next_attempt: 0,
            ts: Vec::new(),
            carriers: Vec::new(),
            scope_wanted: false,
            scopes: None,
            raw: Vec::new(),
            plan: None,
            measure_state: MeasureState::default(),
            heat_rate: 0.0,
            heat: Vec::new(),
            dev_denoise: std::env::var("DECDVBT_DENOISE").ok().map(|v| v == "1"),
            dev_mobile: std::env::var("DECDVBT_MOBILE").ok().map(|v| v == "1"),
        }
    }

    pub fn status(&self) -> &ReceiverStatus {
        &self.status
    }

    /// Ask for the cells of the next symbol (for [`take_scopes`](Self::take_scopes)):
    /// nothing is copied or made until this is called, and then only once.
    pub fn request_scopes(&mut self) {
        self.scope_wanted = true;
    }

    /// The cells asked for, once the next symbol has come.
    pub fn take_scopes(&mut self) -> Option<Scopes> {
        self.scopes.take()
    }

    /// Make a row of the channel (as [`measure`](Self::measure) gives it)
    /// `rows_per_s` times a second, for a heat map of it over time; 0 stops.
    /// Rows cost a transform each and are made only while asked for.
    pub fn set_heat(&mut self, rows_per_s: f64) {
        self.heat_rate = rows_per_s.max(0.0);
        if self.heat_rate == 0.0 {
            self.heat.clear();
        }
    }

    /// The rows made since the last call, oldest first, appended to `out`.
    pub fn take_heat(&mut self, out: &mut Vec<Measurements>) {
        out.append(&mut self.heat);
    }

    /// The channel as the last symbol's estimate has it (the gain and the
    /// signal-to-noise ratio across the band, the impulse response and its
    /// echoes), or `None` with no lock or before the estimate is whole.
    pub fn measure(&mut self) -> Option<Measurements> {
        let t = self.track.as_ref().filter(|t| t.eq.ready())?;
        let fft = t.reader.fft;
        let guard_us = (fft.n() as f64 / t.guard.divisor() as f64 / self.fs_native * 1e6) as f32;
        Some(scope::measure(
            &mut self.measure_state,
            fft.n(),
            self.fs_native,
            guard_us,
            t.eq.channel(),
            &t.eq.w,
            &t.eq.noise,
        ))
    }

    /// Forget the lock and the stretch wanted: the stream has jumped.
    pub fn reset(&mut self) {
        self.win.cancel();
        self.track = None;
        self.next_attempt = 0;
        self.status.tracking = false;
        self.status.tps_synced = false;
        self.status.decoding = false;
    }

    /// Transport packets (188 bytes each) decoded since the last call,
    /// appended to `out`.
    pub fn take_ts(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.ts);
    }

    /// Hand over a block of the radio's samples, whose first is the stream's
    /// `abs_start`-th.
    pub fn push(&mut self, block: &[Iq], abs_start: u64) {
        // A strong CW tone is taken out of the samples first (a copy, only
        // while one is held).
        if self.tone.process(block, abs_start) {
            let cleaned = self.tone.take_out();
            self.push_samples(&cleaned, abs_start);
            self.tone.give_back(cleaned);
        } else {
            self.push_samples(block, abs_start);
        }
        self.status.tone = self.tone.stat();
    }

    fn push_samples(&mut self, block: &[Iq], abs_start: u64) {
        if !self.win.is_wanting() {
            self.want_acquisition(abs_start.max(self.next_attempt));
        }
        loop {
            if !self.win.feed(block, abs_start) {
                return;
            }
            if self.track.is_some() {
                self.track_symbol();
            } else {
                self.try_acquire();
            }
            if !self.win.is_wanting() {
                return;
            }
        }
    }

    fn want_acquisition(&mut self, start: u64) {
        let len = (ACQ_NATIVE as f64 * self.rho) as usize + 2 * PAD;
        self.win.want(start, len);
    }

    fn want_symbol(&mut self) {
        let Some(t) = &self.track else { return };
        match t.reader.want() {
            Some((start, len)) => self.win.want(start, len),
            None => self.drop_lock(),
        }
    }

    /// Back to searching, a while on.
    fn drop_lock(&mut self) {
        self.win.cancel();
        self.track = None;
        self.status.tracking = false;
        self.status.tps_synced = false;
        self.status.decoding = false;
    }

    fn try_acquire(&mut self) {
        let t0 = Instant::now();
        self.acquire_inner();
        self.status.profile.acquire += t0.elapsed();
    }

    fn acquire_inner(&mut self) {
        self.status.attempts += 1;
        let (start, len) = self.win.wanted().unwrap();
        let next = start + len as u64;
        self.next_attempt = start + (RETRY_S * self.cfg.fs_in) as u64;
        // The first symbol read is the first at least a symbol past the end
        // of the stretch, so its samples are still to come.
        let min_apex = next as f64 / self.rho + (8192 + 2048) as f64;
        let l = match lock_on(
            &self.interp,
            self.win.planar(),
            start,
            self.rho,
            self.fs_native,
            self.cfg.vfo_offset_hz,
            min_apex,
        ) {
            Ok(l) => l,
            Err(f) => {
                let c = match f {
                    Failure::NoPrefix(c) => {
                        self.status.acq_no_prefix += 1;
                        c
                    }
                    Failure::NotDvbt(c) => {
                        self.status.acq_not_dvbt += 1;
                        c
                    }
                };
                self.status.contrast = self.status.contrast.max(c);
                self.win.cancel();
                return;
            }
        };
        self.status.contrast = self.status.contrast.max(l.contrast);
        let k_total = l.fft.active_carriers();
        let inv = prbs(k_total)
            .iter()
            .map(|&w| 0.75 * (1.0 - 2.0 * f32::from(w)))
            .collect();
        let mut reader = SymbolReader::new(
            l.fft,
            l.guard,
            l.apex,
            l.f_total,
            self.rho,
            self.fs_native,
            1,
        );
        // The cyclic prefix finds the symbol; the pilots' phase slope, a few
        // hundredths of a sample accurate, does the fine work.
        reader.alpha_cp = 0.05;
        reader.beta_cp = 0.0005;
        let mut eq = Equalizer::new(l.fft);
        eq.set_guard(l.guard);
        eq.forced = self.dev_mobile;
        self.track = Some(Track {
            reader,
            eq,
            tps: TpsReceiver::new(l.fft),
            phase: l.phase,
            guard: l.guard,
            inv,
            chain: None,
            bad: 0,
            symbols: 0,
            health: Health::default(),
            quality: Quality::default(),
            watch: {
                // The continual pilots are 4/3 in amplitude and always there;
                // the scattered ones (a carrier in three) the same, one
                // symbol in four (EN 300 744, 4.5).
                let mut w = CarrierWatch::new();
                let mut expect = vec![1.0f32; l.fft.active_carriers()];
                for k in (0..expect.len()).step_by(3) {
                    expect[k] = 1.0 + (16.0 / 9.0 - 1.0) / 4.0;
                }
                for &k in decdvbt_ofdm::pilots::continual_pilots(l.fft) {
                    if let Some(e) = expect.get_mut(k as usize) {
                        *e = 16.0 / 9.0;
                    }
                }
                w.set_expected(expect);
                w
            },
            denoise: DenoiseGate::default(),
        });
        self.status.locks += 1;
        self.status.attempts = 0;
        self.status.tracking = true;
        self.status.tps_synced = false;
        self.status.decoding = false;
        self.status.fft = Some(l.fft);
        self.status.guard = Some(l.guard);
        self.status.tps = None;
        self.status.symbols = 0;
        self.want_symbol();
    }

    fn track_symbol(&mut self) {
        let Some(mut t) = self.track.take() else {
            return;
        };
        let (win_start, _) = self.win.wanted().unwrap();
        let mut carriers = std::mem::take(&mut self.carriers);
        let t0 = Instant::now();
        let read = if t.reader.is_direct() {
            t.reader
                .read_direct(self.win.samples(), win_start, &mut carriers)
        } else {
            t.reader
                .read(&self.interp, self.win.planar(), win_start, &mut carriers)
        };
        if read.is_err() {
            self.carriers = carriers;
            self.drop_lock();
            return;
        }
        let t1 = Instant::now();
        if self.scope_wanted {
            self.raw.clear();
            self.raw.extend_from_slice(&carriers);
        }
        // Narrowband interference: every second symbol's power per carrier
        // goes into the watcher's average, the flags are redone every 64
        // symbols, and the flagged carriers' noise goes up by the tone's
        // power (so the demapper gives their cells no weight).
        if t.symbols % 2 == 0 {
            t.watch.observe(&carriers);
            if t.watch.seen() % 32 == 0 {
                t.watch.refresh();
                self.status.notch = t.watch.stat();
            }
        }
        t.eq.process(&mut carriers, t.phase);
        t.watch.add_to_noise(&mut t.eq.noise);
        t.reader.correct(f64::from(t.eq.timing_error()));
        let t2 = Instant::now();
        let pr = &mut self.status.profile;
        pr.read += t1 - t0;
        pr.equalise += t2 - t1;

        // Coherence of neighbouring scattered pilots: high on a signal.
        let k_total = carriers.len();
        let (mut num, mut den) = (Iq::new(0.0, 0.0), 0.0f32);
        let mut prev: Option<Iq> = None;
        for k in (3 * t.phase..k_total).step_by(12) {
            let h = carriers[k] * t.inv[k];
            if let Some(p) = prev {
                num += h * p.conj();
                den += h.norm() * p.norm();
            }
            prev = Some(h);
        }
        let coherence = if den > 0.0 { num.norm() / den } else { 0.0 };
        t.bad = if coherence < MIN_COHERENCE {
            t.bad + 1
        } else {
            0
        };
        self.carriers = carriers;

        if self.scope_wanted {
            self.scope_wanted = false;
            let fft = t.reader.fft;
            if self.plan.as_ref().is_none_or(|p| p.fft != fft) {
                self.plan = Some(SymbolPlan::new(fft));
            }
            if let Some(plan) = &self.plan {
                self.scopes = Some(scope::snapshot(
                    plan,
                    t.phase,
                    &self.raw,
                    &t.eq.z,
                    self.status.tps.map(|i| i.modulation),
                    t.symbols,
                ));
            }
        }

        let phase_now = t.phase;
        t.phase = (t.phase + 1) % 4;
        t.symbols += 1;

        if self.heat_rate > 0.0 && t.eq.ready() {
            // A row every so many symbols (the symbol rate over the rows').
            let fft = t.reader.fft;
            let sym_s = (fft.n() + fft.n() / t.guard.divisor()) as f64 / self.fs_native;
            let every = (1.0 / (sym_s * self.heat_rate)).round().max(1.0) as u64;
            if t.symbols % every == 0 {
                let guard_us =
                    (fft.n() as f64 / t.guard.divisor() as f64 / self.fs_native * 1e6) as f32;
                if self.heat.len() >= HEAT_QUEUE {
                    self.heat.remove(0);
                }
                self.heat.push(scope::measure(
                    &mut self.measure_state,
                    fft.n(),
                    self.fs_native,
                    guard_us,
                    t.eq.channel(),
                    &t.eq.w,
                    &t.eq.noise,
                ));
            }
        }

        // The TPS: where in the frame this symbol is.
        let t3 = Instant::now();
        let pos = t.tps.push(&t.eq.z, &t.eq.w);
        self.status.profile.tps += t3.elapsed();
        let synced = t.tps.synced();
        if let Some(p) = pos
            && synced
            && p.l as usize % 4 != phase_now
        {
            // The lock's pilot phase disagrees with the frame: the
            // equaliser has been working from the wrong pilots.
            self.drop_lock();
            return;
        }
        match (pos, synced) {
            (Some(p), true) => {
                if let Some(info) = t.tps.info().copied() {
                    self.status.tps = Some(info);
                    let fft = t.reader.fft;
                    if info.fft != fft || info.guard != t.guard {
                        // What the TPS says is not what we locked on.
                        self.drop_lock();
                        return;
                    }
                    self.status.hierarchical = info.alpha != Alpha::NonHierarchical;
                    let stale = t
                        .chain
                        .as_ref()
                        .is_none_or(|c| (c.modulation, c.rate) != (info.modulation, info.rate_hp));
                    if self.status.hierarchical {
                        t.chain = None;
                    } else if stale {
                        t.chain = Some(Chain::new(fft, info.modulation, info.rate_hp));
                    }
                }
                if let Some(c) = &mut t.chain {
                    let (u0, pr) = (Instant::now(), &mut self.status.profile);
                    c.demap
                        .symbol(p.l as usize, &t.eq.z, &t.eq.w, &t.eq.noise, &mut c.soft);
                    let u1 = Instant::now();
                    c.bytes.clear();
                    c.viterbi.push(&c.soft, &mut c.bytes);
                    let u2 = Instant::now();
                    c.outer.push(&c.bytes, &mut self.ts);
                    let u3 = Instant::now();
                    pr.demap += u1 - u0;
                    pr.viterbi += u2 - u1;
                    pr.outer += u3 - u2;
                    self.status.outer = c.outer.stats;
                    self.status.snr_db = c.demap.snr_db();
                    self.status.effective_snr_db = c.demap.effective_snr_db();
                }
            }
            (Some(_), false) => {
                // The frame was lost: the decoders start over when it is
                // found again.
                t.chain = None;
            }
            _ => {}
        }

        self.status.tps_synced = synced;
        self.status.decoding = synced && t.chain.is_some();
        self.status.coherence = coherence;
        if t.symbols % QUALITY_SYMBOLS == 0 {
            let fft = t.reader.fft;
            let ts = (fft.n() + fft.n() / t.guard.divisor()) as f64 / self.fs_native;
            self.status.doppler_hz = t.eq.doppler_hz(ts);
        }
        self.status.mobile = t.eq.mobile();
        self.status.denoise = t.eq.denoising();
        self.status.cfo_hz = t.reader.f_total;
        self.status.clock_ppm = t.reader.rate * 1e6;
        self.status.symbols = t.symbols;
        self.status.tps_frames_ok = t.tps.frames_ok;
        self.status.tps_frames_bad = t.tps.frames_bad;

        if t.bad >= LOST_AFTER {
            self.drop_lock();
            return;
        }
        if t.symbols % QUALITY_SYMBOLS == 0
            && let Some(info) = self.status.tps
            && self.status.decoding
            && !self.status.hierarchical
        {
            // The delay-domain filter on the channel estimate costs about 40 %
            // of the equaliser's time and buys what a signal with little
            // margin over the mode's threshold needs (up to ~1 dB of MER):
            // see `DenoiseGate`.
            let margin = self.status.effective_snr_db - qef_cn_db(info.modulation, info.rate_hp);
            let wanted = t.denoise.look(t.eq.denoising(), margin, &self.status.outer);
            let on = self.dev_denoise.unwrap_or(wanted);
            t.eq.set_denoise(on);
        }
        if t.symbols % QUALITY_SYMBOLS == 0
            && t.eq.ready()
            && t.quality
                .look(t.eq.snr_db(), Quality::floor(self.status.tps.as_ref()))
        {
            self.status.stalled_locks += 1;
            self.drop_lock();
            return;
        }
        if t.symbols % HEALTH_SYMBOLS == 0 {
            let o = &self.status.outer;
            if t.health
                .look(synced, t.chain.is_some(), o.clean + o.corrected)
            {
                self.status.stalled_locks += 1;
                self.drop_lock();
                return;
            }
        }
        self.track = Some(t);
        self.want_symbol();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::dvbt::Mode;
    use decdvbt_mod::channel::{Channel, Path};
    use decdvbt_mod::sim::{SimOptions, generate};
    use decdvbt_mod::ts::check_test_payload;

    /// Run `seconds` of `mode_text` through `ch` and the receiver, in blocks
    /// of an awkward size; the transport stream it made, and the receiver.
    fn run(mode_text: &str, ch: Channel, seconds: f64) -> (Vec<u8>, Receiver) {
        run_at(mode_text, ch, seconds, 10e6)
    }

    /// The same at the radio's rate `fs`.
    fn run_at(mode_text: &str, ch: Channel, seconds: f64, fs: f64) -> (Vec<u8>, Receiver) {
        let opts = SimOptions {
            mode: Mode::parse(mode_text).unwrap(),
            cell_id: None,
            ts_file: None,
            seconds,
            out_rate_hz: Some(fs),
            level_dbfs: -20.0,
            channel: ch,
        };
        let mut x = Vec::new();
        generate(&opts, |b| {
            x.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        let mut rx = Receiver::new(ReceiverConfig::new(fs, 0.0));
        let mut ts = Vec::new();
        let mut at = 0;
        while at < x.len() {
            let n = 100_003.min(x.len() - at);
            rx.push(&x[at..at + n], at as u64);
            rx.take_ts(&mut ts);
            at += n;
        }
        (ts, rx)
    }

    /// The test stream's data packets in `ts`, and how many of the counters
    /// follow on from the one before.
    fn data_packets(ts: &[u8]) -> (usize, usize) {
        let mut last: Option<u32> = None;
        let (mut n, mut consecutive) = (0, 0);
        for p in ts.as_chunks::<188>().0 {
            if let Some(c) = check_test_payload(p) {
                n += 1;
                if last.is_some_and(|l| c == l + 1) {
                    consecutive += 1;
                }
                last = Some(c);
            }
        }
        (n, consecutive)
    }

    #[test]
    fn a_clean_uk_signal_decodes_to_the_transport_stream() {
        let ch = Channel {
            cn_db: Some(30.0),
            ..Default::default()
        };
        let (ts, rx) = run("64qam-2/3-8k-1/32", ch, 1.2);
        let st = rx.status();
        assert!(st.decoding, "{st:?}");
        assert_eq!(st.tps.unwrap().modulation, Modulation::Qam64);
        assert_eq!(st.outer.uncorrectable, 0, "{:?}", st.outer);
        let (n, consecutive) = data_packets(&ts);
        assert!(n > 800, "only {n} data packets from {:?}", st.outer);
        assert_eq!(consecutive, n - 1, "packets lost in between");
    }

    #[test]
    fn it_decodes_through_echoes_a_carrier_offset_and_clock_error() {
        let ch = Channel {
            paths: vec![
                Path::echo(0.0, 0.0),
                Path::echo(2.5, -9.0),
                Path::echo(7.0, -14.0),
            ],
            cfo_hz: 166_667.0,
            sco_ppm: 15.0,
            cn_db: Some(28.0),
            ..Default::default()
        };
        let (ts, rx) = run("64qam-2/3-8k-1/32", ch, 1.2);
        let st = rx.status();
        let (n, consecutive) = data_packets(&ts);
        assert!(n > 500, "only {n} data packets from {:?}", st.outer);
        assert!(consecutive + 5 >= n, "{n} packets, {consecutive} in a row");
    }

    #[test]
    fn it_decodes_at_other_radio_rates_whole_or_not() {
        // 20 and 8 MS/s make the symbol a whole number of samples (17920 and
        // 7168), so it is read directly; 9.6 MS/s does not (8601.6), so it is
        // interpolated. All three give the stream.
        let ch = || Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(2.0, -10.0)],
            cn_db: Some(30.0),
            ..Default::default()
        };
        for fs in [20e6, 8e6, 9.6e6] {
            let (ts, rx) = run_at("64qam-2/3-8k-1/32", ch(), 1.0, fs);
            let (n, consecutive) = data_packets(&ts);
            assert!(
                n > 300,
                "{fs}: only {n} data packets from {:?}",
                rx.status()
            );
            assert!(
                consecutive + 5 >= n,
                "{fs}: {n} packets, {consecutive} in a row"
            );
        }
    }

    #[test]
    fn the_scopes_and_measurements_show_the_signal_and_its_echo() {
        let opts = SimOptions {
            mode: Mode::parse("64qam-2/3-8k-1/32").unwrap(),
            cell_id: None,
            ts_file: None,
            seconds: 1.2,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: Channel {
                paths: vec![Path::echo(0.0, 0.0), Path::echo(3.0, -10.0)],
                cn_db: Some(30.0),
                ..Default::default()
            },
        };
        let mut x = Vec::new();
        generate(&opts, |b| {
            x.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        let mut rx = Receiver::new(ReceiverConfig::new(10e6, 0.0));
        let mut scopes = None;
        let mut at = 0;
        while at < x.len() {
            let n = 100_003.min(x.len() - at);
            if rx.status().decoding {
                rx.request_scopes();
            }
            rx.push(&x[at..at + n], at as u64);
            scopes = rx.take_scopes().or(scopes);
            at += n;
        }
        let s = scopes.expect("scopes");
        assert_eq!(s.modulation, Some(Modulation::Qam64));
        assert!(s.raw.len() <= 3000 && s.raw.len() > 1000);
        assert_eq!(s.tps.len(), 68);
        // Pilots at ±4/3 on the real axis, the TPS cells at ±1, the data at
        // unit mean power (30 dB of noise leaves them near their points).
        let mean = |v: &[Iq], f: fn(&Iq) -> f32| v.iter().map(f).sum::<f32>() / v.len() as f32;
        assert!((mean(&s.pilots, |c| c.re.abs()) - 4.0 / 3.0).abs() < 0.05);
        assert!(mean(&s.pilots, |c| c.im.abs()) < 0.1);
        assert!((mean(&s.tps, |c| c.norm()) - 1.0).abs() < 0.1);
        assert!((mean(&s.data, |c| c.norm_sqr()) - 1.0).abs() < 0.1);

        let m = rx.measure().expect("measurements");
        assert!(m.mean_snr_db > 20.0, "{}", m.mean_snr_db);
        assert!((m.guard_us - 28.0).abs() < 0.01, "{}", m.guard_us);
        let e = m.echoes.iter().find(|e| e.db > -14.0).expect("an echo");
        assert!((e.delay_us.abs() - 3.0).abs() < 0.3, "{e:?}");
        assert!((e.db + 10.0).abs() < 2.0, "{e:?}");
    }

    /// What a retune looks like to a receiver that is handed the old
    /// channel's samples as the new one's: a signal, then another mux at
    /// another carrier offset with its symbols at another phase. The receiver
    /// has to find the second without being restarted.
    #[test]
    fn heat_rows_follow_the_channel_at_the_rate_asked() {
        let opts = SimOptions {
            mode: Mode::parse("64qam-2/3-8k-1/32").unwrap(),
            cell_id: None,
            ts_file: None,
            seconds: 1.4,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: Channel {
                paths: vec![Path::echo(0.0, 0.0), Path::echo(3.0, -10.0)],
                cn_db: Some(30.0),
                ..Default::default()
            },
        };
        let mut x = Vec::new();
        generate(&opts, |b| {
            x.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        let mut rx = Receiver::new(ReceiverConfig::new(10e6, 0.0));
        rx.set_heat(20.0);
        let (mut rows, mut at) = (Vec::new(), 0);
        while at < x.len() {
            let n = 100_003.min(x.len() - at);
            rx.push(&x[at..at + n], at as u64);
            rx.take_heat(&mut rows);
            at += n;
        }
        // About 20 a second from the lock (a little after the first 0.2 s).
        assert!((15..=30).contains(&rows.len()), "{} rows", rows.len());
        let last = rows.last().unwrap();
        assert_eq!(last.gain_db.len(), last.snr_db.len());
        let e = last.echoes.iter().find(|e| e.db > -14.0).expect("an echo");
        assert!((e.delay_us.abs() - 3.0).abs() < 0.3, "{e:?}");
        // Off: nothing more is made.
        rx.set_heat(0.0);
        rx.push(&x[..200_000], at as u64);
        let mut more = Vec::new();
        rx.take_heat(&mut more);
        assert!(more.is_empty());
    }

    #[test]
    fn a_lock_with_no_signal_to_noise_is_given_up_after_a_few_looks() {
        let mut q = Quality::default();
        for _ in 0..POOR_LOOKS - 1 {
            assert!(!q.look(-1.8, MIN_SNR_DB));
        }
        assert!(q.look(-1.8, MIN_SNR_DB));
        // One look over the floor starts the count again.
        let mut q = Quality::default();
        for _ in 0..POOR_LOOKS - 1 {
            q.look(0.0, MIN_SNR_DB);
        }
        assert!(!q.look(4.0, MIN_SNR_DB));
        for _ in 0..POOR_LOOKS - 1 {
            assert!(!q.look(0.0, MIN_SNR_DB));
        }
        // The floor follows the mode: 8 dB under what 64-QAM 2/3 needs (16.5)
        // is 8.5, QPSK 1/2's would be under the least, 2.
        let tps = |modulation, rate_hp| TpsFrame {
            frame: 0,
            modulation,
            alpha: Alpha::NonHierarchical,
            rate_hp,
            rate_lp: rate_hp,
            guard: Guard::G1_32,
            fft: FftSize::K8,
            length: 23,
            cell_id_byte: 0,
        };
        assert_eq!(Quality::floor(None), MIN_SNR_DB);
        let f = Quality::floor(Some(&tps(Modulation::Qam64, Rate::R2_3)));
        assert!((f - 8.5).abs() < 1e-3, "{f}");
        let f = Quality::floor(Some(&tps(Modulation::Qpsk, Rate::R1_2)));
        assert_eq!(f, MIN_SNR_DB);
    }

    #[test]
    fn a_lock_that_does_no_good_is_given_up_after_a_few_looks() {
        // Not synced: four looks.
        let mut h = Health::default();
        assert!(!h.look(false, false, 0));
        assert!(!h.look(false, false, 0));
        assert!(!h.look(false, false, 0));
        assert!(h.look(false, false, 0));
        // Synced and decoding, blocks coming out: never.
        let mut h = Health::default();
        for k in 1..=20 {
            assert!(!h.look(true, true, k * 100));
        }
        // Synced and decoding, but not a block right: four looks.
        let mut h = Health::default();
        assert!(!h.look(true, true, 500));
        assert!(!h.look(true, true, 500));
        assert!(!h.look(true, true, 500));
        assert!(!h.look(true, true, 500));
        assert!(h.look(true, true, 500));
        // One good block in between starts the count again.
        let mut h = Health::default();
        for _ in 0..3 {
            h.look(true, true, 0);
        }
        assert!(!h.look(true, true, 1));
        assert!(!h.look(true, true, 1));
        // Synced and not decoding (hierarchical): fine.
        let mut h = Health::default();
        for _ in 0..10 {
            assert!(!h.look(true, false, 0));
        }
    }

    #[test]
    fn it_finds_a_new_signal_spliced_on_after_the_old_one() {
        let make = |cfo_hz: f64, shift_us: f64, seconds: f64| {
            let opts = SimOptions {
                mode: Mode::parse("64qam-2/3-8k-1/32").unwrap(),
                cell_id: None,
                ts_file: None,
                seconds,
                out_rate_hz: Some(10e6),
                level_dbfs: -20.0,
                channel: Channel {
                    paths: vec![Path::echo(shift_us, 0.0)],
                    cfo_hz,
                    cn_db: Some(28.0),
                    ..Default::default()
                },
            };
            let mut x = Vec::new();
            generate(&opts, |b| {
                x.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            x
        };
        let mut x = make(20_000.0, 0.0, 1.3);
        let splice = x.len();
        // The second signal: another carrier offset, and a start that falls
        // mid-symbol (the first 0.4 ms of it are cut).
        x.extend_from_slice(&make(-170_000.0, 0.0, 3.4)[4000..]);
        let mut rx = Receiver::new(ReceiverConfig::new(10e6, 0.0));
        let (mut ts, mut at, mut ts_at_splice) = (Vec::new(), 0, 0);
        while at < x.len() {
            let n = 100_003.min(x.len() - at);
            rx.push(&x[at..at + n], at as u64);
            rx.take_ts(&mut ts);
            at += n;
            if at <= splice {
                ts_at_splice = ts.len();
            }
        }
        let (n, consecutive) = data_packets(&ts[ts_at_splice..]);
        assert!(
            n > 1500 && consecutive + 20 >= n,
            "{n} packets of the second signal, {consecutive} in a row; {:?}",
            rx.status()
        );
    }

    /// The nastier splice: the second signal's symbols fall where the
    /// first's did and its carrier offset is the same, so the receiver sees
    /// nothing wrong with the lock, but its channel (the echoes) is another's,
    /// and an estimate held from the first one is wrong for every carrier.
    /// (Two muxes from the same site, a retune that leaves the old channel's
    /// last samples ahead of the new one's.)
    #[test]
    fn it_follows_a_channel_that_changes_under_a_good_lock() {
        let make = |echoes: &[(f64, f64)], seconds: f64| {
            let opts = SimOptions {
                mode: Mode::parse("64qam-2/3-8k-1/32").unwrap(),
                cell_id: None,
                ts_file: None,
                seconds,
                out_rate_hz: Some(10e6),
                level_dbfs: -20.0,
                channel: Channel {
                    paths: echoes.iter().map(|&(us, db)| Path::echo(us, db)).collect(),
                    cn_db: Some(28.0),
                    ..Default::default()
                },
            };
            let mut x = Vec::new();
            generate(&opts, |b| {
                x.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            x
        };
        // A whole number of 8960-sample symbols of the first, then the second.
        let mut x = make(&[(0.0, 0.0), (2.0, -6.0)], 1.3);
        x.truncate(1400 * 8960);
        let splice = x.len();
        x.extend(make(&[(0.0, 0.0), (5.5, -4.0), (-1.5, -9.0)], 3.0));
        let mut rx = Receiver::new(ReceiverConfig::new(10e6, 0.0));
        let (mut ts, mut at, mut ts_at_splice) = (Vec::new(), 0, 0);
        while at < x.len() {
            let n = 100_003.min(x.len() - at);
            rx.push(&x[at..at + n], at as u64);
            rx.take_ts(&mut ts);
            at += n;
            if at <= splice {
                ts_at_splice = ts.len();
            }
        }
        let (n, consecutive) = data_packets(&ts[ts_at_splice..]);
        assert!(
            n > 1500 && consecutive + 20 >= n,
            "{n} packets of the second signal, {consecutive} in a row; {:?}",
            rx.status()
        );
    }

    #[test]
    fn a_2k_qpsk_signal_decodes() {
        let ch = Channel {
            cn_db: Some(12.0),
            ..Default::default()
        };
        let (ts, rx) = run("qpsk-1/2-2k-1/8", ch, 1.5);
        let (n, _) = data_packets(&ts);
        assert!(n > 100, "only {n} data packets from {:?}", rx.status());
    }
}
