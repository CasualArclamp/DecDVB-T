// made by CasualArclamp
//! One T2 frame's OFDM symbols to equalised cells (EN 302 755 §8-9): the
//! symbols' positions found and a line fitted through them, each transformed,
//! the continual pilots used to bring every symbol to the same phase and
//! timing, the channel estimated from the scattered pilots (a cubic through
//! the four nearest sightings in time, a windowed sinc across frequency),
//! and the data cells equalised, frequency-deinterleaved, each with the
//! noise variance it has after equalisation.
//!
//! What a symbol's samples are for the transform: the window is the symbol's
//! useful part started `early` samples ahead of where its guard interval
//! ends, so that it lies inside the guard interval's protection against echoes
//! on both sides whatever the timing's small errors.

use std::sync::Arc;
use std::time::{Duration, Instant};

use decdvbt_core::Iq;
use decdvbt_ofdm::t2::carriers::p2_deal;
use decdvbt_ofdm::t2::{Carriers, FreqInterleaver, Mode, Reference, Role};
use rustfft::{Fft, FftPlanner};

/// The sinc interpolator across the pilot lattice: nodes either side.
const KERNEL_HALF: usize = 8;
/// Samples either side of a symbol's nominal place searched for its cyclic
/// prefix.
const SEARCH: usize = 40;
/// A symbol's timing counts for the line if within this of it.
const OUTLIER: f64 = 6.0;

/// The shape of a frame.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub mode: Mode,
    pub n: usize,
    pub g: usize,
    /// Samples in a symbol, guard included.
    pub sym: usize,
    pub c_ps: usize,
    pub mid: usize,
    pub n_p2: usize,
    /// OFDM symbols in the frame: P2 symbols and data symbols (the closing
    /// one included).
    pub symbols: usize,
    pub has_fc: bool,
    /// Samples of the early start of the transform's window.
    pub early: usize,
}

impl Geometry {
    pub fn new(mode: Mode, data_symbols: usize) -> Geometry {
        let n = mode.fft.n();
        let g = mode.guard.samples(mode.fft);
        Geometry {
            mode,
            n,
            g,
            sym: n + g,
            c_ps: mode.c_ps(),
            mid: (mode.c_ps() - 1) / 2,
            n_p2: mode.fft.p2_symbols(),
            symbols: mode.fft.p2_symbols() + data_symbols,
            has_fc: mode.has_closing_symbol(),
            early: (g / 4).clamp(8, 128),
        }
    }

    /// Samples from the P1 symbol's first to the next frame's: P1 and the
    /// symbols.
    pub fn frame_samples(&self) -> usize {
        decdvbt_ofdm::t2::p1::SAMPLES + self.symbols * self.sym
    }
}

/// Where each symbol of a frame is, and the carrier offset left in the signal.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The first sample of each symbol (its guard interval's), fractional.
    pub start: Vec<f64>,
    /// Offset of the line from the nominal places, samples, and its slope,
    /// samples a symbol (the clock).
    pub offset: f64,
    pub drift: f64,
    /// Symbols within [`OUTLIER`] of the line.
    pub inliers: usize,
    /// The carrier offset left after what was taken out already, in carriers
    /// (the cyclic prefixes' phase).
    pub eps: f64,
}

/// Wall time in the parts of reading a frame, since the start.
#[derive(Debug, Clone, Copy, Default)]
pub struct DemodProfile {
    /// The symbols' transforms.
    pub fft: Duration,
    /// The continual pilots' alignment of the symbols.
    pub align: Duration,
    /// The scattered pilots' sightings, and the channel from them.
    pub channel: Duration,
    /// The equalised cells, deinterleaved.
    pub cells: Duration,
}

/// Most sightings of a node that make its value at a symbol.
const MAX_TAPS: usize = 12;

/// Which sightings of a node make its value at a symbol, and their weights:
/// a polynomial in time fitted through the nearest few by least squares
/// (which averages their noise as well as joining them).
#[derive(Debug, Clone, Copy, Default)]
struct TimeTap {
    start: u8,
    len: u8,
    w: [f32; MAX_TAPS],
}

/// The pilots and carriers of a mode, and the machinery to read frames.
pub struct Demod {
    pub geo: Geometry,
    pub prof: DemodProfile,
    pub carriers: Carriers,
    reference: Reference,
    fft: SymbolFft,
    /// The lattice of scattered pilots: carriers (every D_x from the first),
    /// and for each node the data symbols it is seen in.
    nodes: Vec<usize>,
    /// The interpolator's weights, tap by tap and then by the phase of a
    /// carrier between nodes (`kernel_t[t · dxp + phase]`, the phases padded
    /// with zeros to a multiple of eight).
    kernel_t: Vec<f32>,
    dxp: usize,
    /// Carriers that are continual pilots in every symbol.
    cps: Vec<usize>,
    /// The last data symbol read (the frame-closing one is read apart).
    last_data: usize,
    /// For each of the D_y places in the scattered pilots' pattern: the
    /// carriers that are data cells, and that are continual pilots.
    data_ks: Vec<Vec<u32>>,
    cp_ks: Vec<Vec<u32>>,
    /// The continual pilots' values at each symbol (aligned with `cp_ks`).
    cp_pv: Vec<Vec<f32>>,
    /// Each node's sightings, back to back: the symbol of each and the
    /// reciprocal of the pilot's value there.
    sight_off: Vec<u32>,
    sight_sym: Vec<u16>,
    sight_inv: Vec<f32>,
    /// Nodes seen at the same symbols share a pattern; a pattern says, for
    /// each data symbol, which sightings make a node's value there.
    pattern_of: Vec<u16>,
    patterns: Vec<Vec<TimeTap>>,
    fi_p2: FreqInterleaver,
    fi_data: FreqInterleaver,
    fi_fc: Option<FreqInterleaver>,
}

