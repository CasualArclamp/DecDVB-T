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

/// The pilots and carriers of a mode, and the machinery to read frames.
pub struct Demod {
    pub geo: Geometry,
    pub carriers: Carriers,
    reference: Reference,
    fft: SymbolFft,
    /// The lattice of scattered pilots: carriers (every D_x from the first),
    /// and for each node the data symbols it is seen in.
    nodes: Vec<usize>,
    seen: Vec<Vec<u16>>,
    /// The interpolator's weights by the phase of a carrier between nodes.
    kernel: Vec<[f32; 2 * KERNEL_HALF]>,
    /// Carriers that are continual pilots in every symbol.
    cps: Vec<usize>,
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
        let mut kernel = Vec::with_capacity(dx);
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
            for wt in &mut w {
                *wt /= sum;
            }
            kernel.push(w);
        }
        let cps: Vec<usize> = (0..geo.c_ps)
            .filter(|&k| (0..dy).all(|ph| carriers.data(ph)[k] == Role::Continual))
            .collect();
        Demod {
            reference: Reference::new(&mode),
            fft: SymbolFft::new(geo.n),
            fi_p2: FreqInterleaver::new(mode.fft, carriers.p2_cells()),
            fi_data: FreqInterleaver::new(mode.fft, carriers.data_cells()),
            fi_fc: geo
                .has_fc
                .then(|| FreqInterleaver::new(mode.fft, carriers.closing_cells())),
            geo,
            carriers,
            nodes,
            seen,
            kernel,
            cps,
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
}

