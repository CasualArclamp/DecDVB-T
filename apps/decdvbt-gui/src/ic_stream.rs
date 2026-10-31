// made by CasualArclamp
//! A stream from the interaction channel: the MPEG transport stream an
//! MHEG-5 application asked to play in place of the broadcast
//! ([`decdvbt_mheg::Request::IcStream`]; Freeview's internet channels, NHK
//! WORLD and the like), read over HTTP on its own thread and handed to the
//! player like a tap's packets.
//!
//! The player needs the stream's programme (its picture and sound PIDs)
//! before it starts, so the thread reads until the PAT and PMT have come,
//! keeping the packets, and then hands over the programme and a receiver
//! that gets those packets first and the rest as they arrive.
//!
//! The packets are handed over at the stream's own pace, by its PCR: the
//! server sends the first seconds in a burst, and the player, made for a
//! broadcast arriving as it is shown, would let the sound run seconds ahead
//! of the pictures and go by the wall clock. The burst waits here instead
//! (as a television's buffer holds it).
//!
//! Cost: one thread copying the stream (a couple of megabits) into 188-byte
//! packets; ffmpeg does the decoding, as for the broadcast.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

use decdvbt_ts::{Programme, TS_LEN, TsAnalyser};

/// Packets handed over at a time.
const BATCH: usize = 64;
/// How much is read for the PAT and PMT before giving up (a few seconds of
/// an SD stream).
const MAX_HEAD: usize = 8 << 20;

/// What the thread hands over once it knows the programme.
pub struct Ready {
    pub programme: Programme,
    pub packets: Receiver<Vec<[u8; TS_LEN]>>,
}

/// A stream being read; dropping it stops the thread.
pub struct IcStream {
    pub url: String,
    ready: Receiver<Result<Ready, String>>,
    quit: Arc<AtomicBool>,
    /// The programme (or the failure) has been handed over.
    done: bool,
}

impl Drop for IcStream {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

pub enum Poll {
    Waiting,
    Ready(Box<Ready>),
    Failed(String),
}

impl IcStream {
    pub fn start(url: &str) -> IcStream {
        let (tx, ready) = sync_channel(1);
        let quit = Arc::new(AtomicBool::new(false));
        let (u, q) = (url.to_string(), quit.clone());
        let spawned = std::thread::Builder::new()
            .name("decdvbt-ic-stream".into())
            .spawn(move || {
                if let Err(e) = read(&u, &q, &tx) {
                    let _ = tx.try_send(Err(e));
                }
            });
        if let Err(e) = spawned {
            let (tx, rx) = sync_channel(1);
            let _ = tx.send(Err(e.to_string()));
            return IcStream {
                url: url.to_string(),
                ready: rx,
                quit,
                done: false,
            };
        }
        IcStream {
            url: url.to_string(),
            ready,
            quit,
            done: false,
        }
    }