impl Demod {
    pub fn new(geo: Geometry) -> Demod {
        let mode = geo.mode;
        let carriers = Carriers::new(mode);
        let (dx, dy) = (mode.pp.dx(), mode.pp.dy());
        let k_ext = mode.k_ext();
        let first = k_ext % dx;
        let nodes: Vec<usize> = (first..geo.c_ps).step_by(dx).collect();
        // The symbols in which each node holds a scattered pilot.
        let mut seen = vec![Vec::new(); nodes.len()];
        for j in geo.n_p2..geo.symbols {
            if geo.has_fc && j == geo.symbols - 1 {
                continue;
            }
            let roles = carriers.data(j);
            for (m, &k) in nodes.iter().enumerate() {
                if roles[k] == Role::Scattered {
                    seen[m].push(j as u16);
                }
            }
        }
        let dxp = dx.next_multiple_of(8);
        let mut kernel_t = vec![0f32; 2 * KERNEL_HALF * dxp];
        for phase in 0..dx {
            let frac = phase as f32 / dx as f32;
            let mut w = [0f32; 2 * KERNEL_HALF];
            let mut sum = 0.0;
            for (t, wt) in w.iter_mut().enumerate() {
                // Node t − (KERNEL_HALF − 1) from the one at or before.
                let x = frac - (t as f32 - (KERNEL_HALF - 1) as f32);
                let sinc = if x.abs() < 1e-6 {
                    1.0
                } else {
                    (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x)
                };
                let win = 0.5 + 0.5 * (std::f32::consts::PI * x / KERNEL_HALF as f32).cos();
                *wt = sinc * win;
                sum += *wt;
            }
            for (t, wt) in w.iter().enumerate() {
                kernel_t[t * dxp + phase] = *wt / sum;
            }
        }
        let cps: Vec<usize> = (0..geo.c_ps)
            .filter(|&k| (0..dy).all(|ph| carriers.data(ph)[k] == Role::Continual))
            .collect();
        let reference = Reference::new(&mode);
        let last_data = if geo.has_fc {
            geo.symbols - 1
        } else {
            geo.symbols
        };
        let data_ks: Vec<Vec<u32>> = (0..dy)
            .map(|ph| {
                carriers
                    .data(ph)
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| **r == Role::Data)
                    .map(|(k, _)| k as u32)
                    .collect()
            })
            .collect();
        let cp_ks: Vec<Vec<u32>> = (0..dy)
            .map(|ph| {
                carriers
                    .data(ph)
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| **r == Role::Continual)
                    .map(|(k, _)| k as u32)
                    .collect()
            })
            .collect();
        let cp_pv: Vec<Vec<f32>> = (0..geo.symbols)
            .map(|j| {
                cp_ks[j % dy]
                    .iter()
                    .map(|&k| mode.pilot_value(Role::Continual, &reference, k as usize, j))
                    .collect()
            })
            .collect();
        let mut sight_off = vec![0u32; nodes.len() + 1];
        let (mut sight_sym, mut sight_inv) = (Vec::new(), Vec::new());
        let mut pattern_of = Vec::with_capacity(nodes.len());
        let mut pattern_ids: std::collections::HashMap<Vec<u16>, u16> = Default::default();
        let mut patterns: Vec<Vec<TimeTap>> = Vec::new();
        let (window, degree) = smoothing();
        for (m, &k) in nodes.iter().enumerate() {
            let ts: Vec<u16> = seen[m]
                .iter()
                .copied()
                .filter(|&j| (j as usize) < last_data)
                .collect();
            for &j in &ts {
                sight_sym.push(j);
                let pv = mode.pilot_value(Role::Scattered, &reference, k, j as usize);
                sight_inv.push(1.0 / pv);
            }
            sight_off[m + 1] = sight_sym.len() as u32;
            let id = *pattern_ids.entry(ts.clone()).or_insert_with(|| {
                patterns.push(
                    (geo.n_p2..last_data)
                        .map(|j| time_tap(&ts, j as u16, window, degree))
                        .collect(),
                );
                (patterns.len() - 1) as u16
            });
            pattern_of.push(id);
        }
        Demod {
            prof: DemodProfile::default(),
            reference,
            fft: SymbolFft::new(geo.n),
            fi_p2: FreqInterleaver::new(mode.fft, carriers.p2_cells()),
            fi_data: FreqInterleaver::new(mode.fft, carriers.data_cells()),
            fi_fc: geo
                .has_fc
                .then(|| FreqInterleaver::new(mode.fft, carriers.closing_cells())),
            geo,
            carriers,
            nodes,
            kernel_t,
            dxp,
            cps,
            last_data,
            data_ks,
            cp_ks,
            cp_pv,
            sight_off,
            sight_sym,
            sight_inv,
            pattern_of,
            patterns,
        }
    }

    /// Where the frame's symbols are: the cyclic prefix searched for near
    /// each nominal place (`first` is the first symbol's guard start in
    /// `y`), a line through the peaks, and the carrier offset from their
    /// phases.
    #[allow(clippy::needless_range_loop)]
    pub fn timing(&self, y: &[Iq], first: f64) -> Option<Timing> {
        let (n, g, sym) = (self.geo.n, self.geo.g, self.geo.sym);
        let count = self.geo.symbols;
        let mut found: Vec<f64> = Vec::with_capacity(count);
        let mut phase_sum = Iq::new(0.0, 0.0);
        let mut ok = vec![false; count];
        for j in 0..count {
            let nominal = first + (j * sym) as f64;
            let (pos, best, best_c) = cp_peak(y, nominal, n, g, SEARCH)?;
            found.push(pos - nominal);
            ok[j] = best > 0.15;
            if ok[j] {
                phase_sum += best_c;
            }
        }
        if ok.iter().filter(|o| **o).count() < count / 3 {
            return None;
        }
        // The line: offset + drift·j through the positions, twice dropping
        // those far from it.
        let mut keep: Vec<bool> = ok.clone();
        let (mut a0, mut b0) = (0.0, 0.0);
        for _ in 0..3 {
            let (mut sw, mut sx, mut sy2, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for j in 0..count {
                if keep[j] {
                    let (x, v) = (j as f64, found[j]);
                    sw += 1.0;
                    sx += x;
                    sy2 += v;
                    sxx += x * x;
                    sxy += x * v;
                }
            }
            let det = sw * sxx - sx * sx;
            if sw < 2.0 || det.abs() < 1e-9 {
                return None;
            }
            b0 = (sw * sxy - sx * sy2) / det;
            a0 = (sy2 - b0 * sx) / sw;
            for j in 0..count {
                keep[j] = ok[j] && (found[j] - (a0 + b0 * j as f64)).abs() < OUTLIER;
            }
        }
        let inliers = keep.iter().filter(|k| **k).count();
        // A cyclic prefix correlation with the offset ε carriers has phase
        // −2πε.
        let eps = -f64::from(phase_sum.arg()) / std::f64::consts::TAU;
        Some(Timing {
            start: (0..count)
                .map(|j| first + (j * sym) as f64 + a0 + b0 * j as f64)
                .collect(),
            offset: a0,
            drift: b0,
            inliers,
            eps,
        })
    }

    /// The carriers of one symbol (see [`SymbolFft::carriers`]).
    pub fn transform(&mut self, y: &[Iq], symbol_start: f64, eps: f64, out: &mut Vec<Iq>) {
        let g = self.geo;
        self.fft
            .carriers(y, symbol_start, g.g, g.early, eps, g.c_ps, out);
    }
}

