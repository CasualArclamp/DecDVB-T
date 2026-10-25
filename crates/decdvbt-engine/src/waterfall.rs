// made by CasualArclamp
//! The waterfall's own thread: the FFTs and averaging of its rows, and their
//! release at a steady pace, so it scrolls evenly whatever the radio does.
//! Ported from DecDAB's engine (`decdab-engine::waterfall`, same author).
//!
//! The receive thread only picks the FFT windows a row needs (the same
//! placement as [`crate::spectrum::RowBuilder`]: spread evenly over the
//! row's span of signal) and hands over copies of them, a few hundred small
//! copies a second. This thread transforms and averages them, and when a
//! row's span of signal has passed it has a row.
//!
//! **Pacing.** Samples come in bursts: a HackRF hands over its USB
//! transfers a few at a time, a file is read ahead and waits. The rows are made in the
//! same bursts, so they are held here and let out one every 1/fps of wall
//! time, a couple of rows behind, which the bursts never outrun. If rows
//! pile up (the signal's clock a little faster than this one's, or the
//! display rate lowered) the extras go at once; if they run out, the next
//! goes as soon as it comes, and the clock runs on from there.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_core::Iq;

use crate::rx::Row;
use crate::spectrum::Spectrum;

/// Rows held back before the first is let out: the burst it rides out.
const LEAD_ROWS: usize = 2;
/// Rows held beyond which the extras are let out at once.
const MAX_HELD: usize = 6;
/// Messages queued for the thread before the receive thread drops windows
/// (a row then averages fewer; nothing waits).
const QUEUE: usize = 512;
/// Rows kept for a GUI that is slow to collect them.
const MAX_QUEUED_ROWS: usize = 100;

enum Msg {
    /// One FFT window of samples.
    Window(Vec<Iq>),
    /// The row's span of signal has passed: average what came and queue it.
    EndRow {
        center_hz: f64,
        sample_rate: f64,
        tuning: u32,
    },
    /// Rows a second (the display's frame rate).
    Rate(f64),
    /// Forget the rows held and the half-made one (a retune).
    Clear,
}

/// The receive thread's side: picks the windows and sends them.
pub struct Waterfall {
    tx: Option<SyncSender<Msg>>,
    join: Option<JoinHandle<()>>,
    fft: usize,
    sample_rate: f64,
    samples_per_row: u64,
    segments_per_row: u32,
    /// Samples of the current row seen, and windows sent for it.
    seen: u64,
    sent: u32,
    /// Windows dropped because the thread was behind.
    pub dropped: u64,
}

impl Waterfall {
    /// Start the thread; its rows go to `rows`.
    pub fn start(
        fft: usize,
        sample_rate: f64,
        rows_per_s: f64,
        segments_per_row: u32,
        rows: Arc<Mutex<VecDeque<Row>>>,
    ) -> Waterfall {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let join = std::thread::Builder::new()
            .name("decdvbt-waterfall".into())
            .spawn(move || run(rx, fft, rows_per_s, rows))
            .expect("cannot start the waterfall thread");
        let mut w = Waterfall {
            tx: Some(tx),
            join: Some(join),
            fft,
            sample_rate,
            samples_per_row: 0,
            segments_per_row: 1,
            seen: 0,
            sent: 0,
            dropped: 0,
        };
        w.set_rate(rows_per_s, segments_per_row);
        w
    }

    /// Rows a second, and FFT windows averaged into each.
    pub fn set_rate(&mut self, rows_per_s: f64, segments_per_row: u32) {
        self.samples_per_row = (self.sample_rate / rows_per_s).max(self.fft as f64) as u64;
        self.segments_per_row = segments_per_row.max(1);
        self.seen = 0;
        self.sent = 0;
        self.send(Msg::Rate(rows_per_s));
    }

    /// A retune: the rows held are the old channel's.
    pub fn clear(&mut self) {
        self.seen = 0;
        self.sent = 0;
        self.send(Msg::Clear);
    }

    fn send(&mut self, m: Msg) -> bool {
        let Some(tx) = &self.tx else { return false };
        match tx.try_send(m) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.tx = None;
                false
            }
        }
    }

    /// Take a block: copy out the windows due in it, and end the rows whose
    /// span it completes. The windows are spread over the block so the row's
    /// share of each is even, as in `RowBuilder`.
    pub fn push(&mut self, block: &[Iq], center_hz: f64, tuning: u32) {
        let mut rest = block;
        while !rest.is_empty() {
            // The part of the block that belongs to the current row.
            let left = (self.samples_per_row - self.seen) as usize;
            let (part, tail) = rest.split_at(left.min(rest.len()));
            if part.len() >= self.fft {
                let after = self.seen + part.len() as u64;
                let due = (after * self.segments_per_row as u64 / self.samples_per_row) as u32;
                let todo = due.saturating_sub(self.sent) as usize;
                let room = part.len() - self.fft;
                for k in 0..todo {
                    let at = if todo > 1 {
                        room * k / (todo - 1)
                    } else {
                        room / 2
                    };
                    self.send(Msg::Window(part[at..at + self.fft].to_vec()));
                    self.sent += 1;
                }
            }
            self.seen += part.len() as u64;
            if self.seen >= self.samples_per_row {
                self.send(Msg::EndRow {
                    center_hz,
                    sample_rate: self.sample_rate,
                    tuning,
                });
                self.seen = 0;
                self.sent = 0;
            }
            rest = tail;
        }
    }
}