impl Demod {
    /// All the symbols' carriers (`symbols · c_ps`, symbol by symbol).
    pub fn all_carriers(&mut self, y: &[Iq], t: &Timing) -> Vec<Iq> {
        let mut out = Vec::with_capacity(self.geo.symbols * self.geo.c_ps);
        for j in 0..self.geo.symbols {
            self.transform(y, t.start[j], t.eps, &mut out);
        }
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
    pub fn data_cells(&mut self, carriers: &mut [Iq]) -> Option<Equalised> {
        let geo = self.geo;
        let (c_ps, mid) = (geo.c_ps, geo.mid);
        let first = geo.n_p2;
        let last = if geo.has_fc {
            geo.symbols - 1
        } else {
            geo.symbols
        };
        if last <= first + 1 || self.cps.len() < 8 {
            return None;
        }
        let mode = geo.mode;
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
            for k in 0..c_ps {
                let ph = -(phi + slope * (k as f64 - mid as f64));
                carriers[j * c_ps + k] *= Iq::new(ph.cos() as f32, ph.sin() as f32);
            }
        }

        // --- The lattice: each node's sightings, then its value at every
        // data symbol by a cubic through the four nearest.
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
        let n_nodes = self.nodes.len();
        let mut sight: Vec<Vec<(u16, Iq)>> = Vec::with_capacity(n_nodes);
        for (m, &k) in self.nodes.iter().enumerate() {
            sight.push(
                self.seen[m]
                    .iter()
                    .filter(|&&j| (j as usize) < last)
                    .map(|&j| {
                        let pv = mode.pilot_value(Role::Scattered, &self.reference, k, j as usize);
                        (j, carriers[j as usize * c_ps + k] / pv)
                    })
                    .collect(),
            );
        }
        let dx = mode.pp.dx();
        let mut cells: Vec<Iq> = Vec::new();
        let mut var: Vec<f32> = Vec::new();
        let mut node_val = vec![Iq::new(0.0, 0.0); n_nodes];
        let mut h = vec![Iq::new(0.0, 0.0); c_ps];
        // For the noise, and the SNR: continual pilots' errors, and the
        // channel's mean power.
        let mut errs: Vec<f32> = Vec::new();
        let mut gain = 0f64;
        let mut chan_mid = vec![Iq::new(0.0, 0.0); c_ps];
        let mut per_symbol: Vec<(Vec<usize>, Vec<Iq>, Vec<Iq>)> = Vec::new();
        let mut sym_snr: Vec<f32> = Vec::new();
        for j in first..last {
            for (m, obs) in sight.iter().enumerate() {
                node_val[m] = interpolate_time(obs, j as u16);
            }
            self.interpolate_frequency(&node_val, dx, &mut h);
            if j == (first + last) / 2 {
                chan_mid.copy_from_slice(&h);
            }
            let roles = self.carriers.data(j);
            let row = &carriers[j * c_ps..(j + 1) * c_ps];
            let mut own: Vec<f32> = Vec::new();
            for (k, r) in roles.iter().enumerate() {
                if *r == Role::Continual {
                    let pv = mode.pilot_value(*r, &self.reference, k, j);
                    let e = (row[k] - h[k] * pv).norm_sqr();
                    errs.push(e);
                    own.push(e);
                }
            }
            own.sort_by(f32::total_cmp);
            let g_j = h.iter().map(|c| c.norm_sqr()).sum::<f32>() / c_ps as f32;
            let v_j = (own.get(own.len() / 2).copied().unwrap_or(1.0) / std::f32::consts::LN_2)
                .max(1e-12);
            sym_snr.push(10.0 * (g_j / v_j).log10());
            gain += f64::from(g_j);
            let ks: Vec<usize> = roles
                .iter()
                .enumerate()
                .filter(|(_, r)| **r == Role::Data)
                .map(|(k, _)| k)
                .collect();
            per_symbol.push((
                ks.clone(),
                ks.iter().map(|&k| row[k]).collect(),
                ks.iter().map(|&k| h[k]).collect(),
            ));
        }
        if errs.is_empty() {
            return None;
        }
        errs.sort_by(f32::total_cmp);
        // The median of |e|² of complex Gaussian noise is variance · ln 2.
        let vy = (errs[errs.len() / 2] / std::f32::consts::LN_2).max(1e-12);
        let mean_gain = (gain / (last - first) as f64) as f32;
        let snr_db = 10.0 * (mean_gain / vy).log10();
        for (idx, (ks, ys, hs)) in per_symbol.into_iter().enumerate() {
            let j = first + idx;
            let x: Vec<Iq> = ys.iter().zip(&hs).map(|(y, h)| *y / *h).collect();
            let v: Vec<f32> = hs.iter().map(|h| vy / h.norm_sqr().max(1e-9)).collect();
            let mut lx = vec![Iq::new(0.0, 0.0); ks.len()];
            let mut lv = vec![0f32; ks.len()];
            self.fi_data.deinterleave(j, &x, &mut lx);
            self.fi_data.deinterleave(j, &v, &mut lv);
            cells.extend(lx);
            var.extend(lv);
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
            self.interpolate_frequency(&node_h, dx, &mut h);
            let ks: Vec<usize> = roles
                .iter()
                .enumerate()
                .filter(|(_, r)| **r == Role::Data)
                .map(|(k, _)| k)
                .collect();
            let x: Vec<Iq> = ks.iter().map(|&k| row[k] / h[k]).collect();
            let v: Vec<f32> = ks.iter().map(|&k| vy / h[k].norm_sqr().max(1e-9)).collect();
            if let Some(fi) = &self.fi_fc {
                let mut lx = vec![Iq::new(0.0, 0.0); ks.len()];
                let mut lv = vec![0f32; ks.len()];
                fi.deinterleave(j, &x, &mut lx);
                fi.deinterleave(j, &v, &mut lv);
                cells.extend(lx);
                var.extend(lv);
            }
        }
        Some(Equalised {
            cells,
            var,
            snr_db,
            channel: chan_mid,
            symbol_snr_db: sym_snr,
            drift_residual,
        })
    }

    /// The channel at every carrier from its values at the lattice's nodes.
    fn interpolate_frequency(&self, node: &[Iq], dx: usize, h: &mut [Iq]) {
        let n_nodes = node.len();
        let first = self.nodes[0];
        for (k, hk) in h.iter_mut().enumerate() {
            // The node at or before k, and the carrier's phase after it.
            let (m0, phase) = if k < first {
                (-1isize, k + dx - first)
            } else {
                (((k - first) / dx) as isize, (k - first) % dx)
            };
            let w = &self.kernel[phase];
            let mut re = 0f32;
            let mut im = 0f32;
            for (t, &wt) in w.iter().enumerate() {
                let m = (m0 + t as isize - (KERNEL_HALF as isize - 1))
                    .clamp(0, n_nodes as isize - 1) as usize;
                re += wt * node[m].re;
                im += wt * node[m].im;
            }
            *hk = Iq::new(re, im);
        }
    }
}