/// What equalising a frame gave.
pub struct Equalised {
    /// Every data cell of the frame, in the order the transmitter's frame
    /// builder made them (P2's, then each data symbol's, frequency
    /// deinterleaved), and the noise variance each has.
    pub cells: Vec<Iq>,
    pub var: Vec<f32>,
    /// The signal-to-noise ratio of the frame's cells, dB.
    pub snr_db: f32,
    /// The channel's gain at each carrier of one symbol (the middle one), for
    /// the display.
    pub channel: Vec<Iq>,
    /// Each data symbol's own SNR, dB (from its continual pilots).
    pub symbol_snr_db: Vec<f32>,
    /// The timing the windows still drifted by, samples a symbol (the slopes
    /// of the alignment, which the symbols' line did not take out).
    pub drift_residual: f64,
    /// The noise power on each carrier (the pilots' own).
    pub noise: f32,
    /// For the display, when asked for: the middle data symbol's carriers as
    /// read, and its pilots equalised and normalised to ±1.
    pub scope_raw: Vec<Iq>,
    pub scope_pilots: Vec<Iq>,
}

impl Demod {
    /// All the symbols' carriers (`symbols · c_ps`, symbol by symbol).
    pub fn all_carriers(&mut self, y: &[Iq], t: &Timing) -> Vec<Iq> {
        let t0 = Instant::now();
        let mut out = Vec::with_capacity(self.geo.symbols * self.geo.c_ps);
        for j in 0..self.geo.symbols {
            self.transform(y, t.start[j], t.eps, &mut out);
        }
        self.prof.fft += t0.elapsed();
        out
    }

    /// The P2 symbols' cells, in the dealt order, equalised by their own
    /// pilots, with the noise variance each has.
    pub fn p2_cells(&self, carriers: &[Iq]) -> (Vec<Iq>, Vec<f32>) {
        p2_equalise(
            &self.geo.mode,
            self.carriers.p2(),
            &self.reference,
            self.geo.n_p2,
            self.geo.c_ps,
            carriers,
        )
    }

    /// The P2 cells in the transmitter's logical order: L1-pre first (the
    /// leading `pre` cells) and, given `post`, L1-post and the PLP's cells
    /// behind it.
    pub fn p2_logical(
        &self,
        dealt: &[Iq],
        dealt_var: &[f32],
        pre: usize,
        post: usize,
    ) -> (Vec<Iq>, Vec<f32>) {
        let (n_p2, c_p2) = (self.geo.n_p2, self.carriers.p2_cells());
        // The frequency interleaver is per symbol; undo it, then the deal.
        let mut by_symbol = vec![Iq::new(0.0, 0.0); dealt.len()];
        let mut by_symbol_var = vec![0f32; dealt.len()];
        for j in 0..n_p2.min(dealt.len() / c_p2.max(1)) {
            let r = j * c_p2..(j + 1) * c_p2;
            self.fi_p2
                .deinterleave(j, &dealt[r.clone()], &mut by_symbol[r.clone()]);
            self.fi_p2
                .deinterleave(j, &dealt_var[r.clone()], &mut by_symbol_var[r]);
        }
        let place = p2_deal(n_p2, c_p2, pre, post);
        let mut cells = vec![Iq::new(0.0, 0.0); by_symbol.len()];
        let mut var = vec![0f32; by_symbol.len()];
        for (i, &p) in place.iter().enumerate() {
            cells[i] = by_symbol[p];
            var[i] = by_symbol_var[p];
        }
        (cells, var)
    }
}

