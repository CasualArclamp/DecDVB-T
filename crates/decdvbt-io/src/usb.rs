// made by CasualArclamp
//! What the RTL-SDR front end needs of USB (from DecLAB's `declab-io/src/usb.rs`):
//! waiting on an `nusb` future with a timeout, and the bulk reader thread that
//! keeps transfers queued and hands the buffers to the consumer.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;
use std::time::{Duration, Instant};

use nusb::transfer::{Queue, RequestBuffer};

/// How often the reader looks at its stop flag while waiting for data.
const POLL: Duration = Duration::from_millis(100);
/// Polls without a completed transfer before the reader gives up.
const STALL_POLLS: u32 = 20;

/// Wakes a thread parked in [`block_on_timeout`].
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Wait for a future on this thread, for at most `timeout`. Rust note: nusb
/// hands out `Future`s, and a future does nothing until it is polled with a
/// `Waker` that says how to wake the poller; this one unparks the thread.
/// (`futures_lite::block_on`, as the HackRF driver uses, cannot give up.)
/// The future is dropped on a timeout, which cancels the transfer.
pub fn block_on_timeout<F: Future>(fut: F, timeout: Duration) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + timeout;
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return Some(v);
        }
        let left = deadline.checked_duration_since(Instant::now())?;
        std::thread::park_timeout(left);
    }
}

/// Wait for the queue's next transfer to complete, for at most `timeout`.
/// `Queue::poll_next` panics with nothing pending, so there must be.
fn next_complete(
    queue: &mut Queue<RequestBuffer>,
    timeout: Duration,
) -> Option<nusb::transfer::Completion<Vec<u8>>> {
    block_on_timeout(queue.next_complete(), timeout)
}

/// What the reader thread needs.
pub struct BulkReader {
    pub interface: nusb::Interface,
    pub endpoint: u8,
    /// Bulk transfers kept queued, and the size of each.
    pub in_flight: usize,
    pub transfer: usize,
    pub tx: SyncSender<Vec<u8>>,
    /// Buffers the consumer is done with, to be used again.
    pub recycled: Receiver<Vec<u8>>,
    pub stop: Arc<AtomicBool>,
    pub overflows: Arc<AtomicU64>,
    /// The radio's name, for the log.
    pub what: &'static str,
}

impl BulkReader {
    /// Run until stopped, the consumer goes, or the radio stops delivering:
    /// keeps the transfers in flight and hands each completed one on. If the
    /// consumer falls behind, whole buffers are dropped and counted rather
    /// than stalling the USB side. Cancels its transfers on the way out.
    pub fn run(self) {
        let mut queue = self.interface.bulk_in_queue(self.endpoint);
        // Buffers that were dropped for want of room in the channel come back
        // here.
        let mut spare: Vec<Vec<u8>> = Vec::new();
        for _ in 0..self.in_flight {
            queue.submit(RequestBuffer::new(self.transfer));
        }
        let mut idle = 0;
        while !self.stop.load(Ordering::Relaxed) {
            let Some(done) = next_complete(&mut queue, POLL) else {
                idle += 1;
                if idle >= STALL_POLLS {
                    tracing::warn!(
                        "{} delivered nothing for {:?}",
                        self.what,
                        POLL * STALL_POLLS
                    );
                    break;
                }
                continue;
            };
            idle = 0;
            if let Err(e) = done.status {
                tracing::warn!("{} read failed: {e}", self.what);
                break;
            }
            // The next transfer goes in before this one is handed on, so the
            // radio never waits.
            let fresh = self.recycled.try_recv().ok().or_else(|| spare.pop());
            queue.submit(RequestBuffer::reuse(
                fresh.unwrap_or_default(),
                self.transfer,
            ));
            match self.tx.try_send(done.data) {
                Ok(()) => {}
                Err(TrySendError::Full(b)) => {
                    self.overflows.fetch_add(1, Ordering::Relaxed);
                    spare.push(b);
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        // Cancelled transfers still come back through the queue.
        queue.cancel_all();
        while queue.pending() > 0 {
            if next_complete(&mut queue, Duration::from_millis(500)).is_none() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_future_that_never_finishes_times_out() {
        let t0 = Instant::now();
        assert!(
            block_on_timeout(std::future::pending::<()>(), Duration::from_millis(50)).is_none()
        );
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert_eq!(
            block_on_timeout(std::future::ready(7), Duration::from_secs(1)),
            Some(7)
        );
    }
}
