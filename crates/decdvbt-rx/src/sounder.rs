// made by CasualArclamp
//! The channel sounder: acquires a DVB-T signal in the radio's samples,
//! follows it, and from the scattered pilots of every M-th symbol makes the
//! channel estimates the fading waterfall and the delay–Doppler map are drawn
//! from.
//!
//! It costs little because it looks at few samples. It never converts the
//! radio's stream: it tells the caller which stretch of samples it wants
//! next (about one symbol in every M, 12 % of the stream at M = 8), takes
//! only those, and reads them at the signal's own rate and at exact
//! fractional positions by interpolation ([`crate::interp`]).
//!
//! Per symbol looked at:
//!
//! 1. The cyclic prefix near the predicted position gives the symbol's timing
//!    (to a fraction of a sample) and the carrier offset's fraction; a
//!    second-order loop follows both, and the sampling clock's drift.
//! 2. The symbol's useful part (started a quarter of the guard early, to be
//!    clear of echoes either side) is read, rotated by the carrier offset
//!    from absolute time (so the carrier's phase runs on from symbol to
//!    symbol, which is what makes Doppler measurable) and transformed.
//! 3. The scattered pilots, whose carriers are the same every time since M is
//!    a multiple of 4, divided by their known values, are the channel's
//!    transfer function at every 12th carrier: a row.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{Bandwidth, FftSize, Guard};
use decdvbt_ofdm::pilots::prbs;

use crate::acquire::{self, Fft1, MIN_CONTRAST, bin_of, parabola, search_best};
use crate::delay_doppler::{ChannelHistory, DelayDoppler};
use crate::interp::{AFTER, BEFORE, Interpolator, Planar};

/// Input samples taken to acquire, and the wait between attempts, seconds.
const ACQ_NATIVE: usize = 58_000;
const RETRY_S: f64 = 0.4;
/// The timing search around the prediction, native samples either side.
const SEARCH: usize = 32;
/// Margin, input samples, either side of a wanted stretch for the
/// interpolator.
const PAD: usize = BEFORE + AFTER + 8;
/// Loop gains: position, clock rate, carrier offset.
const ALPHA: f64 = 0.25;
const BETA: f64 = 0.02;
const GAMMA: f64 = 0.03;
/// Rows of power averaged into one fading row.
const FADE_GROUP: usize = 4;
/// Lock is lost after this many rows with no coherence.
const LOST_AFTER: u32 = 12;
const MIN_COHERENCE: f32 = 0.2;

#[derive(Debug, Clone, Copy)]
pub struct SounderConfig {
    /// The radio's sample rate.
    pub fs_in: f64,
    /// How far above the radio's centre the signal's centre is: taken out
    /// before anything else.
    pub vfo_offset_hz: f64,
    pub bandwidth: Bandwidth,
    /// Symbols between those looked at: a multiple of 4.
    pub step: usize,
}

impl SounderConfig {
    pub fn new(fs_in: f64, vfo_offset_hz: f64) -> SounderConfig {
        SounderConfig {
            fs_in,
            vfo_offset_hz,
            bandwidth: Bandwidth::Mhz8,
            step: 8,
        }
    }
}

/// What the rows are rows of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub fft: FftSize,
    pub guard: Guard,
    /// Samples of a row (pilots in a symbol).
    pub columns: usize,
    /// The carrier of the first column, and the middle carrier (DC).
    pub first_carrier: usize,
    pub mid_carrier: usize,
    pub carrier_spacing_hz: f64,
    /// Seconds a symbol lasts, and between rows (a symbol in `step`).
    pub symbol_s: f64,
    pub row_s: f64,
    pub guard_us: f64,
}