impl Demod {
    /// Equalise the data symbols (and the closing one): align them by the
    /// continual pilots, estimate the channel from the scattered ones, divide.
    /// `carriers` is changed (the alignment). Returns the cells of the data
    /// symbols in logical order and their variances, the SNR, and the
    /// channel at the middle symbol.
    pub fn data_cells(&mut self, carriers: &mut [Iq], scope: bool) -> Option<Equalised> {
        let geo = self.geo;
        let (c_ps, mid) = (geo.c_ps, geo.mid);
        let first = geo.n_p2;
        let last = self.last_data;
        if last <= first + 1 || self.cps.len() < 8 {
            return None;
        }
        let mode = geo.mode;
        let dy = self.data_ks.len();
        let t_align = Instant::now();
        // --- Alignment to the first data symbol by the stronger continual
        // pilots.
        let obs = |j: usize, c: &[Iq], k: usize| -> Iq {
            c[j * c_ps + k] / mode.pilot_value(Role::Continual, &self.reference, k, j)
        };
        let ref_obs: Vec<Iq> = self.cps.iter().map(|&k| obs(first, carriers, k)).collect();
        let mut amps: Vec<f32> = ref_obs.iter().map(|c| c.norm()).collect();
        amps.sort_by(f32::total_cmp);
        let floor = amps[amps.len() / 2];
        let sel: Vec<usize> = (0..self.cps.len())
            .filter(|&i| ref_obs[i].norm() >= floor)
            .collect();
        let mut slopes: Vec<(f64, f64)> = Vec::new();
        for j in first + 1..last {
            let d: Vec<Iq> = self
                .cps
                .iter()
                .enumerate()
                .map(|(i, &k)| obs(j, carriers, k) * ref_obs[i].conj())
                .collect();
            let (slope, phi) = align(&d, &sel, &self.cps, mid);
            slopes.push((j as f64, slope));
            ramp(
                &mut carriers[j * c_ps..(j + 1) * c_ps],
                -phi + slope * mid as f64,
                -slope,
            );
        }
        self.prof.align += t_align.elapsed();
        let t_chan = Instant::now();

        // The alignment's slopes grow with the symbol if the windows' line has
        // the wrong drift: a slope of 2πδ/N is the window δ samples late.
        let drift_residual = if slopes.len() >= 4 {
            let n_s = slopes.len() as f64;
            let (sx, sy) = slopes
                .iter()
                .fold((0.0, 0.0), |a, (x, y)| (a.0 + x, a.1 + y));
            let (sxx, sxy) = slopes
                .iter()
                .fold((0.0, 0.0), |a, (x, y)| (a.0 + x * x, a.1 + x * y));
            let det = n_s * sxx - sx * sx;
            if det.abs() > 1e-9 {
                (n_s * sxy - sx * sy) / det * geo.n as f64 / std::f64::consts::TAU
            } else {
                0.0
            }
        } else {
            0.0
        };

        // --- The lattice: each node's sightings, then its value at every
        // data symbol (its sighting or a cubic through the four nearest), the
        // channel across the symbol from those, and the cells divided by it.
        let n_nodes = self.nodes.len();
        let mut sight: Vec<Iq> = Vec::with_capacity(self.sight_sym.len());
        for (m, &k) in self.nodes.iter().enumerate() {
            let (a, b) = (self.sight_off[m] as usize, self.sight_off[m + 1] as usize);
            for i in a..b {
                let j = self.sight_sym[i] as usize;
                sight.push(carriers[j * c_ps + k] * self.sight_inv[i]);
            }
        }
        let c_data = self.data_ks[first % dy].len();
        let n_syms = last - first;
        let mut cells = vec![Iq::new(0.0, 0.0); n_syms * c_data];
        // The reciprocal of each cell's channel power until the noise is known.
        let mut var = vec![0f32; n_syms * c_data];
        let (mut pad_re, mut pad_im) = (
            vec![0f32; n_nodes + 2 * KERNEL_HALF + 1],
            vec![0f32; n_nodes + 2 * KERNEL_HALF + 1],
        );
        let (mut hr, mut hi) = (vec![0f32; c_ps], vec![0f32; c_ps]);
        // For the noise, and the SNR: continual pilots' errors, and the
        // channel's mean power.
        let mut errs: Vec<f32> = Vec::new();
        let mut gain = 0f64;
        let mut chan_mid = vec![Iq::new(0.0, 0.0); c_ps];
        let mut sym_snr: Vec<f32> = Vec::new();
        let mut own: Vec<f32> = Vec::new();
        let (mut scope_raw, mut scope_pilots) = (Vec::new(), Vec::new());
        for j in first..last {
            let rel = j - first;
            for m in 0..n_nodes {
                let tap = self.patterns[self.pattern_of[m] as usize][rel];
                let s = &sight[self.sight_off[m] as usize..self.sight_off[m + 1] as usize];
                let (mut re, mut im) = (0f32, 0f32);
                for i in 0..tap.len as usize {
                    let v = s[tap.start as usize + i];
                    re += tap.w[i] * v.re;
                    im += tap.w[i] * v.im;
                }
                pad_re[m + KERNEL_HALF] = re;
                pad_im[m + KERNEL_HALF] = im;
            }
            self.interpolate_frequency(&mut pad_re, &mut pad_im, n_nodes, &mut hr, &mut hi);
            if j == (first + last) / 2 {
                for (k, c) in chan_mid.iter_mut().enumerate() {
                    *c = Iq::new(hr[k], hi[k]);
                }
            }
            let row = &carriers[j * c_ps..(j + 1) * c_ps];
            if scope && j == (first + last) / 2 {
                scope_raw = row.to_vec();
                for (&k, &pv) in self.cp_ks[j % dy].iter().zip(&self.cp_pv[j]) {
                    let k = k as usize;
                    scope_pilots.push(row[k] / (Iq::new(hr[k], hi[k]) * pv));
                }
            }
            own.clear();
            for (&k, &pv) in self.cp_ks[j % dy].iter().zip(&self.cp_pv[j]) {
                let k = k as usize;
                let e = (row[k] - Iq::new(hr[k], hi[k]) * pv).norm_sqr();
                errs.push(e);
                own.push(e);
            }
            own.sort_by(f32::total_cmp);
            let g_j = hr.iter().zip(&hi).map(|(a, b)| a * a + b * b).sum::<f32>() / c_ps as f32;
            let v_j = (own.get(own.len() / 2).copied().unwrap_or(1.0) / std::f32::consts::LN_2)
                .max(1e-12);
            sym_snr.push(10.0 * (g_j / v_j).log10());
            gain += f64::from(g_j);
            // The data cells, undone from the frequency interleaver as they
            // are written.
            let perm = self.fi_data.h(j);
            let base = rel * c_data;
            let (cells_j, var_j) = (
                &mut cells[base..base + c_data],
                &mut var[base..base + c_data],
            );
            for (&k, &p) in self.data_ks[j % dy].iter().zip(perm) {
                let k = k as usize;
                let (y, hre, him) = (row[k], hr[k], hi[k]);
                let r = 1.0 / (hre * hre + him * him).max(1e-9);
                cells_j[p as usize] =
                    Iq::new((y.re * hre + y.im * him) * r, (y.im * hre - y.re * him) * r);
                var_j[p as usize] = r;
            }
        }
        if errs.is_empty() {
            return None;
        }
        self.prof.channel += t_chan.elapsed();
        let t_cells = Instant::now();
        errs.sort_by(f32::total_cmp);
        // The median of |e|² of complex Gaussian noise is variance · ln 2.
        let vy = (errs[errs.len() / 2] / std::f32::consts::LN_2).max(1e-12);
        let mean_gain = (gain / (last - first) as f64) as f32;
        let snr_db = 10.0 * (mean_gain / vy).log10();
        for v in &mut var {
            *v *= vy;
        }
        // The frame-closing symbol, which has scattered pilots at every node
        // of the lattice and so is equalised by its own alone.
        if geo.has_fc {
            let j = geo.symbols - 1;
            let row = &carriers[j * c_ps..(j + 1) * c_ps];
            let roles = self.carriers.closing();
            let mut node_h: Vec<Iq> = Vec::with_capacity(n_nodes);
            for &k in &self.nodes {
                let pv = mode.pilot_value(Role::Scattered, &self.reference, k, j);
                node_h.push(row[k] / pv);
            }
            let mut d2: Vec<f32> = node_h
                .windows(3)
                .map(|w| (w[1] - (w[0] + w[2]) * 0.5).norm_sqr())
                .collect();
            d2.sort_by(f32::total_cmp);
            let amp = mode.pilot_amplitude(Role::Scattered);
            let var_h = d2[d2.len() / 2] / std::f32::consts::LN_2 / 1.5;
            let vy = (var_h * amp * amp).max(1e-12);
            for (m, v) in node_h.iter().enumerate() {
                pad_re[m + KERNEL_HALF] = v.re;
                pad_im[m + KERNEL_HALF] = v.im;
            }
            self.interpolate_frequency(&mut pad_re, &mut pad_im, n_nodes, &mut hr, &mut hi);
            let ks: Vec<usize> = roles
                .iter()
                .enumerate()
                .filter(|(_, r)| **r == Role::Data)
                .map(|(k, _)| k)
                .collect();
            let h_of = |k: usize| Iq::new(hr[k], hi[k]);
            let x: Vec<Iq> = ks.iter().map(|&k| row[k] / h_of(k)).collect();
            let v: Vec<f32> = ks
                .iter()
                .map(|&k| vy / h_of(k).norm_sqr().max(1e-9))
                .collect();
            if let Some(fi) = &self.fi_fc {
                let mut lx = vec![Iq::new(0.0, 0.0); ks.len()];
                let mut lv = vec![0f32; ks.len()];
                fi.deinterleave(j, &x, &mut lx);
                fi.deinterleave(j, &v, &mut lv);
                cells.extend(lx);
                var.extend(lv);
            }
        }
        self.prof.cells += t_cells.elapsed();
        Some(Equalised {
            cells,
            var,
            snr_db,
            channel: chan_mid,
            symbol_snr_db: sym_snr,
            drift_residual,
            noise: vy,
            scope_raw,
            scope_pilots,
        })
    }

