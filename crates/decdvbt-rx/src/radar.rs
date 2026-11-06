// made by CasualArclamp
//! Passive radar: the range–Doppler map of what reflects the multiplex's
//! signal (aircraft, traffic), from every symbol the DVB-T receiver
//! demodulates. Ported from DecDAB's (`decdab_engine::radar`, same author).
//!
//! **The reference is the signal itself, decided.** Each symbol's carriers
//! as read, over what was sent on them, are the channel at that moment (the
//! reciprocal filter of OFDM passive radar). What was sent: the pilots'
//! known values (EN 300 744 §4.5.3), the TPS carriers' sign, and each data
//! cell's nearest constellation point, decided on the equaliser's own
//! estimate for that symbol (so a wrong decision stays in its cell). Its
//! inverse FFT over the carriers is the impulse response: the direct path,
//! and every echo at its excess delay (bistatic range = c·τ).
//! [`Profiler`] makes one a symbol, its delay samples summed in fours
//! ([`DECIM`]: 131 m bins in 8 MHz). The sum is a filter on the spectrum
//! and taking every fourth sample folds the spectrum onto a quarter of its
//! length, so both are done before the transform, which is then a quarter
//! the size (2048 points in 8K): the profile's cost about a quarter.
//!
//! **The map:** over a coherent interval (CPI) of about a second, each range
//! bin's values are a time series; its FFT is the Doppler spectrum at that
//! range. DVB-T has no null symbol, so the series is evenly spaced, a symbol
//! apart (±541 Hz in 8K 1/32). Before the FFT:
//!
//! - each profile is turned by the direct path's phase and scaled by its
//!   level, so 0 Hz is the direct signal's frequency and the receiver's own
//!   phase and gain wander, common to every path, cancel;
//! - each bin's mean over the CPI is taken out: everything that does not
//!   move (the direct path, buildings, hills);
//! - in the strong bins ([`STRONG_DB`] over the typical bin) the mean is
//!   taken per symbol of the pilots' 4-symbol cycle, so a pattern that
//!   repeats with it does not copy them at multiples of a quarter of the
//!   symbol rate.
//!
//! The method after Juha Vierinen's FM passive radar and the OFDM
//! reciprocal-filter literature (e.g. Searle, Howard & Palmer, "Evaluating
//! the use of DAB+ for passive radar", 2015); the code is new.

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::Arc;

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{FftSize, Modulation};
use decdvbt_ofdm::pilots::{Role, SymbolPlan};
use rustfft::{Fft, FftPlanner};

/// Delay samples summed into one range bin.
pub const DECIM: usize = 4;
/// Range bins of a profile before the delay window's start: the strongest
/// path is found among them and the rest (range counts from it).
pub const PRE: usize = 32;
/// Range bins a profile, at most (201 km in 8 MHz; less in 2K, whose
/// delays repeat after 224 µs).
pub const MAX_BINS: usize = 1536;
/// A bin this far over the median bin (mean power over the CPI, dB) has
/// its pattern over the pilots' cycle taken out, not just its mean.
pub const STRONG_DB: f32 = 25.0;
/// The pilots' cycle, symbols (EN 300 744 §4.5.3: the scattered pilots move
/// by 3 carriers a symbol).
pub const PILOT_CYCLE: usize = 4;
const C: f64 = 299_792_458.0;

/// The range–Doppler map.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RangeDoppler {
    /// Power, dB above the noise floor: `ranges` rows of `dopplers` values,
    /// the nearest range first, the lowest Doppler first in each.
    pub db: Vec<f32>,
    pub ranges: usize,
    pub dopplers: usize,
    /// Bistatic range of a row, km (row 0 is 0 km).
    pub range_step_km: f64,
    pub doppler_start_hz: f64,
    pub doppler_step_hz: f64,
    pub cpi_s: f64,
    /// The noise floor (the map's median) against the direct path, dB:
    /// how far down an echo can be seen.
    pub floor_db: f32,
    /// The strongest point off the zero-Doppler line, dB above the floor.
    pub max_snr_db: f32,
}

