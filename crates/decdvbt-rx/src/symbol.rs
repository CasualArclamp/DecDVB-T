// made by CasualArclamp
//! Reading OFDM symbols from the radio's samples, one after another: the
//! timing, carrier-offset and clock tracker and the transform, shared by the
//! channel sounder (which reads one symbol in M) and the receiver (which
//! reads every one).
//!
//! Per symbol, from a position predicted from the last:
//!
//! 1. The cyclic prefix near the prediction gives the symbol's timing (to a
//!    fraction of a sample, by a parabola) and the carrier offset's fraction;
//!    an α-β loop follows the position and the sampling clock's drift, a slow
//!    loop the carrier.
//! 2. The useful part, started a quarter of the guard early (clear of echoes
//!    either side), is read at exact fractional positions by interpolation
//!    and rotated by the carrier offset's *integrated* phase (a rotation from
//!    absolute time would jump at every update of the offset, which smears
//!    Doppler and the carriers' phase from symbol to symbol), and transformed.
//! 3. The K active carriers are picked out, the early start's phase ramp
//!    e^{+j2πkΔ/N} undone.
//!
//! **Direct reading.** When the radio's rate makes the symbol a whole number of
//! samples (8960 at 10 MS/s: the carrier spacing is then exactly a bin of the
//! transform) there is no need to interpolate at all: the window is the
//! radio's samples from the nearest integer position, transformed as they
//! are, and the fraction of a sample by which the position missed becomes a
//! phase ramp over the carriers (the same one, in fact, that undoes the early
//! start). That drops the interpolator's 13 ns a sample, 80 % of what a read
//! cost. What it gives up is the clock rate's correction *within* a symbol
//! (the window is not stretched by the rate tracked): at 10 ppm that is an
//! inter-carrier interference of −29 dB at the band's very edge, under the
//! noise of any real signal; the position still follows the clock from symbol
//! to symbol.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{FftSize, Guard};

use crate::acquire::{Fft1, parabola};
use crate::interp::{AFTER, BEFORE, Interpolator, Planar};

/// The timing search around the prediction, native samples either side.
pub const SEARCH: usize = 32;
/// Margin, input samples, either side of a wanted stretch for the
/// interpolator.
pub const PAD: usize = BEFORE + AFTER + 8;
/// Loop gains of the cyclic-prefix tracker: position, clock rate, carrier
/// offset. (A flat triangle of a peak: noisy to a sample or more with echoes,
/// so a receiver turns them down and lets the pilots' phase slope do the fine
/// work: see [`SymbolReader::correct`].)
const ALPHA: f64 = 0.25;
const BETA: f64 = 0.02;
const GAMMA: f64 = 0.03;
/// The gains of the fine timing loop fed by the pilots (position, rate).
const FINE_ALPHA: f64 = 0.3;
const FINE_BETA: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The stretch of samples the symbol needs is not (all) in the buffer.
    OutOfRange,
}

/// What a read learned about the symbol.
#[derive(Debug, Clone, Copy)]
pub struct ReadInfo {
    /// Where its guard interval started (native samples, fractional,
    /// absolute).
    pub apex: f64,
    /// The cyclic-prefix correlation at the peak, against its mean over the
    /// search: a locked symbol stands well clear.
    pub cp_contrast: f32,
}

pub struct SymbolReader {
    pub fft: FftSize,
    pub guard: Guard,
    pub n: usize,
    pub g: usize,
    pub per: usize,
    pub delta: usize,
    pub kmid: usize,
    pub k_total: usize,
    /// The predicted native position of the next symbol's guard start.
    pub apex: f64,
    /// The radio's clock relative to the signal's (fraction).
    pub rate: f64,
    /// The carrier offset, Hz.
    pub f_total: f64,
    rot_phase: f64,
    rot_at: f64,
    /// Symbols between the ones read.
    interval: usize,
    /// The cyclic-prefix loop's gains (position, clock rate): [`ALPHA`] and
    /// [`BETA`] until a receiver turns them down.
    pub alpha_cp: f64,
    pub beta_cp: f64,
    plan: Fft1,
    /// The early-start phase ramp of every carrier.
    ramp: Vec<Iq>,
    rho: f64,
    fs_native: f64,
    /// Direct reading: the symbol's length and the guard's in the radio's
    /// samples (0: the symbol is not a whole number of them), the transform
    /// of that size, and scratch (the window, the correlation, the ramp).
    n_in: usize,
    g_in: usize,
    search_in: usize,
    plan_in: Option<Fft1>,
    window: Vec<Iq>,
    corr: Vec<Iq>,
    ramp_in: Vec<Iq>,
}

