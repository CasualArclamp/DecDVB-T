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
        SymbolReader {
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
        }
    }

    /// The carrier spacing, Hz.
    pub fn spacing_hz(&self) -> f64 {
        self.fs_native / self.n as f64
    }

    /// The stretch of the radio's stream the next read needs, as an absolute
    /// start index and a length; `None` if it would start before the
    /// stream does.
    pub fn want(&self) -> Option<(u64, usize)> {
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
        let pos = |native: f64| native * self.rho - base as f64;

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
        let mean = mag.iter().sum::<f32>() / mag.len() as f32;
        // The carrier offset's fraction, against what we expect.
        let df = self.spacing_hz();
        let eps_meas = -f64::from(c[j0].im).atan2(f64::from(c[j0].re)) / two_pi;
        let expected = self.f_total / df;
        let eps_exp = expected - expected.round();
        let mut resid = eps_meas - eps_exp;
        resid -= resid.round();

        // 2. The loops.
        let step_len = (self.interval * self.per) as f64;
        let err = measured - self.apex;
        self.apex += self.alpha_cp * err;
        self.rate += self.beta_cp * err / step_len;
        self.f_total += GAMMA * resid * df;

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
