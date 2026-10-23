// made by CasualArclamp
//! The DVB-T2 receiver: the radio's samples in, transport packets out.
//!
//! Fed blocks of the radio's stream in order, it asks (as the DVB-T receiver
//! does, [`crate::window`]) for the stretch it wants next: while searching,
//! a quarter of a second, in which a P1 symbol is looked for; once locked, a
//! frame at a time, aimed at where the next P1 symbol is expected. A frame is
//! read to the signal's own rate and clock by the interpolator, its symbols
//! equalised ([`crate::t2::frame`]), L1 read from its P2 symbols, and its
//! PLP's cells decoded ([`crate::t2::plp`]).

use std::time::{Duration, Instant};

use decdvbt_core::Iq;
use decdvbt_fec::t2::l1::{L1Coder, L1Mod, PRE_CELLS, PostLayout, post_llrs};
use decdvbt_ofdm::t2::carriers::p2_roles;
use decdvbt_ofdm::t2::l1::{L1Post, L1Pre};
use decdvbt_ofdm::t2::p1::SAMPLES as P1_SAMPLES;
use decdvbt_ofdm::t2::{Fft, Guard, Mode, PilotPattern, Reference};

use crate::interp::Interpolator;
use crate::scope::{MeasureState, Measurements, Scopes, measure as measure_channel, thin};
use crate::symbol::PAD;
use crate::t2::frame::{Demod, Geometry, SymbolFft, cp_peak, p2_equalise};
use crate::t2::p1det;
use crate::t2::pipe::{Done, FecPipe, Job};
use crate::t2::plp::PlpParams;
use crate::window::Window;

/// The signal's sample rate for 8 MHz channels, Hz.
pub const FS_NATIVE: f64 = 64e6 / 7.0;
/// Samples (at the signal's rate) read either side of an expected frame.
const MARGIN: usize = 3072;
/// A search window, in the signal's samples: a little over a long frame's
/// duration (250 ms) and a P1 symbol, so that one P1 is in it.
const ACQ_NATIVE: usize = 2_500_000;
/// The wait between attempts to acquire, seconds.
const RETRY_S: f64 = 0.2;
/// Frames in a row in which no P1 symbol, or no L1, was found, that end a lock.
const MAX_MISSES: u32 = 3;
/// Radio samples the window keeps behind the latest block: two margins, the
/// interpolator's reach and room for a rate up to 20 MS/s.
const HISTORY: usize = 32 * 1024;
/// The search for a P1's whole carrier spacings either side (8.9 kHz each).
const P1_BINS: isize = 64;

#[derive(Debug, Clone, Copy)]
pub struct T2Config {
    /// The radio's sample rate.
    pub fs_in: f64,
    /// How far above the radio's centre the signal's centre is.
    pub vfo_offset_hz: f64,
    /// The most threads the FEC blocks are spread over (on top of the FEC
    /// stage's own thread, which hands them out); the stage uses as few as
    /// keep it to about half of real time.
    pub fec_threads: usize,
    /// The PLP to decode by its id; `None`: the first data PLP (type 1).
    pub plp: Option<u8>,
}

impl T2Config {
    pub fn new(fs_in: f64, vfo_offset_hz: f64) -> T2Config {
        T2Config {
            fs_in,
            vfo_offset_hz,
            // A quarter of the machine's threads, two to four: about what a
            // 40 Mbit/s 256-QAM multiplex takes, with room to spare.
            fec_threads: (std::thread::available_parallelism().map_or(2, |n| n.get()) / 4)
                .clamp(2, 4),
            plp: None,
        }
    }
}

/// Wall time spent in each stage since the start.
#[derive(Debug, Clone, Copy, Default)]
pub struct T2Profile {
    pub acquire: Duration,
    /// The stretch to the signal's rate, P1, timing and the transforms.
    pub read: Duration,
    /// Channel estimates and equalised cells.
    pub equalise: Duration,
    /// Handing the frame to the FEC stage, and waiting for it if it is two
    /// frames behind (the FEC work itself runs on its own threads: see the
    /// fields below).
    pub fec: Duration,
    /// Of `fec`: the interleavers undone (one thread), and, summed over the
    /// threads, the demapper, the bit deinterleaver, LDPC and BCH.
    pub deinterleave: Duration,
    pub demap: Duration,
    pub gather: Duration,
    pub ldpc: Duration,
    pub bch: Duration,
    /// Of `read`: the stretch resampled to the signal's rate, the carrier
    /// offset taken out, and P1 and the symbols' timing found.
    pub resample: Duration,
    pub rotate: Duration,
    pub p1: Duration,
    pub timing: Duration,
    /// Of `read` and `equalise`: the transforms, the alignment, the channel
    /// from the pilots, and the cells.
    pub fft: Duration,
    pub align: Duration,
    pub channel: Duration,
    pub cells: Duration,
}

