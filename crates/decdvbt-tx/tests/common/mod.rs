// made by CasualArclamp
//! Shared by the integration tests: clips made with ffmpeg's test sources,
//! sessions run to a number of seconds on a sink that is not a radio, and the
//! transport stream checks.
//!
//! **Nothing here can transmit**: every session uses the `Null` or `IqFile`
//! sink, nothing arms, and `DECDVBT_TX_NO_RADIO` is set for the process.

#![allow(dead_code)] // each test file uses some of this

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use decdvbt_core::dvbt::{Guard, Mode, Modulation, Rate};
use decdvbt_tx::config::{TxConfig, TxSink};
use decdvbt_tx::{TxSession, TxState, TxStatus, mux_rate_bps};

/// Never a radio, whatever a test does.
pub fn no_radio() {
    // SAFETY: tests only ever set it to the same value, before they start
    // threads that read it (and the library reads it only to refuse).
    unsafe { std::env::set_var(decdvbt_tx::radio::NO_RADIO_ENV, "1") };
}

/// Held by a test that counts threads, so no other test of the file starts or
/// ends one meanwhile.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// ffmpeg, or `None` (the test then says so and passes: a machine without it
/// cannot run these).
pub fn ffmpeg() -> Option<PathBuf> {
    no_radio();
    let f = decdvbt_tx::ffmpeg::find_ffmpeg(None);
    if f.is_none() {
        eprintln!("SKIPPED: ffmpeg is not installed");
    }
    f
}

/// A fresh, empty directory for one test.
pub fn test_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("decdvbt-tx-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A test clip from ffmpeg's `lavfi` sources: `testsrc2` (or random noise for a
/// hard one) and a sine, H.264 + AAC, `secs` long.
pub fn make_clip(
    ff: &Path,
    dir: &Path,
    name: &str,
    w: u32,
    h: u32,
    secs: u32,
    noise: bool,
) -> PathBuf {
    let path = dir.join(name);
    let video = if noise {
        format!("nullsrc=s={w}x{h}:r=25:d={secs},geq=random(1)*255:128:128")
    } else {
        format!("testsrc2=s={w}x{h}:r=25:d={secs}")
    };
    let st = Command::new(ff)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(video)
        .args(["-f", "lavfi", "-i"])
        .arg(format!("sine=frequency=440:sample_rate=48000:d={secs}"))
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "26",
            "-g",
            "25",
        ])
        .args([
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-shortest",
        ])
        .arg(&path)
        .stdin(Stdio::null())
        .status()
        .expect("run ffmpeg");
    assert!(st.success(), "ffmpeg could not make the test clip");
    path
}

/// The three modes of the video scout's runs.
pub fn mode_a() -> Mode {
    mode(Modulation::Qam64, Rate::R2_3, Guard::G1_32)
}
pub fn mode_b() -> Mode {
    mode(Modulation::Qam16, Rate::R1_2, Guard::G1_32)
}
pub fn mode_c() -> Mode {
    mode(Modulation::Qpsk, Rate::R1_2, Guard::G1_8)
}

pub fn mode(m: Modulation, r: Rate, g: Guard) -> Mode {
    let mut mode = Mode::uk(m, r);
    mode.guard = g;
    mode
}

