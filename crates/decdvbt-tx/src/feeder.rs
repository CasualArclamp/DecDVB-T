// made by CasualArclamp
//! The ffmpeg child process and the threads that read it.
//!
//! ```text
//! ffmpeg stdout --(big pipe)--> [tx-ts-read] --push_blocking--> TsRing
//! ffmpeg stderr --------------> [tx-ff-log]  --> FeederShared (progress, warnings)
//! ```
//!
//! The reader resynchronises on 0x47 (three in a row, 188 apart), pushes whole
//! packets, and blocks when the ring is full, which fills the pipe, which
//! blocks ffmpeg: **the radio's drain throttles the encoder**. ffmpeg is never
//! run with `-re`.
//!
//! Windows details taken from `decdvbt-video/src/lib.rs`: `CREATE_NO_WINDOW`,
//! normal priority (a below-normal ffmpeg starved), and a 4 MiB `CreatePipe`
//! for stdout (the default Windows pipe holds 4 KB).

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_core::{Error, Result};
use decdvbt_mod::ts::{TS_LEN, TS_SYNC};

use crate::ffmpeg::no_console;
use crate::ring::TsRing;

/// What the feeder knows, shared with the session's status.
#[derive(Debug, Default)]
pub struct FeederShared {
    /// Bytes read from ffmpeg (a rate is derived by the session).
    pub bytes_in: AtomicU64,
    /// Packets dropped while resynchronising.
    pub resync_losses: AtomicU64,
    /// ffmpeg's `[mpegts] dts < pcr, TS is invalid` count: the stream is
    /// over budget (the video bitrate is too high for the mode).
    pub dts_pcr_warnings: AtomicU64,
    /// ffmpeg's `-progress` figures, microseconds of programme done.
    pub out_time_us: AtomicU64,
    /// ffmpeg's `speed=` as hundredths (1.50x is 150).
    pub speed_x100: AtomicU64,
    /// Frames ffmpeg reported dropping.
    pub drop_frames: AtomicU64,
    /// The pipe closed (ffmpeg exited or the file ended).
    pub ended: AtomicBool,
    /// The last stderr lines that were not progress, newest last (at most 20).
    pub log: Mutex<Vec<String>>,
    /// Set when ffmpeg exited non-zero, with why.
    pub error: Mutex<Option<String>>,
    /// The child's process id (0 before it is spawned).
    pub pid: AtomicU32,
}

/// Lines of ffmpeg's log kept for the GUI.
const LOG_KEEP: usize = 20;
/// Bytes read from the pipe at a time.
const READ_CHUNK: usize = 128 * 1024;
/// Room in the pipe ffmpeg writes to, bytes (the Windows default is 4 KB).
#[cfg(windows)]
const PIPE_BYTES: u32 = 4 << 20;

/// A running ffmpeg. Dropping it kills the child and joins the threads.
#[non_exhaustive]
pub struct Feeder {
    pub shared: Arc<FeederShared>,
    child: Arc<Mutex<Child>>,
    ring: Arc<TsRing>,
    stopping: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    cpu_last: (f64, Instant),
    cpu_pct: f32,
}

impl Feeder {
    /// Start ffmpeg with `args` and feed `ring`. Returns once the process is
    /// spawned; it fails if ffmpeg cannot be started.
    pub fn spawn(ffmpeg: PathBuf, args: Vec<String>, ring: Arc<TsRing>) -> Result<Feeder> {
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(&args).stdin(Stdio::null()).stderr(Stdio::piped());
        no_console(&mut cmd);
        let pipe = output_pipe(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Other(format!("cannot start {}: {e}", ffmpeg.display())))?;
        // The command still holds our copy of the pipe's write end; ffmpeg
        // closing its copy must read as end of file, so drop ours now.
        drop(cmd);
        let stdout: Box<dyn Read + Send> = match pipe {
            Some(p) => p,
            None => Box::new(child.stdout.take().expect("stdout was piped")),
        };
        let stderr = child.stderr.take().expect("stderr was piped");

        let shared = Arc::new(FeederShared::default());
        shared.pid.store(child.id(), Ordering::Relaxed);
        let stopping = Arc::new(AtomicBool::new(false));
        let log_done = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(child));