impl T2Status {
    /// How long a frame lasts, seconds, once the mode and L1-pre are known.
    pub fn frame_period_s(&self) -> Option<f64> {
        let (mode, pre) = (self.mode.as_ref()?, self.l1pre.as_ref()?);
        let geo = Geometry::new(*mode, pre.num_data_symbols as usize);
        Some(geo.frame_samples() as f64 / FS_NATIVE)
    }
}

#[derive(Debug, Clone, Default)]
pub struct T2Status {
    pub tracking: bool,
    /// L1 is read and agrees with what the lock took it for.
    pub l1_ok: bool,
    /// The PLP is being decoded.
    pub decoding: bool,
    pub mode: Option<Mode>,
    pub l1pre: Option<L1Pre>,
    pub l1post: Option<L1Post>,
    pub plp: Option<PlpParams>,
    /// FEC blocks in the PLP's frame and the PLP's id.
    pub blocks_per_frame: usize,
    pub plp_id: u8,
    pub cfo_hz: f64,
    pub clock_ppm: f64,
    pub snr_db: f32,
    pub frames_ok: u64,
    pub frames_bad: u64,
    pub blocks_ok: u64,
    pub blocks_bad: u64,
    pub ldpc_iterations: u64,
    pub bch_corrected: u64,
    pub packets: u64,
    pub resyncs: u64,
    pub locks: u32,
    pub attempts: u32,
    /// Acquisitions that found no P1, and ones that found one that is not
    /// T2's (or that no L1 could be read from).
    pub acq_no_p1: u32,
    pub acq_no_l1: u32,
    pub profile: T2Profile,
    /// The FEC stage's threads at work and the share of a frame's period the
    /// last frame took on them.
    pub fec_threads: usize,
    pub fec_load: f32,
    /// The PLPs L1 lists: id and type (0 common, 1 and 2 data).
    pub plp_ids: Vec<(u8, u8)>,
    /// What the last frame looked like, for a developer.
    pub debug: String,
}

struct Track {
    geo: Geometry,
    demod: Demod,
    l1pre: L1Pre,
    /// The carrier offset from the radio's centre, Hz, and the radio's
    /// samples a sample of the signal's.
    cfo_hz: f64,
    step: f64,
    /// Where the next P1 symbol is expected, in the radio's stream (samples,
    /// fractional).
    next_p1: f64,
    misses: u32,
}

pub struct T2Receiver {
    cfg: T2Config,
    rho: f64,
    interp: Interpolator,
    win: Window,
    track: Option<Track>,
    status: T2Status,
    next_attempt: u64,
    ts: Vec<u8>,
    coder: L1Coder,
    acq_fft: Option<SymbolFft>,
    y: Vec<Iq>,
    /// The FEC stage, started at the first lock.
    fec: Option<FecPipe>,
    /// For the display: cells asked for, the ones made; the last frame's
    /// channel (at the middle symbol) and its noise; rows of the channel
    /// wanted a second and those made.
    scope_wanted: bool,
    scopes: Option<Scopes>,
    channel: Option<ChannelEstimate>,
    measure_state: MeasureState,
    heat_rate: f64,
    heat: Vec<Measurements>,
}

/// A frame's channel estimate at its middle symbol, for the display.
struct ChannelEstimate {
    mode: Mode,
    h: Vec<Iq>,
    noise: f32,
}

