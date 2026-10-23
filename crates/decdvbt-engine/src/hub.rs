// made by CasualArclamp
//! The transport stream after the receiver: the analyser behind the service
//! panel and the TS viewer, the outputs (a `.ts` file, UDP, an HTTP/TCP
//! server) and the tap the in-app player reads.
//!
//! It runs on a thread of its own and the receive thread only hands it
//! batches of packets, so none of it can make the receive thread late (a
//! HackRF that is not read in time drops samples). What the thread has not
//! taken by [`MAX_QUEUED`] bytes is dropped and counted, never waited for.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use decdvbt_ts::backlog::Backlog;
use decdvbt_ts::{Epg, TS_LEN, TcpSink, TsAnalyser, TsFile, TsReport, TsTap, UdpSink};

/// Bytes waiting for the thread: about four seconds of a UK multiplex.
const MAX_QUEUED: usize = 12 * 1024 * 1024;
/// The analyser's report is rebuilt this often, and only while someone looks.
const REPORT_EVERY: Duration = Duration::from_millis(250);
/// The guide is copied out this often at most, and only while someone looks.
const EPG_EVERY: Duration = Duration::from_secs(2);
/// "Someone looks" means a view was asked for this recently.
const WATCH_FOR: Duration = Duration::from_secs(2);

/// Where the stream goes besides the analyser.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outputs {
    /// Write a `.ts` file into `dir` (named from `name` and the time).
    pub record: bool,
    pub dir: PathBuf,
    pub name: String,
    pub udp: Option<SocketAddr>,
    pub tcp: Option<SocketAddr>,
    /// The in-app player's copy; it is handed the stream from the last
    /// keyframe of its video PID first.
    pub tap: Option<TsTap>,
}

/// What the hub tells the GUI.
#[derive(Debug, Clone, Default)]
pub struct TsView {
    /// Packets taken since the multiplex was tuned.
    pub packets: u64,
    pub rate_bps: f64,
    /// What the analyser makes of it (rebuilt four times a second).
    pub report: Arc<TsReport>,
    pub file: Option<(PathBuf, u64)>,
    pub file_active: bool,
    pub udp: Option<(SocketAddr, u64)>,
    pub tcp: Option<(SocketAddr, Vec<SocketAddr>)>,
    pub error: Option<String>,
    /// Packets lost between the receiver and the hub, and to the player.
    pub dropped: u64,
    pub tap_dropped: u64,
    /// The hub thread's busy time as a percentage of one core, over the last
    /// second or so.
    pub load_percent: f32,
}

enum Msg {
    Ts(Vec<u8>),
    /// A new multiplex: forget the tables and the guide.
    Reset,
    Outputs(Outputs),
    Stop,
}

struct Shared {
    view: Mutex<TsView>,
    epg: Mutex<Arc<Epg>>,
    /// When the GUI last asked for the view and the guide.
    watched: Mutex<(Instant, Instant)>,
    queued: AtomicUsize,
    dropped: AtomicU64,
    /// `load_percent` in thousandths.
    load: AtomicU64,
}

/// The receive thread's end: cheap to clone, sends only.
#[derive(Clone)]
pub struct Feeder {
    tx: Sender<Msg>,
    sh: Arc<Shared>,
}

impl Feeder {
    /// Hand over decoded transport packets (a whole number of them).
    pub fn feed(&self, ts: Vec<u8>) {
        if ts.is_empty() {
            return;
        }
        if self.sh.queued.load(Ordering::Relaxed) + ts.len() > MAX_QUEUED {
            self.sh
                .dropped
                .fetch_add((ts.len() / TS_LEN) as u64, Ordering::Relaxed);
            return;
        }
        self.sh.queued.fetch_add(ts.len(), Ordering::Relaxed);
        let _ = self.tx.send(Msg::Ts(ts));
    }

    /// The stream is a different multiplex from here on.
    pub fn reset(&self) {
        let _ = self.tx.send(Msg::Reset);
    }
}