impl RangeDoppler {
    pub fn at(&self, range: usize, doppler: usize) -> f32 {
        self.db[range * self.dopplers + doppler]
    }

    pub fn range_km(&self, r: usize) -> f64 {
        r as f64 * self.range_step_km
    }

    pub fn doppler_hz(&self, d: usize) -> f64 {
        self.doppler_start_hz + d as f64 * self.doppler_step_hz
    }
}

fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (0.5 - 0.5 * (2.0 * PI * (i as f64 + 0.5) / n as f64).cos()) as f32)
        .collect()
}

/// The map from `rows` (range profiles `slot_s` apart, the same number of
/// bins each, `bin_km` of bistatic range a bin), `ranges` bins from the
/// strongest path (range 0; bins before it are left out), strong bins'
/// patterns over `period` rows taken out (0: none): `None` for too few
/// rows or nothing in them.
pub fn range_doppler(
    rows: &[Vec<Iq>],
    slot_s: f64,
    bin_km: f64,
    ranges: usize,
    period: usize,
) -> Option<RangeDoppler> {
    let t_len = rows.len();
    let bins = rows.first()?.len();
    if t_len < 64 || bins == 0 || rows.iter().any(|r| r.len() != bins) {
        return None;
    }
    // The direct path: the strongest bin on average.
    let mut mean_p = vec![0f32; bins];
    for row in rows {
        for (m, z) in mean_p.iter_mut().zip(row) {
            *m += z.norm_sqr();
        }
    }
    let (ref_bin, _) = mean_p
        .iter()
        .enumerate()
        .fold((0, 0f32), |b, (i, &p)| if p > b.1 { (i, p) } else { b });
    // Each row's turn and scale: the direct path's phase undone, its level
    // made the mean level (a deep fade of the direct path is not divided
    // into: the scale is capped at 10×).
    let mean_a = rows.iter().map(|r| r[ref_bin].norm()).sum::<f32>() / t_len as f32;
    if mean_a <= 0.0 || !mean_a.is_finite() {
        return None;
    }
    let g: Vec<Iq> = rows
        .iter()
        .map(|r| {
            let c = r[ref_bin];
            let a = c.norm().max(0.1 * mean_a);
            c.conj() * (mean_a / (a * a))
        })
        .collect();

    let win = hann(t_len);
    let nf = t_len.next_power_of_two();
    let fft: Arc<dyn Fft<f32>> = FftPlanner::<f32>::new().plan_fft_forward(nf);
    let mut scratch = vec![Iq::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let ranges = ranges.min(bins - ref_bin);
    // The bins' series are read a block of bins at a time: each row's
    // block is one run of memory, where a bin at a time would touch every
    // row's memory once per bin.
    const BLOCK: usize = 16;
    let mut cols = vec![vec![Iq::new(0.0, 0.0); nf]; BLOCK];
    let mut power = vec![0f32; ranges * nf];
    let win_sum: f32 = win.iter().sum();
    let direct = (mean_a * win_sum).powi(2);
    let strong = {
        let mut m = mean_p.clone();
        let mid = m.len() / 2;
        let (_, &mut med, _) = m.select_nth_unstable_by(mid, f32::total_cmp);
        med * 10f32.powf(STRONG_DB / 10.0)
    };
    // Per-slot means need a good many of each slot.
    let periodic = period > 1 && t_len >= 16 * period;
    let mut per = vec![Iq::new(0.0, 0.0); period.max(1)];
    let cnt: Vec<f32> = (0..period.max(1))
        .map(|t| t_len.saturating_sub(t).div_ceil(period.max(1)) as f32)
        .collect();
    for b0 in (ref_bin..ref_bin + ranges).step_by(BLOCK) {
        let bw = BLOCK.min(ref_bin + ranges - b0);
        for (t, row) in rows.iter().enumerate() {
            for (c, &v) in cols.iter_mut().zip(&row[b0..b0 + bw]) {
                c[t] = v * g[t];
            }
        }
        for (j, col) in cols[..bw].iter_mut().enumerate() {
            let (b, r) = (b0 + j, b0 + j - ref_bin);
            // The bin's series, what stands still taken out.
            if periodic && mean_p[b] > strong {
                per.fill(Iq::new(0.0, 0.0));
                for (t, c) in col[..t_len].iter().enumerate() {
                    per[t % period] += c;
                }
                for (p, n) in per.iter_mut().zip(&cnt) {
                    *p /= *n;
                }
                for (t, (c, &w)) in col[..t_len].iter_mut().zip(&win).enumerate() {
                    *c = (*c - per[t % period]) * w;
                }
            } else {
                let mean = col[..t_len].iter().sum::<Iq>() / t_len as f32;
                for (c, &w) in col[..t_len].iter_mut().zip(&win) {
                    *c = (*c - mean) * w;
                }
            }
            col[t_len..].fill(Iq::new(0.0, 0.0));
            fft.process_with_scratch(col, &mut scratch);
            // Lowest Doppler first: bins nf/2 … nf−1, then 0 … nf/2−1.
            let out = &mut power[r * nf..(r + 1) * nf];
            for (d, o) in out.iter_mut().enumerate() {
                *o = col[(d + nf / 2) % nf].norm_sqr();
            }
        }
    }
    // The noise floor: the median.
    let mut sorted = power.clone();
    let mid = sorted.len() / 2;
    let (_, &mut floor, _) = sorted.select_nth_unstable_by(mid, f32::total_cmp);
    if floor <= 0.0 || !floor.is_finite() {
        return None;
    }
    // Each Doppler's own floor too: its median over the ranges, where that
    // is higher. Whatever is the same at every range (a weak tone in the
    // band, the radio's DC, under what the receiver's notch flags) is a
    // line up the whole map at its Doppler; against its own column's
    // median it is flat. An echo is at a few ranges and stands out still.
    // (Every fourth range is plenty for a median.)
    let col_floor: Vec<f32> = if ranges >= 64 {
        let mut col = vec![0f32; ranges.div_ceil(4)];
        (0..nf)
            .map(|d| {
                for (j, c) in col.iter_mut().enumerate() {
                    *c = power[4 * j * nf + d];
                }
                let mid = col.len() / 2;
                let (_, &mut m, _) = col.select_nth_unstable_by(mid, f32::total_cmp);
                m.max(floor)
            })
            .collect()
    } else {
        vec![floor; nf]
    };
    let doppler_step_hz = 1.0 / (nf as f64 * slot_s);
    let doppler_start_hz = -((nf / 2) as f64) * doppler_step_hz;
    // The zero-Doppler line (±2 Hz) is clutter's leftovers, not targets.
    let near_zero = (2.0 / doppler_step_hz).ceil() as usize;
    let mut max_snr = 0f32;
    let db: Vec<f32> = power
        .iter()
        .enumerate()
        .map(|(i, &p)| {
            let v = 10.0 * (p / col_floor[i % nf]).max(1e-6).log10();
            if (i % nf).abs_diff(nf / 2) > near_zero {
                max_snr = max_snr.max(v);
            }
            v
        })
        .collect();
    Some(RangeDoppler {
        db,
        ranges,
        dopplers: nf,
        range_step_km: bin_km,
        doppler_start_hz,
        doppler_step_hz,
        cpi_s: t_len as f64 * slot_s,
        floor_db: 10.0 * (floor / direct).log10(),
        max_snr_db: max_snr,
    })
}

/// An echo off the clutter ridge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Echo {
    pub range_km: f64,
    pub doppler_hz: f64,
    /// dB above the noise floor.
    pub db: f32,
}