    /// The channel at every carrier from its values at the lattice's nodes
    /// (in `pad_re`/`pad_im` from index `KERNEL_HALF`, the edges padded here
    /// by repeating the end nodes): each node's cell of carriers is the 16
    /// nearest nodes weighted by the carrier's phase, with the weights laid
    /// out so that the phases are the vector lanes.
    fn interpolate_frequency(
        &self,
        pad_re: &mut [f32],
        pad_im: &mut [f32],
        n_nodes: usize,
        hr: &mut [f32],
        hi: &mut [f32],
    ) {
        for p in [&mut *pad_re, &mut *pad_im] {
            let (lo, hi_v) = (p[KERNEL_HALF], p[KERNEL_HALF + n_nodes - 1]);
            p[..KERNEL_HALF].fill(lo);
            p[KERNEL_HALF + n_nodes..].fill(hi_v);
        }
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU has the feature the function was compiled for.
            return unsafe { self.interpolate_frequency_avx2(pad_re, pad_im, hr, hi) };
        }
        self.interpolate_frequency_body(pad_re, pad_im, hr, hi);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn interpolate_frequency_avx2(
        &self,
        pad_re: &[f32],
        pad_im: &[f32],
        hr: &mut [f32],
        hi: &mut [f32],
    ) {
        self.interpolate_frequency_body(pad_re, pad_im, hr, hi);
    }

