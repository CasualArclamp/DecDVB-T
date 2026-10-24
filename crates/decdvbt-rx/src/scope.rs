// made by CasualArclamp
//! What the display shows of the signal: constellation scopes (the cells of one
//! symbol, at four stages of the receiver) and measurements (the channel's gain
//! and signal-to-noise ratio across the band, its impulse response and echoes).
//!
//! Both are made on request and a few times a second, from the last symbol and
//! channel estimate: nothing here runs for a scope or a plot that is not shown.

use std::sync::Arc;

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Modulation;
use decdvbt_ofdm::pilots::{Role, SymbolPlan};
use rustfft::{Fft, FftPlanner};

/// Points kept of a scope's cells (a symbol has up to 6048 data cells).
const MAX_POINTS: usize = 3000;
/// Points of each of the curves across the band.
const CURVE_POINTS: usize = 1024;
/// The impulse response's floor, dB below its strongest tap.
const FLOOR_DB: f32 = -60.0;
/// Echoes listed, and how far below the strongest path they may be.
const MAX_ECHOES: usize = 8;
const ECHO_RANGE_DB: f32 = 35.0;

/// The cells of one symbol at four stages.
#[derive(Debug, Clone, Default)]
pub struct Scopes {
    /// The transform's output, before anything is done to it (scaled by the
    /// channel and the radio's gain).
    pub raw: Vec<Iq>,
    /// The data cells equalised (unit mean power).
    pub data: Vec<Iq>,
    /// The scattered and continual pilots equalised: ±4/3.
    pub pilots: Vec<Iq>,
    /// The TPS cells equalised: ±1.
    pub tps: Vec<Iq>,
    /// The constellation the data cells are on, once the TPS says.
    pub modulation: Option<Modulation>,
    /// A DVB-T2 signal's: the constellation's bits a cell and whether it is
    /// rotated (then `modulation` is `None`), and the level the pilots are
    /// normalised to (4/3 for DVB-T).
    pub t2_qam: Option<(u8, bool)>,
    /// A rotated T2 constellation's `data` are its points put back together
    /// from their two cells and turned back: on the square grid.
    pub derotated: bool,
    pub pilot_mark: Option<f32>,
    /// The symbol's number since the lock.
    pub symbol: u64,
}

/// Every `stride`-th of `x`, so that no more than `max` remain.
pub fn thin(x: impl ExactSizeIterator<Item = Iq>, max: usize) -> Vec<Iq> {
    let stride = x.len().div_ceil(max).max(1);
    x.step_by(stride).collect()
}

/// The scopes' cells from a symbol: `raw` the carriers as read, `z` them
/// equalised, `phase` the symbol's place among four (which carriers are
/// pilots).
pub fn snapshot(
    plan: &SymbolPlan,
    phase: usize,
    raw: &[Iq],
    z: &[Iq],
    modulation: Option<Modulation>,
    symbol: u64,
) -> Scopes {
    let roles = plan.roles(phase);
    let pick = |role: Role| {
        roles
            .iter()
            .zip(z)
            .filter(move |(r, _)| **r == role)
            .map(|(_, &c)| c)
    };
    Scopes {
        t2_qam: None,
        derotated: false,
        pilot_mark: None,
        raw: thin(raw.iter().copied(), MAX_POINTS),
        data: thin(
            plan.data_carriers(phase).iter().map(|&k| z[k as usize]),
            MAX_POINTS,
        ),
        pilots: thin(
            pick(Role::Pilot).collect::<Vec<_>>().into_iter(),
            MAX_POINTS,
        ),
        tps: pick(Role::Tps).collect(),
        modulation,
        symbol,
    }
}

/// One echo of the channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Echo {
    /// Delay against the strongest path, µs (negative: before it).
    pub delay_us: f32,
    /// Level against the strongest path, dB (0 or below).
    pub db: f32,
}