/// The bins nearest the direct path that echoes are not looked for in
/// (0.4 km in 8 MHz): its own skirt, and whatever modulates it (a radio's
/// supply ripple puts lines at ±100 Hz there), are not echoes.
pub const ECHO_FROM_BIN: usize = 3;

/// The strongest points of `m` beyond ±`ridge_hz`, from [`ECHO_FROM_BIN`]
/// out to `max_km`, at least `min_db` over the floor, local peaks (none
/// other within 4 bins each way stronger), strongest first, at most `n`.
pub fn echoes(m: &RangeDoppler, ridge_hz: f64, max_km: f64, min_db: f32, n: usize) -> Vec<Echo> {
    let zero = m.dopplers / 2;
    let near = (ridge_hz / m.doppler_step_hz).ceil() as usize;
    let ranges = ((max_km / m.range_step_km).ceil() as usize).min(m.ranges);
    let mut pts: Vec<(usize, usize, f32)> = Vec::new();
    for r in ECHO_FROM_BIN.min(ranges)..ranges {
        for d in 0..m.dopplers {
            let v = m.at(r, d);
            if d.abs_diff(zero) > near && v >= min_db {
                pts.push((r, d, v));
            }
        }
    }
    // Strongest first; each kept unless a stronger one is already near it.
    pts.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut out: Vec<(usize, usize, f32)> = Vec::new();
    for p in pts {
        if out
            .iter()
            .all(|q| q.0.abs_diff(p.0) > 4 || q.1.abs_diff(p.1) > 4)
        {
            out.push(p);
            if out.len() == n {
                break;
            }
        }
    }
    out.iter()
        .map(|&(r, d, db)| Echo {
            range_km: m.range_km(r),
            doppler_hz: m.doppler_hz(d),
            db,
        })
        .collect()
}

