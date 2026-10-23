// made by CasualArclamp
//! The delay–Doppler map: the channel's scattering function, from the channel
//! estimates of the last few seconds. Each propagation path is a spot at its
//! delay and its Doppler shift; a path's Doppler spread smears its spot along
//! the Doppler axis.
//!
//! The method and the display are DecDRM's (`decdrm-core::rx::scatter`, same
//! author): per row, the transfer function over frequency becomes an impulse
//! response (inverse FFT, Hann window); per delay, the impulse response over
//! the window of rows becomes a Doppler spectrum (FFT over time, Hann
//! window).
//!
//! What differs from DRM is the rows: DVB-T's scattered pilots every 12th
//! carrier give one transfer function sample per 12 carriers, so a row is
//! `P` ≈ K/12 samples 12 carrier spacings apart (13.4 kHz for 8k), and its
//! delays are unambiguous to 1/(12 Δf), 74.7 µs in 8k mode, beyond the guard
//! interval of any mode. Rows are of one symbol in M (a multiple of 4, so
//! the same carriers are pilots every time), `row_s` apart.

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::Arc;

use decdvbt_core::Iq;
use rustfft::{Fft, FftPlanner};

/// Seconds of rows behind each map.
pub const WINDOW_S: f64 = 4.0;
/// The map's floor below its strongest point, dB.
pub const FLOOR_DB: f32 = -40.0;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DelayDoppler {
    /// Power, dB relative to the strongest point (down to [`FLOOR_DB`]):
    /// `dopplers` rows of `delays` values, the lowest Doppler first.
    pub db: Vec<f32>,
    pub delays: usize,
    pub dopplers: usize,
    /// Delay of the first column and the step between columns, µs.
    pub delay_start_us: f64,
    pub delay_step_us: f64,
    /// Doppler shift of the first row and the step between rows, Hz.
    pub doppler_start_hz: f64,
    pub doppler_step_hz: f64,
    pub window_s: f64,
    /// The guard interval, µs: echoes beyond it interfere with the next
    /// symbol.
    pub guard_us: f64,
}

impl DelayDoppler {
    pub fn at(&self, r: usize, c: usize) -> f32 {
        self.db[r * self.delays + c]
    }

    pub fn delay_us(&self, c: usize) -> f64 {
        self.delay_start_us + c as f64 * self.delay_step_us
    }

    pub fn doppler_hz(&self, r: usize) -> f64 {
        self.doppler_start_hz + r as f64 * self.doppler_step_hz
    }
}

/// The Doppler spectrum at each carrier: how fast each part of the band
/// fades. A static channel is a line at 0 Hz across the band; a path that
/// moves puts energy at its Doppler shift at the carriers where it is strong
/// against the others, so a fast fade shows as a pair of bright patches
/// either side of 0 Hz that swell and shrink across the band.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FreqDoppler {
    /// Power, dB relative to the strongest point (down to [`FLOOR_DB`]):
    /// `dopplers` rows of `cols` values, the lowest Doppler first.
    pub db: Vec<f32>,
    pub cols: usize,
    pub dopplers: usize,
    /// Frequency of the first column from the middle of the band, and the
    /// step between columns, MHz.
    pub freq_start_mhz: f64,
    pub freq_step_mhz: f64,
    pub doppler_start_hz: f64,
    pub doppler_step_hz: f64,
    pub window_s: f64,
}

impl FreqDoppler {
    pub fn at(&self, r: usize, c: usize) -> f32 {
        self.db[r * self.cols + c]
    }

    pub fn freq_mhz(&self, c: usize) -> f64 {
        self.freq_start_mhz + c as f64 * self.freq_step_mhz
    }

    pub fn doppler_hz(&self, r: usize) -> f64 {
        self.doppler_start_hz + r as f64 * self.doppler_step_hz
    }
}

