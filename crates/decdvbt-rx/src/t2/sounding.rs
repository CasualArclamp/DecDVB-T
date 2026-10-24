// made by CasualArclamp
//! The channel sounder for a DVB-T2 signal: the fading waterfall's and the
//! delay–Doppler maps' rows made from what the T2 receiver has already
//! estimated, so it costs next to nothing (no transforms of its own).
//!
//! DVB-T's sounder ([`crate::sounder`]) reads one symbol in eight itself. A
//! T2 frame's equaliser has the channel at every node of the scattered
//! pilots' lattice (every D_x-th carrier) at every data symbol already, a
//! cubic through the node's sightings D_y symbols apart; one row every D_y
//! symbols holds all of it ([`crate::t2::frame::Equalised::node_rows`]).
//!
//! What has to be done to them:
//!
//! 1. **The window's early start** (a quarter of the guard interval, to be
//!    clear of echoes either side) is in the estimate as a delay: taken out
//!    by a phase ramp across the carriers, so the main path is at 0 µs.
//! 2. **Frames joined**: the equaliser brings each frame's symbols to the
//!    phase and timing of its first data symbol (by the continual pilots),
//!    so each frame starts from a phase of its own. A frame's rows are
//!    turned (a common phase and a slope across the band) to continue the
//!    last row of the frame before, which is what the alignment does from
//!    symbol to symbol within a frame: the common phase of the strongest
//!    path stays at 0 Hz, the other paths' Doppler against it is kept.
//! 3. **Even time**: P1, the P2 symbols and the closing symbol leave a gap
//!    between frames, and a row is put every D_y symbols' time throughout by
//!    linear interpolation between the rows either side.

use decdvbt_core::Iq;
use decdvbt_ofdm::t2::p1::SAMPLES as P1_SAMPLES;

use crate::delay_doppler::{ChannelHistory, DelayDoppler, FreqDoppler};
use crate::sounder::{Geometry, Sounding, Status};
use crate::t2::frame::Geometry as FrameGeometry;

/// Rows of power averaged into one fading row.
const FADE_GROUP: usize = 4;

/// A frame's rows, as the T2 receiver hands them over.
pub struct FrameRows<'a> {
    pub geo: &'a FrameGeometry,
    /// The lattice's carriers, and data cells in a data symbol.
    pub nodes: &'a [usize],
    pub data_cells: usize,
    /// `nodes.len()` values a row, and the data symbol each row is of.
    pub rows: &'a [Iq],
    pub symbols: &'a [usize],
    /// Where the frame's P1 symbol starts, in the signal's samples since
    /// the stream began.
    pub p1_at: f64,
    pub cfo_hz: f64,
    pub clock_ppm: f64,
}

pub struct T2Sounder {
    fs: f64,
    hist: Option<ChannelHistory>,
    status: Status,
    /// The last row put out (after the corrections), and its time
    /// (samples); when the next is due.
    prev: Option<(f64, Vec<Iq>)>,
    next_t: f64,
    /// The phase ramp that takes the window's early start out, per node.
    early: Vec<Iq>,
    group: Vec<f32>,
    group_n: usize,
    fading: Vec<Vec<f32>>,
    row: Vec<Iq>,
}