/// The shape of a lock's profiles: what the maps need to know of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarGeometry {
    /// Seconds between profiles: a symbol.
    pub slot_s: f64,
    /// Bistatic range a bin, km.
    pub bin_km: f64,
    /// Bins a profile, [`PRE`] of them before the delay window's start.
    pub bins: usize,
}

/// The profiles of the last CPI.
pub struct RadarHistory {
    rows: VecDeque<Vec<Iq>>,
    cpi: usize,
    geo: RadarGeometry,
    added: usize,
    /// The maps' ranges (bins from the strongest path): only what is shown
    /// is worked out.
    ranges: usize,
}

impl RadarHistory {
    /// A history of `cpi` profiles of the shape `geo`.
    pub fn new(cpi: usize, geo: RadarGeometry) -> RadarHistory {
        RadarHistory {
            rows: VecDeque::with_capacity(cpi + 64),
            cpi,
            geo,
            added: 0,
            ranges: geo.bins - PRE,
        }
    }

    /// Make the maps out to `km` (bistatic) only.
    pub fn set_max_km(&mut self, km: f64) {
        self.ranges = ((km / self.geo.bin_km).ceil() as usize).clamp(1, self.geo.bins - PRE);
    }

    /// The CPI, in profiles, nearest `seconds` that is a power of two (the
    /// Doppler FFT's size: none wasted on padding).
    pub fn cpi_for(seconds: f64, slot_s: f64) -> usize {
        let n = (seconds / slot_s).max(64.0);
        1usize << (n.log2().round() as u32)
    }

    pub fn geometry(&self) -> RadarGeometry {
        self.geo
    }

    /// The coherent interval, profiles.
    pub fn cpi(&self) -> usize {
        self.cpi
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.added = 0;
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Add profiles, oldest first. True when a map is due
    /// ([`RadarHistory::map`]): every half CPI once a CPI is full.
    pub fn push_rows(&mut self, profiles: &mut Vec<Vec<Iq>>) -> bool {
        let mut due = false;
        for p in profiles.drain(..) {
            if p.len() != self.geo.bins {
                continue;
            }
            self.rows.push_back(p);
            if self.rows.len() > self.cpi {
                self.rows.pop_front();
            }
            self.added += 1;
            due |= self.rows.len() == self.cpi && self.added.is_multiple_of(self.cpi / 2);
        }
        due
    }

    /// The map of the profiles held (the last CPI's), when there are enough.
    pub fn map(&mut self) -> Option<RangeDoppler> {
        range_doppler(
            self.rows.make_contiguous(),
            self.geo.slot_s,
            self.geo.bin_km,
            self.ranges,
            PILOT_CYCLE,
        )
    }
}

/// What a data cell is decided on: its modulation's levels per axis and
/// their scale (unit mean power, EN 300 744 §4.3.5, non-hierarchical).
#[derive(Debug, Clone, Copy)]
struct Slicer {
    /// The largest level, odd (1, 3, 7), and √(mean power) of the grid.
    top: f32,
    scale: f32,
}

impl Slicer {
    fn new(m: Modulation) -> Slicer {
        let levels: f32 = match m {
            Modulation::Qpsk => 2.0,
            Modulation::Qam16 => 4.0,
            Modulation::Qam64 => 8.0,
        };
        Slicer {
            top: levels - 1.0,
            scale: (2.0 * (levels * levels - 1.0) / 3.0).sqrt(),
        }
    }

