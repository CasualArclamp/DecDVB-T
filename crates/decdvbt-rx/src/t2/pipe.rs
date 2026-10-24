// made by CasualArclamp
//! The FEC stage on a thread of its own. A frame's cells go to it as soon as
//! they are equalised, and the receive thread goes on to the next frame while
//! the blocks are deinterleaved, demapped and decoded (on as many threads as
//! asked). The transport packets come back a frame behind. At most two frames
//! wait: if the FEC stage cannot keep up, [`FecPipe::submit`] holds the
//! receive thread until it has caught up, which is the right thing to feel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_core::Iq;

use crate::t2::plp::{FrameStats, PlpDecoder, PlpParams};

/// Frames in the FEC stage's hands (decoding or queued) at most.
const MAX_IN_FLIGHT: usize = 2;

/// One frame's PLP cells for the FEC stage.
pub struct Job {
    pub params: PlpParams,
    pub cells: Vec<Iq>,
    pub var: Vec<f32>,
    /// Where the PLP's first block starts in `cells`, and how many blocks.
    pub start: usize,
    pub blocks: usize,
    /// How long a frame lasts: the time the stage has for one.
    pub period: Duration,
}

/// What a frame made.
pub struct Done {
    /// The transport packets, back to back.
    pub ts: Vec<u8>,
    pub stats: Option<FrameStats>,
    pub resyncs: u64,
    /// Threads the blocks were decoded on, and the wall time the frame took
    /// as a share of its period.
    pub threads: usize,
    pub load: f32,
    /// The job's cells and variances, to be used for a later frame (see
    /// [`crate::t2::frame::Demod::data_cells_into`]).
    pub buffers: (Vec<Iq>, Vec<f32>),
}

enum Msg {
    Frame(Job),
    /// The stream has jumped: forget a packet cut by the last frame.
    Reset,
}

pub struct FecPipe {
    tx: Option<Sender<Msg>>,
    rx: Receiver<Done>,
    join: Option<JoinHandle<()>>,
    in_flight: usize,
    /// Set when the pipe is dropped: frames still queued are not decoded.
    cancel: Arc<AtomicBool>,
}

impl FecPipe {
    pub fn new(threads: usize) -> FecPipe {
        let (tx, jobs) = channel();
        let (done_tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let join = std::thread::Builder::new()
            .name("decdvbt-t2-fec".into())
            .spawn(move || worker(threads, jobs, done_tx, &flag))
            .expect("cannot start the FEC thread");
        FecPipe {
            tx: Some(tx),
            rx,
            join: Some(join),
            in_flight: 0,
            cancel,
        }
    }

    /// Hand a frame over, waiting first while two are already in hand; with
    /// `wait` false the frame is given up instead (`None`: the FEC stage is
    /// behind, and holding the receive thread would lose the radio's samples,
    /// and the lock, rather than one frame's blocks). Returns every frame
    /// finished meanwhile.
    pub fn submit(&mut self, job: Job, wait: bool) -> (Vec<Done>, bool) {
        let mut done = self.poll();
        if !wait && self.in_flight >= MAX_IN_FLIGHT {
            return (done, false);
        }
        while self.in_flight >= MAX_IN_FLIGHT {
            match self.rx.recv() {
                Ok(d) => {
                    self.in_flight -= 1;
                    done.push(d);
                }
                Err(_) => {
                    self.in_flight = 0;
                    break;
                }
            }
        }
        if let Some(tx) = &self.tx
            && tx.send(Msg::Frame(job)).is_ok()
        {
            self.in_flight += 1;
        }
        (done, true)
    }

    /// The frames finished, without waiting.
    pub fn poll(&mut self) -> Vec<Done> {
        let mut done = Vec::new();
        while let Ok(d) = self.rx.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            done.push(d);
        }
        done
    }

    /// Wait for every frame in hand.
    pub fn flush(&mut self) -> Vec<Done> {
        let mut done = Vec::new();
        while self.in_flight > 0 {
            match self.rx.recv() {
                Ok(d) => {
                    self.in_flight -= 1;
                    done.push(d);
                }
                Err(_) => self.in_flight = 0,
            }
        }
        done
    }

    /// The stream has jumped (after the frames in hand).
    pub fn reset(&mut self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Reset);
        }
    }
}

impl Drop for FecPipe {
    fn drop(&mut self) {
        // Closing the channel ends the thread's loop, after the frame it is on.
        self.cancel.store(true, Ordering::Relaxed);
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The worker threads in use: as few as keep a frame's work to about half its
/// period, for threads running side by side slow each other (shared cache,
/// lower clocks), so that the same frames cost more CPU on four than on two.
fn adapt(threads: usize, max: usize, load: f32) -> usize {
    if load > 0.6 && threads < max {
        threads + 1
    } else if threads > 1 && load * (threads as f32 / (threads - 1) as f32) < 0.45 {
        threads - 1
    } else {
        threads
    }
}

fn worker(max_threads: usize, jobs: Receiver<Msg>, out: Sender<Done>, cancel: &AtomicBool) {
    let mut decoder: Option<PlpDecoder> = None;
    let mut packets = Vec::new();
    // Two to start with.
    let mut threads = 2.min(max_threads).max(1);
    for msg in jobs {
        let job = match msg {
            Msg::Reset => {
                if let Some(d) = &mut decoder {
                    d.reset();
                }
                continue;
            }
            Msg::Frame(_) if cancel.load(Ordering::Relaxed) => continue,
            Msg::Frame(job) => job,
        };
        if decoder.as_ref().map(|d| d.params) != Some(job.params) {
            decoder = PlpDecoder::new(job.params, max_threads);
        }
        let done = match &mut decoder {
            Some(d) => {
                packets.clear();
                let t0 = Instant::now();
                let stats = d.decode_frame(
                    &job.cells,
                    &job.var,
                    job.start,
                    job.blocks,
                    threads,
                    &mut packets,
                );
                let load = (t0.elapsed().as_secs_f64() / job.period.as_secs_f64().max(1e-3)) as f32;
                let used = threads;
                threads = adapt(threads, d.max_threads(), load);
                let mut ts = Vec::with_capacity(packets.len() * 188);
                for p in &packets {
                    ts.extend_from_slice(p);
                }
                Done {
                    ts,
                    stats,
                    resyncs: d.resyncs(),
                    threads: used,
                    load,
                    buffers: (job.cells, job.var),
                }
            }
            None => Done {
                ts: Vec::new(),
                stats: None,
                resyncs: 0,
                threads,
                load: 0.0,
                buffers: (job.cells, job.var),
            },
        };
        if out.send(done).is_err() {
            return;
        }
    }
}
