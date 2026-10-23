// made by CasualArclamp
//! Averaged power spectrum rows for the waterfall.
//!
//! Adapted from DecSAT's `Spectrum` (same author), reshaped for CPU cost: a
//! row is built up block by block ([`RowBuilder`]) from a few FFT segments
//! per block, so there is no second pass over a big buffer, and no
//! allocation per segment. Only the segments are touched: the rest of a
//! 20 MS/s block is never looked at.

use std::sync::Arc;

use decdvbt_core::Iq;
use rustfft::{Fft, FftPlanner};

/// Windowed FFT of fixed size, accumulating power.
pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    size: usize,
    window: Vec<f32>,
    /// (Σ window)²: the power gain of a unit tone, which the row divides
    /// out so that a full-scale tone reads 0 dBFS.
    tone_gain: f32,
    /// FFT work area; rustfft transforms in place.
    buf: Vec<Iq>,
    /// Scratch rustfft needs, kept so `process_with_scratch` never allocates.
    scratch: Vec<Iq>,
    accum: Vec<f32>,
    segments: u32,
}

impl Spectrum {
    pub fn new(size: usize) -> Self {
        assert!(
            size.is_power_of_two() && size >= 64,
            "fft size must be a power of two >= 64"
        );
        let fft = FftPlanner::new().plan_fft_forward(size);
        // Hann window: cheap and good enough to keep sidelobes off the plot.
        let window: Vec<f32> = (0..size)
            .map(|k| 0.5 - 0.5 * (std::f32::consts::TAU * k as f32 / size as f32).cos())
            .collect();
        let scratch = vec![Iq::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let tone_gain = window.iter().sum::<f32>().powi(2);
        Spectrum {
            fft,
            size,
            window,
            tone_gain,
            buf: vec![Iq::new(0.0, 0.0); size],
            scratch,
            accum: vec![0.0; size],
            segments: 0,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Add the periodogram of `samples[..size]` to the running sum.
    pub fn add_segment(&mut self, samples: &[Iq]) {
        for ((o, s), w) in self.buf.iter_mut().zip(samples).zip(&self.window) {
            *o = *s * *w;
        }
        self.fft
            .process_with_scratch(&mut self.buf, &mut self.scratch);
        for (a, c) in self.accum.iter_mut().zip(&self.buf) {
            *a += c.norm_sqr();
        }
        self.segments += 1;
    }

    pub fn segments(&self) -> u32 {
        self.segments
    }

    /// The average so far, dBFS per bin (a full-scale tone reads 0), FFT-shifted (index 0 the most
    /// negative frequency, the middle DC), written to `row`; then starts
    /// over. `None` if no segment was added.
    pub fn take_row(&mut self, row: &mut Vec<f32>) -> Option<()> {
        if self.segments == 0 {
            return None;
        }
        let norm = 1.0 / (self.segments as f32 * self.tone_gain);
        let half = self.size / 2;
        // The negative frequencies live in the top half of an FFT's output,
        // so the shifted row reads the upper half first.
        let (neg, pos) = self.accum.split_at(half);
        row.clear();
        row.extend(
            pos.iter()
                .chain(neg.iter())
                .map(|&p| 10.0 * (p * norm).max(1e-20).log10()),
        );
        self.accum.fill(0.0);
        self.segments = 0;
        Some(())
    }
}

/// Builds rows over time: a row every `1/rows_per_s` of signal, from
/// `segments_per_row` FFTs spread over the blocks that make it up.
pub struct RowBuilder {
    spec: Spectrum,
    samples_per_row: u64,
    segments_per_row: u32,
    seen: u64,
}

impl RowBuilder {
    pub fn new(fft: usize, sample_rate: f64, rows_per_s: f64, segments_per_row: u32) -> Self {
        RowBuilder {
            spec: Spectrum::new(fft),
            samples_per_row: (sample_rate / rows_per_s).max(fft as f64) as u64,
            segments_per_row: segments_per_row.max(1),
            seen: 0,
        }
    }

    pub fn fft_size(&self) -> usize {
        self.spec.size()
    }

    /// Feed one block; returns true when a row is ready for
    /// [`RowBuilder::take_row`]. The segments are placed so that the row's
    /// share of each block is even, not clustered at the block's start.
    pub fn push(&mut self, block: &[Iq]) -> bool {
        let size = self.spec.size();
        if block.len() >= size {
            // Segments due by the end of this block: the row's budget, in
            // proportion to how much of the row's time has passed.
            let after = (self.seen + block.len() as u64).min(self.samples_per_row);
            let due = (after * self.segments_per_row as u64 / self.samples_per_row) as u32;
            let todo = due.saturating_sub(self.spec.segments());
            // Spread over the block, `step` apart (at least `size`, no overlap).
            let room = block.len() - size;
            for k in 0..todo as usize {
                let at = if todo > 1 {
                    room * k / (todo as usize - 1)
                } else {
                    room / 2
                };
                self.spec.add_segment(&block[at..at + size]);
            }
        }
        self.seen += block.len() as u64;
        self.seen >= self.samples_per_row
    }

    pub fn take_row(&mut self, row: &mut Vec<f32>) -> Option<()> {
        self.seen = self.seen.saturating_sub(self.samples_per_row);
        self.spec.take_row(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(frac: f32, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|k| {
                let ph = TAU * frac * k as f32;
                Iq::new(ph.cos(), ph.sin())
            })
            .collect()
    }

    #[test]
    fn tone_lands_in_the_right_bin() {
        let size = 1024;
        let mut sp = Spectrum::new(size);
        let samples = tone(0.125, size * 4);
        for k in 0..4 {
            sp.add_segment(&samples[k * size..(k + 1) * size]);
        }
        let mut db = Vec::new();
        sp.take_row(&mut db).unwrap();
        let peak = db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        // After the shift DC is at size/2, so +0.125 fs is an eighth above it.
        assert!(peak.abs_diff(size / 2 + size / 8) <= 1, "peak at {peak}");
        // A full-scale tone on a bin centre reads 0 dBFS.
        assert!(db[peak].abs() < 0.1, "{}", db[peak]);
    }

    #[test]
    fn nothing_added_gives_no_row() {
        let mut sp = Spectrum::new(256);
        assert!(sp.take_row(&mut Vec::new()).is_none());
    }

    #[test]
    fn rows_come_at_the_asked_rate_with_the_asked_budget() {
        // 1 MS/s, 10 rows/s: a row every 100 000 samples, 8 segments each.
        let mut rb = RowBuilder::new(512, 1e6, 10.0, 8);
        let block = tone(0.1, 25_000);
        let mut rows = 0;
        let mut row = Vec::new();
        for _ in 0..40 {
            if rb.push(&block) {
                // Eight segments went into each row, however the blocks fell.
                assert_eq!(rb.spec.segments(), 8);
                assert!(rb.take_row(&mut row).is_some());
                rows += 1;
            }
        }
        assert_eq!(rows, 10);
        assert_eq!(row.len(), 512);
    }
}
