// made by CasualArclamp
//! Raw USB buffers to IQ, between a radio's reader thread and the engine
//! (from DecLAB's `raw_stream.rs`, DecDAB's `airspy_dsp` before it). Used by
//! the RTL-SDR front end.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use decdvbt_core::{Error, Iq, Result};

/// What turns a radio's raw USB bytes into IQ: here the RTL-SDR's plain
/// unsigned bytes (`rtlsdr_proto::U8ToIq`).
pub trait RawToIq: Send {
    /// Append the samples `bytes` make to `out`.
    fn process(&mut self, bytes: &[u8], out: &mut Vec<Iq>);
    /// Forget the past (filters start afresh), after a retune.
    fn reset(&mut self);
    fn output_rate(&self) -> f64;
}

/// Raw buffers from the radio's reader to the converter, and back again for
/// re-use: everything between the USB thread and the engine that needs no
/// USB.
pub struct RawStream<C: RawToIq> {
    rx: Receiver<Vec<u8>>,
    recycle: Sender<Vec<u8>>,
    conv: C,
    /// Buffers still to be thrown away after a retune.
    skip: u32,
    /// How long to wait for a buffer before calling the radio stalled.
    timeout: Duration,
    what: &'static str,
}

impl<C: RawToIq> RawStream<C> {
    pub fn new(
        rx: Receiver<Vec<u8>>,
        recycle: Sender<Vec<u8>>,
        conv: C,
        what: &'static str,
    ) -> Self {
        RawStream {
            rx,
            recycle,
            conv,
            skip: 0,
            timeout: Duration::from_secs(2),
            what,
        }
    }

    pub fn set_timeout(&mut self, t: Duration) {
        self.timeout = t;
    }

    pub fn sample_rate(&self) -> f64 {
        self.conv.output_rate()
    }

    /// The next block of IQ in `out` (cleared first).
    pub fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        out.clear();
        loop {
            match self.rx.recv_timeout(self.timeout) {
                Ok(bytes) => {
                    if self.skip > 0 {
                        self.skip -= 1;
                    } else {
                        self.conv.process(&bytes, out);
                    }
                    // Back to the reader; if it has gone nobody needs it.
                    let _ = self.recycle.send(bytes);
                    if !out.is_empty() {
                        return Ok(out.len());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::Other(format!(
                        "{} stopped delivering samples",
                        self.what
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Other(format!(
                        "{} reader stopped (unplugged?)",
                        self.what
                    )));
                }
            }
        }
    }

    /// After a retune: throw away what is queued and the next `skip` buffers
    /// (the radio was still filling them as it retuned), and start the
    /// filters afresh.
    pub fn restart(&mut self, skip: u32) {
        while let Ok(b) = self.rx.try_recv() {
            let _ = self.recycle.send(b);
        }
        self.skip = skip;
        self.conv.reset();
    }

    /// Empty the queue (the reader is stopping and may be waiting to hand
    /// over a buffer).
    pub fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }
}