/// The hub's thread and the GUI's end.
pub struct TsHub {
    tx: Sender<Msg>,
    sh: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl TsHub {
    pub fn start() -> TsHub {
        let now = Instant::now();
        let sh = Arc::new(Shared {
            view: Mutex::new(TsView::default()),
            epg: Mutex::new(Arc::new(Epg::new())),
            watched: Mutex::new((now - WATCH_FOR * 2, now - WATCH_FOR * 2)),
            queued: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            load: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let shared = Arc::clone(&sh);
        let join = std::thread::Builder::new()
            .name("decdvbt-ts".into())
            .spawn(move || run(rx, shared))
            .expect("cannot start the transport stream thread");
        TsHub {
            tx,
            sh,
            join: Some(join),
        }
    }

    pub fn feeder(&self) -> Feeder {
        Feeder {
            tx: self.tx.clone(),
            sh: Arc::clone(&self.sh),
        }
    }

    pub fn set_outputs(&self, o: Outputs) {
        let _ = self.tx.send(Msg::Outputs(o));
    }

    /// What the hub makes of the stream; asking for it is what keeps its
    /// report being built.
    pub fn view(&self) -> TsView {
        self.sh.watched.lock().unwrap().0 = Instant::now();
        self.sh.view.lock().unwrap().clone()
    }

    /// The programme guide as of a couple of seconds ago.
    pub fn epg(&self) -> Arc<Epg> {
        self.sh.watched.lock().unwrap().1 = Instant::now();
        Arc::clone(&self.sh.epg.lock().unwrap())
    }
}

impl Drop for TsHub {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The thread's state.
struct Stage {
    analyser: TsAnalyser,
    backlog: Backlog,
    file: Option<TsFile>,
    file_path: Option<PathBuf>,
    udp: Option<UdpSink>,
    tcp: Option<TcpSink>,
    udp_want: Option<SocketAddr>,
    tcp_want: Option<SocketAddr>,
    tap: Option<TsTap>,
    error: Option<String>,
    packets: u64,
    report: Arc<TsReport>,
    report_at: Instant,
    epg_at: Instant,
    epg_version: u64,
}

impl Stage {
    fn new() -> Stage {
        let now = Instant::now();
        Stage {
            analyser: TsAnalyser::new(),
            backlog: Backlog::new(),
            file: None,
            file_path: None,
            udp: None,
            tcp: None,
            udp_want: None,
            tcp_want: None,
            tap: None,
            error: None,
            packets: 0,
            report: Arc::new(TsReport::default()),
            report_at: now - REPORT_EVERY,
            epg_at: now - EPG_EVERY,
            epg_version: u64::MAX,
        }
    }

    /// A new multiplex: the tables and the guide start over; the outputs go
    /// on.
    fn reset(&mut self) {
        self.analyser = TsAnalyser::new();
        self.backlog = Backlog::new();
        self.packets = 0;
        self.report = Arc::new(TsReport::default());
        self.report_at = Instant::now() - REPORT_EVERY;
        self.epg_version = u64::MAX;
    }

    /// Open, close or move the outputs to match `o`.
    fn follow(&mut self, o: &Outputs) {
        if self.tap != o.tap {
            self.tap = o.tap.clone();
            // A player starting: the stream from its last keyframe first,
            // so its picture (and sound) need not wait for the next.
            if let Some(t) = &self.tap {
                t.send(&self.backlog.since_keyframe(t.video()));
            }
        }
        match (o.record, self.file.is_some()) {
            (true, false) => {
                let stamp = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let path = o.dir.join(format!(
                    "decdvbt-{}-{stamp}.ts",
                    o.name.replace([' ', '/', '\\', ':'], "_")
                ));
                let _ = std::fs::create_dir_all(&o.dir);
                match TsFile::create(&path) {
                    Ok(f) => self.file = Some(f),
                    Err(e) => self.error = Some(format!("{}: {e}", path.display())),
                }
                self.file_path = Some(path);
            }
            (false, true) => {
                if let Some(mut f) = self.file.take() {
                    let _ = f.flush();
                }
            }
            _ => {}
        }
        if o.udp != self.udp_want {
            self.udp_want = o.udp;
            self.udp = None;
            if let Some(a) = o.udp {
                match UdpSink::new(a) {
                    Ok(u) => self.udp = Some(u),
                    Err(e) => self.error = Some(format!("UDP {a}: {e}")),
                }
            }
        }
        if o.tcp != self.tcp_want {
            self.tcp_want = o.tcp;
            self.tcp = None; // closes the old server and its clients
            if let Some(a) = o.tcp {
                match TcpSink::bind(a) {
                    Ok(t) => self.tcp = Some(t),
                    Err(e) => self.error = Some(format!("TCP {a}: {e}")),
                }
            }
        }
    }

    fn packets(&mut self, bytes: &[u8]) {
        let pkts: &[[u8; TS_LEN]] = bytes.as_chunks::<TS_LEN>().0;
        for p in pkts {
            self.analyser.packet(p);
            self.backlog.push(p);
        }
        self.packets += pkts.len() as u64;
        if let Some(f) = &mut self.file
            && let Err(e) = f.write(pkts)
        {
            self.error = Some(e.to_string());
            self.file = None;
        }
        if let Some(u) = &mut self.udp {
            u.write(pkts);
        }
        if let Some(t) = &mut self.tcp {
            t.write(pkts);
        }
        if let Some(t) = &self.tap {
            t.send(pkts);
        }
    }

    fn publish(&mut self, sh: &Shared) {
        let (view_seen, epg_seen) = *sh.watched.lock().unwrap();
        if view_seen.elapsed() < WATCH_FOR && self.report_at.elapsed() >= REPORT_EVERY {
            self.report = Arc::new(self.analyser.report());
            self.report_at = Instant::now();
        }
        if epg_seen.elapsed() < WATCH_FOR
            && self.epg_at.elapsed() >= EPG_EVERY
            && self.analyser.epg.version != self.epg_version
        {
            self.analyser.epg.prune();
            self.epg_version = self.analyser.epg.version;
            self.epg_at = Instant::now();
            *sh.epg.lock().unwrap() = Arc::new(self.analyser.epg.clone());
        }
        *sh.view.lock().unwrap() = TsView {
            packets: self.packets,
            rate_bps: self.analyser.rate_bps(),
            report: Arc::clone(&self.report),
            file: self
                .file_path
                .clone()
                .map(|p| (p, self.file.as_ref().map_or(0, |f| f.packets))),
            file_active: self.file.is_some(),
            udp: self.udp.as_ref().map(|u| (u.target, u.datagrams)),
            tcp: self.tcp.as_ref().map(|t| (t.addr, t.clients())),
            error: self.error.clone(),
            dropped: sh.dropped.load(Ordering::Relaxed),
            tap_dropped: self.tap.as_ref().map_or(0, TsTap::dropped),
            load_percent: sh.load.load(Ordering::Relaxed) as f32 / 1000.0,
        };
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
    }
}

fn run(rx: Receiver<Msg>, sh: Arc<Shared>) {
    let mut st = Stage::new();
    let mut last = Instant::now();
    // Time spent working, against the time passed, for the load figure.
    let (mut busy, mut since) = (Duration::ZERO, Instant::now());
    loop {
        let msg = rx.recv_timeout(Duration::from_millis(100));
        let t0 = Instant::now();
        match msg {
            Ok(Msg::Ts(bytes)) => {
                sh.queued.fetch_sub(bytes.len(), Ordering::Relaxed);
                st.packets(&bytes);
            }
            Ok(Msg::Reset) => st.reset(),
            Ok(Msg::Outputs(o)) => st.follow(&o),
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let dt = last.elapsed();
        if dt >= Duration::from_millis(100) {
            last = Instant::now();
            st.analyser.tick(dt.as_secs_f64());
            st.publish(&sh);
        }
        busy += t0.elapsed();
        if since.elapsed() >= Duration::from_secs(1) {
            let pct = 100.0 * busy.as_secs_f64() / since.elapsed().as_secs_f64();
            sh.load.store((pct * 1000.0) as u64, Ordering::Relaxed);
            (busy, since) = (Duration::ZERO, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_ts::EsInfo;
    use decdvbt_ts::psi::{section_packet, test_tables};

    fn stream() -> Vec<u8> {
        let mut ts = Vec::new();
        let tables = test_tables(7, 0x1000, EsInfo::new(0x100, 0x1B), "DecDVB-T", "Test card");
        for (k, (pid, s)) in tables.iter().enumerate() {
            ts.extend_from_slice(&section_packet(*pid, k as u8, s));
        }
        ts
    }

    /// The tables reach the analyser, the tap and the file, and a reset
    /// starts the tables over.
    #[test]
    fn the_hub_analyses_taps_and_records() {
        let hub = TsHub::start();
        let (tap, rx) = TsTap::new(64);
        let dir = std::env::temp_dir().join("decdvbt-hub-test");
        let _ = std::fs::remove_dir_all(&dir);
        hub.set_outputs(Outputs {
            record: true,
            dir: dir.clone(),
            name: "test mux".into(),
            tap: Some(tap),
            ..Default::default()
        });
        let ts = stream();
        let feeder = hub.feeder();
        feeder.feed(ts.clone());
        let t0 = Instant::now();
        let mut view = hub.view();
        while view.report.programmes.is_empty() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
            view = hub.view();
        }
        assert_eq!(view.packets, (ts.len() / TS_LEN) as u64);
        assert_eq!(view.report.programmes[0].name.as_deref(), Some("Test card"));
        let tapped: usize = rx.try_iter().map(|b| b.len()).sum();
        assert_eq!(tapped, ts.len() / TS_LEN);
        let (path, written) = view.file.clone().unwrap();
        assert_eq!(written, (ts.len() / TS_LEN) as u64);

        feeder.reset();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(hub.view().packets, 0);
        drop(feeder);
        hub.set_outputs(Outputs::default());
        drop(hub);
        assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, ts.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
