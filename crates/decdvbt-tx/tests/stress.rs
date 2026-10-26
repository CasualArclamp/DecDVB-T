// made by CasualArclamp
//! Failure and stress tests (docs/TX_DESIGN.md T11 and T12): ffmpeg dying or
//! stalling mid-run (a stand-in program, `examples/fake_ffmpeg.rs`, plays a
//! transport stream and misbehaves on cue), bad inputs, stopping in every
//! state, twenty prepare/transmit/stop cycles with threads and processes
//! counted, and the radio's refusal. Nothing here can reach the HackRF: see
//! `common/mod.rs`; the radio test goes through the whole session with the
//! refusal switch set and checks it is refused.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use decdvbt_tx::arm::{LICENCE_WARNING, LicenceAck};
use decdvbt_tx::config::{TxConfig, TxSink};
use decdvbt_tx::{TxSession, TxState};

/// The stand-in ffmpeg, built with the tests as an example.
fn fake_ffmpeg() -> Option<PathBuf> {
    no_radio();
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/stress-HASH.exe -> .../target/<profile>/examples/
    let dir = exe.parent()?.parent()?.join("examples");
    let name = if cfg!(windows) {
        "fake_ffmpeg.exe"
    } else {
        "fake_ffmpeg"
    };
    let p = dir.join(name);
    if p.is_file() {
        Some(p)
    } else {
        eprintln!(
            "SKIPPED: {} is not built (cargo test builds examples)",
            p.display()
        );
        None
    }
}

/// A transport stream of `packets` valid packets (PID 0x100, counted payload).
fn make_ts(path: &Path, packets: usize) {
    let mut v = Vec::with_capacity(packets * 188);
    for i in 0..packets {
        let mut p = [0xA5u8; 188];
        p[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10 | (i as u8 & 0x0F)]);
        p[4..8].copy_from_slice(&(i as u32).to_be_bytes());
        v.extend_from_slice(&p);
    }
    std::fs::write(path, v).unwrap();
}

fn fake_config(fake: &Path, ts: &Path, sink: TxSink) -> TxConfig {
    let mut cfg = TxConfig::new(ts.to_path_buf());
    cfg.mode = mode_a();
    cfg.loop_input = false;
    cfg.sink = sink;
    cfg.ffmpeg_path = Some(fake.to_path_buf());
    cfg.preview = false;
    cfg
}

fn ready(s: &mut TxSession) {
    s.prepare().unwrap();
    wait_for(s, Duration::from_secs(30), |st| {
        matches!(st.state, TxState::Ready | TxState::Failed)
    });
    assert_eq!(s.status().state, TxState::Ready, "{:?}", s.status().error);
}

/// Threads in this process.
#[cfg(windows)]
fn thread_count() -> usize {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    // SAFETY: a snapshot handle is made, walked with a correctly sized
    // THREADENTRY32 and closed.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut e: THREADENTRY32 = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let me = GetCurrentProcessId();
        let mut n = 0;
        if Thread32First(snap, &mut e) != 0 {
            loop {
                if e.th32OwnerProcessID == me {
                    n += 1;
                }
                if Thread32Next(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        n
    }
}

#[cfg(not(windows))]
fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |d| d.count())
}