impl T2Sounder {
    /// For a signal sampled at `fs` (the T2 receiver's own rate).
    pub fn new(fs: f64) -> T2Sounder {
        T2Sounder {
            fs,
            hist: None,
            status: Status::default(),
            prev: None,
            next_t: 0.0,
            early: Vec::new(),
            group: Vec::new(),
            group_n: 0,
            fading: Vec::new(),
            row: Vec::new(),
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// Forget the lock: the next frame starts afresh.
    pub fn reset(&mut self) {
        self.hist = None;
        self.prev = None;
        self.status.tracking = false;
        self.status.geometry = None;
        self.group.clear();
        self.group_n = 0;
    }

    /// Fading rows made since the last call: |H|² in dB at each node.
    pub fn take_fading(&mut self, out: &mut Vec<Vec<f32>>) {
        out.append(&mut self.fading);
    }

    pub fn delay_doppler(&mut self) -> Option<&DelayDoppler> {
        self.hist.as_mut()?.map()
    }

    pub fn freq_doppler(&mut self) -> Option<&FreqDoppler> {
        self.hist.as_mut()?.freq_map()
    }

    /// A frame's rows.
    pub fn push_frame(&mut self, f: &FrameRows) {
        let cols = f.nodes.len();
        if cols < 8 || f.symbols.is_empty() || f.rows.len() != cols * f.symbols.len() {
            return;
        }
        let geo = f.geo;
        let mode = geo.mode;
        let dy = mode.pp.dy();
        let row_samples = (dy * geo.sym) as f64;
        let g = Geometry {
            fft_name: mode.fft.name(),
            guard_name: mode.guard.name(),
            dvbt: None,
            active_carriers: geo.c_ps,
            data_carriers: f.data_cells,
            columns: cols,
            pilot_step: mode.pp.dx(),
            first_carrier: f.nodes[0],
            mid_carrier: geo.mid,
            carrier_spacing_hz: self.fs / geo.n as f64,
            symbol_s: geo.sym as f64 / self.fs,
            row_s: row_samples / self.fs,
            guard_us: geo.g as f64 / self.fs * 1e6,
        };
        if self.status.geometry != Some(g) || self.hist.is_none() {
            self.reset();
            self.hist = Some(ChannelHistory::new(
                g.pilot_step as f64 * g.carrier_spacing_hz,
                g.row_s,
                g.guard_us,
            ));
            // The window starts `early` samples ahead: the signal is that
            // much late in it, a carrier k turned by −2πk·early/N.
            let w = std::f64::consts::TAU * geo.early as f64 / geo.n as f64;
            self.early = f
                .nodes
                .iter()
                .map(|&k| {
                    let a = w * (k as f64 - geo.mid as f64);
                    Iq::new(a.cos() as f32, a.sin() as f32)
                })
                .collect();
            self.status.geometry = Some(g);
            self.status.locks += 1;
            self.status.rows = 0;
        }
        self.status.tracking = true;
        self.status.cfo_hz = f.cfo_hz;
        self.status.clock_ppm = f.clock_ppm;

        let time_of = |i: usize| {
            f.p1_at + P1_SAMPLES as f64 + (f.symbols[i] * geo.sym) as f64 + 0.5 * geo.sym as f64
        };
        // A frame or more missed: the rows either side are not one channel's
        // history, start again.
        if let Some((t, _)) = &self.prev
            && time_of(0) - t > geo.frame_samples() as f64
        {
            if let Some(h) = &mut self.hist {
                h.clear();
            }
            self.prev = None;
        }
        // The turn that makes this frame's first row continue the last one.
        let first: Vec<Iq> = f.rows[..cols]
            .iter()
            .zip(&self.early)
            .map(|(v, e)| v * e)
            .collect();
        let turn = match &self.prev {
            Some((_, last)) => continuation(last, &first),
            None => vec![Iq::new(1.0, 0.0); cols],
        };
        for i in 0..f.symbols.len() {
            let mut row = std::mem::take(&mut self.row);
            row.clear();
            row.extend(
                f.rows[i * cols..(i + 1) * cols]
                    .iter()
                    .zip(&self.early)
                    .zip(&turn)
                    .map(|((v, e), t)| v * e * t),
            );
            let t = time_of(i);
            match self.prev.take() {
                None => {
                    self.emit(&row);
                    self.next_t = t + row_samples;
                }
                Some((tp, last)) => {
                    while self.next_t <= t {
                        let w = ((self.next_t - tp) / (t - tp).max(1.0)) as f32;
                        let lerp: Vec<Iq> = last
                            .iter()
                            .zip(&row)
                            .map(|(a, b)| a + (b - a) * w)
                            .collect();
                        self.emit(&lerp);
                        self.next_t += row_samples;
                    }
                    self.row = last;
                }
            }
            self.prev = Some((t, row));
        }
    }

    /// A row at its even place in time: into the history, the fading
    /// rows, and the coherence.
    fn emit(&mut self, row: &[Iq]) {
        let Some(h) = &mut self.hist else { return };
        h.push(row);
        let (mut num, mut den) = (Iq::new(0.0, 0.0), 0.0f32);
        for w2 in row.windows(2) {
            num += w2[1] * w2[0].conj();
            den += w2[1].norm() * w2[0].norm();
        }
        self.status.coherence = if den > 0.0 { num.norm() / den } else { 0.0 };
        self.status.rows += 1;
        if self.group.len() != row.len() {
            self.group = vec![0.0; row.len()];
            self.group_n = 0;
        }
        for (a, v) in self.group.iter_mut().zip(row) {
            *a += v.norm_sqr();
        }
        self.group_n += 1;
        if self.group_n == FADE_GROUP {
            let g = FADE_GROUP as f32;
            self.fading.push(
                self.group
                    .iter()
                    .map(|&p| 10.0 * (p / g).max(1e-12).log10())
                    .collect(),
            );
            self.group.fill(0.0);
            self.group_n = 0;
            // A GUI that stops collecting does not make this grow.
            let extra = self.fading.len().saturating_sub(600);
            self.fading.drain(..extra);
        }
    }
}

impl Sounding for T2Sounder {
    fn status(&self) -> Status {
        T2Sounder::status(self)
    }
    fn take_fading(&mut self, out: &mut Vec<Vec<f32>>) {
        T2Sounder::take_fading(self, out)
    }
    fn delay_doppler(&mut self) -> Option<&DelayDoppler> {
        T2Sounder::delay_doppler(self)
    }
    fn freq_doppler(&mut self) -> Option<&FreqDoppler> {
        T2Sounder::freq_doppler(self)
    }
}

/// Per node, the turn (a common phase and a slope across the band) that
/// takes `next` closest to `last`: the slope from neighbouring nodes'
/// phase differences, then the phase of what is left.
fn continuation(last: &[Iq], next: &[Iq]) -> Vec<Iq> {
    let d: Vec<Iq> = next.iter().zip(last).map(|(n, l)| n * l.conj()).collect();
    let slope = d.windows(2).map(|w| w[1] * w[0].conj()).sum::<Iq>().arg();
    let mid = (d.len() / 2) as f32;
    let phi = d
        .iter()
        .enumerate()
        .map(|(m, v)| v * Iq::from_polar(1.0, -slope * (m as f32 - mid)))
        .sum::<Iq>()
        .arg();
    (0..d.len())
        .map(|m| Iq::from_polar(1.0, -(phi + slope * (m as f32 - mid))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turned_row_is_turned_back() {
        let last: Vec<Iq> = (0..100)
            .map(|m| Iq::from_polar(1.0 + 0.3 * (m as f32 * 0.2).sin(), 0.05 * m as f32))
            .collect();
        let next: Vec<Iq> = last
            .iter()
            .enumerate()
            .map(|(m, v)| v * Iq::from_polar(1.0, 0.7 + 0.01 * (m as f32 - 50.0)))
            .collect();
        let turn = continuation(&last, &next);
        for ((n, t), l) in next.iter().zip(&turn).zip(&last) {
            assert!((n * t - l).norm() < 1e-3, "{} vs {}", n * t, l);
        }
    }
}