impl Drop for Waterfall {
    fn drop(&mut self) {
        self.tx = None; // closing the channel ends the thread
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn run(rx: Receiver<Msg>, fft: usize, rows_per_s: f64, out: Arc<Mutex<VecDeque<Row>>>) {
    let mut spec = Spectrum::new(fft);
    let mut period = Duration::from_secs_f64(1.0 / rows_per_s.max(1.0));
    let mut held: VecDeque<Row> = VecDeque::new();
    // When the next row is let out; `None` until there are rows to let out
    // (and again after running dry).
    let mut next: Option<Instant> = None;
    let mut row = Vec::new();
    loop {
        let wait = match next {
            Some(t) if !held.is_empty() => t.saturating_duration_since(Instant::now()),
            _ => Duration::from_millis(200),
        };
        match rx.recv_timeout(wait) {
            Ok(Msg::Window(w)) => spec.add_segment(&w),
            Ok(Msg::EndRow {
                center_hz,
                sample_rate,
                tuning,
            }) => {
                if spec.take_row(&mut row).is_some() {
                    held.push_back(Row {
                        db: row.clone(),
                        center_hz,
                        sample_rate,
                        tuning,
                    });
                }
                // Starting (or starting again): wait for a lead first.
                if next.is_none() && held.len() >= LEAD_ROWS {
                    next = Some(Instant::now());
                }
            }
            Ok(Msg::Rate(r)) => {
                period = Duration::from_secs_f64(1.0 / r.max(1.0));
                held.clear();
                next = None;
                let _ = spec.take_row(&mut row);
            }
            Ok(Msg::Clear) => {
                held.clear();
                next = None;
                let _ = spec.take_row(&mut row);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // The receive thread has ended (a file's end): the rows held
                // are still the signal's, so they go out.
                let mut q = out.lock().unwrap();
                q.extend(held.drain(..));
                let extra = q.len().saturating_sub(MAX_QUEUED_ROWS);
                q.drain(..extra);
                return;
            }
        }
        // Let out the rows that are due.
        let now = Instant::now();
        while let Some(t) = next {
            if held.is_empty() {
                // Dry: the next row goes as soon as it comes.
                break;
            }
            if now < t && held.len() <= MAX_HELD {
                break;
            }
            let r = held.pop_front().expect("not empty");
            {
                let mut q = out.lock().unwrap();
                q.push_back(r);
                let extra = q.len().saturating_sub(MAX_QUEUED_ROWS);
                q.drain(..extra);
            }
            // The clock runs on from its own time, not from now, so the
            // rows stay evenly spaced; too far behind, it starts again.
            let n = t + period;
            next = Some(if now.saturating_duration_since(n) > 4 * period {
                now + period
            } else {
                n
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_come_out_evenly_from_bursty_input() {
        let rows = Arc::new(Mutex::new(VecDeque::new()));
        let rate = 409_600.0; // 20 rows a second of 20 480 samples
        let mut w = Waterfall::start(1024, rate, 20.0, 4, rows.clone());
        let block = vec![Iq::new(0.1, 0.0); 81_920]; // four rows at a time
        let t0 = Instant::now();
        let mut stamps = Vec::new();
        let mut seen = 0;
        // Bursts of four rows every 200 ms, for 1.4 s.
        for _ in 0..7 {
            w.push(&block, 546e6, 0);
            let until = Instant::now() + Duration::from_millis(200);
            while Instant::now() < until {
                let n = rows.lock().unwrap().len();
                if n > seen {
                    for _ in seen..n {
                        stamps.push(t0.elapsed().as_secs_f64());
                    }
                    seen = n;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        // The gaps between rows are near 50 ms, not bunched in fours.
        let gaps: Vec<f64> = stamps.windows(2).map(|p| p[1] - p[0]).collect();
        assert!(gaps.len() >= 15, "{gaps:?}");
        let bunched = gaps.iter().filter(|g| **g < 0.02).count();
        assert!(bunched <= 2, "{gaps:?}");
    }
}