/// The Doppler spectrum of every `stride`-th pilot's transfer function over
/// `rows` (`row_s` apart): `None` for too few rows or a channel of zeros.
pub fn freq_doppler(
    rows: &[Vec<Iq>],
    pilot_spacing_hz: f64,
    row_s: f64,
    doppler_max_hz: f64,
    stride: usize,
) -> Option<FreqDoppler> {
    let t_len = rows.len();
    let n_p = rows.first()?.len();
    if t_len < 8 || n_p < 8 || rows.iter().any(|r| r.len() != n_p) {
        return None;
    }
    let stride = stride.max(1);
    let win = hann(t_len);
    let nf = t_len.next_power_of_two().max(256);
    let doppler_step_hz = 1.0 / (nf as f64 * row_s);
    let half = ((doppler_max_hz / doppler_step_hz).floor() as usize).min(nf / 2 - 1);
    let dopplers = 2 * half + 1;
    let fft: Arc<dyn Fft<f32>> = FftPlanner::<f32>::new().plan_fft_forward(nf);
    let mut scratch = vec![Iq::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut col = vec![Iq::new(0.0, 0.0); nf];
    let picks: Vec<usize> = (0..n_p).step_by(stride).collect();
    let cols = picks.len();
    let mut power = vec![0.0f32; dopplers * cols];
    for (c, &p) in picks.iter().enumerate() {
        col.fill(Iq::new(0.0, 0.0));
        for (t, row) in rows.iter().enumerate() {
            col[t] = row[p] * win[t];
        }
        fft.process_with_scratch(&mut col, &mut scratch);
        for (r, m) in (-(half as i64)..=half as i64).enumerate() {
            power[r * cols + c] = col[m.rem_euclid(nf as i64) as usize].norm_sqr();
        }
    }
    let peak = power.iter().copied().fold(0.0, f32::max);
    if peak <= 0.0 || !peak.is_finite() {
        return None;
    }
    let db = power
        .iter()
        .map(|&p| (10.0 * (p / peak).max(1e-12).log10()).max(FLOOR_DB))
        .collect();
    let step_mhz = stride as f64 * pilot_spacing_hz / 1e6;
    Some(FreqDoppler {
        db,
        cols,
        dopplers,
        freq_start_mhz: -((n_p - 1) as f64 / 2.0) * pilot_spacing_hz / 1e6,
        freq_step_mhz: step_mhz,
        doppler_start_hz: -(half as f64) * doppler_step_hz,
        doppler_step_hz,
        window_s: t_len as f64 * row_s,
    })
}

fn hann(n: usize) -> Vec<f32> {
    if n < 2 {
        return vec![1.0; n];
    }
    (0..n)
        .map(|i| (0.5 - 0.5 * (2.0 * PI * i as f64 / (n - 1) as f64).cos()) as f32)
        .collect()
}

/// The map of `rows` (transfer function at pilots `pilot_spacing_hz` apart),
/// `row_s` apart in time: delays in `delay_range_us` (negative are
/// pre-echoes), Doppler shifts within ±`doppler_max_hz`. `None` for too few
/// rows or a channel of zeros.
pub fn delay_doppler(
    rows: &[Vec<Iq>],
    pilot_spacing_hz: f64,
    row_s: f64,
    delay_range_us: (f64, f64),
    doppler_max_hz: f64,
) -> Option<DelayDoppler> {
    let t_len = rows.len();
    let n_p = rows.first()?.len();
    if t_len < 8 || n_p < 8 || rows.iter().any(|r| r.len() != n_p) {
        return None;
    }

    // The delay transform: the samples zero-padded to a power of two (568
    // pilots to 1024: finer than the 1/(bandwidth) the data can resolve).
    let nd = n_p.next_power_of_two();
    let delay_step_us = 1e6 / (nd as f64 * pilot_spacing_hz);
    let first = (delay_range_us.0 / delay_step_us).floor() as i64;
    let last = (delay_range_us.1 / delay_step_us).ceil() as i64;
    let delays = ((last - first + 1).max(1) as usize).min(nd);
    let win_f = hann(n_p);
    let win_t = hann(t_len);
    let mut planner = FftPlanner::<f32>::new();
    let ifft: Arc<dyn Fft<f32>> = planner.plan_fft_inverse(nd);
    let mut buf = vec![Iq::new(0.0, 0.0); nd];
    let mut scratch = vec![Iq::new(0.0, 0.0); ifft.get_inplace_scratch_len()];
    // h[t · delays + d]: the impulse response at the kept delays, windowed in
    // time.
    let mut h = vec![Iq::new(0.0, 0.0); t_len * delays];
    for (t, row) in rows.iter().enumerate() {
        buf.fill(Iq::new(0.0, 0.0));
        for ((b, &v), &w) in buf.iter_mut().zip(row).zip(&win_f) {
            *b = v * w;
        }
        ifft.process_with_scratch(&mut buf, &mut scratch);
        for d in 0..delays {
            let bin = (first + d as i64).rem_euclid(nd as i64) as usize;
            h[t * delays + d] = buf[bin] * win_t[t];
        }
    }

    // The Doppler transform per delay, the window zero-padded to a power of
    // two (540 rows to 1024).
    let nf = t_len.next_power_of_two().max(256);
    let doppler_step_hz = 1.0 / (nf as f64 * row_s);
    let half = ((doppler_max_hz / doppler_step_hz).floor() as usize).min(nf / 2 - 1);
    let dopplers = 2 * half + 1;
    let fft: Arc<dyn Fft<f32>> = planner.plan_fft_forward(nf);
    let mut scratch = vec![Iq::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut col = vec![Iq::new(0.0, 0.0); nf];
    let mut power = vec![0.0f32; dopplers * delays];
    for d in 0..delays {
        col.fill(Iq::new(0.0, 0.0));
        for t in 0..t_len {
            col[t] = h[t * delays + d];
        }
        fft.process_with_scratch(&mut col, &mut scratch);
        for (r, m) in (-(half as i64)..=half as i64).enumerate() {
            power[r * delays + d] = col[m.rem_euclid(nf as i64) as usize].norm_sqr();
        }
    }
    let peak = power.iter().copied().fold(0.0, f32::max);
    if peak <= 0.0 || !peak.is_finite() {
        return None;
    }
    let db = power
        .iter()
        .map(|&p| (10.0 * (p / peak).max(1e-12).log10()).max(FLOOR_DB))
        .collect();
    Some(DelayDoppler {
        db,
        delays,
        dopplers,
        delay_start_us: first as f64 * delay_step_us,
        delay_step_us,
        doppler_start_hz: -(half as f64) * doppler_step_hz,
        doppler_step_hz,
        window_s: t_len as f64 * row_s,
        guard_us: 0.0,
    })
}

/// The rows of the last [`WINDOW_S`] seconds, and the latest map: made when
/// asked for rather than per row.
pub struct ChannelHistory {
    rows: VecDeque<Vec<Iq>>,
    max_rows: usize,
    stale: bool,
    pilot_spacing_hz: f64,
    row_s: f64,
    guard_us: f64,
    doppler_max_hz: f64,
    map: Option<DelayDoppler>,
    freq_map: Option<FreqDoppler>,
    freq_stale: bool,
}

impl ChannelHistory {
    pub fn new(pilot_spacing_hz: f64, row_s: f64, guard_us: f64) -> ChannelHistory {
        ChannelHistory {
            rows: VecDeque::new(),
            max_rows: (WINDOW_S / row_s).round() as usize,
            stale: false,
            pilot_spacing_hz,
            row_s,
            guard_us,
            doppler_max_hz: (0.45 / row_s).min(100.0),
            map: None,
            freq_map: None,
            freq_stale: false,
        }
    }

    pub fn push(&mut self, row: &[Iq]) {
        if self.rows.front().is_some_and(|r| r.len() != row.len()) {
            self.clear();
        }
        let mut r = if self.rows.len() >= self.max_rows {
            self.rows.pop_front().unwrap_or_default()
        } else {
            Vec::new()
        };
        r.clear();
        r.extend_from_slice(row);
        self.rows.push_back(r);
        self.stale = true;
        self.freq_stale = true;
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.stale = false;
        self.map = None;
        self.freq_stale = false;
        self.freq_map = None;
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The Doppler spectrum at each carrier from the rows so far, made anew
    /// if rows have come since the last; none before half the window is
    /// filled.
    pub fn freq_map(&mut self) -> Option<&FreqDoppler> {
        if self.freq_stale && self.rows.len() >= self.max_rows / 2 {
            self.freq_stale = false;
            self.freq_map = freq_doppler(
                self.rows.make_contiguous(),
                self.pilot_spacing_hz,
                self.row_s,
                self.doppler_max_hz,
                2,
            );
        }
        self.freq_map.as_ref()
    }

    /// The map of the rows so far, made anew if rows have come since the
    /// last; none before half the window is filled.
    pub fn map(&mut self) -> Option<&DelayDoppler> {
        if self.stale && self.rows.len() >= self.max_rows / 2 {
            self.stale = false;
            let range = (-0.25 * self.guard_us, 1.25 * self.guard_us);
            self.map = delay_doppler(
                self.rows.make_contiguous(),
                self.pilot_spacing_hz,
                self.row_s,
                range,
                self.doppler_max_hz,
            )
            .map(|m| DelayDoppler {
                guard_us: self.guard_us,
                ..m
            });
        }
        self.map.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8k: pilots every 12 carriers of 1.116 kHz, 568 of them; a row every
    /// 8 symbols of 924 µs.
    const SPACING: f64 = 12.0 * 64e6 / 7.0 / 8192.0;
    const N_P: usize = 568;
    const ROW_S: f64 = 8.0 * 924e-6;

    /// Rows of a channel made of paths (delay µs, gain, Doppler Hz).
    fn rows(paths: &[(f64, f32, f64)], n: usize) -> Vec<Vec<Iq>> {
        (0..n)
            .map(|t| {
                (0..N_P)
                    .map(|p| {
                        paths
                            .iter()
                            .map(|&(tau_us, g, nu)| {
                                let ph = -2.0 * PI * p as f64 * SPACING * tau_us * 1e-6
                                    + 2.0 * PI * nu * t as f64 * ROW_S;
                                Iq::new((g as f64 * ph.cos()) as f32, (g as f64 * ph.sin()) as f32)
                            })
                            .sum()
                    })
                    .collect()
            })
            .collect()
    }

    fn peak(m: &DelayDoppler) -> (f64, f64, f32) {
        let (mut best, mut at) = (f32::MIN, (0, 0));
        for r in 0..m.dopplers {
            for c in 0..m.delays {
                if m.at(r, c) > best {
                    best = m.at(r, c);
                    at = (r, c);
                }
            }
        }
        (m.delay_us(at.1), m.doppler_hz(at.0), best)
    }

    #[test]
    fn paths_appear_at_their_delay_and_doppler() {
        let r = rows(&[(0.0, 1.0, 0.0), (3.5, 0.5, 6.0)], 540);
        let m = delay_doppler(&r, SPACING, ROW_S, (-7.0, 35.0), 60.0).unwrap();
        let (tau, nu, db) = peak(&m);
        assert!(
            tau.abs() < 0.2 && nu.abs() < 0.5,
            "strongest at {tau} µs, {nu} Hz"
        );
        assert_eq!(db, 0.0);
        // The second path: 6 dB down, at 3.5 µs and 6 Hz.
        let c = ((3.5 - m.delay_start_us) / m.delay_step_us).round() as usize;
        let rr = ((6.0 - m.doppler_start_hz) / m.doppler_step_hz).round() as usize;
        let level = (rr.saturating_sub(2)..=rr + 2)
            .flat_map(|r| (c.saturating_sub(2)..=c + 2).map(move |c| (r, c)))
            .map(|(r, c)| m.at(r, c))
            .fold(f32::MIN, f32::max);
        assert!((level + 6.0).abs() < 1.5, "second path at {level} dB");
        // Elsewhere the map is far down.
        let c0 = ((20.0 - m.delay_start_us) / m.delay_step_us).round() as usize;
        assert!(m.at(m.dopplers / 2, c0) < -30.0);
        assert!((m.window_s - 540.0 * ROW_S).abs() < 1e-9);
    }

    #[test]
    fn a_moving_path_shows_at_its_doppler_across_the_band() {
        // A static path, and two of equal strength 8 Hz away in Doppler, 3.5 µs
        // apart. Each carrier has its energy at 0 Hz and at 8 Hz; the 8 Hz
        // pair add and cancel from carrier to carrier (once every 1/3.5 µs =
        // 286 kHz), so that line comes and goes across the band while the
        // static one is steady.
        let r = rows(&[(0.0, 1.0, 0.0), (0.0, 0.5, 8.0), (3.5, 0.5, 8.0)], 540);
        let m = freq_doppler(&r, SPACING, ROW_S, 60.0, 2).unwrap();
        assert_eq!(m.cols, N_P / 2);
        assert!((m.freq_step_mhz - 2.0 * SPACING / 1e6).abs() < 1e-9);
        let row_of = |hz: f64| ((hz - m.doppler_start_hz) / m.doppler_step_hz).round() as usize;
        let near = |hz: f64, c: usize| {
            let r0 = row_of(hz);
            (r0.saturating_sub(2)..=r0 + 2)
                .map(|r| m.at(r, c))
                .fold(f32::MIN, f32::max)
        };
        let across = |hz: f64| {
            (0..m.cols).fold((f32::MAX, f32::MIN), |(lo, hi), c| {
                (lo.min(near(hz, c)), hi.max(near(hz, c)))
            })
        };
        // The static path is the peak of all, and steady.
        let (lo0, hi0) = across(0.0);
        assert!(hi0.abs() < 0.5 && hi0 - lo0 < 1.0, "{lo0} .. {hi0}");
        // The moving pair: as strong as it when they add, gone when they cancel.
        let (lo8, hi8) = across(8.0);
        assert!(hi8 > -1.5, "{hi8}");
        assert!(hi8 - lo8 > 12.0, "{lo8} .. {hi8}");
        // Elsewhere the map is far down in every column.
        let r_other = row_of(30.0);
        assert!((0..m.cols).all(|c| m.at(r_other, c) < -25.0));
    }

    #[test]
    fn a_pre_echo_is_a_negative_delay() {
        let r = rows(&[(0.0, 1.0, 0.0), (-4.0, 0.7, -3.0)], 540);
        let m = delay_doppler(&r, SPACING, ROW_S, (-7.0, 35.0), 60.0).unwrap();
        let c = ((-4.0 - m.delay_start_us) / m.delay_step_us).round() as usize;
        let rr = ((-3.0 - m.doppler_start_hz) / m.doppler_step_hz).round() as usize;
        let level = (rr.saturating_sub(2)..=rr + 2)
            .flat_map(|r| (c.saturating_sub(2)..=c + 2).map(move |c| (r, c)))
            .map(|(r, c)| m.at(r, c))
            .fold(f32::MIN, f32::max);
        assert!((level + 3.1).abs() < 1.5, "{level}");
    }

    #[test]
    fn history_makes_a_map_when_asked() {
        let mut h = ChannelHistory::new(SPACING, ROW_S, 28.0);
        let row = vec![Iq::new(1.0, 0.0); N_P];
        for _ in 0..100 {
            h.push(&row);
        }
        assert!(h.map().is_none(), "less than half the window");
        for _ in 0..300 {
            h.push(&row);
        }
        let m = h.map().expect("a map");
        assert_eq!(m.guard_us, 28.0);
        // A flat channel: one path at delay 0, Doppler 0.
        let (tau, nu, _) = peak(m);
        assert!(tau.abs() < 0.2 && nu.abs() < 0.5, "{tau} {nu}");
        // Made once per new data.
        assert!(!h.stale && h.map().is_some());
        h.push(&row);
        assert!(h.stale);
        // A new layout starts afresh.
        h.push(&row[..100]);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn too_little_to_go_on() {
        assert!(delay_doppler(&[], SPACING, ROW_S, (0.0, 30.0), 50.0).is_none());
        let few = rows(&[(0.0, 1.0, 0.0)], 4);
        assert!(delay_doppler(&few, SPACING, ROW_S, (0.0, 30.0), 50.0).is_none());
        let zeros = vec![vec![Iq::new(0.0, 0.0); N_P]; 50];
        assert!(delay_doppler(&zeros, SPACING, ROW_S, (0.0, 30.0), 50.0).is_none());
    }
}
