// made by CasualArclamp
//! Spectrum rows of the signal being sent, for the GUI's waterfall.
//!
//! The tap sits in the modulator thread **after** the gain and the cs8
//! rounding (so the row shows what the radio is given, quantisation noise and
//! clipping included). It takes one 2048-sample window per 50 ms of signal
//! (20 rows a second; one FFT each, a negligible cost), Hann-windowed, and
//! publishes the power in 1024 bins (max-held pairs) as dBFS. Nothing is
//! computed unless `TxConfig::preview` is on.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use decdvbt_core::Iq;
use rustfft::{Fft, FftPlanner};

/// Bins in a row.
pub const BINS: usize = 1024;
/// Rows kept for the GUI to collect; older ones are dropped.
pub const MAX_QUEUED: usize = 64;
/// Samples in the window one row is made from.
pub const WINDOW: usize = 2 * BINS;
/// Rows a second.
const ROWS_PER_SECOND: f64 = 20.0;

/// One spectrum row.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewRow {
    /// Power per bin in dBFS (a full-scale complex tone reads 0), from
    /// -fs/2 to +fs/2 of the baseband, DC in the middle.
    pub bins: Vec<f32>,
    /// The radio's sample rate: the row spans this many hertz.
    pub sample_rate_hz: f64,
    /// Samples of signal since the session started when the row was taken.
    pub at_sample: u64,
}

/// Rows waiting for the GUI.
pub type PreviewQueue = Arc<Mutex<VecDeque<PreviewRow>>>;

/// The tap itself.
pub struct PreviewTap {
    queue: PreviewQueue,
    sample_rate_hz: f64,
    next_at: u64,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// 1 / (sum of the window)^2: a full-scale tone then reads 0 dBFS.
    norm: f32,
    buf: Vec<Iq>,
    scratch: Vec<Iq>,
}

impl PreviewTap {
    pub fn new(queue: PreviewQueue, sample_rate_hz: f64) -> PreviewTap {
        let fft = FftPlanner::new().plan_fft_forward(WINDOW);
        // A periodic Hann window.
        let window: Vec<f32> = (0..WINDOW)
            .map(|n| 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / WINDOW as f64).cos() as f32)
            .collect();
        let sum: f32 = window.iter().sum();
        PreviewTap {
            queue,
            sample_rate_hz,
            next_at: 0,
            scratch: vec![Iq::new(0.0, 0.0); fft.get_inplace_scratch_len()],
            fft,
            window,
            norm: 1.0 / (sum * sum),
            buf: vec![Iq::new(0.0, 0.0); WINDOW],
        }
    }

    /// Whether a block of `len` samples starting at `first_sample` has room
    /// for the row that is due. The caller converts its cs8 bytes to samples
    /// only when this is true, so a block with no row costs nothing.
    pub fn is_due(&self, first_sample: u64, len: usize) -> bool {
        let start = self.next_at.max(first_sample);
        start + WINDOW as u64 <= first_sample + len as u64
    }

    /// Offer the block just produced (`first_sample` is its index in the
    /// session); take a row from it if one is due.
    pub fn offer(&mut self, block: &[Iq], first_sample: u64) {
        if !self.is_due(first_sample, block.len()) {
            return;
        }
        let start_abs = self.next_at.max(first_sample);
        let start = (start_abs - first_sample) as usize;
        for ((d, &s), &w) in self
            .buf
            .iter_mut()
            .zip(&block[start..start + WINDOW])
            .zip(&self.window)
        {
            *d = s * w;
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        // fftshift, and the max of each pair of bins: 2048 to 1024. Bin k of
        // the shifted spectrum is FFT bin (k + N/2) mod N.
        let mut bins = Vec::with_capacity(BINS);
        for k in 0..BINS {
            let at = |j: usize| self.buf[(j + WINDOW / 2) % WINDOW].norm_sqr();
            let p = at(2 * k).max(at(2 * k + 1)) * self.norm;
            bins.push(10.0 * p.max(1e-20).log10());
        }
        let row = PreviewRow {
            bins,
            sample_rate_hz: self.sample_rate_hz,
            at_sample: start_abs,
        };
        let mut q = self.queue.lock().unwrap();
        q.push_back(row);
        while q.len() > MAX_QUEUED {
            q.pop_front();
        }
        drop(q);
        self.next_at = start_abs + (self.sample_rate_hz / ROWS_PER_SECOND) as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn tap(rate: f64) -> (PreviewTap, PreviewQueue) {
        let q: PreviewQueue = Default::default();
        (PreviewTap::new(q.clone(), rate), q)
    }

    #[test]
    fn a_full_scale_tone_reads_zero_dbfs_at_its_frequency() {
        let (mut t, q) = tap(10e6);
        // A tone on bin 2048/4 above centre = fs/4 = 2.5 MHz, amplitude 1.
        let x: Vec<Iq> = (0..4096)
            .map(|n| {
                let p = TAU * 0.25 * n as f64;
                Iq::new(p.cos() as f32, p.sin() as f32)
            })
            .collect();
        t.offer(&x, 0);
        let rows: Vec<_> = q.lock().unwrap().drain(..).collect();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.bins.len(), BINS);
        let (peak_bin, peak) = r
            .bins
            .iter()
            .enumerate()
            .fold((0, f32::MIN), |a, (i, &v)| if v > a.1 { (i, v) } else { a });
        // DC is bin 512; +fs/4 is 256 bins above.
        assert_eq!(peak_bin, 512 + 256);
        assert!(peak.abs() < 0.2, "{peak}");
        // Far from the tone it is very low.
        assert!(r.bins[100] < -80.0, "{}", r.bins[100]);
    }

    #[test]
    fn rows_come_twenty_a_second_and_the_queue_is_bounded() {
        let (mut t, q) = tap(1e6);
        let block = vec![Iq::new(0.1, 0.0); 10_000];
        // 5 s of signal in 10 000-sample blocks: 100 rows made, 64 kept.
        for b in 0..500u64 {
            t.offer(&block, b * 10_000);
        }
        let g = q.lock().unwrap();
        assert_eq!(g.len(), MAX_QUEUED);
        let (a, b) = (g[0].at_sample, g[1].at_sample);
        assert!((b - a).abs_diff(50_000) < 10_000, "{}", b - a);
    }

    #[test]
    fn a_block_too_short_for_a_window_waits_for_the_next() {
        let (mut t, q) = tap(10e6);
        let short = vec![Iq::new(0.1, 0.0); 1000];
        assert!(!t.is_due(0, short.len()));
        t.offer(&short, 0);
        assert!(q.lock().unwrap().is_empty());
        let long = vec![Iq::new(0.1, 0.0); 2100];
        assert!(t.is_due(1000, long.len()));
    }
}