        let log_thread = {
            let (shared, log_done) = (shared.clone(), log_done.clone());
            std::thread::Builder::new()
                .name("tx-ff-log".into())
                .spawn(move || {
                    read_log(stderr, &shared);
                    log_done.store(true, Ordering::Release);
                })
                .map_err(|e| Error::Other(format!("cannot start the ffmpeg log thread: {e}")))?
        };
        let ts_thread = {
            let (shared, ring, stopping, child) = (
                shared.clone(),
                ring.clone(),
                stopping.clone(),
                child.clone(),
            );
            std::thread::Builder::new()
                .name("tx-ts-read".into())
                .spawn(move || {
                    read_ts(stdout, &ring, &shared);
                    finish(&child, &shared, &stopping, &log_done);
                    // After the exit status is known, so a session that sees
                    // the ring finished also sees the error if there was one.
                    if !stopping.load(Ordering::Acquire) {
                        ring.close();
                    }
                })
                .map_err(|e| Error::Other(format!("cannot start the ffmpeg reader: {e}")))?
        };
        Ok(Feeder {
            shared,
            child,
            ring,
            stopping,
            threads: vec![ts_thread, log_thread],
            cpu_last: (0.0, Instant::now()),
            cpu_pct: 0.0,
        })
    }

    /// ffmpeg's CPU use as a percentage of one core, over the last interval
    /// (its process times; 0 if unknown). Renewed about once a second.
    pub fn ffmpeg_cpu_pct(&mut self) -> f32 {
        let dt = self.cpu_last.1.elapsed().as_secs_f64();
        if dt >= 1.0 {
            let cpu = child_cpu_seconds(&self.child.lock().unwrap());
            if self.cpu_last.0 > 0.0 || cpu > 0.0 {
                self.cpu_pct = (100.0 * (cpu - self.cpu_last.0) / dt).max(0.0) as f32;
            }
            self.cpu_last = (cpu, Instant::now());
        }
        self.cpu_pct
    }

    /// Kill the child and wait for the threads (also done on drop).
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        // A reader blocked on a full ring wakes with `false`; one blocked on
        // the pipe wakes when the child dies.
        self.ring.abort();
        {
            let mut c = self.child.lock().unwrap();
            let _ = c.kill();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let _ = self.child.lock().unwrap().wait();
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// ffmpeg's output pipe. A pipe from `Stdio::piped()` holds 4 KB on Windows, so
/// a 24 Mbit/s stream crosses it in 750 round trips a second between the two
/// processes. A pipe made here holds 4 MiB: ffmpeg runs ahead by up to that,
/// the reader takes it in 128 KiB reads, and it fills (stalling ffmpeg) only
/// when the ring is full too. `None`: the pipe could not be made (or this is
/// not Windows) and the command's own is used.
#[cfg(windows)]
fn output_pipe(cmd: &mut Command) -> Option<Box<dyn Read + Send>> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    let (mut read, mut write) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: both out-pointers are valid for a handle; no security attributes
    // are given (the handles are not inheritable; `Stdio::from` makes the
    // child's copy so).
    let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), PIPE_BYTES) };
    if ok == 0 {
        cmd.stdout(Stdio::piped());
        return None;
    }
    // SAFETY: both handles were just made and nothing else owns them.
    let (read, write) = unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    };
    cmd.stdout(Stdio::from(write));
    Some(Box::new(std::fs::File::from(read)))
}

#[cfg(not(windows))]
fn output_pipe(cmd: &mut Command) -> Option<Box<dyn Read + Send>> {
    cmd.stdout(Stdio::piped());
    None
}