/// The peak of the correlation magnitudes: its place (with a parabola's
/// fraction), its height and the mean.
fn find_peak(c: &[Iq]) -> (usize, f64, f32, f32) {
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
    let mean = mag.iter().sum::<f32>() / mag.len() as f32;
    (j0, sub, peak, mean)
}

impl SymbolReader {
    /// A reader for `fft`/`guard`, the first symbol's guard predicted at
    /// native position `apex`, the carrier offset `f_total`. `rho` is the
    /// radio's rate over the signal's, `interval` the symbols between reads.
    pub fn new(
        fft: FftSize,
        guard: Guard,
        apex: f64,
        f_total: f64,
        rho: f64,
        fs_native: f64,
        interval: usize,
    ) -> SymbolReader {
        let n = fft.n();
        let g = n / guard.divisor();
        let delta = (g / 4).clamp(8, 128);
        let k_total = fft.active_carriers();
        let kmid = (k_total - 1) / 2;
        let ramp = (0..k_total)
            .map(|k| {
                let ph = std::f64::consts::TAU * (k as f64 - kmid as f64) * delta as f64 / n as f64;
                Iq::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let mut r = SymbolReader {
            fft,
            guard,
            n,
            g,
            per: n + g,
            delta,
            kmid,
            k_total,
            apex,
            rate: 0.0,
            f_total,
            rot_phase: 0.0,
            rot_at: apex,
            interval,
            alpha_cp: ALPHA,
            beta_cp: BETA,
            plan: Fft1::new(n),
            ramp,
            rho,
            fs_native,
            n_in: 0,
            g_in: 0,
            search_in: 0,
            plan_in: None,
            window: Vec::new(),
            corr: Vec::new(),
            ramp_in: Vec::new(),
        };
        // A whole number of the radio's samples to the symbol?
        let n_in = n as f64 * rho;
        if (n_in - n_in.round()).abs() < 1e-6 && n_in.round() >= n as f64 * 0.5 {
            r.n_in = n_in.round() as usize;
            r.g_in = (g as f64 * rho).round() as usize;
            r.search_in = (SEARCH as f64 * rho).ceil() as usize;
            r.plan_in = Some(Fft1::new(r.n_in));
            r.window = vec![Iq::new(0.0, 0.0); r.n_in];
            r.ramp_in = vec![Iq::new(1.0, 0.0); k_total];
        }
        r
    }

    /// Whether [`SymbolReader::read_direct`] can be used.
    pub fn is_direct(&self) -> bool {
        self.n_in > 0
    }

    /// The carrier spacing, Hz.
    pub fn spacing_hz(&self) -> f64 {
        self.fs_native / self.n as f64
    }

    /// The stretch of the radio's stream the next read needs, as an absolute
    /// start index and a length; `None` if it would start before the
    /// stream does.
    pub fn want(&self) -> Option<(u64, usize)> {
        if self.is_direct() {
            // The correlation's two regions (the second the farther) and the
            // window, which lies inside them.
            let a = (self.apex * self.rho).floor() as i64 - self.search_in as i64 - 2;
            let len = self.n_in + self.g_in + 2 * self.search_in + 8;
            return (a >= 0).then_some((a as u64, len));
        }
        let lo = self.apex - SEARCH as f64 - 4.0;
        let hi = self.apex + SEARCH as f64 + self.per as f64 + 4.0;
        let a = (lo * self.rho).floor() as i64 - PAD as i64;
        let b = (hi * self.rho).ceil() as i64 + PAD as i64;
        (a >= 0).then_some((a as u64, (b - a) as usize))
    }

    /// The fine timing correction: the last symbol was read `tau` samples
    /// later than the reference's (from the pilots' phase slope, which is
    /// accurate to a few hundredths of a sample). Moves the prediction and
    /// the clock estimate to take it up.
    pub fn correct(&mut self, tau: f64) {
        let step_len = (self.interval * self.per) as f64;
        self.apex -= FINE_ALPHA * tau;
        self.rate -= FINE_BETA * tau / step_len;
    }

    /// The loops: the cyclic prefix put the symbol at `measured` (native
    /// samples) and its correlation there, `peak`, has the phase of the
    /// carrier offset's fraction.
    fn follow(&mut self, measured: f64, peak: Iq) {
        let df = self.spacing_hz();
        let eps_meas = -f64::from(peak.im).atan2(f64::from(peak.re)) / std::f64::consts::TAU;
        let expected = self.f_total / df;
        let eps_exp = expected - expected.round();
        let mut resid = eps_meas - eps_exp;
        resid -= resid.round();
        let step_len = (self.interval * self.per) as f64;
        let err = measured - self.apex;
        self.apex += self.alpha_cp * err;
        self.rate += self.beta_cp * err / step_len;
        self.f_total += GAMMA * resid * df;
    }

    /// Read the next symbol straight from the radio's samples `x` (the
    /// first the stream's `base`-th): see the module's note. Needs
    /// [`SymbolReader::is_direct`].
    pub fn read_direct(
        &mut self,
        x: &[Iq],
        base: u64,
        out: &mut Vec<Iq>,
    ) -> Result<ReadInfo, ReadError> {
        debug_assert!(self.is_direct());
        let (n_in, g_in, search) = (self.n_in, self.g_in, self.search_in);
        let rho = self.rho;
        let two_pi = std::f64::consts::TAU;

        // 1. The cyclic prefix near the prediction, on the radio's samples: no
        // copy, the two regions are slices of the stream.
        let a_int = (self.apex * rho).floor() as i64 - search as i64;
        let len = g_in + 2 * search + 1;
        let off = a_int - base as i64;
        if off < 0 || off as usize + n_in + len > x.len() {
            return Err(ReadError::OutOfRange);
        }
        let off = off as usize;
        let (r1, r2) = (&x[off..off + len], &x[off + n_in..off + n_in + len]);
        self.corr.clear();
        let mut acc = Iq::new(0.0, 0.0);
        for m in 0..g_in {
            acc += r1[m] * r2[m].conj();
        }
        self.corr.push(acc);
        for j in 0..2 * search {
            acc += r1[j + g_in] * r2[j + g_in].conj() - r1[j] * r2[j].conj();
            self.corr.push(acc);
        }
        let (j0, sub, peak, mean) = find_peak(&self.corr);
        let measured = (a_int as f64 + j0 as f64 + sub) / rho;
        let at_peak = self.corr[j0];

        // 2. The loops.
        self.follow(measured, at_peak);
        let step_len = (self.interval * self.per) as f64;

        // 3. The window: the radio's samples from the integer position nearest
        // where the filtered timing puts it, `delta` native samples ahead of the
        // useful part, rotated by the integrated offset as it is copied.
        let useful_in = (self.apex + self.g as f64) * rho;
        let start_f = useful_in - self.delta as f64 * rho;
        let s = start_f.round() as i64;
        let from = s - base as i64;
        if from < 0 || from as usize + n_in > x.len() {
            return Err(ReadError::OutOfRange);
        }
        let from = from as usize;
        // How many of the radio's samples (a fraction) the window starts
        // before the useful part.
        let early = useful_in - s as f64;

        let tt = 1.0 / (self.fs_native * rho);
        let start_native = s as f64 / rho;
        self.rot_phase = (self.rot_phase
            + two_pi * self.f_total * (start_native - self.rot_at) / self.fs_native)
            .rem_euclid(two_pi);
        self.rot_at = start_native;
        let w = -two_pi * self.f_total * tt;
        // Eight chains of phasors a step of eight apart, which the CPU can run
        // side by side (one chain is a wait of four multiplies' latency a
        // sample); no renormalising over 1120 steps each.
        let mut rots = [Iq::new(1.0, 0.0); 8];
        for (t, r) in rots.iter_mut().enumerate() {
            let ph = -self.rot_phase + w * t as f64;
            *r = Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        let step8 = Iq::new((8.0 * w).cos() as f32, (8.0 * w).sin() as f32);
        let src = &x[from..from + n_in];
        let (src8, tail) = src.as_chunks::<8>();
        let (dst8, _) = self.window.as_chunks_mut::<8>();
        for (d, sx) in dst8.iter_mut().zip(src8) {
            for t in 0..8 {
                d[t] = sx[t] * rots[t];
                rots[t] *= step8;
            }
        }
        let done = src8.len() * 8;
        for (t, v) in tail.iter().enumerate() {
            self.window[done + t] = v * rots[t];
        }
        self.plan_in.as_mut().unwrap().forward(&mut self.window);

        // 4. The carriers, the ramp of the early start (and of the fraction of
        // a sample the position missed by) undone: e^{+j2π(k − kmid)·early/N}.
        let th = two_pi * early / n_in as f64;
        let step = Iq::new(th.cos() as f32, th.sin() as f32);
        let mut p = Iq::new(1.0, 0.0);
        self.ramp_in[self.kmid] = p;
        for i in 1..=self.kmid {
            p *= step;
            if i % 256 == 0 {
                let a = th * i as f64;
                p = Iq::new(a.cos() as f32, a.sin() as f32);
            }
            self.ramp_in[self.kmid + i] = p;
            self.ramp_in[self.kmid - i] = p.conj();
        }
        out.clear();
        out.reserve(self.k_total);
        // Carriers below the middle are the transform's upper bins.
        let lower = &self.window[n_in - self.kmid..];
        let upper = &self.window[..self.kmid + 1];
        out.extend(
            lower
                .iter()
                .chain(upper)
                .zip(&self.ramp_in)
                .map(|(b, r)| b * r),
        );

        let info = ReadInfo {
            apex: self.apex,
            cp_contrast: peak / mean.max(1e-20),
        };
        self.apex += step_len * (1.0 + self.rate);
        Ok(info)
    }

    /// Read the next symbol: its K carriers into `out`, from `x`, whose
    /// first sample is the stream's `base`-th. Moves the prediction on by
    /// `interval` symbols.
    pub fn read(
        &mut self,
        interp: &Interpolator,
        x: &Planar,
        base: u64,
        out: &mut Vec<Iq>,
    ) -> Result<ReadInfo, ReadError> {
        let (n, g) = (self.n, self.g);
        let tt = 1.0 / self.fs_native;
        let two_pi = std::f64::consts::TAU;
        let rho = self.rho;
        let pos = |native: f64| native * rho - base as f64;

        // 1. The cyclic prefix near the prediction: two regions a symbol
        // apart, read on the native grid.
        let a_int = (self.apex - SEARCH as f64).floor();
        let len = g + 2 * SEARCH + 1;
        let ok = pos(a_int) >= BEFORE as f64
            && pos(a_int + (n + len) as f64) + AFTER as f64 <= x.len() as f64;
        if !ok {
            return Err(ReadError::OutOfRange);
        }
        let (mut r1, mut r2) = (Vec::with_capacity(len), Vec::with_capacity(len));
        interp.resample_planar(x, pos(a_int), self.rho, len, &mut r1);
        interp.resample_planar(x, pos(a_int + n as f64), self.rho, len, &mut r2);
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
        let (j0, sub, peak, mean) = find_peak(&c);
        let measured = a_int + j0 as f64 + sub;

        // 2. The loops.
        self.follow(measured, c[j0]);
        let step_len = (self.interval * self.per) as f64;

        // 3. The useful part, from where the filtered timing puts it,
        // rotated by the integrated offset.
        let start = self.apex + (g - self.delta) as f64;
        let p0 = pos(start);
        if p0 < BEFORE as f64 || p0 + n as f64 * self.rho + AFTER as f64 > x.len() as f64 {
            return Err(ReadError::OutOfRange);
        }
        // The window is read at the clock's rate as tracked, not the nominal:
        // 15 ppm is 0.12 sample over a symbol, which at the band's edge is
        // an ICI of about −30 dB.
        let mut buf = Vec::with_capacity(n);
        interp.resample_planar(x, p0, self.rho * (1.0 + self.rate), n, &mut buf);
        // The rotation's phase at this window's start: what was taken out
        // before, plus the offset now believed over the time since.
        self.rot_phase = (self.rot_phase + two_pi * self.f_total * (start - self.rot_at) * tt)
            .rem_euclid(two_pi);
        self.rot_at = start;
        let w = -two_pi * self.f_total * tt;
        let step = Iq::new(w.cos() as f32, w.sin() as f32);
        let mut ph = -self.rot_phase;
        let mut rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
        for (j, v) in buf.iter_mut().enumerate() {
            *v *= rot;
            rot *= step;
            if j % 512 == 511 {
                // Renormalise the running phasor against rounding.
                ph = -self.rot_phase + w * (j + 1) as f64;
                rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
            }
        }
        self.plan.forward(&mut buf);

        // 4. The carriers, the early start's ramp undone.
        out.clear();
        out.reserve(self.k_total);
        for k in 0..self.k_total {
            let bin = (k as isize - self.kmid as isize).rem_euclid(n as isize) as usize;
            out.push(buf[bin] * self.ramp[k]);
        }

        let info = ReadInfo {
            apex: self.apex,
            cp_contrast: peak / mean.max(1e-20),
        };
        self.apex += step_len * (1.0 + self.rate);
        Ok(info)
    }
}