    /// The nearest level to `v` (equalised) on the unscaled grid (odd
    /// numbers). The floor is a truncation of a number made positive: `as
    /// i32` vectorises with SSE2, where `f32::floor` is a call without
    /// SSE4.1.
    #[inline(always)]
    fn level(&self, v: f32) -> f32 {
        let q = v * self.scale * 0.5 + 64.0;
        (((q as i32 - 64) * 2 + 1) as f32).clamp(-self.top, self.top)
    }

    /// The nearest point to `z` (equalised, unit mean power).
    #[cfg(test)]
    fn point(&self, z: Iq) -> Iq {
        Iq::new(self.level(z.re), self.level(z.im)) / self.scale
    }

    /// 1 / (the nearest point to `z`).
    #[inline(always)]
    fn inverse(&self, z: Iq) -> Iq {
        let (i, q) = (self.level(z.re), self.level(z.im));
        let r = self.scale / (i * i + q * q);
        Iq::new(i * r, -q * r)
    }
}

/// Every carrier's channel as if it were data: `raw` over the nearest
/// point to `z`.
#[inline(always)]
fn decide(slicer: &Slicer, h: &mut [Iq], raw: &[Iq], z: &[Iq]) {
    for ((h, &r), &zz) in h.iter_mut().zip(raw).zip(z) {
        *h = r * slicer.inverse(zz);
    }
}

/// [`decide`] compiled for AVX2 (eight lanes rather than SSE2's four).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn decide_avx2(slicer: &Slicer, h: &mut [Iq], raw: &[Iq], z: &[Iq]) {
    decide(slicer, h, raw, z);
}

/// Makes a lock's range profiles, one a symbol.
pub struct Profiler {
    n: usize,
    k: usize,
    geo: RadarGeometry,
    ifft: Arc<dyn Fft<f32>>,
    scratch: Vec<Iq>,
    /// The folded spectrum (N / DECIM), and per carrier: where it folds to
    /// and its weight (the window, the delay sum's response, 1/K).
    buf: Vec<Iq>,
    fold: Vec<u32>,
    weight: Vec<Iq>,
    /// The channel at each carrier, this symbol.
    h: Vec<Iq>,
    /// For each pilot phase, its pilot and TPS carriers (listed for the
    /// FFT size in `roles_of`).
    special: [(Vec<u32>, Vec<u32>); 4],
    roles_of: Option<FftSize>,
    avx2: bool,
    /// Profiles made and not yet taken, and the symbol of the last.
    rows: Vec<Vec<Iq>>,
    last_symbol: Option<u64>,
    /// A symbol was missed since the last take: the series has a gap.
    gap: bool,
}

impl Profiler {
    /// For symbols of `n` samples (FFT size), `k` active carriers, `slot_s`
    /// apart, at `fs` samples a second.
    pub fn new(n: usize, k: usize, slot_s: f64, fs: f64) -> Profiler {
        let m = n / DECIM;
        let bins = MAX_BINS.min(m - PRE);
        let kmid = (k / 2) as isize;
        let win = hann(k);
        // Bin b of the full transform, summed over delays DECIM·j + d
        // (d = −1 … 2), is the spectrum times Σ_d e^{j2πbd/N}; the sum's
        // every DECIM-th sample is the inverse transform of that spectrum
        // folded onto m bins.
        let (fold, weight): (Vec<u32>, Vec<Iq>) = (0..k)
            .map(|i| {
                let b = (i as isize - kmid).rem_euclid(n as isize) as usize;
                let g: Iq = (-1..DECIM as isize - 1)
                    .map(|d| {
                        Iq::from_polar(1.0, (2.0 * PI * (b as f64) * d as f64 / n as f64) as f32)
                    })
                    .sum();
                ((b % m) as u32, g * (win[i] / k as f32))
            })
            .unzip();
        Profiler {
            n,
            k,
            geo: RadarGeometry {
                slot_s,
                bin_km: DECIM as f64 * C / fs / 1000.0,
                bins,
            },
            ifft: FftPlanner::<f32>::new().plan_fft_inverse(m),
            scratch: Vec::new(),
            buf: vec![Iq::new(0.0, 0.0); m],
            fold,
            weight,
            h: vec![Iq::new(0.0, 0.0); k],
            special: Default::default(),
            roles_of: None,
            #[cfg(target_arch = "x86_64")]
            avx2: is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma"),
            #[cfg(not(target_arch = "x86_64"))]
            avx2: false,
            rows: Vec::new(),
            last_symbol: None,
            gap: true,
        }
    }