fn rotate(y: &mut [Iq], f_hz: f64, fs: f64) {
    if f_hz == 0.0 {
        return;
    }
    let w = -std::f64::consts::TAU * f_hz / fs;
    let step = Iq::new(w.cos() as f32, w.sin() as f32);
    let mut rot = Iq::new(1.0, 0.0);
    for (i, v) in y.iter_mut().enumerate() {
        *v *= rot;
        rot *= step;
        if i % 512 == 511 {
            let ph = w * (i + 1) as f64;
            rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
    }
}

impl T2Receiver {
    pub fn new(cfg: T2Config) -> T2Receiver {
        T2Receiver {
            cfg,
            rho: cfg.fs_in / FS_NATIVE,
            interp: Interpolator::new(),
            // The next frame's stretch starts a little before the end of this
            // one's: keep what went by.
            win: Window::with_history(HISTORY),
            track: None,
            status: T2Status::default(),
            next_attempt: 0,
            ts: Vec::new(),
            coder: L1Coder::new(),
            acq_fft: None,
            y: Vec::new(),
            fec: None,
            scope_wanted: false,
            scopes: None,
            channel: None,
            measure_state: MeasureState::default(),
            heat_rate: 0.0,
            heat: Vec::new(),
        }
    }

    pub fn status(&self) -> &T2Status {
        &self.status
    }

    /// Decode this PLP (by id) from the next frame on; `None` for the first
    /// data PLP.
    pub fn set_plp(&mut self, plp: Option<u8>) {
        self.cfg.plp = plp;
    }

    /// Ask for the cells of a symbol of the next frame (for
    /// [`take_scopes`](Self::take_scopes)); nothing is copied until asked.
    pub fn request_scopes(&mut self) {
        self.scope_wanted = true;
    }

    /// The cells asked for, once the next frame has come.
    pub fn take_scopes(&mut self) -> Option<Scopes> {
        self.scopes.take()
    }

    /// Make a row of the channel for each frame (about 4.6 a second) while
    /// `rows_per_s` is more than zero, for a heat map of it over time.
    pub fn set_heat(&mut self, rows_per_s: f64) {
        self.heat_rate = rows_per_s.max(0.0);
        if self.heat_rate == 0.0 {
            self.heat.clear();
        }
    }

    pub fn take_heat(&mut self, out: &mut Vec<Measurements>) {
        out.append(&mut self.heat);
    }

    /// The channel as the last frame's estimate has it (gain and
    /// signal-to-noise ratio across the band, the impulse response and its
    /// echoes), or `None` with no lock.
    pub fn measure(&mut self) -> Option<Measurements> {
        let c = self.channel.as_ref()?;
        Some(Self::measurements(&mut self.measure_state, c))
    }

    fn measurements(state: &mut MeasureState, c: &ChannelEstimate) -> Measurements {
        let hr: Vec<f32> = c.h.iter().map(|v| v.re).collect();
        let hi: Vec<f32> = c.h.iter().map(|v| v.im).collect();
        let w: Vec<f32> = c.h.iter().map(|v| v.norm_sqr()).collect();
        let noise = vec![c.noise; c.h.len()];
        let guard_us = (c.mode.guard.samples(c.mode.fft) as f64 / FS_NATIVE * 1e6) as f32;
        measure_channel(
            state,
            c.mode.fft.n(),
            FS_NATIVE,
            guard_us,
            (&hr, &hi),
            &w,
            &noise,
        )
    }

    /// Forget the lock and the stretch wanted: the stream has jumped.
    pub fn reset(&mut self) {
        if let Some(f) = &mut self.fec {
            f.reset();
        }
        self.win.cancel();
        self.track = None;
        self.next_attempt = 0;
        self.status.tracking = false;
        self.status.l1_ok = false;
        self.status.decoding = false;
    }

    /// Transport packets (188 bytes each) decoded since the last call.
    pub fn take_ts(&mut self, out: &mut Vec<u8>) {
        self.collect();
        out.append(&mut self.ts);
    }

    /// Wait for the frames the FEC stage has in hand: their packets are
    /// then there for [`take_ts`](Self::take_ts) (for the end of a file).
    pub fn flush(&mut self) {
        if let Some(f) = &mut self.fec {
            let done = f.flush();
            for d in done {
                self.absorb(d);
            }
        }
    }

    /// Take in the frames the FEC stage has finished.
    fn collect(&mut self) {
        if let Some(f) = &mut self.fec {
            for d in f.poll() {
                self.absorb(d);
            }
        }
    }

    fn absorb(&mut self, d: Done) {
        self.status.fec_threads = d.threads;
        self.status.fec_load = d.load;
        self.ts.extend_from_slice(&d.ts);
        self.account(d.stats, d.resyncs);
    }

    /// Hand over a block of the radio's samples, whose first is the stream's
    /// `abs_start`-th.
    pub fn push(&mut self, block: &[Iq], abs_start: u64) {
        self.collect();
        if !self.win.is_wanting() {
            self.want_acquisition(abs_start.max(self.next_attempt));
        }
        loop {
            if !self.win.feed(block, abs_start) {
                return;
            }
            if self.track.is_some() {
                self.track_frame();
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

    fn want_frame(&mut self) {
        let Some(t) = &self.track else { return };
        let native = t.geo.frame_samples() + 2 * MARGIN;
        let x0 = t.next_p1 - MARGIN as f64 * t.step;
        let start = (x0.floor() as i64 - PAD as i64).max(0) as u64;
        let len = (native as f64 * t.step).ceil() as usize + 2 * PAD + 8;
        self.win.want(start, len);
    }

    fn drop_lock(&mut self) {
        if let Some(f) = &mut self.fec {
            f.reset();
        }
        self.win.cancel();
        self.track = None;
        self.status.tracking = false;
        self.status.l1_ok = false;
        self.status.decoding = false;
    }

    // ---- acquisition ------------------------------------------------------

    fn try_acquire(&mut self) {
        let t0 = Instant::now();
        self.acquire();
        self.status.profile.acquire += t0.elapsed();
    }

    fn acquire(&mut self) {
        self.status.attempts += 1;
        let (start, len) = self.win.wanted().expect("a stretch");
        let end = start + len as u64;
        self.next_attempt = start + (RETRY_S * self.cfg.fs_in) as u64;
        // The stretch at the signal's rate, the VFO's offset taken out.
        let n = ((len - 2 * PAD) as f64 / self.rho) as usize;
        self.y.clear();
        self.interp
            .resample_planar(self.win.planar(), PAD as f64, self.rho, n, &mut self.y);
        rotate(&mut self.y, self.cfg.vfo_offset_hz, FS_NATIVE);
        let base = start as f64 + PAD as f64; // the stream position of y[0]
        let hits = p1det::find(&self.y, FS_NATIVE);
        if hits.is_empty() {
            self.status.acq_no_p1 += 1;
            self.want_acquisition(self.next_attempt);
            return;
        }
        for hit in hits {
            let Some(info) = p1det::read(&self.y, &hit, FS_NATIVE, P1_BINS) else {
                continue;
            };
            // T2 SISO only (S1 0); the FFT size from S2's first field.
            if info.s1 != 0 || info.quality < 0.6 {
                continue;
            }
            let Some(fft) = Fft::from_s2_code(info.s2 >> 1) else {
                continue;
            };
            let Some((l1pre, l1post, mode, eps)) = self.read_l1(&hit, fft, info.cfo_hz) else {
                self.status.acq_no_l1 += 1;
                continue;
            };
            let data_symbols = l1pre.num_data_symbols as usize;
            let geo = Geometry::new(mode, data_symbols);
            let step = self.rho;
            let cfo = self.cfg.vfo_offset_hz + info.cfo_hz + eps * FS_NATIVE / geo.n as f64;
            // The next P1 symbol at or after the end of what has been read,
            // plus the margin the frame window opens ahead of it.
            let frame = geo.frame_samples() as f64 * step;
            let mut p1 = base + hit.start * step;
            while p1 - MARGIN as f64 * step < end as f64 {
                p1 += frame;
            }
            let plp = l1post.plps.first().and_then(PlpParams::from_l1);
            self.status.mode = Some(mode);
            self.status.l1pre = Some(l1pre.clone());
            self.status.l1post = Some(l1post);
            self.status.plp = plp;
            self.status.tracking = true;
            self.status.l1_ok = true;
            self.status.locks += 1;
            self.status.cfo_hz = cfo;
            self.track = Some(Track {
                demod: Demod::new(geo),
                geo,
                l1pre,
                cfo_hz: cfo,
                step,
                next_p1: p1,
                misses: 0,
            });
            self.want_frame();
            return;
        }
        self.want_acquisition(self.next_attempt);
    }

    /// Read L1 of the frame whose P1 symbol is `hit`, for an FFT size S2 gave:
    /// the guard interval and the extended carrier mode are not known until
    /// L1-pre is read, so they are tried (the guard intervals by how well
    /// their cyclic prefixes correlate, best first). Returns L1 and the mode,
    /// and the carrier offset left (carriers).
    fn read_l1(
        &mut self,
        hit: &p1det::P1Hit,
        fft: Fft,
        cfo_hz: f64,
    ) -> Option<(L1Pre, L1Post, Mode, f64)> {
        let n = fft.n();
        let p2_start = hit.start + P1_SAMPLES as f64;
        // The carrier offset the P1 gave, taken out of a copy of the P2
        // region (the samples are already at the signal's rate).
        let n_p2 = fft.p2_symbols();
        let region_start = (p2_start - 64.0).max(0.0) as usize;
        let max_g = Guard::G1_4.samples(fft);
        let region_len = n_p2 * (n + max_g) + n + 256;
        if region_start + region_len + 2 * n > self.y.len() {
            return None;
        }
        let mut region: Vec<Iq> = self.y[region_start..region_start + region_len + 2 * n].to_vec();
        rotate(&mut region, cfo_hz, FS_NATIVE);
        let offset = p2_start - region_start as f64;
        // The guards, by correlation at the first P2 symbol.
        let mut scored: Vec<(f32, Guard)> = Guard::ALL
            .into_iter()
            .filter_map(|gd| {
                let g = gd.samples(fft);
                cp_peak(&region, offset, n, g, 24).map(|(_, s, _)| (s, gd))
            })
            .collect();
        // A window as long as the guard interval or shorter, starting at its
        // beginning, correlates equally well: the guard is the longest
        // window that does (within the noise of the best).
        let best = scored.iter().map(|s| s.0).fold(0.0f32, f32::max);
        scored.retain(|s| s.0 >= 0.9 * best);
        scored.sort_by_key(|(_, g)| std::cmp::Reverse(g.samples(fft)));
        let fft_t = self.acq_fft.get_or_insert_with(|| SymbolFft::new(n));
        if fft_t.n != n {
            *fft_t = SymbolFft::new(n);
        }
        let exts: &[bool] = if fft.has_extended() {
            &[true, false]
        } else {
            &[false]
        };
        for &(_, guard) in scored.iter().take(3) {
            let g = guard.samples(fft);
            let sym = n + g;
            for &ext in exts {
                let mode = Mode::new(fft, guard, PilotPattern::Pp7, ext);
                let c_ps = mode.c_ps();
                let roles = p2_roles(&mode);
                let reference = Reference::new(&mode);
                // The P2 symbols' carriers: positions refined by their cyclic
                // prefixes, the offset's remainder from their phases.
                let mut carriers: Vec<Iq> = Vec::new();
                let mut eps_sum = Iq::new(0.0, 0.0);
                let mut positions = Vec::new();
                for j in 0..n_p2 {
                    let nominal = offset + (j * sym) as f64;
                    let (pos, _, c) = cp_peak(&region, nominal, n, g, 24)?;
                    eps_sum += c;
                    positions.push(pos);
                }
                let eps = -f64::from(eps_sum.arg()) / std::f64::consts::TAU;
                let early = (g / 4).clamp(8, 128);
                for &pos in &positions {
                    fft_t.carriers(&region, pos, g, early, eps, c_ps, &mut carriers);
                }
                let (cells, var) = p2_equalise(&mode, &roles, &reference, n_p2, c_ps, &carriers);
                let c_p2 = cells.len() / n_p2;
                let fi = decdvbt_ofdm::t2::FreqInterleaver::new(fft, c_p2);
                // L1-pre only: the first 1840 logical cells, dealt over the
                // P2 symbols (the post count does not move them).
                let mut by_symbol = vec![Iq::new(0.0, 0.0); cells.len()];
                let mut by_var = vec![0f32; cells.len()];
                for j in 0..n_p2 {
                    let r = j * c_p2..(j + 1) * c_p2;
                    fi.deinterleave(j, &cells[r.clone()], &mut by_symbol[r.clone()]);
                    fi.deinterleave(j, &var[r.clone()], &mut by_var[r]);
                }
                let place = decdvbt_ofdm::t2::carriers::p2_deal(n_p2, c_p2, PRE_CELLS, 0);
                let llr: Vec<f32> = (0..PRE_CELLS)
                    .map(|i| {
                        let c = by_symbol[place[i]];
                        4.0 * c.re / by_var[place[i]].max(1e-6)
                    })
                    .collect();
                let Some(bits) = self.coder.decode_pre(&llr) else {
                    continue;
                };
                let Some(pre) = L1Pre::from_bits(&bits) else {
                    continue;
                };
                if pre.fft() != Some(fft) || pre.guard() != Some(guard) || pre.bwt_ext != ext {
                    continue;
                }
                let post = Self::decode_post(&mut self.coder, &pre, n_p2, &by_symbol, &by_var);
                let (Some(post), Some(m)) = (post, pre.mode()) else {
                    continue;
                };
                return Some((pre, post, m, eps));
            }
        }
        None
    }

    /// L1-post from the P2 cells (symbol by symbol, deinterleaved).
    fn decode_post(
        coder: &mut L1Coder,
        pre: &L1Pre,
        n_p2: usize,
        by_symbol: &[Iq],
        by_var: &[f32],
    ) -> Option<L1Post> {
        let l1mod = L1Mod::from_code(pre.l1_mod)?;
        let layout = PostLayout::new(pre.l1_post_info_size as usize + 32, l1mod, n_p2)?;
        if layout.cells != pre.l1_post_size as usize {
            return None;
        }
        let c_p2 = by_symbol.len() / n_p2;
        let place = decdvbt_ofdm::t2::carriers::p2_deal(n_p2, c_p2, PRE_CELLS, layout.cells);
        let cells: Vec<Iq> = (0..layout.cells)
            .map(|i| by_symbol[place[PRE_CELLS + i]])
            .collect();
        let var: Vec<f32> = (0..layout.cells)
            .map(|i| by_var[place[PRE_CELLS + i]])
            .collect();
        let mut llr = Vec::new();
        post_llrs(&cells, &var, l1mod, &mut llr);
        let bits = coder.decode_post(&llr, &layout, pre.l1_post_scrambled)?;
        L1Post::from_bits(&bits, pre)
    }

    // ---- tracking ---------------------------------------------------------

    fn track_frame(&mut self) {
        self.frame();
        if self.track.is_some() {
            self.want_frame();
        } else {
            self.status.tracking = false;
            self.want_acquisition(self.next_attempt);
        }
    }

    fn frame(&mut self) {
        let Some(mut t) = self.track.take() else {
            return;
        };
        let (start, len) = self.win.wanted().expect("a stretch");
        let t_read = Instant::now();
        // The frame at the signal's rate, its offset taken out.
        let native = t.geo.frame_samples() + 2 * MARGIN;
        let x0 = t.next_p1 - MARGIN as f64 * t.step;
        let idx0 = x0 - start as f64;
        debug_assert!(len > 0 && idx0 >= PAD as f64 - 1.0);
        self.y.clear();
        self.interp
            .resample_planar(self.win.planar(), idx0, t.step, native, &mut self.y);
        self.status.profile.resample += t_read.elapsed();
        let t_rot = Instant::now();
        rotate(&mut self.y, t.cfo_hz, FS_NATIVE);
        self.status.profile.rotate += t_rot.elapsed();
        // P1 near where it is expected.
        let t_p1 = Instant::now();
        let lo = MARGIN.saturating_sub(2048);
        let hi = (MARGIN + 2048 + P1_SAMPLES + 64).min(self.y.len());
        let hits = p1det::find(&self.y[lo..hi], FS_NATIVE);
        self.status.profile.p1 += t_p1.elapsed();
        let Some(p1) = hits
            .iter()
            .max_by(|a, b| a.metric.total_cmp(&b.metric))
            .filter(|h| h.metric > 0.4)
            .map(|h| h.start + lo as f64)
        else {
            self.status.debug = format!(
                "frame lost: no P1 symbol (best metric {:.2})",
                hits.iter().map(|h| h.metric).fold(0.0, f32::max)
            );
            self.status.frames_bad += 1;
            t.misses += 1;
            // Aim again at the next frame (the one seen is lost).
            t.next_p1 += t.geo.frame_samples() as f64 * t.step;
            self.finish_frame(t);
            return;
        };
        let t_timing = Instant::now();
        let timing = t.demod.timing(&self.y, p1 + P1_SAMPLES as f64);
        self.status.profile.timing += t_timing.elapsed();
        let Some(timing) = timing else {
            self.status.debug = "frame lost: the symbols' timing could not be found".into();
            self.status.frames_bad += 1;
            t.misses += 1;
            t.next_p1 += t.geo.frame_samples() as f64 * t.step;
            self.finish_frame(t);
            return;
        };
        self.status.debug = format!(
            "P1 at {p1:.1} metric {:.2}; timing offset {:+.2} drift {:+.4} inliers {}/{} eps {:+.4}",
            hits.iter().map(|h| h.metric).fold(0.0, f32::max),
            timing.offset,
            timing.drift,
            timing.inliers,
            t.geo.symbols,
            timing.eps
        );
        let mut carriers = t.demod.all_carriers(&self.y, &timing);
        self.status.profile.read += t_read.elapsed();

        let t_eq = Instant::now();
        let geo = t.geo;
        let n_p2 = geo.n_p2;
        // P2: L1 again, every frame.
        let (dealt, dealt_var) = t.demod.p2_cells(&carriers[..n_p2 * geo.c_ps]);
        let (logical, lvar) =
            t.demod
                .p2_logical(&dealt, &dealt_var, PRE_CELLS, t.l1pre.l1_post_size as usize);
        let llr: Vec<f32> = (0..PRE_CELLS)
            .map(|i| 4.0 * logical[i].re / lvar[i].max(1e-6))
            .collect();
        let pre = self
            .coder
            .decode_pre(&llr)
            .and_then(|b| L1Pre::from_bits(&b));
        let l1_ok = pre.as_ref().is_some_and(|p| *p == t.l1pre);
        let mut post = None;
        if l1_ok {
            let l1mod = L1Mod::from_code(t.l1pre.l1_mod);
            if let Some(l1mod) = l1mod
                && let Some(layout) =
                    PostLayout::new(t.l1pre.l1_post_info_size as usize + 32, l1mod, n_p2)
            {
                let start_post = PRE_CELLS;
                let mut pl = Vec::new();
                post_llrs(
                    &logical[start_post..start_post + layout.cells],
                    &lvar[start_post..start_post + layout.cells],
                    l1mod,
                    &mut pl,
                );
                post = self
                    .coder
                    .decode_post(&pl, &layout, t.l1pre.l1_post_scrambled)
                    .and_then(|b| L1Post::from_bits(&b, &t.l1pre));
            }
        }
        let Some(post) = post else {
            self.status.debug = format!(
                "frame lost: L1 not read ({})",
                if pre.is_some() { "post" } else { "pre" }
            );
            self.status.frames_bad += 1;
            self.status.l1_ok = false;
            t.misses += 1;
            self.advance(&mut t, p1, &timing, 0.0);
            self.finish_frame(t);
            return;
        };
        self.status.l1_ok = true;
        self.status.l1post = Some(post.clone());

        let scope = std::mem::take(&mut self.scope_wanted);
        let data = t.demod.data_cells(&mut carriers, scope);
        self.status.profile.equalise += t_eq.elapsed();
        {
            let d = &t.demod.prof;
            let p = &mut self.status.profile;
            (p.fft, p.align, p.channel, p.cells) = (d.fft, d.align, d.channel, d.cells);
        }
        let Some(data) = data else {
            self.status.debug = "frame lost: no equalised cells".into();
            self.status.frames_bad += 1;
            t.misses += 1;
            self.advance(&mut t, p1, &timing, 0.0);
            self.finish_frame(t);
            return;
        };
        self.status.snr_db = data.snr_db;
        let c_data = t.demod.carriers.data_cells();
        let mid_symbol = (data.cells.len() / c_data.max(1)) / 2;
        if scope && let Some(plp) = self.status.plp {
            let cells = data.cells
                [mid_symbol * c_data..((mid_symbol + 1) * c_data).min(data.cells.len())]
                .iter()
                .copied();
            self.scopes = Some(Scopes {
                raw: thin(data.scope_raw.iter().copied(), 3000),
                data: thin(cells.collect::<Vec<_>>().into_iter(), 3000),
                pilots: data.scope_pilots.clone(),
                t2_qam: Some((plp.qam.bits() as u8, plp.rotated)),
                pilot_mark: Some(1.0),
                symbol: self.status.frames_ok,
                ..Scopes::default()
            });
        }
        // The channel at the middle symbol, for the display.
        let estimate = ChannelEstimate {
            mode: t.geo.mode,
            h: data.channel.clone(),
            noise: data.noise,
        };
        if self.heat_rate > 0.0 {
            if self.heat.len() >= 400 {
                self.heat.remove(0);
            }
            let row = Self::measurements(&mut self.measure_state, &estimate);
            self.heat.push(row);
        }
        self.channel = Some(estimate);
        self.status.debug.push_str(&format!(
            " | symbol SNR dB {}",
            data.symbol_snr_db
                .iter()
                .map(|v| format!("{v:.0}"))
                .collect::<Vec<_>>()
                .join(" ")
        ));

        // The PLP: the first of type 1 (data PLP, type 1) L1 lists.
        let t_fec = Instant::now();
        self.status.plp_ids = post.plps.iter().map(|p| (p.plp_id, p.plp_type)).collect();
        let pick = match self.cfg.plp {
            Some(id) => post.plps.iter().position(|p| p.plp_id == id),
            None => post
                .plps
                .iter()
                .position(|p| p.plp_type == 1)
                .or(Some(0))
                .filter(|i| *i < post.plps.len()),
        };
        if let Some(i) = pick {
            let params = PlpParams::from_l1(&post.plps[i]);
            self.status.plp = params;
            let dynamic = post.dynamic.get(i);
            if let (Some(params), Some(dynamic)) = (params, dynamic) {
                // The cells of the frame: P2's (all of them logical), then
                // the data symbols'.
                let mut cells = logical;
                let mut var = lvar;
                cells.extend_from_slice(&data.cells);
                var.extend_from_slice(&data.var);
                let job = Job {
                    params,
                    cells,
                    var,
                    start: PRE_CELLS + t.l1pre.l1_post_size as usize + dynamic.plp_start as usize,
                    blocks: dynamic.plp_num_blocks as usize,
                    period: Duration::from_secs_f64(t.geo.frame_samples() as f64 / FS_NATIVE),
                };
                let threads = self.cfg.fec_threads;
                let done = self
                    .fec
                    .get_or_insert_with(|| FecPipe::new(threads))
                    .submit(job);
                for d in done {
                    self.absorb(d);
                }
                self.status.plp_id = post.plps[i].plp_id;
                self.status.blocks_per_frame = dynamic.plp_num_blocks as usize;
            } else {
                self.status.decoding = false;
            }
        }
        self.status.profile.fec += t_fec.elapsed();
        t.misses = 0;
        self.status.frames_ok += 1;
        self.advance(&mut t, p1, &timing, data.drift_residual);
        self.finish_frame(t);
    }

    fn account(&mut self, stats: Option<crate::t2::plp::FrameStats>, resyncs: u64) {
        match stats {
            Some(s) => {
                let p = &mut self.status.profile;
                p.deinterleave += s.deinterleave;
                p.demap += s.blocks_time.demap;
                p.gather += s.blocks_time.gather;
                p.ldpc += s.blocks_time.ldpc;
                p.bch += s.blocks_time.bch;
                self.status.decoding = s.ok > 0;
                self.status.blocks_ok += s.ok as u64;
                self.status.blocks_bad += (s.blocks - s.ok) as u64;
                self.status.ldpc_iterations += s.ldpc_iterations;
                self.status.bch_corrected += s.bch_corrected;
                self.status.packets += s.packets as u64;
            }
            None => self.status.decoding = false,
        }
        self.status.resyncs = resyncs;
    }

    /// Aim at the next frame from this one's measured place, and follow the
    /// carrier offset and the clock.
    fn advance(
        &mut self,
        t: &mut Track,
        p1: f64,
        timing: &crate::t2::frame::Timing,
        drift_residual: f64,
    ) {
        let sym = t.geo.sym as f64;
        // Where this frame's P1 symbol was, in the radio's stream.
        let p1_abs = t.next_p1 + (p1 - MARGIN as f64) * t.step;
        // The clock: symbols appearing `drift` samples further apart than
        // they should means the signal's samples are `drift/sym` wider than
        // the interpolator's step takes them.
        t.step *= 1.0 + 0.5 * (timing.drift - drift_residual) / sym;
        t.cfo_hz += 0.7 * timing.eps * FS_NATIVE / t.geo.n as f64;
        t.next_p1 = p1_abs + t.geo.frame_samples() as f64 * t.step;
        self.status.cfo_hz = t.cfo_hz;
        self.status.clock_ppm = (t.step / self.rho - 1.0) * 1e6;
    }

    fn finish_frame(&mut self, t: Track) {
        if t.misses >= MAX_MISSES {
            self.drop_lock();
            return;
        }
        self.track = Some(t);
    }
}
