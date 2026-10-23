// made by CasualArclamp
//! The stretch of the radio's stream a reader is waiting for: the caller says
//! which samples it needs next, blocks of the stream are offered in order, and
//! when the stretch is whole it is there as it came, and (made when asked for)
//! as separate real and imaginary arrays, which is what the interpolator
//! reads.

use decdvbt_core::Iq;

use crate::interp::Planar;

pub struct Window {
    /// The stretch wanted: its first sample's place in the stream, and its
    /// length.
    want: Option<(u64, usize)>,
    win: Vec<Iq>,
    /// The stretch as real and imaginary arrays, made by [`Window::planar`].
    planar: Planar,
    planar_ready: bool,
    filled: usize,
    /// The last `keep` samples before the end of the latest block, for a
    /// stretch that starts before the block it is completed in (the next
    /// frame's stretch overlaps the end of the last one's), and where they
    /// end in the stream.
    keep: usize,
    hist: Vec<Iq>,
    hist_end: u64,
}

impl Default for Window {
    fn default() -> Self {
        Window::new()
    }
}

impl Window {
    pub fn new() -> Window {
        Window {
            want: None,
            win: Vec::new(),
            planar: Planar::default(),
            planar_ready: false,
            filled: 0,
            keep: 0,
            hist: Vec::new(),
            hist_end: 0,
        }
    }

    /// A window that also remembers the last `keep` samples, so that a
    /// stretch wanted from a place just behind the block being offered is
    /// made from what went by.
    pub fn with_history(keep: usize) -> Window {
        Window {
            keep,
            ..Window::new()
        }
    }

    /// Remember the block, once, for the history.
    fn remember(&mut self, block: &[Iq], abs_start: u64) {
        if self.keep == 0 {
            return;
        }
        let end = abs_start + block.len() as u64;
        if end <= self.hist_end && !self.hist.is_empty() {
            return;
        }
        if abs_start != self.hist_end {
            // The stream has jumped.
            self.hist.clear();
        }
        self.hist.extend_from_slice(block);
        // Trimmed now and then, not every block.
        if self.hist.len() > 2 * self.keep + block.len() {
            let drop = self.hist.len() - self.keep;
            self.hist.drain(..drop);
        }
        self.hist_end = end;
    }

    pub fn is_wanting(&self) -> bool {
        self.want.is_some()
    }

    /// Nothing wanted.
    pub fn cancel(&mut self) {
        self.want = None;
    }

    /// The stretch wanted: start and length.
    pub fn wanted(&self) -> Option<(u64, usize)> {
        self.want
    }

    /// Want `len` samples from the stream's `start`-th.
    pub fn want(&mut self, start: u64, len: usize) {
        self.want = Some((start, len));
        self.win.clear();
        self.win.resize(len, Iq::new(0.0, 0.0));
        self.planar_ready = false;
        self.filled = 0;
    }

    /// The stretch, once whole.
    pub fn samples(&self) -> &[Iq] {
        &self.win
    }

    /// The stretch as separate real and imaginary arrays (made the first
    /// time it is asked for after the stretch is whole).
    pub fn planar(&mut self) -> &Planar {
        if !self.planar_ready {
            self.planar.set(&self.win);
            self.planar_ready = true;
        }
        &self.planar
    }

    /// Offer a block whose first sample is the stream's `abs_start`-th.
    /// True once the stretch is whole; a stretch
    /// that lay in samples already gone is asked for again from the end of
    /// this block.
    pub fn feed(&mut self, block: &[Iq], abs_start: u64) -> bool {
        self.remember(block, abs_start);
        let Some((w0, len)) = self.want else {
            return false;
        };
        let (end, w1) = (abs_start + block.len() as u64, w0 + len as u64);
        // From the history, if there is one, what lies before this block too.
        let hist_start = self.hist_end - self.hist.len() as u64;
        let from = w0 + self.filled as u64;
        if self.keep > 0 && from < abs_start && self.hist_end == end {
            let (a, b) = (from.max(hist_start), w1.min(abs_start));
            if a < b {
                let src = &self.hist[(a - hist_start) as usize..(b - hist_start) as usize];
                let at = (a - w0) as usize;
                self.win[at..at + src.len()].copy_from_slice(src);
                self.filled = at + src.len();
            }
        }
        let from = w0 + self.filled as u64;
        let (a, b) = (
            w0.max(abs_start)
                .max(if self.filled > 0 { from } else { 0 }),
            w1.min(end),
        );
        if a < b {
            let src = &block[(a - abs_start) as usize..(b - abs_start) as usize];
            let at = (a - w0) as usize;
            self.win[at..at + src.len()].copy_from_slice(src);
            // Blocks arrive in order: what is filled is a prefix.
            self.filled = at + src.len();
        }
        if self.filled < len {
            if w1 <= end {
                self.want(end, len);
            }
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(a: u64, n: usize) -> Vec<Iq> {
        (a..a + n as u64).map(|i| Iq::new(i as f32, 0.0)).collect()
    }

    #[test]
    fn a_stretch_is_gathered_from_blocks_of_any_size() {
        let mut w = Window::new();
        w.want(25, 40);
        let mut at = 0u64;
        let mut done = false;
        for n in [7usize, 30, 3, 100] {
            done = w.feed(&ramp(at, n), at);
            at += n as u64;
            if done {
                break;
            }
        }
        assert!(done);
        assert_eq!(w.samples().len(), 40);
    }

    #[test]
    fn a_stretch_that_starts_behind_the_block_is_made_from_the_history() {
        let mut w = Window::with_history(100);
        // Blocks of 30 go by with nothing wanted.
        for k in 0..4u64 {
            w.feed(&ramp(k * 30, 30), k * 30);
        }
        // Then a stretch from 100, 50 long, wanted as the block 90..120 is
        // offered again: its first 20 samples are in the history.
        w.want(100, 50);
        assert!(!w.feed(&ramp(90, 30), 90));
        assert!(w.feed(&ramp(120, 30), 120));
        let got: Vec<f32> = w.samples().iter().map(|c| c.re).collect();
        let want: Vec<f32> = (100..150).map(|i| i as f32).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn a_stretch_in_samples_gone_is_asked_for_again() {
        let mut w = Window::new();
        w.want(10, 20);
        // The first block starts past it.
        assert!(!w.feed(&ramp(50, 5), 50));
        assert_eq!(w.wanted(), Some((55, 20)));
        assert!(w.feed(&ramp(55, 30), 55));
    }
}