/// Whether a process with this id is still running.
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is opened, asked once and closed.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == 259 // STILL_ACTIVE
    }
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// T11a: ffmpeg dies mid-run: `Failed` with a message, the sink stopped, no hang.
#[test]
fn ffmpeg_dying_mid_run_fails_the_session() {
    let _serial = serial();
    let Some(fake) = fake_ffmpeg() else { return };
    let dir = test_dir("die");
    let ts = dir.join("in.ts");
    make_ts(&ts, 20_000); // 3.76 MB
    // Two megabytes (more than the 0.5 s prefill), then 1.5 s, then a crash.
    std::fs::write(format!("{}.mode", ts.display()), "die 2000000 1500").unwrap();
    let mut s = TxSession::new(fake_config(&fake, &ts, TxSink::Null { paced: true })).unwrap();
    ready(&mut s);
    s.transmit().unwrap();
    let st = wait_for(&mut s, Duration::from_secs(30), |st| {
        st.state == TxState::Failed
    });
    let msg = st.error.unwrap();
    assert!(msg.contains("ffmpeg"), "{msg}");
    assert!(!st.radiating);
    // Everything is stopped: nothing is sent any more.
    let sent = s.status().sent_s;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(s.status().sent_s, sent);
    s.stop();
    assert_eq!(s.status().state, TxState::Idle);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T11b: an encoder that stops for a while: the stream is stuffed with nulls
/// (counted), nothing panics, and it carries on and ends when ffmpeg does.
#[test]
fn a_stalled_encoder_is_stuffed_and_recovers() {
    let _serial = serial();
    let Some(fake) = fake_ffmpeg() else { return };
    let dir = test_dir("stall");
    let ts = dir.join("in.ts");
    make_ts(&ts, 14_000);
    std::fs::write(format!("{}.mode", ts.display()), "stall 2000000 2500").unwrap();
    let mut s = TxSession::new(fake_config(&fake, &ts, TxSink::Null { paced: true })).unwrap();
    ready(&mut s);
    assert_eq!(s.status().ts_stuffed, 0, "no stuffing before transmitting");
    s.transmit().unwrap();
    let st = wait_for(&mut s, Duration::from_secs(40), |st| {
        matches!(st.state, TxState::Finished | TxState::Failed)
    });
    assert_eq!(st.state, TxState::Finished, "{:?}", st.error);
    assert!(st.ts_stuffed > 1000, "stuffed {}", st.ts_stuffed);
    assert_eq!(st.underruns, 0, "the radio never starved");
    s.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// T11c: bad inputs give a clear error and leave nothing running.
#[test]
fn bad_inputs_fail_cleanly() {
    let _serial = serial();
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("bad");
    let before = thread_count();
    // A file that is not a video.
    let text = dir.join("notes.txt");
    std::fs::write(&text, "this is not a video").unwrap();
    for input in [text, dir.join("missing.mp4")] {
        let mut cfg = TxConfig::new(input.clone());
        cfg.ffmpeg_path = Some(ff.clone());
        cfg.sink = TxSink::Null { paced: false };
        let mut s = TxSession::new(cfg).unwrap();
        s.prepare().unwrap();
        let st = wait_for(&mut s, Duration::from_secs(30), |st| {
            st.state == TxState::Failed
        });
        assert!(!st.error.unwrap().is_empty(), "{}", input.display());
        s.stop();
    }
    // An unwritable IQ file: transmit says so; the session can be stopped.
    let clip = make_clip(&ff, &dir, "c.mp4", 640, 360, 2, false);
    let mut cfg = null_config(&clip, mode_c());
    cfg.sink = TxSink::IqFile {
        path: dir.join("no-such-folder").join("x.cs8"),
        paced: false,
        seconds: None,
    };
    let mut s = TxSession::new(cfg).unwrap();
    ready(&mut s);
    let e = s.transmit().unwrap_err();
    assert!(e.to_string().contains("cannot create"), "{e}");
    s.stop();
    assert_eq!(thread_count(), before, "threads left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

/// T11d: twenty cycles of prepare, transmit and stop, stopping in each state:
/// no thread, no ffmpeg process left behind, no deadlock.
#[test]
fn twenty_cycles_leave_nothing_behind() {
    let _serial = serial();
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("cycles");
    let clip = make_clip(&ff, &dir, "c.mp4", 640, 360, 4, false);
    let mut cfg = null_config(&clip, mode_c());
    cfg.loop_input = true;
    cfg.sink = TxSink::Null { paced: true };
    // One cycle first so lazily started things (the NVENC test, the clock) are
    // not counted as leaks.
    let mut warm = TxSession::new(cfg.clone()).unwrap();
    ready(&mut warm);
    warm.stop();
    let before = thread_count();
    let mut pids = Vec::new();
    for cycle in 0..20 {
        let mut s = TxSession::new(cfg.clone()).unwrap();
        s.prepare().unwrap();
        // Stop during preparing, when ready, or when transmitting.
        match cycle % 3 {
            0 => std::thread::sleep(Duration::from_millis(30)),
            1 => {
                wait_for(&mut s, Duration::from_secs(30), |st| {
                    st.state == TxState::Ready
                });
            }
            _ => {
                wait_for(&mut s, Duration::from_secs(30), |st| {
                    st.state == TxState::Ready
                });
                s.transmit().unwrap();
                wait_for(&mut s, Duration::from_secs(10), |st| st.sent_s > 0.2);
            }
        }
        pids.extend(s.ffmpeg_pid());
        s.stop();
        assert_eq!(s.status().state, TxState::Idle);
    }
    assert!(!pids.is_empty());
    for pid in pids {
        assert!(!process_alive(pid), "ffmpeg {pid} was left running");
    }
    assert_eq!(thread_count(), before, "threads left behind");
    let _ = std::fs::remove_dir_all(&dir);
}

/// T11e: changing the mode is a stop and a prepare; each of the three
/// targets prepares and runs.
#[test]
fn a_mode_change_cycle_runs_each_target() {
    let _serial = serial();
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("modes");
    let clip = make_clip(&ff, &dir, "c.mp4", 1280, 720, 4, false);
    let mut s: Option<TxSession> = None;
    for mode in [mode_a(), mode_b(), mode_c(), mode_a()] {
        if let Some(mut old) = s.take() {
            old.stop();
        }
        let mut cfg = null_config(&clip, mode);
        cfg.loop_input = true;
        cfg.sink = TxSink::Null { paced: true };
        let mut next = TxSession::new(cfg).unwrap();
        ready(&mut next);
        next.transmit().unwrap();
        let st = wait_for(&mut next, Duration::from_secs(20), |st| st.sent_s > 1.5);
        assert_eq!(st.ts_stuffed, 0);
        assert_eq!(st.ts_rate_bps, mode.bitrate_hp());
        s = Some(next);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// T12: with the refusal switch set the radio is not opened, even armed and
/// with a ready session; the session stays safe and `radiating` stays false.
#[test]
fn the_radio_is_refused_by_the_switch_even_when_armed() {
    let _serial = serial();
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("radio");
    let clip = make_clip(&ff, &dir, "c.mp4", 640, 360, 2, false);
    let mut cfg = null_config(&clip, mode_c());
    cfg.sink = TxSink::HackRf(Default::default());
    let mut s = TxSession::new(cfg).unwrap();
    // Unarmed: refused as such.
    assert!(s.transmit().unwrap_err().to_string().contains("not armed"));
    ready(&mut s);
    assert!(s.transmit().unwrap_err().to_string().contains("not armed"));
    // Armed and ready: the switch refuses it before any USB access.
    s.arm(LicenceAck::accept(LICENCE_WARNING).unwrap());
    assert!(s.status().armed);
    let e = s.transmit().unwrap_err().to_string();
    assert!(
        e.contains("disabled") || e.contains("without the hackrf"),
        "{e}"
    );
    let st = s.status();
    assert!(
        !st.radiating && !st.armed,
        "the arm is used up by the attempt"
    );
    assert_eq!(st.state, TxState::Ready);
    s.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