    pub fn geometry(&self) -> RadarGeometry {
        self.geo
    }

    /// Whether this profiler is for symbols of this shape.
    pub fn fits(&self, n: usize, k: usize, slot_s: f64) -> bool {
        self.n == n && self.k == k && (self.geo.slot_s - slot_s).abs() < 1e-9
    }

    /// Symbol number `symbol`'s profile, from its carriers (`raw`),
    /// equalised (`z`), its place in the frame `l` (which carriers are
    /// pilots: `plan`), and the pilots' reciprocal values (`inv`); data
    /// cells decided as `modulation`.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        symbol: u64,
        raw: &[Iq],
        z: &[Iq],
        plan: &SymbolPlan,
        l: usize,
        inv: &[f32],
        modulation: Modulation,
    ) {
        if raw.len() != self.k || z.len() != self.k || plan.fft.active_carriers() != self.k {
            return;
        }
        if self.last_symbol.is_some_and(|s| s + 1 != symbol) {
            self.gap = true;
            self.rows.clear();
        }
        self.last_symbol = Some(symbol);
        // The pilots' and TPS carriers of each phase, listed once.
        if self.roles_of != Some(plan.fft) {
            self.roles_of = Some(plan.fft);
            for (ph, lists) in self.special.iter_mut().enumerate() {
                let roles = plan.roles(ph);
                lists.0 = (0..self.k as u32)
                    .filter(|&i| roles[i as usize] == Role::Pilot)
                    .collect();
                lists.1 = (0..self.k as u32)
                    .filter(|&i| roles[i as usize] == Role::Tps)
                    .collect();
            }
        }
        // The channel at each carrier: what came over what was sent. Every
        // carrier first as data (one loop with no branches, which the
        // compiler vectorises), then the pilots and the TPS put right.
        let slicer = Slicer::new(modulation);
        #[cfg(target_arch = "x86_64")]
        if self.avx2 {
            // SAFETY: AVX2 and FMA were detected at construction.
            unsafe { decide_avx2(&slicer, &mut self.h, raw, z) };
        } else {
            decide(&slicer, &mut self.h, raw, z);
        }
        #[cfg(not(target_arch = "x86_64"))]
        decide(&slicer, &mut self.h, raw, z);
        let (pilots, tps) = &self.special[l % 4];
        for &i in pilots {
            let i = i as usize;
            self.h[i] = raw[i] * inv[i];
        }
        for &i in tps {
            let i = i as usize;
            self.h[i] = raw[i] * 1f32.copysign(z[i].re);
        }
        self.buf.fill(Iq::new(0.0, 0.0));
        for ((&h, &f), &w) in self.h.iter().zip(&self.fold).zip(&self.weight) {
            self.buf[f as usize] += h * w;
        }
        if self.scratch.len() < self.ifft.get_inplace_scratch_len() {
            self.scratch = vec![Iq::new(0.0, 0.0); self.ifft.get_inplace_scratch_len()];
        }
        self.ifft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        // From PRE bins before the window's start.
        let m = self.buf.len();
        let row: Vec<Iq> = (0..self.geo.bins)
            .map(|j| self.buf[(j + m - PRE) % m])
            .collect();
        self.rows.push(row);
    }

    /// The profiles made since the last call, oldest first, appended to
    /// `out`; true when the series broke since (a missed symbol, a new
    /// lock): what came before does not join on.
    pub fn take(&mut self, out: &mut Vec<Vec<Iq>>) -> bool {
        out.append(&mut self.rows);
        std::mem::take(&mut self.gap)
    }

    /// The series broke (the lock was lost).
    pub fn mark_gap(&mut self) {
        self.gap = true;
        self.rows.clear();
        self.last_symbol = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8K 1/32 at 64/7 MHz.
    const FS: f64 = 64e6 / 7.0;
    const SLOT: f64 = (8192.0 + 256.0) / FS;
    const BIN_KM: f64 = 4.0 * 299_792.458 / FS;

    /// Profiles of a direct path (bin `PRE`, gain 1, wandering in phase as a
    /// receiver's would) and an echo `bin` bins later, `gain`, `hz`.
    fn rows(n: usize, bin: usize, gain: f32, hz: f64) -> Vec<Vec<Iq>> {
        (0..n)
            .map(|t| {
                let wander = 0.3 * (t as f64 * 0.01).sin() + 2.0 * PI * 3.0 * t as f64 * SLOT;
                let common = Iq::from_polar(1.0, wander as f32);
                let mut r = vec![Iq::new(0.0, 0.0); 256];
                r[PRE] = common;
                r[PRE + 1] = common * 0.3; // a static echo: clutter
                r[PRE + bin] +=
                    common * Iq::from_polar(gain, (2.0 * PI * hz * t as f64 * SLOT) as f32);
                // A little noise, the same every run (a hash of the cell).
                for (i, z) in r.iter_mut().enumerate() {
                    let u = |salt: u64| {
                        let mut x = (t as u64) << 32 ^ (i as u64) << 8 ^ salt;
                        x = (x ^ (x >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
                        x = (x ^ (x >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
                        ((x ^ (x >> 33)) >> 40) as f32 / (1u64 << 24) as f32 - 0.5
                    };
                    *z += Iq::new(u(1), u(2)) * 1e-3;
                }
                r
            })
            .collect()
    }

    fn peak(m: &RangeDoppler) -> (f64, f64, f32) {
        let (mut best, mut at) = (f32::MIN, (0, 0));
        for r in 0..m.ranges {
            for d in 0..m.dopplers {
                if m.at(r, d) > best {
                    best = m.at(r, d);
                    at = (r, d);
                }
            }
        }
        (m.range_km(at.0), m.doppler_hz(at.1), best)
    }

    #[test]
    fn an_echo_shows_at_its_range_and_doppler() {
        // −40 dB, 100 bins (9.8 km), +250 Hz: an aircraft's.
        let m = range_doppler(&rows(1024, 100, 0.01, 250.0), SLOT, BIN_KM, 200, 4).unwrap();
        let (km, hz, db) = peak(&m);
        assert!((km - 100.0 * BIN_KM).abs() < 0.01, "{km} km");
        assert!((hz - 250.0).abs() < 1.5, "{hz} Hz");
        assert!(db > 30.0, "{db} dB");
        // ±541 Hz in 8K 1/32.
        assert!(m.doppler_start_hz < -540.0 && m.doppler_start_hz > -542.0);
        let zero = m.dopplers / 2;
        assert!(m.at(0, zero) < db - 20.0 && m.at(1, zero) < db - 20.0);
        let e = echoes(&m, 15.0, 100.0, 25.0, 5);
        assert!((e[0].range_km - 100.0 * BIN_KM).abs() < 0.01);
        assert!(echoes(&m, 15.0, 5.0, 25.0, 5).is_empty());
    }

    #[test]
    fn history_makes_a_map_every_half_cpi() {
        let geo = RadarGeometry {
            slot_s: SLOT,
            bin_km: BIN_KM,
            bins: 256,
        };
        assert_eq!(RadarHistory::cpi_for(1.0, SLOT), 1024);
        let mut h = RadarHistory::new(512, geo);
        let all = rows(512 + 600, 40, 0.01, -120.0);
        let mut due = 0;
        for chunk in all.chunks(68) {
            if h.push_rows(&mut chunk.to_vec()) {
                due += 1;
                let m = h.map().unwrap();
                let (km, hz, _) = peak(&m);
                assert!((km - 40.0 * BIN_KM).abs() < 0.01 && (hz + 120.0).abs() < 2.5);
            }
        }
        // Full at 512, then every 256: at 512, 768 and 1024.
        assert_eq!(due, 3);
    }

    #[test]
    fn the_slicer_finds_the_nearest_point() {
        let near = |a: Iq, b: Iq| (a - b).norm() < 1e-5;
        let s = Slicer::new(Modulation::Qam64);
        let k = 1.0 / 42f32.sqrt();
        assert!(near(
            s.point(Iq::new(6.2 * k, -0.4 * k)),
            Iq::new(7.0 * k, -k)
        ));
        assert!(near(
            s.point(Iq::new(2.1 * k, 9.0 * k)),
            Iq::new(3.0 * k, 7.0 * k)
        ));
        let q = Slicer::new(Modulation::Qpsk);
        let r = std::f32::consts::FRAC_1_SQRT_2;
        assert!(near(q.point(Iq::new(0.2, -3.0)), Iq::new(r, -r)));
        let z = Iq::new(-5.3 * k, 0.9 * k);
        assert!(near(s.inverse(z) * s.point(z), Iq::new(1.0, 0.0)));
        // Beyond the grid: the outermost point.
        assert!(near(
            s.point(Iq::new(30.0 * k, -30.0 * k)),
            Iq::new(7.0 * k, -7.0 * k)
        ));
    }

    #[test]
    fn a_profile_puts_a_path_at_its_delay() {
        // 2K: carriers with a path 40 samples late (10 bins), all data,
        // QPSK, decided right.
        let (n, k) = (2048usize, 1705usize);
        let mut p = Profiler::new(n, k, SLOT, FS);
        let r = std::f32::consts::FRAC_1_SQRT_2;
        let x: Vec<Iq> = (0..k)
            .map(|i| {
                Iq::new(
                    if i % 3 == 0 { r } else { -r },
                    if i % 5 == 0 { -r } else { r },
                )
            })
            .collect();
        let kmid = (k / 2) as f64;
        let raw: Vec<Iq> = x
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let f = (i as f64 - kmid) / n as f64;
                v * Iq::from_polar(1.0, (-2.0 * PI * f * 40.0) as f32)
            })
            .collect();
        // The pilot and TPS carriers sent as 1 here (their reciprocal 1, the
        // TPS's sign from z = 1), the data as `x`.
        let plan = SymbolPlan::new(FftSize::K2);
        let inv: Vec<f32> = vec![1.0; k];
        let sent: Vec<Iq> = x
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                if plan.roles(0)[i] == Role::Data {
                    v
                } else {
                    Iq::new(1.0, 0.0)
                }
            })
            .collect();
        let raw: Vec<Iq> = raw
            .iter()
            .zip(&x)
            .zip(&sent)
            .map(|((&r, &v), &s)| r / v * s)
            .collect();
        let x = sent;
        p.push(4, &raw, &x, &plan, 0, &inv, Modulation::Qpsk);
        let mut out = Vec::new();
        assert!(p.take(&mut out), "the first take starts a series");
        let row = &out[0];
        let best = (0..row.len())
            .max_by(|&a, &b| row[a].norm().total_cmp(&row[b].norm()))
            .unwrap();
        assert_eq!(best, PRE + 10);
        // The next symbol joins on; one skipped breaks the series.
        p.push(5, &raw, &x, &plan, 0, &inv, Modulation::Qpsk);
        assert!(!p.take(&mut out));
        p.push(7, &raw, &x, &plan, 0, &inv, Modulation::Qpsk);
        assert!(p.take(&mut out));
    }

    #[test]
    fn too_little() {
        assert!(range_doppler(&[], SLOT, BIN_KM, 100, 4).is_none());
        assert!(range_doppler(&rows(10, 5, 0.1, 0.0), SLOT, BIN_KM, 100, 4).is_none());
    }
}