    #[inline(always)]
    fn interpolate_frequency_body(
        &self,
        pad_re: &[f32],
        pad_im: &[f32],
        hr: &mut [f32],
        hi: &mut [f32],
    ) {
        match self.dxp {
            8 => self.interpolate_cells::<8>(pad_re, pad_im, hr, hi),
            16 => self.interpolate_cells::<16>(pad_re, pad_im, hr, hi),
            _ => self.interpolate_cells::<24>(pad_re, pad_im, hr, hi),
        }
    }

    #[inline(always)]
    fn interpolate_cells<const DXP: usize>(
        &self,
        pad_re: &[f32],
        pad_im: &[f32],
        hr: &mut [f32],
        hi: &mut [f32],
    ) {
        let dx = self.geo.mode.pp.dx();
        let first = self.nodes[0];
        let c_ps = hr.len();
        for m0 in -1..self.nodes.len() as isize {
            let q0 = (m0 + 1) as usize;
            // The carriers between this node and the next, and the phase of
            // the first of them.
            let (k0, k1, p0) = if m0 < 0 {
                (0, first, dx - first)
            } else {
                let k0 = first + m0 as usize * dx;
                (k0, (k0 + dx).min(c_ps), 0)
            };
            if k1 <= k0 {
                continue;
            }
            let (mut ar, mut ai) = ([0f32; DXP], [0f32; DXP]);
            for t in 0..2 * KERNEL_HALF {
                let (xr, xi) = (pad_re[q0 + t], pad_im[q0 + t]);
                let w: &[f32; DXP] = self.kernel_t[t * DXP..(t + 1) * DXP]
                    .try_into()
                    .expect("DXP weights");
                for p in 0..DXP {
                    ar[p] += w[p] * xr;
                    ai[p] += w[p] * xi;
                }
            }
            let len = k1 - k0;
            hr[k0..k1].copy_from_slice(&ar[p0..p0 + len]);
            hi[k0..k1].copy_from_slice(&ai[p0..p0 + len]);
        }
    }
}

/// The least-squares fit's window and polynomial degree: four sightings and
/// a cubic, which passes through them (interpolation). Wider windows and
/// lower degrees average more noise (the MER the pilots show rises by up to
/// 2 dB) but follow a changing channel less well, and on the off-air UK
/// signal lost two or three times as many blocks.
fn smoothing() -> (usize, usize) {
    (4, 3)
}

/// How a node's value at symbol `j` is made from its sightings `ts` (sorted
/// symbols): a polynomial of `degree` fitted by least squares through the
/// `window` sightings nearest `j` and read at `j`. The weights are those of
/// the fit: `(AᵀA)⁻¹Aᵀ`'s first row, for the sightings' times taken from `j`.
fn time_tap(ts: &[u16], j: u16, window: usize, degree: usize) -> TimeTap {
    let after = ts.partition_point(|t| *t < j);
    // Beyond the first or last sighting a cubic would extrapolate, with
    // weights that amplify the noise many times over: a straight line through
    // the nearest six does it with less than the noise of one sighting.
    let outside = ts.first().is_none_or(|t| j < *t) || ts.last().is_none_or(|t| j > *t);
    let (window, degree) = if outside { (6, 1) } else { (window, degree) };
    let w_len = window.min(ts.len()).min(MAX_TAPS);
    let mut tap = TimeTap::default();
    if w_len == 0 {
        return tap;
    }
    // The window centred on `j` as far as the sightings reach.
    let lo = after.saturating_sub(w_len / 2).min(ts.len() - w_len);
    tap.start = lo as u8;
    tap.len = w_len as u8;
    let pts = &ts[lo..lo + w_len];
    let d = degree.min(w_len - 1);
    // Normal equations of the fit in x = t − j.
    let n = d + 1;
    let mut m = [[0f64; 6]; 6];
    for t in pts {
        let x = f64::from(*t) - f64::from(j);
        let pw: Vec<f64> = (0..2 * n).map(|p| x.powi(p as i32)).collect();
        for r in 0..n {
            for c in 0..n {
                m[r][c] += pw[r + c];
            }
        }
    }
    // First row of the inverse: solve (AᵀA)·u = e₀ by Gauss-Jordan.
    let mut aug = [[0f64; 7]; 6];
    for r in 0..n {
        aug[r][..n].copy_from_slice(&m[r][..n]);
        aug[r][n] = if r == 0 { 1.0 } else { 0.0 };
    }
    for c in 0..n {
        let piv = (c..n)
            .max_by(|&a, &b| aug[a][c].abs().total_cmp(&aug[b][c].abs()))
            .unwrap();
        aug.swap(c, piv);
        let div = aug[c][c];
        if div.abs() < 1e-12 {
            // Degenerate: take the nearest sighting.
            tap.w = [0.0; MAX_TAPS];
            let near = (0..w_len)
                .min_by_key(|&i| (i32::from(pts[i]) - i32::from(j)).abs())
                .unwrap();
            tap.w[near] = 1.0;
            return tap;
        }
        for v in aug[c][..=n].iter_mut() {
            *v /= div;
        }
        for r in 0..n {
            if r != c {
                let f = aug[r][c];
                let pivot_row = aug[c];
                for (v, p) in aug[r][..=n].iter_mut().zip(&pivot_row) {
                    *v -= f * p;
                }
            }
        }
    }
    let u: Vec<f64> = (0..n).map(|r| aug[r][n]).collect();
    for (i, t) in pts.iter().enumerate() {
        let x = f64::from(*t) - f64::from(j);
        tap.w[i] = (0..n).map(|p| u[p] * x.powi(p as i32)).sum::<f64>() as f32;
    }
    tap
}