/// The child's CPU seconds (user + kernel) so far; 0 if it cannot be read.
#[cfg(windows)]
fn child_cpu_seconds(child: &Child) -> f64 {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the handle belongs to `child`, which the caller holds, and the
    // four pointers are to live locals the call fills in.
    let ok = unsafe {
        GetProcessTimes(
            child.as_raw_handle(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return 0.0;
    }
    let ticks = |t: FILETIME| (((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64) as f64;
    (ticks(kernel) + ticks(user)) * 1e-7
}

#[cfg(not(windows))]
fn child_cpu_seconds(child: &Child) -> f64 {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", child.id())) else {
        return 0.0;
    };
    let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
        return 0.0;
    };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tick = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (tick(11) + tick(12)) / 100.0
}

/// Index of the first byte that starts three packets in a row (0x47 at i,
/// i+188 and i+376), or `None`.
fn find_sync(buf: &[u8]) -> Option<usize> {
    let last = buf.len().checked_sub(2 * TS_LEN + 1)?;
    (0..=last).find(|&i| {
        buf[i] == TS_SYNC && buf[i + TS_LEN] == TS_SYNC && buf[i + 2 * TS_LEN] == TS_SYNC
    })
}

/// How many leading bytes of `acc` are whole packets in sync. Bytes before a
/// sync are dropped (and counted in `lost`); a bad sync byte inside the run
/// ends it and the next call resynchronises. Call until it returns 0.
fn whole_packets(acc: &mut Vec<u8>, synced: &mut bool, lost: &mut u64) -> usize {
    loop {
        if *synced {
            let mut n = 0;
            while n + TS_LEN <= acc.len() && acc[n] == TS_SYNC {
                n += TS_LEN;
            }
            if n > 0 {
                if n + TS_LEN <= acc.len() {
                    *synced = false; // a bad byte follows the run
                }
                return n;
            }
            if acc.len() < TS_LEN {
                return 0;
            }
            *synced = false;
        }
        match find_sync(acc) {
            Some(i) => {
                if i > 0 {
                    *lost += i.div_ceil(TS_LEN) as u64;
                    acc.drain(..i);
                }
                *synced = true;
            }
            None => {
                // Keep a tail that a sync could still start in.
                let keep = (3 * TS_LEN - 1).min(acc.len());
                let cut = acc.len() - keep;
                if cut > 0 {
                    *lost += cut.div_ceil(TS_LEN) as u64;
                    acc.drain(..cut);
                }
                return 0;
            }
        }
    }
}

/// Read ffmpeg's stdout into the ring until the pipe closes or the ring is
/// aborted.
fn read_ts(mut src: impl Read, ring: &TsRing, shared: &FeederShared) {
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut acc: Vec<u8> = Vec::with_capacity(READ_CHUNK + 4 * TS_LEN);
    let (mut synced, mut lost) = (false, 0u64);
    loop {
        let n = match src.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        shared.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
        acc.extend_from_slice(&chunk[..n]);
        loop {
            let take = whole_packets(&mut acc, &mut synced, &mut lost);
            if lost > 0 {
                shared.resync_losses.fetch_add(lost, Ordering::Relaxed);
                lost = 0;
            }
            if take == 0 {
                break;
            }
            if !ring.push_blocking(&acc[..take]) {
                return; // aborted
            }
            acc.drain(..take);
        }
    }
}

/// The pipe has closed: learn how ffmpeg ended, and say so.
fn finish(
    child: &Mutex<Child>,
    shared: &FeederShared,
    stopping: &AtomicBool,
    log_done: &AtomicBool,
) {
    let t0 = Instant::now();
    let status = loop {
        {
            let mut c = child.lock().unwrap();
            match c.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if t0.elapsed() > Duration::from_secs(5) => {
                    // Closed its output and will not exit: end it.
                    let _ = c.kill();
                    break c.wait().ok();
                }
                Ok(None) => {}
                Err(_) => break None,
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Let the log thread read the last lines before the message is made.
    let t1 = Instant::now();
    while !log_done.load(Ordering::Acquire) && t1.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !stopping.load(Ordering::Acquire)
        && let Some(s) = status
        && !s.success()
    {
        let tail: Vec<String> = shared
            .log
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(6)
            .rev()
            .cloned()
            .collect();
        *shared.error.lock().unwrap() = Some(format!("ffmpeg stopped ({s}): {}", tail.join(" | ")));
    }
    shared.ended.store(true, Ordering::Release);
}

/// ffmpeg's stderr: `-progress` key=value lines and its warnings.
fn read_log(err: impl Read, shared: &FeederShared) {
    let mut last_was_dts = false;
    for line in BufReader::new(err).split(b'\n') {
        let Ok(line) = line else { break };
        let line = String::from_utf8_lossy(&line);
        parse_log_line(line.trim_end(), shared, &mut last_was_dts);
    }
}

/// Keys `-progress` writes (and `stream_N_N_q`).
fn is_progress_key(k: &str) -> bool {
    matches!(
        k,
        "frame"
            | "fps"
            | "bitrate"
            | "total_size"
            | "out_time_us"
            | "out_time_ms"
            | "out_time"
            | "dup_frames"
            | "drop_frames"
            | "speed"
            | "progress"
    ) || k.starts_with("stream_")
}

/// One line of ffmpeg's stderr to the shared figures.
fn parse_log_line(line: &str, shared: &FeederShared, last_was_dts: &mut bool) {
    if line.is_empty() {
        return;
    }
    if let Some((k, v)) = line.split_once('=')
        && is_progress_key(k)
    {
        match k {
            "out_time_us" => {
                if let Ok(us) = v.trim().parse::<u64>() {
                    shared.out_time_us.store(us, Ordering::Relaxed);
                }
            }
            "speed" => {
                if let Ok(x) = v.trim().trim_end_matches('x').trim().parse::<f64>() {
                    shared
                        .speed_x100
                        .store((x * 100.0) as u64, Ordering::Relaxed);
                }
            }
            "drop_frames" => {
                if let Ok(n) = v.trim().parse::<u64>() {
                    shared.drop_frames.store(n, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        return;
    }
    // ffmpeg folds identical warnings into "Last message repeated N times".
    if let Some(rest) = line.strip_prefix("Last message repeated ") {
        if *last_was_dts
            && let Some(n) = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
        {
            shared.dts_pcr_warnings.fetch_add(n, Ordering::Relaxed);
        }
        return;
    }
    *last_was_dts = line.contains("dts < pcr");
    if *last_was_dts {
        shared.dts_pcr_warnings.fetch_add(1, Ordering::Relaxed);
    }
    let mut log = shared.log.lock().unwrap();
    if log.len() >= LOG_KEEP {
        log.remove(0);
    }
    log.push(line.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(n: usize, first: u8) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..n {
            let mut p = [first.wrapping_add(i as u8); TS_LEN];
            p[0] = TS_SYNC;
            v.extend_from_slice(&p);
        }
        v
    }

    #[test]
    fn whole_packets_are_taken_and_a_partial_one_waits() {
        let mut acc = packets(5, 1);
        acc.extend_from_slice(&[TS_SYNC, 1, 2, 3]); // the start of a sixth
        let (mut synced, mut lost) = (false, 0);
        assert_eq!(whole_packets(&mut acc, &mut synced, &mut lost), 5 * TS_LEN);
        acc.drain(..5 * TS_LEN);
        assert_eq!(whole_packets(&mut acc, &mut synced, &mut lost), 0);
        assert_eq!(lost, 0);
        assert_eq!(acc.len(), 4);
    }

    #[test]
    fn it_resynchronises_after_garbage() {
        let mut acc = vec![0x12u8; 100];
        acc.extend(packets(6, 9));
        let (mut synced, mut lost) = (false, 0);
        let n = whole_packets(&mut acc, &mut synced, &mut lost);
        assert_eq!(n, 6 * TS_LEN);
        assert_eq!(lost, 1);
        // A byte slipped in mid-stream: the run up to it is delivered, then
        // the next call finds the sync again.
        let mut acc = packets(4, 1);
        acc.insert(2 * TS_LEN, 0xAA);
        acc.extend(packets(4, 50));
        let (mut synced, mut lost) = (true, 0);
        let mut got = Vec::new();
        loop {
            let n = whole_packets(&mut acc, &mut synced, &mut lost);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&acc[..n]);
            acc.drain(..n);
        }
        assert_eq!(got.len() % TS_LEN, 0);
        assert!(got.chunks(TS_LEN).all(|p| p[0] == TS_SYNC));
        assert!(got.len() >= 6 * TS_LEN, "{}", got.len());
        assert!(lost >= 1);
    }

    #[test]
    fn a_chopped_stream_comes_through_in_order() {
        let data = packets(200, 0);
        let ring = TsRing::new(300);
        let shared = FeederShared::default();
        // A reader that returns odd-sized pieces, as a pipe does.
        struct Chop(Vec<u8>, usize);
        impl Read for Chop {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = (self.1 % 997 + 1).min(buf.len()).min(self.0.len());
                self.1 += 313;
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0.drain(..n);
                Ok(n)
            }
        }
        read_ts(Chop(data.clone(), 7), &ring, &shared);
        let mut out = vec![0u8; 200 * TS_LEN];
        assert_eq!(ring.pop(&mut out), 200);
        assert_eq!(out, data);
        assert_eq!(shared.resync_losses.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_log_is_parsed() {
        let shared = FeederShared::default();
        let mut dts = false;
        for l in [
            "frame=100",
            "fps=50.0",
            "stream_0_0_q=-1.0",
            "out_time_us=4000000",
            "out_time=00:00:04.000000",
            "drop_frames=3",
            "speed=1.50x",
            "progress=continue",
            "[mpegts @ 000001] dts < pcr, TS is invalid",
            "Last message repeated 4 times",
            "[aac @ 0000] something else",
            "speed=N/A",
        ] {
            parse_log_line(l, &shared, &mut dts);
        }
        assert_eq!(shared.out_time_us.load(Ordering::Relaxed), 4_000_000);
        assert_eq!(shared.speed_x100.load(Ordering::Relaxed), 150);
        assert_eq!(shared.drop_frames.load(Ordering::Relaxed), 3);
        assert_eq!(shared.dts_pcr_warnings.load(Ordering::Relaxed), 5);
        assert_eq!(shared.log.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_log_keeps_the_last_twenty_lines() {
        let shared = FeederShared::default();
        let mut dts = false;
        for i in 0..30 {
            parse_log_line(&format!("warning {i}"), &shared, &mut dts);
        }
        let log = shared.log.lock().unwrap();
        assert_eq!(log.len(), LOG_KEEP);
        assert_eq!(log.last().unwrap(), "warning 29");
    }

    #[test]
    fn a_missing_ffmpeg_is_an_error_not_a_panic() {
        let ring = Arc::new(TsRing::new(10));
        let r = Feeder::spawn("definitely-not-ffmpeg-here".into(), vec![], ring);
        assert!(r.is_err());
    }
}