/// A node's value at symbol `j`: a sighting if there is one, else a cubic
/// (Lagrange) through the four nearest.
fn interpolate_time(obs: &[(u16, Iq)], j: u16) -> Iq {
    let after = obs.partition_point(|(t, _)| *t < j);
    if after < obs.len() && obs[after].0 == j {
        return obs[after].1;
    }
    let lo = after.saturating_sub(2);
    let hi = (after + 2).min(obs.len());
    let pts = &obs[lo..hi];
    match pts.len() {
        0 => Iq::new(0.0, 0.0),
        1 => pts[0].1,
        _ => {
            let mut acc = Iq::new(0.0, 0.0);
            for (i, (ti, vi)) in pts.iter().enumerate() {
                let mut w = 1.0f32;
                for (m, (tm, _)) in pts.iter().enumerate() {
                    if m != i {
                        w *= (f32::from(j) - f32::from(*tm)) / (f32::from(*ti) - f32::from(*tm));
                    }
                }
                acc += *vi * w;
            }
            acc
        }
    }
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
        let w = -std::f64::consts::TAU * eps / n as f64;
        let step = Iq::new(w.cos() as f32, w.sin() as f32);
        let mut rot = Iq::new(1.0, 0.0);
        for (k, (b, x)) in self.buf.iter_mut().zip(&y[s..s + n]).enumerate() {
            *b = *x * rot;
            rot *= step;
            if k % 256 == 255 {
                let ph = w * (k + 1) as f64;
                rot = Iq::new(ph.cos() as f32, ph.sin() as f32);
            }
        }
        self.plan
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        let mid = (c_ps - 1) / 2;
        let (mid_i, n_i) = (mid as isize, n as isize);
        // A window `frac` samples later than the symbol's true place sees each
        // carrier advanced; the start's earliness is the same for every
        // symbol and left to the channel estimate.
        let dphi = std::f64::consts::TAU * frac / n as f64;
        for k in 0..c_ps {
            let bin = (k as isize - mid_i).rem_euclid(n_i) as usize;
            let ph = dphi * (k as f64 - mid as f64);
            out.push(self.buf[bin] * Iq::new(ph.cos() as f32, ph.sin() as f32));
        }
    }
}

/// The slope (radians a carrier) and the constant phase that bring the
/// continual pilots' ratios to a reference `d` (indexed like `cps`, of which
/// `sel` are the ones to use) to the same phase: the slope is the timing's
/// difference, a few samples at most, so it is searched over the range that
/// allows (±3·10⁻³ rad a carrier is ±15 samples at 32K) for the best coherent
/// sum, then refined.
fn align(d: &[Iq], sel: &[usize], cps: &[usize], mid: usize) -> (f64, f64) {
    let coherent = |slope: f64| -> Iq {
        let mut acc = Iq::new(0.0, 0.0);
        for &i in sel {
            let ph = -slope * (cps[i] as f64 - mid as f64);
            acc += d[i] * Iq::new(ph.cos() as f32, ph.sin() as f32);
        }
        acc
    };
    let (mut best, mut best_s) = (0f32, 0f64);
    let steps = 300;
    for i in 0..=steps {
        let s = (i as f64 / steps as f64 - 0.5) * 6e-3;
        let m = coherent(s).norm();
        if m > best {
            (best, best_s) = (m, s);
        }
    }
    // Refine: a few halvings around the best.
    let mut width = 6e-3 / steps as f64;
    for _ in 0..12 {
        let candidates = [best_s - width * 0.5, best_s, best_s + width * 0.5];
        for c in candidates {
            let m = coherent(c).norm();
            if m > best {
                (best, best_s) = (m, c);
            }
        }
        width *= 0.5;
    }
    (best_s, f64::from(coherent(best_s).arg()))
}