/// The cells of `n_p2` P2 symbols' carriers (`c_ps` each, one after the
/// other), equalised by the symbols' own pilots (the channel linear between
/// them), with the noise variance each has after equalising.
pub fn p2_equalise(
    mode: &Mode,
    roles: &[Role],
    reference: &Reference,
    n_p2: usize,
    c_ps: usize,
    carriers: &[Iq],
) -> (Vec<Iq>, Vec<f32>) {
    let mut cells = Vec::new();
    let mut var = Vec::new();
    for j in 0..n_p2 {
        let y = &carriers[j * c_ps..(j + 1) * c_ps];
        let mut pilots: Vec<(usize, Iq)> = Vec::new();
        for (k, r) in roles.iter().enumerate() {
            if *r == Role::P2Pilot {
                let pv = mode.pilot_value(Role::P2Pilot, reference, k, j);
                pilots.push((k, y[k] / pv));
            }
        }
        if pilots.len() < 3 {
            continue;
        }
        // The noise on a pilot's estimate, from its second difference.
        let mut d2: Vec<f32> = pilots
            .windows(3)
            .map(|w| (w[1].1 - (w[0].1 + w[2].1) * 0.5).norm_sqr())
            .collect();
        d2.sort_by(f32::total_cmp);
        let amp = mode.pilot_amplitude(Role::P2Pilot);
        // The median of |d|^2 for complex Gaussian noise is variance times
        // ln 2; the second difference has 3/2 the estimate's variance.
        let var_h = d2[d2.len() / 2] / std::f32::consts::LN_2 / 1.5;
        let vy = var_h * amp * amp;
        let mut h = vec![Iq::new(0.0, 0.0); c_ps];
        for w in pilots.windows(2) {
            let ((k0, h0), (k1, h1)) = (w[0], w[1]);
            for (k, hk) in h.iter_mut().enumerate().take(k1 + 1).skip(k0) {
                let t = (k - k0) as f32 / (k1 - k0) as f32;
                *hk = h0 * (1.0 - t) + h1 * t;
            }
        }
        let (k_first, k_last) = (pilots[0].0, pilots[pilots.len() - 1].0);
        for (k, hk) in h.iter_mut().enumerate() {
            if k < k_first {
                *hk = pilots[0].1;
            } else if k > k_last {
                *hk = pilots[pilots.len() - 1].1;
            }
        }
        for (k, r) in roles.iter().enumerate() {
            if *r == Role::Data {
                cells.push(y[k] / h[k]);
                var.push(vy / h[k].norm_sqr().max(1e-9));
            }
        }
    }
    (cells, var)
}

/// The cyclic prefix of the symbol whose guard interval is near `nominal`
/// (within `search` samples): the guard's correlation with the end of the
/// symbol, `n` later, at its best place (a parabola's fraction), how well
/// they correlate (0.5 for a clean signal) and the correlation there, whose
/// phase is −2π times the carrier offset in carriers.
pub fn cp_peak(
    y: &[Iq],
    nominal: f64,
    n: usize,
    g: usize,
    search: usize,
) -> Option<(f64, f32, Iq)> {
    let lo = (nominal.round() as isize - search as isize).max(0) as usize;
    let hi = nominal.round() as usize + search;
    if hi + g + n + 1 > y.len() {
        return None;
    }
    let prod = |p: usize| y[p] * y[p + n].conj();
    let mut acc: Iq = (lo..lo + g).map(prod).sum();
    let mut energy: f32 = (lo..lo + g)
        .map(|p| y[p].norm_sqr() + y[p + n].norm_sqr())
        .sum();
    let (mut best, mut at, mut best_c) = (0f32, lo, acc);
    let mut curve: Vec<f32> = Vec::with_capacity(hi - lo + 1);
    for p in lo..=hi {
        let m = acc.norm() / energy.max(1e-12);
        curve.push(m);
        if m > best {
            (best, at, best_c) = (m, p, acc);
        }
        acc += prod(p + g) - prod(p);
        energy +=
            y[p + g].norm_sqr() + y[p + g + n].norm_sqr() - y[p].norm_sqr() - y[p + n].norm_sqr();
    }
    let sub = if at > lo && at < hi {
        crate::acquire::parabola(curve[at - lo - 1], best, curve[at - lo + 1])
    } else {
        0.0
    };
    Some((at as f64 + sub, best, best_c))
}