/// The channel as the pilots measure it.
#[derive(Debug, Clone, Default)]
pub struct Measurements {
    /// Across the band, for each point: the frequency from the channel's
    /// centre (MHz), the channel's gain against its mean (dB) and the signal-
    /// to-noise ratio there (dB).
    pub freq_mhz: Vec<f32>,
    pub gain_db: Vec<f32>,
    pub snr_db: Vec<f32>,
    /// The impulse response: for each tap its delay (µs) and its level against
    /// the strongest (dB, down to a floor), over a window around the guard.
    pub delay_us: Vec<f32>,
    pub impulse_db: Vec<f32>,
    /// The strongest echoes, nearest the strongest path first.
    pub echoes: Vec<Echo>,
    pub guard_us: f32,
    /// The mean signal-to-noise ratio over the band, dB.
    pub mean_snr_db: f32,
}

/// What `measure` needs and keeps between calls: the inverse transform.
#[derive(Default)]
pub struct MeasureState {
    ifft: Option<(usize, Arc<dyn Fft<f32>>)>,
}

/// The curves and the impulse response from a channel estimate: `hr`, `hi` its
/// real and imaginary parts per carrier, `w` its power |H|² and `noise` the
/// noise power at each carrier. `n` is the transform's length, `fs_native` the signal's own sample rate,
/// `guard_us` the guard interval.
pub fn measure(
    st: &mut MeasureState,
    n: usize,
    fs_native: f64,
    guard_us: f32,
    (hr, hi): (&[f32], &[f32]),
    w: &[f32],
    noise: &[f32],
) -> Measurements {
    let k_total = hr.len();
    let kmid = (k_total - 1) / 2;
    let df_mhz = (fs_native / n as f64 / 1e6) as f32;

    // Across the band: groups of carriers.
    let group = k_total.div_ceil(CURVE_POINTS).max(1);
    let mean_w = w.iter().sum::<f32>() / k_total as f32;
    let (mut freq, mut gain, mut snr) = (Vec::new(), Vec::new(), Vec::new());
    for g in (0..k_total).step_by(group) {
        let end = (g + group).min(k_total);
        let (sw, sn) = (
            w[g..end].iter().sum::<f32>(),
            noise[g..end].iter().sum::<f32>(),
        );
        let cnt = (end - g) as f32;
        let mid = (g + end - 1) as f32 / 2.0;
        freq.push((mid - kmid as f32) * df_mhz);
        gain.push(10.0 * (sw / cnt / mean_w.max(1e-20)).max(1e-9).log10());
        snr.push(10.0 * (sw / sn.max(1e-20)).max(1e-9).log10());
    }
    let mean_snr = 10.0
        * (w.iter().sum::<f32>() / noise.iter().sum::<f32>().max(1e-20))
            .max(1e-9)
            .log10();

    // The impulse response: the inverse transform of the estimate over the
    // carriers (a Hann window across them to keep a strong path's sidelobes
    // from filling the floor).
    let ifft = match &st.ifft {
        Some((len, f)) if *len == n => f.clone(),
        _ => {
            let f = FftPlanner::<f32>::new().plan_fft_inverse(n);
            st.ifft = Some((n, f.clone()));
            f
        }
    };
    let mut buf = vec![Iq::new(0.0, 0.0); n];
    for k in 0..k_total {
        let win = 0.5 - 0.5 * (std::f32::consts::TAU * (k as f32 + 0.5) / k_total as f32).cos();
        let bin = (k as isize - kmid as isize).rem_euclid(n as isize) as usize;
        buf[bin] = Iq::new(hr[k], hi[k]) * win;
    }
    ifft.process(&mut buf);
    let power: Vec<f32> = buf.iter().map(|c| c.norm_sqr()).collect();
    let peak = power.iter().copied().fold(1e-30, f32::max);
    let tap_us = (1e6 / fs_native) as f32;
    // A window around the guard: half a guard before the strongest path to a
    // guard and a half after, never less than ±20 µs.
    let before = (guard_us * 0.5).max(20.0);
    let after = (guard_us * 1.5).max(20.0);
    let lo = -((before / tap_us) as isize).min(n as isize / 2 - 1);
    let hi_tap = ((after / tap_us) as isize).min(n as isize / 2 - 1);
    let (mut delay, mut level) = (Vec::new(), Vec::new());
    for t in lo..=hi_tap {
        let idx = t.rem_euclid(n as isize) as usize;
        delay.push(t as f32 * tap_us);
        level.push((10.0 * (power[idx] / peak).max(1e-9).log10()).max(FLOOR_DB));
    }
    // The strongest path, in the window, and the local maxima against it.
    let top = level
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i);
    let mut peaks: Vec<Echo> = Vec::new();
    for i in 1..level.len().saturating_sub(1) {
        let is_peak = level[i] >= level[i - 1] && level[i] > level[i + 1];
        if is_peak && i != top && level[i] - level[top] > -ECHO_RANGE_DB {
            peaks.push(Echo {
                delay_us: delay[i] - delay[top],
                db: level[i] - level[top],
            });
        }
    }
    peaks.sort_by(|a, b| b.db.total_cmp(&a.db));
    peaks.truncate(MAX_ECHOES);
    peaks.sort_by(|a, b| a.delay_us.total_cmp(&b.delay_us));

    Measurements {
        freq_mhz: freq,
        gain_db: gain,
        snr_db: snr,
        delay_us: delay,
        impulse_db: level,
        echoes: peaks,
        guard_us,
        mean_snr_db: mean_snr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::dvbt::FftSize;

    /// A channel of two paths, 2 µs apart and 6 dB down: its estimate is
    /// 1 + a·e^(−j2πfτ); the impulse response has the second path at +2 µs,
    /// 6 dB below.
    #[test]
    fn an_echo_is_found_at_its_delay_and_level() {
        let fft = FftSize::K8;
        let fs = 64e6 / 7.0;
        let k_total = fft.active_carriers();
        let kmid = (k_total - 1) / 2;
        let df = fs / fft.n() as f64;
        let (tau, a) = (2e-6f64, 0.5f64);
        let (mut hr, mut hi) = (vec![0.0f32; k_total], vec![0.0f32; k_total]);
        for k in 0..k_total {
            let f = (k as f64 - kmid as f64) * df;
            let ph = -std::f64::consts::TAU * f * tau;
            hr[k] = (1.0 + a * ph.cos()) as f32;
            hi[k] = (a * ph.sin()) as f32;
        }
        let w: Vec<f32> = hr.iter().zip(&hi).map(|(r, i)| r * r + i * i).collect();
        let noise = vec![0.001f32; k_total];
        let m = measure(
            &mut MeasureState::default(),
            fft.n(),
            fs,
            28.0,
            (&hr, &hi),
            &w,
            &noise,
        );
        assert_eq!(m.freq_mhz.len(), m.gain_db.len());
        assert!(m.freq_mhz.len() <= CURVE_POINTS + 1);
        let e = m.echoes.iter().find(|e| e.db > -9.0).expect("an echo");
        assert!((e.delay_us - 2.0).abs() < 0.15, "{e:?}");
        assert!((e.db + 6.0).abs() < 1.0, "{e:?}");
        // The gain across the band swings by the two paths' beat: 20·log10(1.5/0.5).
        let (lo, hi_) = (
            m.gain_db.iter().copied().fold(f32::MAX, f32::min),
            m.gain_db.iter().copied().fold(f32::MIN, f32::max),
        );
        assert!(hi_ - lo > 8.0, "{lo} .. {hi_}");
    }

    #[test]
    fn scopes_take_each_kind_of_cell_from_the_roles() {
        let fft = FftSize::K2;
        let plan = SymbolPlan::new(fft);
        let k_total = fft.active_carriers();
        let z: Vec<Iq> = (0..k_total).map(|k| Iq::new(k as f32, 0.0)).collect();
        let s = snapshot(&plan, 1, &z, &z, Some(Modulation::Qam16), 7);
        assert_eq!(s.symbol, 7);
        assert_eq!(s.data.len(), plan.data_carriers(1).len());
        assert_eq!(s.tps.len(), 17);
        assert!(s.pilots.len() > 100);
        assert!(s.raw.len() <= MAX_POINTS);
        // The data cells are those carriers' values.
        assert_eq!(s.data[0].re, plan.data_carriers(1)[0] as f32);
    }
}