impl Geometry {
    /// Frequency of column `c` relative to the channel's centre, Hz.
    pub fn column_hz(&self, c: usize) -> f64 {
        (self.first_carrier as f64 + 12.0 * c as f64 - self.mid_carrier as f64)
            * self.carrier_spacing_hz
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub tracking: bool,
    pub geometry: Option<Geometry>,
    /// The signal's offset from the radio's centre, Hz (the VFO offset and
    /// the carrier offset found).
    pub cfo_hz: f64,
    /// The radio's clock relative to the signal's, ppm.
    pub clock_ppm: f64,
    /// Symbols looked at since the lock.
    pub rows: u64,
    pub coherence: f32,
    /// Acquisition attempts since the last lock, and the best contrast of
    /// the cyclic-prefix search so far (low: no DVB-T here).
    pub attempts: u32,
    pub contrast: f32,
    pub locks: u32,
}

struct Want {
    start: u64,
    len: usize,
}

struct Track {
    n: usize,
    g: usize,
    per: usize,
    delta: usize,
    kmid: usize,
    pilots: Vec<u16>,
    /// 3/4 · (1 − 2 w_k) for each pilot: the reciprocal of its value.
    inv: Vec<f32>,
    /// Native position of the next symbol's guard start, predicted.
    apex: f64,
    rate: f64,
    f_total: f64,
    /// The rotation taken out so far, radians, at native position `rot_at`:
    /// the integral of the carrier offset, so that a change of the offset
    /// changes the phase's slope from then on and never jumps it.
    rot_phase: f64,
    rot_at: f64,
    plan: Fft1,
    hist: ChannelHistory,
    group: Vec<f32>,
    group_n: usize,
    bad: u32,
}

pub struct Sounder {
    cfg: SounderConfig,
    rho: f64,
    fs_native: f64,
    interp: Interpolator,
    want: Option<Want>,
    win: Vec<Iq>,
    /// The window again as separate real and imaginary arrays, made when it
    /// is complete, which is what the interpolation reads.
    planar: Planar,
    win_start: u64,
    filled: usize,
    track: Option<Track>,
    fading: Vec<Vec<f32>>,
    status: Status,
    next_attempt: u64,
}

impl Sounder {
    pub fn new(cfg: SounderConfig) -> Sounder {
        let fs_native = cfg.bandwidth.sample_rate_hz();
        Sounder {
            cfg,
            rho: cfg.fs_in / fs_native,
            fs_native,
            interp: Interpolator::new(),
            want: None,
            win: Vec::new(),
            planar: Planar::default(),
            win_start: 0,
            filled: 0,
            track: None,
            fading: Vec::new(),
            status: Status::default(),
            next_attempt: 0,
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// Forget the lock and the stretch wanted: the stream has jumped.
    pub fn reset(&mut self) {
        self.want = None;
        self.track = None;
        self.next_attempt = 0;
        self.status.tracking = false;
        self.status.geometry = None;
    }

    /// Fading rows made since the last call: |H|² in dB at each pilot.
    pub fn take_fading(&mut self, out: &mut Vec<Vec<f32>>) {
        out.append(&mut self.fading);
    }

    /// The delay–Doppler map of the last few seconds' rows.
    pub fn delay_doppler(&mut self) -> Option<&DelayDoppler> {
        self.track.as_mut()?.hist.map()
    }

    /// Hand over a block of the radio's samples, whose first is the stream's
    /// `abs_start`-th. Takes what it wants of it.
    pub fn push(&mut self, block: &[Iq], abs_start: u64) {
        let end = abs_start + block.len() as u64;
        if self.want.is_none() {
            self.want_acquisition(abs_start.max(self.next_attempt));
        }
        loop {
            let Some(want) = &self.want else { return };
            let (w0, w1) = (want.start, want.start + want.len as u64);
            // Copy the overlap with the stretch wanted.
            let (a, b) = (w0.max(abs_start), w1.min(end));
            if a < b {
                let src = &block[(a - abs_start) as usize..(b - abs_start) as usize];
                let at = (a - w0) as usize;
                self.win[at..at + src.len()].copy_from_slice(src);
                // Blocks arrive in order, so what is filled is a prefix.
                self.filled = at + src.len();
            }
            if self.filled < want.len {
                if w1 <= end {
                    // The stretch lay in samples we missed: ask again.
                    self.filled = 0;
                    let start = end;
                    self.retarget(start);
                }
                return;
            }
            self.process();
            if self.want.is_none() {
                return;
            }
        }
    }

    fn set_want(&mut self, start: u64, len: usize) {
        self.want = Some(Want { start, len });
        self.win.clear();
        self.win.resize(len, Iq::new(0.0, 0.0));
        self.win_start = start;
        self.filled = 0;
    }

    fn retarget(&mut self, start: u64) {
        if let Some(w) = &self.want {
            let len = w.len;
            self.set_want(start, len);
        }
    }

    fn want_acquisition(&mut self, start: u64) {
        let len = (ACQ_NATIVE as f64 * self.rho) as usize + 2 * PAD;
        self.set_want(start, len);
    }

    /// Native position (absolute, fractional) of window sample `j`.
    fn native_of_window(&self, j: f64) -> f64 {
        (self.win_start as f64 + j) / self.rho
    }

    /// Window coordinate of an absolute native position.
    fn window_of_native(&self, native: f64) -> f64 {
        native * self.rho - self.win_start as f64
    }

    fn process(&mut self) {
        self.planar.set(&self.win);
        self.status.tracking = self.track.is_some();
        if self.track.is_some() {
            self.track_symbol();
        } else {
            self.try_acquire();
        }
    }

    fn try_acquire(&mut self) {
        self.status.attempts += 1;
        // The window at the signal's rate, from the first sample the
        // interpolator can use, the vfo offset taken out.
        let first = PAD as f64;
        let native0 = self.native_of_window(first);
        let n_native = ((self.win.len() - 2 * PAD) as f64 / self.rho) as usize;
        let mut x = Vec::with_capacity(n_native);
        self.interp
            .resample_planar(&self.planar, first, self.rho, n_native, &mut x);
        if self.cfg.vfo_offset_hz != 0.0 {
            let w = -std::f64::consts::TAU * self.cfg.vfo_offset_hz / self.fs_native;
            let t0 = native0;
            let mut ph = (w * t0).rem_euclid(std::f64::consts::TAU);
            let step = Iq::new(w.cos() as f32, w.sin() as f32);
            let mut rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
            for (j, v) in x.iter_mut().enumerate() {
                *v *= rot;
                rot *= step;
                if j % 512 == 511 {
                    ph = (w * (t0 + j as f64 + 1.0)).rem_euclid(std::f64::consts::TAU);
                    rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
                }
            }
        }
        let next = self.win_start + self.win.len() as u64;
        self.next_attempt = self.win_start + (RETRY_S * self.cfg.fs_in) as u64;
        let Some(cp) = search_best(&x) else {
            self.after_failed_attempt();
            return;
        };
        self.status.contrast = self.status.contrast.max(cp.contrast);
        if cp.contrast < MIN_CONTRAST {
            self.after_failed_attempt();
            return;
        }
        let Some(a) = acquire::acquire(&x, &cp) else {
            self.after_failed_attempt();
            return;
        };

        // Locked. The first symbol looked at is the first at least a
        // symbol past the end of the stretch (so its samples are ahead).
        let fft = a.fft;
        let n = fft.n();
        let g = n / a.guard.divisor();
        let per = n + g;
        let apex_abs = native0 + a.apex;
        let now_native = next as f64 / self.rho;
        let m = ((now_native + per as f64 - apex_abs) / per as f64)
            .ceil()
            .max(1.0);
        let apex = apex_abs + m * per as f64;
        // That symbol's scattered phase.
        let phase = (a.phase + m as usize) % 4;
        let k_total = fft.active_carriers();
        let pilots: Vec<u16> = (3 * phase..k_total).step_by(12).map(|k| k as u16).collect();
        let wk = prbs(k_total);
        let inv = pilots
            .iter()
            .map(|&k| 0.75 * (1.0 - 2.0 * f32::from(wk[k as usize])))
            .collect();
        let df = self.fs_native / n as f64;
        let symbol_s = per as f64 / self.fs_native;
        let row_s = self.cfg.step as f64 * symbol_s;
        let delta = (g / 4).clamp(8, 128);
        self.track = Some(Track {
            n,
            g,
            per,
            delta,
            kmid: (k_total - 1) / 2,
            pilots,
            inv,
            apex,
            rate: 0.0,
            f_total: self.cfg.vfo_offset_hz + a.cfo_spacings * df,
            rot_phase: 0.0,
            rot_at: apex,
            plan: Fft1::new(n),
            hist: ChannelHistory::new(12.0 * df, row_s, g as f64 / self.fs_native * 1e6),
            group: Vec::new(),
            group_n: 0,
            bad: 0,
        });
        self.status.tracking = true;
        self.status.locks += 1;
        self.status.rows = 0;
        self.status.attempts = 0;
        self.status.geometry = Some(Geometry {
            fft,
            guard: a.guard,
            columns: (3 * phase..k_total).step_by(12).count(),
            first_carrier: 3 * phase,
            mid_carrier: (k_total - 1) / 2,
            carrier_spacing_hz: df,
            symbol_s,
            row_s,
            guard_us: g as f64 / self.fs_native * 1e6,
        });
        self.want_symbol();
    }

    fn after_failed_attempt(&mut self) {
        self.want = None;
        // Next attempt on fresh samples, a while on.
        self.status.tracking = false;
    }

    /// Ask for the stretch the next symbol lies in.
    fn want_symbol(&mut self) {
        let Some(t) = &self.track else { return };
        let lo = t.apex - SEARCH as f64 - 4.0;
        let hi = t.apex + SEARCH as f64 + (t.n + t.g) as f64 + 4.0;
        let a = (lo * self.rho).floor() as i64 - PAD as i64;
        let b = (hi * self.rho).ceil() as i64 + PAD as i64;
        if a < 0 {
            self.track = None;
            return;
        }
        self.set_want(a as u64, (b - a) as usize);
    }

    fn track_symbol(&mut self) {
        let Some(mut t) = self.track.take() else {
            return;
        };
        let (n, g) = (t.n, t.g);
        let tt = 1.0 / self.fs_native;
        let two_pi = std::f64::consts::TAU;

        // 1. The cyclic prefix near the prediction: two regions a symbol
        // apart, read on the native grid.
        let a_int = (t.apex - SEARCH as f64).floor();
        let len = g + 2 * SEARCH + 1;
        let (mut r1, mut r2) = (Vec::with_capacity(len), Vec::with_capacity(len));
        let pos = |native: f64| self.window_of_native(native);
        let ok = pos(a_int) >= BEFORE as f64
            && pos(a_int + (n + len) as f64) + AFTER as f64 <= self.win.len() as f64;
        if !ok {
            self.track = None;
            self.after_failed_attempt();
            return;
        }
        self.interp
            .resample_planar(&self.planar, pos(a_int), self.rho, len, &mut r1);
        self.interp
            .resample_planar(&self.planar, pos(a_int + n as f64), self.rho, len, &mut r2);
        let mut c: Vec<Iq> = Vec::with_capacity(2 * SEARCH + 1);
        let mut acc = Iq::new(0.0, 0.0);
        for m in 0..g {
            acc += r1[m] * r2[m].conj();
        }
        c.push(acc);
        for j in 0..2 * SEARCH {
            acc += r1[j + g] * r2[j + g].conj() - r1[j] * r2[j].conj();
            c.push(acc);
        }
        let mag: Vec<f32> = c.iter().map(|v| v.norm()).collect();
        let (j0, &peak) = mag
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        let edge = j0 == 0 || j0 == mag.len() - 1;
        let sub = if edge {
            0.0
        } else {
            parabola(mag[j0 - 1], peak, mag[j0 + 1])
        };
        let measured = a_int + j0 as f64 + sub;
        // The carrier offset's fraction: measured against what we expect.
        let df = self.fs_native / n as f64;
        let eps_meas = -(c[j0].im as f64).atan2(c[j0].re as f64) / two_pi;
        let expected = t.f_total / df;
        let eps_exp = expected - expected.round();
        let mut resid = eps_meas - eps_exp;
        resid -= resid.round();

        // 2. The loops.
        let step_len = (self.cfg.step * t.per) as f64;
        let err = measured - t.apex;
        t.apex += ALPHA * err;
        t.rate += BETA * err / step_len;
        t.f_total += GAMMA * resid * df;

        // 3. The useful part, read from where the filtered timing puts it,
        // and rotated from absolute time.
        let start = t.apex + (g - t.delta) as f64;
        let p0 = pos(start);
        if p0 < BEFORE as f64 || p0 + n as f64 * self.rho + AFTER as f64 > self.win.len() as f64 {
            self.track = None;
            self.after_failed_attempt();
            return;
        }
        let mut buf = Vec::with_capacity(n);
        self.interp
            .resample_planar(&self.planar, p0, self.rho, n, &mut buf);
        // The rotation's phase at this window's start: what was taken out
        // before, plus the offset now believed over the time since.
        t.rot_phase =
            (t.rot_phase + two_pi * t.f_total * (start - t.rot_at) * tt).rem_euclid(two_pi);
        t.rot_at = start;
        let w = -two_pi * t.f_total * tt;
        let step = Iq::new(w.cos() as f32, w.sin() as f32);
        let mut ph = -t.rot_phase;
        let mut rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
        for (j, v) in buf.iter_mut().enumerate() {
            *v *= rot;
            rot *= step;
            if j % 512 == 511 {
                // Renormalise the running phasor against rounding.
                ph = -t.rot_phase + w * (j + 1) as f64;
                rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
            }
        }
        t.plan.forward(&mut buf);

        // 4. The pilots: the channel at every 12th carrier. The window began
        // `delta` samples early, which is a ramp of e^{+j2πkΔ/N} to undo.
        let mut row = Vec::with_capacity(t.pilots.len());
        for (&k, &inv) in t.pilots.iter().zip(&t.inv) {
            let bin = bin_of(k as usize, t.kmid, 0, n);
            let k_signed = k as f64 - t.kmid as f64;
            let ramp = two_pi * k_signed * t.delta as f64 / n as f64;
            row.push(buf[bin] * Iq::new(ramp.cos() as f32, ramp.sin() as f32) * inv);
        }
        // Coherence of neighbouring pilots: high on a locked signal.
        let (mut num, mut den) = (Iq::new(0.0, 0.0), 0.0f32);
        for w2 in row.windows(2) {
            num += w2[1] * w2[0].conj();
            den += w2[1].norm() * w2[0].norm();
        }
        let coherence = if den > 0.0 { num.norm() / den } else { 0.0 };
        t.bad = if coherence < MIN_COHERENCE {
            t.bad + 1
        } else {
            0
        };

        t.hist.push(&row);
        if t.group.is_empty() {
            t.group = vec![0.0; row.len()];
        }
        for (a, v) in t.group.iter_mut().zip(&row) {
            *a += v.norm_sqr();
        }
        t.group_n += 1;
        if t.group_n == FADE_GROUP {
            let g = FADE_GROUP as f32;
            self.fading.push(
                t.group
                    .iter()
                    .map(|&p| 10.0 * (p / g).max(1e-12).log10())
                    .collect(),
            );
            t.group.fill(0.0);
            t.group_n = 0;
        }

        self.status.rows += 1;
        self.status.coherence = coherence;
        self.status.cfo_hz = t.f_total;
        self.status.clock_ppm = t.rate * 1e6;

        // Next symbol.
        t.apex += step_len * (1.0 + t.rate);
        if t.bad >= LOST_AFTER {
            self.track = None;
            self.status.tracking = false;
            self.status.geometry = None;
            self.after_failed_attempt();
            return;
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

    /// A sounder run over `seconds` of `mode` through `ch`, fed in blocks as
    /// the generator makes them, at the 10 MS/s of a HackRF.
    fn run(mode: &str, ch: Channel, seconds: f64) -> Sounder {
        let opts = SimOptions {
            mode: Mode::parse(mode).unwrap(),
            cell_id: None,
            ts_file: None,
            seconds,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: ch,
        };
        let mut s = Sounder::new(SounderConfig::new(10e6, 0.0));
        let mut abs = 0u64;
        generate(&opts, |b| {
            s.push(b, abs);
            abs += b.len() as u64;
            Ok(())
        })
        .unwrap();
        s
    }

    fn peak(m: &DelayDoppler) -> (f64, f64) {
        let (mut best, mut at) = (f32::MIN, (0, 0));
        for r in 0..m.dopplers {
            for c in 0..m.delays {
                if m.at(r, c) > best {
                    best = m.at(r, c);
                    at = (r, c);
                }
            }
        }
        (m.delay_us(at.1), m.doppler_hz(at.0))
    }

    /// The strongest value within ±`dd` µs and ±`dh` Hz of a point.
    fn near(m: &DelayDoppler, us: f64, hz: f64, dd: f64, dh: f64) -> f32 {
        let mut best = f32::MIN;
        for r in 0..m.dopplers {
            if (m.doppler_hz(r) - hz).abs() > dh {
                continue;
            }
            for c in 0..m.delays {
                if (m.delay_us(c) - us).abs() <= dd {
                    best = best.max(m.at(r, c));
                }
            }
        }
        best
    }

    #[test]
    fn it_locks_and_follows_a_carrier_offset_and_a_clock_offset() {
        let ch = Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(3.0, -6.0)],
            cfo_hz: 337_000.0,
            sco_ppm: 12.0,
            cn_db: Some(25.0),
            ..Default::default()
        };
        let mut s = run("64qam-2/3-8k-1/32", ch, 2.5);
        let st = s.status();
        assert!(st.tracking, "{st:?}");
        let g = st.geometry.unwrap();
        assert_eq!((g.fft, g.guard), (FftSize::K8, Guard::G1_32));
        assert_eq!(g.columns, 568);
        assert!((st.cfo_hz - 337_000.0).abs() < 300.0, "cfo {}", st.cfo_hz);
        // The clock offset follows to ~1 ppm on a clean channel; an echo makes
        // the cyclic-prefix timing wobble with the symbol's fractional
        // position, so a snapshot can be a few ppm out (the timing itself is
        // tracked all the same).
        assert!(
            (st.clock_ppm - 12.0).abs() < 8.0,
            "clock {} ppm",
            st.clock_ppm
        );
        assert!(st.coherence > 0.3, "{}", st.coherence);
        assert!(st.rows > 250, "{} rows", st.rows);
        // A fading row is 4 looked-at symbols.
        let mut rows = Vec::new();
        s.take_fading(&mut rows);
        assert!(rows.len() as u64 >= st.rows / 4 - 2 && rows[0].len() == 568);
    }

    #[test]
    fn the_map_shows_the_echo_at_its_delay() {
        let ch = Channel {
            paths: vec![Path::echo(0.0, 0.0), Path::echo(3.0, -6.0)],
            cfo_hz: 120_000.0,
            cn_db: Some(28.0),
            ..Default::default()
        };
        let mut s = run("64qam-2/3-8k-1/32", ch, 3.2);
        let m = s.delay_doppler().expect("a map").clone();
        let (tau, nu) = peak(&m);
        assert!(
            tau.abs() < 0.4 && nu.abs() < 1.5,
            "strongest at {tau} us, {nu} Hz"
        );
        // The echo: 6 dB down, 3 us on, the same Doppler.
        let e = near(&m, 3.0, nu, 0.4, 1.5);
        assert!((e + 6.0).abs() < 2.5, "echo at {e} dB");
        // Nothing at 15 us.
        assert!(near(&m, 15.0, nu, 1.0, 2.0) < -25.0);
        assert_eq!(m.guard_us.round(), 28.0);
    }

    #[test]
    fn the_map_shows_a_doppler_shifted_path() {
        // A static main path and an echo moving at +8 Hz.
        let ch = Channel {
            paths: vec![
                Path::echo(0.0, 0.0),
                Path {
                    doppler_hz: 8.0,
                    ..Path::echo(4.0, -6.0)
                },
            ],
            cfo_hz: -80_000.0,
            cn_db: Some(28.0),
            ..Default::default()
        };
        let mut s = run("64qam-2/3-8k-1/32", ch, 3.2);
        let m = s.delay_doppler().expect("a map").clone();
        let (tau, main_hz) = peak(&m);
        assert!(tau.abs() < 0.4, "main path at {tau} us");
        // The carrier loop sits between the two paths' frequencies, weighted
        // by their powers: the main path reads a little below 0, the echo
        // 8 Hz above it, whatever the offset.
        let echo = near(&m, 4.0, main_hz + 8.0, 0.5, 1.5);
        assert!((echo + 6.0).abs() < 2.5, "echo at {echo} dB");
        let wrong = near(&m, 4.0, main_hz - 8.0, 0.5, 1.5);
        assert!(wrong < -20.0, "{wrong} dB at the mirror");
    }

    #[test]
    fn it_works_in_2k_with_a_long_guard() {
        let ch = Channel {
            cfo_hz: -45_000.0,
            cn_db: Some(25.0),
            ..Default::default()
        };
        let s = run("16qam-3/4-2k-1/4", ch, 1.5);
        let st = s.status();
        assert!(st.tracking, "{st:?}");
        let g = st.geometry.unwrap();
        assert_eq!((g.fft, g.guard, g.columns), (FftSize::K2, Guard::G1_4, 142));
        assert!((st.cfo_hz + 45_000.0).abs() < 300.0, "{}", st.cfo_hz);
    }

    #[test]
    fn noise_alone_never_locks_and_costs_little() {
        let mut s = Sounder::new(SounderConfig::new(10e6, 0.0));
        let mut r = decdvbt_mod::rng::Rng::new(3);
        let mut abs = 0u64;
        for _ in 0..40 {
            let block: Vec<Iq> = (0..131_072)
                .map(|_| {
                    let (a, b) = r.gaussian_pair();
                    Iq::new(a as f32 * 0.1, b as f32 * 0.1)
                })
                .collect();
            s.push(&block, abs);
            abs += block.len() as u64;
        }
        let st = s.status();
        assert!(!st.tracking && st.locks == 0, "{st:?}");
        // 0.52 s of signal, retried every 0.4 s: a couple of attempts.
        assert!(
            st.attempts <= 14 && st.attempts >= 1,
            "{} attempts",
            st.attempts
        );
    }
}
