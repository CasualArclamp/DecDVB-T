// made by CasualArclamp
//! The `TsSource` the modulator pulls from: a lead-in of null packets, then
//! ffmpeg's stream out of the [`TsRing`], with stuffing when it runs dry.
//!
//! The modulator's contract (`decdvbt-mod/src/ts.rs`): infallible, blocking,
//! gapless, constant rate. This source never blocks (blocking would stall the
//! modulator thread; the radio has 0.3 s of signal queued behind it). When the
//! ring is empty it hands out a null packet and counts it, so the channel
//! stays gapless at exactly its rate and the receiver only sees the programme
//! pause. Rate policy (docs/TX_DESIGN.md §3):
//!
//! - ring full: the reader blocks, ffmpeg stalls on its pipe, nothing is lost;
//! - ring empty: null packets, counted in `stuffed`, shown in the GUI;
//! - end of file with no loop: nulls after the last packet, `finished` is set.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use decdvbt_mod::ts::{TS_LEN, TS_SYNC, TsSource};

use crate::ring::TsRing;
use crate::si::SiInjector;

/// Counters the session reads.
#[derive(Debug, Default)]
pub struct TsSourceStats {
    /// Packets of the programme handed to the modulator (not stuffing). Added
    /// in batches of up to 64, so it lags by that much.
    pub passed: AtomicU64,
    /// Null packets handed out because the ring was empty after the lead-in
    /// (an underrun of the encoder, the one the user should see).
    pub stuffed: AtomicU64,
    /// Lead-in nulls still to send.
    pub lead_in_left: AtomicU64,
    /// Ring is closed and drained.
    pub finished: AtomicBool,
}

/// Packets taken from the ring in one lock.
const BATCH_PACKETS: usize = 64;

/// A null packet (PID 0x1FFF, payload 0xFF).
fn null_packet() -> [u8; TS_LEN] {
    let mut p = [0xFFu8; TS_LEN];
    p[..4].copy_from_slice(&[TS_SYNC, 0x1F, 0xFF, 0x10]);
    p
}

/// See the module docs.
pub struct FeederTs {
    ring: Arc<TsRing>,
    stats: Arc<TsSourceStats>,
    si: Option<SiInjector>,
    /// Packets taken from the ring in one lock, handed out one at a time
    /// (so the lock is taken once per 64 packets, not per packet).
    batch: Vec<u8>,
    batch_len: usize,
    batch_pos: usize,
    /// Lead-in nulls left (the hot path reads this, not the atomic).
    lead_left: u64,
    /// Packets a second at the mode's rate, for the SI timers.
    packets_per_second: f64,
    /// Passed packets not yet added to `stats.passed`.
    passed_pending: u64,
    /// While true every packet is null and the ring is untouched: the
    /// modulator's warm-up symbols (`Modulator::warmup_symbols`) and the
    /// lead-in. Cleared by the modulator thread.
    pub priming: Arc<AtomicBool>,
    /// Every packet handed to the modulator after the warm-up, to a file (the
    /// test tool's save-ts option).
    dump: Option<BufWriter<File>>,
}

impl FeederTs {
    /// `lead_in_packets` nulls first (after priming ends).
    pub fn new(
        ring: Arc<TsRing>,
        stats: Arc<TsSourceStats>,
        si: Option<SiInjector>,
        lead_in_packets: u64,
    ) -> FeederTs {
        stats.lead_in_left.store(lead_in_packets, Ordering::Relaxed);
        FeederTs {
            ring,
            stats,
            si,
            batch: vec![0; BATCH_PACKETS * TS_LEN],
            batch_len: 0,
            batch_pos: 0,
            lead_left: lead_in_packets,
            packets_per_second: 1.0,
            passed_pending: 0,
            priming: Arc::new(AtomicBool::new(true)),
            dump: None,
        }
    }

    /// Write the packets the modulator is given (SI injected, stuffing
    /// included, the warm-up nulls not) to `w`, so a received stream can be
    /// compared with what was sent.
    pub fn with_dump(mut self, w: BufWriter<File>) -> FeederTs {
        self.dump = Some(w);
        self
    }

    /// The mode's rate in packets a second, for the TDT/TOT timers.
    pub fn with_rate(mut self, packets_per_second: f64) -> FeederTs {
        self.packets_per_second = packets_per_second;
        self
    }

    fn publish_passed(&mut self) {
        if self.passed_pending > 0 {
            self.stats
                .passed
                .fetch_add(self.passed_pending, Ordering::Relaxed);
            self.passed_pending = 0;
        }
    }
}