    /// Whether the programme is known yet: Ready or Failed once, then
    /// Waiting for good.
    pub fn poll(&mut self) -> Poll {
        if self.done {
            return Poll::Waiting;
        }
        let p = match self.ready.try_recv() {
            Ok(Ok(r)) => Poll::Ready(Box::new(r)),
            Ok(Err(e)) => Poll::Failed(e),
            Err(TryRecvError::Empty) => return Poll::Waiting,
            Err(TryRecvError::Disconnected) => Poll::Failed("the stream ended".into()),
        };
        self.done = true;
        p
    }
}

/// Packets out of a byte stream: whole 188-byte packets from a sync byte on,
/// finding the sync again after a slip.
struct Packetiser {
    buf: Vec<u8>,
}

impl Packetiser {
    fn push(&mut self, data: &[u8], out: &mut Vec<[u8; TS_LEN]>) {
        self.buf.extend_from_slice(data);
        let mut at = 0;
        while self.buf.len() - at >= TS_LEN {
            if self.buf[at] != 0x47 {
                at += 1;
                continue;
            }
            let mut p = [0u8; TS_LEN];
            p.copy_from_slice(&self.buf[at..at + TS_LEN]);
            out.push(p);
            at += TS_LEN;
        }
        self.buf.drain(..at);
    }
}

/// A packet's PCR, in 27 MHz ticks (ISO/IEC 13818-1 §2.4.3.5: 33 bits of
/// 90 kHz base and 9 bits of extension in the adaptation field).
fn pcr(p: &[u8; TS_LEN]) -> Option<u64> {
    let has_af = p[3] & 0x20 != 0;
    if !has_af || p[4] < 7 || p[5] & 0x10 == 0 {
        return None;
    }
    let b = &p[6..12];
    let base = (b[0] as u64) << 25
        | (b[1] as u64) << 17
        | (b[2] as u64) << 9
        | (b[3] as u64) << 1
        | (b[4] as u64) >> 7;
    let ext = ((b[4] as u64 & 1) << 8) | b[5] as u64;
    Some(base * 300 + ext)
}

/// Holds packets back to the stream's pace: a PCR of the programme's PCR
/// PID is sent when the wall clock reaches it (from the first one seen).
struct Pacer {
    pid: u16,
    anchor: Option<(std::time::Instant, u64)>,
}

impl Pacer {
    /// Wait until the newest PCR among `batch` is due.
    fn wait(&mut self, batch: &[[u8; TS_LEN]], quit: &AtomicBool) {
        let Some(t) = batch
            .iter()
            .filter(|p| ((p[1] as u16 & 0x1f) << 8 | p[2] as u16) == self.pid)
            .filter_map(pcr)
            .next_back()
        else {
            return;
        };
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let (at, t0) = *self.anchor.get_or_insert((now, t));
        // A PCR that went back (a wrap, a new programme), or one due more
        // than ten seconds on (a jump; a burst is a few seconds): the pace
        // starts again from it.
        let Some(ticks) = t.checked_sub(t0) else {
            self.anchor = Some((now, t));
            return;
        };
        let due = at + Duration::from_nanos(ticks * 1000 / 27);
        if due > now + Duration::from_secs(10) {
            self.anchor = Some((now, t));
            return;
        }
        while Instant::now() < due && !quit.load(Ordering::Relaxed) {
            std::thread::sleep((due - Instant::now()).min(Duration::from_millis(50)));
        }
    }
}

fn read(
    url: &str,
    quit: &AtomicBool,
    ready: &SyncSender<Result<Ready, String>>,
) -> Result<(), String> {
    let mut body = decdvbt_mheg::ic::open_stream(url)?;
    let mut pk = Packetiser { buf: Vec::new() };
    let mut an = TsAnalyser::new();
    let mut head: Vec<[u8; TS_LEN]> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    // Until the PAT and a PMT with streams have come.
    let programme = loop {
        if quit.load(Ordering::Relaxed) {
            return Ok(());
        }
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the stream ended before its programme was known".into());
        }
        let from = head.len();
        pk.push(&buf[..n], &mut head);
        for p in &head[from..] {
            an.packet(p);
        }
        if let Some(p) = an.programmes.values().find(|p| !p.streams.is_empty()) {
            break p.clone();
        }
        if head.len() * TS_LEN > MAX_HEAD {
            return Err("no programme in the stream".into());
        }
    };
    // Room for a second or so: the player's feed takes them as they come.
    let (tx, packets) = sync_channel(256);
    let mut pacer = programme.pcr_pid.map(|pid| Pacer { pid, anchor: None });
    ready
        .send(Ok(Ready { programme, packets }))
        .map_err(|_| "the player went away")?;
    for chunk in head.chunks(BATCH) {
        if let Some(p) = &mut pacer {
            p.wait(chunk, quit);
        }
        if tx.send(chunk.to_vec()).is_err() {
            return Ok(());
        }
    }
    let mut batch = Vec::with_capacity(BATCH);
    loop {
        if quit.load(Ordering::Relaxed) {
            return Ok(());
        }
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        pk.push(&buf[..n], &mut batch);
        if batch.len() >= BATCH {
            if let Some(p) = &mut pacer {
                p.wait(&batch, quit);
            }
            // The player dropped its end: stop.
            if tx.send(std::mem::take(&mut batch)).is_err() {
                return Ok(());
            }
            batch.reserve(BATCH);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_from_bytes() {
        let mut stream = vec![0x00, 0x12]; // junk before the first sync
        for i in 0..3u8 {
            let mut p = [i; TS_LEN];
            p[0] = 0x47;
            stream.extend_from_slice(&p);
        }
        let mut pk = Packetiser { buf: Vec::new() };
        let mut out = Vec::new();
        // In uneven pieces, as a socket gives them.
        for piece in stream.chunks(100) {
            pk.push(piece, &mut out);
        }
        assert_eq!(out.len(), 3);
        assert_eq!(out[2][1], 2);
        assert!(pk.buf.is_empty());
    }

    #[test]
    fn pcr_of_a_packet() {
        // PCR base 0x1_2345_6789 (33 bits), extension 0x123.
        let base: u64 = 0x1_2345_6789;
        let ext: u64 = 0x123;
        let mut p = [0xffu8; TS_LEN];
        p[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x30]);
        p[4] = 7; // adaptation field length
        p[5] = 0x10; // PCR flag
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = ((base & 1) << 7) as u8 | 0x7e | (ext >> 8) as u8;
        p[11] = ext as u8;
        assert_eq!(pcr(&p), Some(base * 300 + ext));
        p[5] = 0;
        assert_eq!(pcr(&p), None);
    }
}
