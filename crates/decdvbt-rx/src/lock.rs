// made by CasualArclamp
//! Locking on: from a stretch of the radio's samples, find a DVB-T signal and
//! say where its next symbol is and how to read it. The sounder and the
//! receiver both start here.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{FftSize, Guard};

use crate::acquire::{self, MIN_CONTRAST, search_best};
use crate::interp::{Interpolator, Planar};
use crate::symbol::PAD;

/// Native samples an acquisition looks at (about 6 symbols of 8k, 1/32).
pub const ACQ_NATIVE: usize = 58_000;

/// A signal found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Locked {
    pub fft: FftSize,
    pub guard: Guard,
    /// Native position (absolute, fractional) of the guard interval of the
    /// first symbol at or after the `min_apex` asked for.
    pub apex: f64,
    /// The carrier offset from the radio's centre, Hz.
    pub f_total: f64,
    /// That symbol's scattered-pilot phase (its index in the frame mod 4).
    pub phase: usize,
    pub contrast: f32,
    pub concentration: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Failure {
    /// No cyclic prefix stands clear of the noise: the best contrast found.
    NoPrefix(f32),
    /// A cyclic prefix, but the pilots are not DVB-T's.
    NotDvbt(f32),
}

/// Look for a signal in `x`, whose first sample is the stream's `base`-th, at
/// `rho` times the signal's rate `fs_native`, `vfo_hz` above the radio's
/// centre. The symbol returned is the first whose guard starts at or after
/// native position `min_apex`.
pub fn lock_on(
    interp: &Interpolator,
    x: &Planar,
    base: u64,
    rho: f64,
    fs_native: f64,
    vfo_hz: f64,
    min_apex: f64,
) -> Result<Locked, Failure> {
    // The stretch at the signal's rate, from the first sample the
    // interpolator can use, the vfo offset taken out.
    let first = PAD as f64;
    let native0 = (base as f64 + first) / rho;
    let n_native = ((x.len() as f64 - 2.0 * PAD as f64) / rho).max(0.0) as usize;
    let mut y: Vec<Iq> = Vec::with_capacity(n_native);
    interp.resample_planar(x, first, rho, n_native, &mut y);
    if vfo_hz != 0.0 {
        let w = -std::f64::consts::TAU * vfo_hz / fs_native;
        let mut ph = (w * native0).rem_euclid(std::f64::consts::TAU);
        let step = Iq::new(w.cos() as f32, w.sin() as f32);
        let mut rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
        for (j, v) in y.iter_mut().enumerate() {
            *v *= rot;
            rot *= step;
            if j % 512 == 511 {
                ph = (w * (native0 + j as f64 + 1.0)).rem_euclid(std::f64::consts::TAU);
                rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
            }
        }
    }
    let Some(cp) = search_best(&y) else {
        return Err(Failure::NoPrefix(0.0));
    };
    if cp.contrast < MIN_CONTRAST {
        return Err(Failure::NoPrefix(cp.contrast));
    }
    let Some(a) = acquire::acquire(&y, &cp) else {
        return Err(Failure::NotDvbt(cp.contrast));
    };
    let n = a.fft.n();
    let g = n / a.guard.divisor();
    let per = (n + g) as f64;
    let apex_abs = native0 + a.apex;
    let m = ((min_apex - apex_abs) / per).ceil().max(0.0);
    let df = fs_native / n as f64;
    Ok(Locked {
        fft: a.fft,
        guard: a.guard,
        apex: apex_abs + m * per,
        f_total: vfo_hz + a.cfo_spacings * df,
        phase: (a.phase + m as usize) % 4,
        contrast: a.contrast,
        concentration: a.concentration,
    })
}