impl TsSource for FeederTs {
    fn next_packet(&mut self, out: &mut [u8; TS_LEN]) {
        // Warm-up: nothing is taken from the ring, and no table is added.
        if self.priming.load(Ordering::Relaxed) {
            *out = null_packet();
            return;
        }
        if self.lead_left > 0 {
            self.lead_left -= 1;
            self.stats
                .lead_in_left
                .store(self.lead_left, Ordering::Relaxed);
            *out = null_packet();
        } else if self.batch_pos < self.batch_len {
            out.copy_from_slice(&self.batch[self.batch_pos..self.batch_pos + TS_LEN]);
            self.batch_pos += TS_LEN;
            self.passed_pending += 1;
        } else {
            self.publish_passed();
            let n = self.ring.pop(&mut self.batch);
            if n > 0 {
                self.batch_len = n * TS_LEN;
                out.copy_from_slice(&self.batch[..TS_LEN]);
                self.batch_pos = TS_LEN;
                self.passed_pending += 1;
            } else {
                self.batch_len = 0;
                self.batch_pos = 0;
                // Empty: a null keeps the channel gapless at its rate. At the
                // end of the file that is the end; otherwise it is the
                // encoder running late, which the user should see.
                if self.ring.finished() {
                    self.stats.finished.store(true, Ordering::Release);
                } else {
                    self.stats.stuffed.fetch_add(1, Ordering::Relaxed);
                }
                *out = null_packet();
            }
        }
        if let Some(si) = &mut self.si {
            si.process(out, self.packets_per_second);
        }
        if let Some(d) = &mut self.dump {
            let _ = d.write_all(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(n: u8) -> Vec<u8> {
        let mut p = vec![n; TS_LEN];
        p[..4].copy_from_slice(&[TS_SYNC, 0x01, 0x00, 0x10]);
        p[4] = n;
        p
    }

    fn is_null(p: &[u8; TS_LEN]) -> bool {
        p[1] == 0x1F && p[2] == 0xFF
    }

    #[test]
    fn priming_hands_out_nulls_without_touching_the_ring() {
        let ring = Arc::new(TsRing::new(100));
        ring.push_blocking(&pkt(1));
        let stats = Arc::new(TsSourceStats::default());
        let mut f = FeederTs::new(ring.clone(), stats.clone(), None, 3);
        let mut p = [0u8; TS_LEN];
        for _ in 0..50 {
            f.next_packet(&mut p);
            assert!(is_null(&p));
        }
        assert_eq!(ring.len_packets(), 1);
        assert_eq!(stats.lead_in_left.load(Ordering::Relaxed), 3);
        assert_eq!(stats.stuffed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_lead_in_comes_before_the_programme_and_stuffing_is_counted() {
        let ring = Arc::new(TsRing::new(100));
        let mut data = Vec::new();
        for i in 1..=5u8 {
            data.extend(pkt(i));
        }
        ring.push_blocking(&data);
        let stats = Arc::new(TsSourceStats::default());
        let mut f = FeederTs::new(ring.clone(), stats.clone(), None, 3);
        f.priming.store(false, Ordering::Relaxed);
        let mut got = Vec::new();
        for _ in 0..12 {
            let mut p = [0u8; TS_LEN];
            f.next_packet(&mut p);
            got.push(if is_null(&p) { 0 } else { p[4] });
        }
        // 3 lead-in nulls, 5 packets in order, then stuffing.
        assert_eq!(got, [0, 0, 0, 1, 2, 3, 4, 5, 0, 0, 0, 0]);
        assert_eq!(stats.stuffed.load(Ordering::Relaxed), 4);
        assert_eq!(stats.lead_in_left.load(Ordering::Relaxed), 0);
        f.publish_passed();
        assert_eq!(stats.passed.load(Ordering::Relaxed), 5);
        assert!(!stats.finished.load(Ordering::Relaxed));
    }

    #[test]
    fn the_end_of_the_file_is_not_stuffing() {
        let ring = Arc::new(TsRing::new(100));
        ring.push_blocking(&pkt(9));
        ring.close();
        let stats = Arc::new(TsSourceStats::default());
        let mut f = FeederTs::new(ring, stats.clone(), None, 0);
        f.priming.store(false, Ordering::Relaxed);
        let mut p = [0u8; TS_LEN];
        f.next_packet(&mut p);
        assert_eq!(p[4], 9);
        f.next_packet(&mut p);
        assert!(is_null(&p));
        assert!(stats.finished.load(Ordering::Acquire));
        assert_eq!(stats.stuffed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_big_stream_passes_in_order_across_batches() {
        let ring = Arc::new(TsRing::new(1000));
        let mut data = Vec::new();
        for i in 0..300u32 {
            let mut p = pkt(0);
            p[4..8].copy_from_slice(&i.to_be_bytes());
            data.extend(p);
        }
        ring.push_blocking(&data);
        let mut f = FeederTs::new(ring, Arc::new(TsSourceStats::default()), None, 0);
        f.priming.store(false, Ordering::Relaxed);
        for i in 0..300u32 {
            let mut p = [0u8; TS_LEN];
            f.next_packet(&mut p);
            assert_eq!(u32::from_be_bytes([p[4], p[5], p[6], p[7]]), i);
        }
    }
}