/// Poll until `want` holds for the state (or fail after `limit`).
pub fn wait_for(s: &mut TxSession, limit: Duration, want: impl Fn(&TxStatus) -> bool) -> TxStatus {
    let t = Instant::now();
    loop {
        let st = s.status();
        if want(&st) {
            return st;
        }
        assert!(
            t.elapsed() < limit,
            "waited {limit:?}; state {:?}, error {:?}, log {:?}",
            st.state,
            st.error,
            st.ffmpeg_log
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Prepare, transmit on `cfg`'s sink, run until `signal_s` seconds of signal
/// have been sent (or the file ends), and stop. Returns the last status before
/// the stop. `dump`: where to write the packets the modulator was given.
pub fn run(cfg: TxConfig, signal_s: f64, dump: Option<&Path>) -> TxStatus {
    assert!(!cfg.sink.is_radio(), "tests never use the radio");
    let mut s = TxSession::new(cfg).unwrap();
    s.set_ts_dump(dump.map(Path::to_path_buf));
    s.prepare().unwrap();
    wait_for(&mut s, Duration::from_secs(60), |st| {
        matches!(st.state, TxState::Ready | TxState::Failed)
    });
    let st = s.status();
    assert_eq!(
        st.state,
        TxState::Ready,
        "{:?} {:?}",
        st.error,
        st.ffmpeg_log
    );
    s.transmit().unwrap();
    let st = wait_for(&mut s, Duration::from_secs(120), |st| {
        st.sent_s >= signal_s || matches!(st.state, TxState::Finished | TxState::Failed)
    });
    s.stop();
    st
}

/// A flat-out `Null` sink config for `input` in `mode`.
pub fn null_config(input: &Path, mode: Mode) -> TxConfig {
    let mut cfg = TxConfig::new(input.to_path_buf());
    cfg.mode = mode;
    cfg.loop_input = false;
    cfg.sink = TxSink::Null { paced: false };
    cfg.preview = false;
    cfg
}

/// What the transport stream checks find.
#[derive(Debug, Default)]
pub struct Analysis {
    pub packets: usize,
    pub bad_sync: usize,
    /// Continuity errors by PID (the null PID left out).
    pub cc_errors: BTreeMap<u16, u64>,
    /// Packets between the first and last PCR that are null.
    pub null_fraction: f64,
    /// PCR-derived stream rate against the mux rate, parts per million.
    pub pcr_ppm: f64,
    /// Longest time between two PCRs, ms.
    pub pcr_max_interval_ms: f64,
    /// PCR pairs whose PCR difference is not the packets' distance at the mux
    /// rate (more than 20 us off).
    pub pcr_jumps: usize,
    pub pcr_count: usize,
    /// Longest time between two packets of a PID, ms (PAT, PMT, SDT, NIT).
    pub max_gap_ms: BTreeMap<u16, f64>,
}

fn pcr_of(p: &[u8]) -> Option<u64> {
    let afc = (p[3] >> 4) & 3;
    if afc < 2 || p[4] < 7 || p[5] & 0x10 == 0 {
        return None;
    }
    let base = (u64::from(p[6]) << 25)
        | (u64::from(p[7]) << 17)
        | (u64::from(p[8]) << 9)
        | (u64::from(p[9]) << 1)
        | u64::from(p[10] >> 7);
    let ext = (u64::from(p[10] & 1) << 8) | u64::from(p[11]);
    Some(base * 300 + ext)
}

/// Check a transport stream made at `rate_bps` (the mux rate).
pub fn analyse(ts: &[u8], rate_bps: f64) -> Analysis {
    assert_eq!(ts.len() % 188, 0, "the stream is not whole packets");
    let mut a = Analysis {
        packets: ts.len() / 188,
        ..Default::default()
    };
    let mut an = decdvbt_ts::TsAnalyser::new();
    let pid_of = |p: &[u8]| (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
    let mut pcrs: Vec<(usize, u64)> = Vec::new();
    for (i, p) in ts.as_chunks::<188>().0.iter().enumerate() {
        if p[0] != 0x47 {
            a.bad_sync += 1;
            continue;
        }
        an.packet(p);
        if let Some(c) = pcr_of(p) {
            pcrs.push((i, c));
        }
    }
    for (pid, st) in &an.pids {
        if *pid != 0x1FFF && st.cc_errors > 0 {
            a.cc_errors.insert(*pid, st.cc_errors);
        }
    }
    a.pcr_count = pcrs.len();
    let pkt_s = 1504.0 / rate_bps;
    if pcrs.len() >= 2 {
        let (first, last) = (pcrs[0], pcrs[pcrs.len() - 1]);
        let measured = (last.0 - first.0) as f64 * 1504.0 / ((last.1 - first.1) as f64 / 27e6);
        a.pcr_ppm = (measured / rate_bps - 1.0) * 1e6;
        for w in pcrs.windows(2) {
            let by_position = (w[1].0 - w[0].0) as f64 * pkt_s;
            let by_pcr = (w[1].1 as f64 - w[0].1 as f64) / 27e6;
            a.pcr_max_interval_ms = a.pcr_max_interval_ms.max(by_pcr * 1000.0);
            if (by_position - by_pcr).abs() > 20e-6 {
                a.pcr_jumps += 1;
            }
        }
        // Null share and the intervals of the PSI, over the programme part.
        let region = &ts[first.0 * 188..(last.0 + 1) * 188];
        let (mut nulls, mut last_at) = (0usize, BTreeMap::<u16, usize>::new());
        for (j, p) in region.as_chunks::<188>().0.iter().enumerate() {
            let pid = pid_of(p);
            if pid == 0x1FFF {
                nulls += 1;
            }
            if let Some(&prev) = last_at.get(&pid) {
                let gap = (j - prev) as f64 * pkt_s * 1000.0;
                let e = a.max_gap_ms.entry(pid).or_insert(0.0);
                *e = e.max(gap);
            }
            last_at.insert(pid, j);
        }
        a.null_fraction = nulls as f64 / (region.len() / 188) as f64;
    }
    a
}

pub fn rate_of(mode: &Mode) -> f64 {
    mux_rate_bps(mode) as f64
}

/// Whether the machine has an encoder chain of this kind.
pub fn nvenc_works(ff: &Path) -> bool {
    decdvbt_tx::ffmpeg::detect_chain(ff, decdvbt_tx::config::EncoderChoice::Nvenc, None).is_ok()
}