/// `e^{j(a0 + k·da)}` as a single-precision phasor.
fn cis(a: f64) -> Iq {
    Iq::new(a.cos() as f32, a.sin() as f32)
}

/// `x[k] *= e^{j(a0 + k·da)}`: a turning phasor by recurrence in place of a
/// sine and cosine for every sample. Four chains run side by side so that
/// their multiplies overlap, and each block of 256 starts afresh from the exact
/// angle, so that the single-precision recurrence never drifts.
pub fn ramp(x: &mut [Iq], a0: f64, da: f64) {
    const BLOCK: usize = 256;
    let step4 = cis(4.0 * da);
    for (b, block) in x.chunks_mut(BLOCK).enumerate() {
        let a = a0 + (b * BLOCK) as f64 * da;
        let mut z = [cis(a), cis(a + da), cis(a + 2.0 * da), cis(a + 3.0 * da)];
        for quad in block.chunks_mut(4) {
            for (v, w) in quad.iter_mut().zip(&z) {
                *v *= *w;
            }
            for w in &mut z {
                *w *= step4;
            }
        }
    }
}

/// The transform of symbols' windows, for one FFT size.
pub struct SymbolFft {
    pub n: usize,
    plan: Arc<dyn Fft<f32>>,
    scratch: Vec<Iq>,
    buf: Vec<Iq>,
}

impl SymbolFft {
    pub fn new(n: usize) -> SymbolFft {
        let plan = FftPlanner::new().plan_fft_forward(n);
        SymbolFft {
            n,
            scratch: vec![Iq::new(0.0, 0.0); plan.get_inplace_scratch_len()],
            plan,
            buf: vec![Iq::new(0.0, 0.0); n],
        }
    }

    /// The `c_ps` carriers of the symbol whose guard interval starts at
    /// `symbol_start` in `y`: the window is its useful part started `early`
    /// samples ahead of the guard's end, the carrier offset `eps` (carriers)
    /// taken out, the fraction of a sample the window's start was rounded by
    /// undone. Appends them to `out`.
    #[allow(clippy::too_many_arguments)]
    pub fn carriers(
        &mut self,
        y: &[Iq],
        symbol_start: f64,
        g: usize,
        early: usize,
        eps: f64,
        c_ps: usize,
        out: &mut Vec<Iq>,
    ) {
        let n = self.n;
        let pos = symbol_start + (g - early) as f64;
        let s = pos.round() as usize;
        let frac = pos - s as f64;
        self.buf.copy_from_slice(&y[s..s + n]);
        ramp(&mut self.buf, 0.0, -std::f64::consts::TAU * eps / n as f64);
        self.plan
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        let mid = (c_ps - 1) / 2;
        // The carriers are the bins around DC: the negative ones at the end of
        // the transform's output come first.
        let start = out.len();
        out.extend_from_slice(&self.buf[n - mid..]);
        out.extend_from_slice(&self.buf[..c_ps - mid]);
        // A window `frac` samples later than the symbol's true place sees each
        // carrier advanced; the start's earliness is the same for every
        // symbol and left to the channel estimate.
        let dphi = std::f64::consts::TAU * frac / n as f64;
        ramp(&mut out[start..], -dphi * mid as f64, dphi);
    }
}

/// The slope (radians a carrier) and the constant phase that bring the
/// continual pilots' ratios to a reference `d` (indexed like `cps`, of which
/// `sel` are the ones to use) to the same phase: the slope is the timing's
/// difference, a few samples at most, so it is searched over the range that
/// allows (±3·10⁻³ rad a carrier is ±15 samples at 32K) for the best coherent
/// sum, then refined.
fn align(d: &[Iq], sel: &[usize], cps: &[usize], mid: usize) -> (f64, f64) {
    const STEPS: usize = 300;
    const RANGE: f64 = 6e-3;
    let coherent = |slope: f64| -> Iq {
        let mut acc = Iq::new(0.0, 0.0);
        for &i in sel {
            acc += d[i] * cis(-slope * (cps[i] as f64 - mid as f64));
        }
        acc
    };
    // The coarse grid, a turning phasor for each pilot along it.
    let (s0, delta) = (-0.5 * RANGE, RANGE / STEPS as f64);
    let mut sums = [Iq::new(0.0, 0.0); STEPS + 1];
    for &i in sel {
        let x = cps[i] as f64 - mid as f64;
        let mut z = cis(-s0 * x);
        let w = cis(-delta * x);
        for a in sums.iter_mut() {
            *a += d[i] * z;
            z *= w;
        }
    }
    let (mut best, mut best_s) = (0f32, 0f64);
    for (m, a) in sums.iter().enumerate() {
        let p = a.norm_sqr();
        if p > best {
            (best, best_s) = (p, s0 + m as f64 * delta);
        }
    }
    // Refine: a few halvings around the best.
    let mut best = best.sqrt();
    let mut width = delta;
    for _ in 0..12 {
        for c in [best_s - width * 0.5, best_s + width * 0.5] {
            let m = coherent(c).norm();
            if m > best {
                (best, best_s) = (m, c);
            }
        }
        width *= 0.5;
    }
    (best_s, f64::from(coherent(best_s).arg()))
}
