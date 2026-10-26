// made by CasualArclamp
//! The whole transmit chain on a sink that is not a radio: a clip made by
//! ffmpeg's test sources goes through ffmpeg, the feeder and the modulator, and
//! comes out as a transport stream (checked) or as IQ that our receiver decodes
//! (loopback).
//!
//! These need ffmpeg; without it each test says SKIPPED and passes. They take a
//! few seconds each (they encode video). Nothing here can reach the HackRF:
//! see `common/mod.rs`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use decdvbt_core::Iq;
use decdvbt_tx::config::{EncoderChoice, TxRate, TxSink};
use decdvbt_tx::feeder::Feeder;
use decdvbt_tx::ffmpeg::{self, Chain};
use decdvbt_tx::modes::VideoProfile;
use decdvbt_tx::ring::TsRing;
use decdvbt_tx::{TxSession, TxState, mux_rate_bps};

/// T2: the stream ffmpeg makes is clean at each target, on each encoder chain.
#[test]
fn the_stream_is_clean_for_each_target_and_chain() {
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("t2");
    let clip = make_clip(&ff, &dir, "hd.mp4", 1920, 1080, 8, false);
    let mut chains = vec![("x264", EncoderChoice::X264)];
    if nvenc_works(&ff) {
        chains.push(("nvenc", EncoderChoice::Nvenc));
    }
    for (name, mode) in [("A", mode_a()), ("B", mode_b()), ("C", mode_c())] {
        for &(chain, choice) in &chains {
            let ts_path = dir.join(format!("{name}-{chain}.ts"));
            let mut cfg = null_config(&clip, mode);
            cfg.video.encoder = choice;
            // At the sample clock: flat out, the modulator would outrun a
            // software encoder and the stream would hold stuffing.
            cfg.sink = TxSink::Null { paced: true };
            let st = run(cfg, 6.0, Some(&ts_path));
            let ts = std::fs::read(&ts_path).unwrap();
            let rate = rate_of(&mode);
            let a = analyse(&ts, rate);
            let why = format!("{name} {chain}: {a:#?} {st:?}");
            assert_eq!(a.bad_sync, 0, "{why}");
            assert!(a.cc_errors.is_empty(), "{why}");
            assert_eq!(st.ts_stuffed, 0, "{why}");
            assert_eq!(st.dts_pcr_warnings, 0, "{why}");
            assert!(a.pcr_ppm.abs() < 5.0, "{why}");
            assert!(a.pcr_max_interval_ms <= 40.0, "{why}");
            assert_eq!(a.pcr_jumps, 0, "{why}");
            assert!(a.null_fraction >= 0.05, "{why}");
            // PAT and PMT every 100.6 ms, SDT and NIT every 500 ms.
            for (pid, limit) in [(0u16, 101.0), (0x1000, 101.0), (0x11, 501.0), (0x10, 501.0)] {
                let gap = a.max_gap_ms.get(&pid).copied();
                assert!(
                    gap.is_some_and(|g| g <= limit),
                    "pid {pid:#x}: {gap:?}; {why}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// T3: the over-budget detector reads zero on the profile's own figures and
/// something when the video is given three times the bitrate.
#[test]
fn over_budget_is_noticed() {
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("t3");
    let clip = make_clip(&ff, &dir, "noise.mp4", 1280, 720, 5, true);
    let mode = mode_c();
    let cfg = null_config(&clip, mode);
    let profile = VideoProfile::for_rate(mux_rate_bps(&mode));
    let warnings = |scale: u32| -> u64 {
        let mut args = ffmpeg::command_line(&cfg, None, Chain::X264, &profile, true);
        for flag in ["-b:v", "-maxrate", "-bufsize"] {
            let i = args.iter().position(|a| a == flag).unwrap();
            let k: u32 = args[i + 1].trim_end_matches('k').parse().unwrap();
            args[i + 1] = format!("{}k", k * scale);
        }
        let ring = Arc::new(TsRing::new(60_000));
        let f = Feeder::spawn(ff.clone(), args, ring).unwrap();
        let t = std::time::Instant::now();
        while !f.shared.ended.load(std::sync::atomic::Ordering::Acquire) {
            assert!(t.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(20));
        }
        f.shared
            .dts_pcr_warnings
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    assert_eq!(warnings(1), 0, "the profile's own figures hold on noise");
    assert!(warnings(3) > 0, "three times the bitrate is over budget");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the receiver makes of an IQ file.
struct Decoded {
    ts: Vec<u8>,
    uncorrectable: u64,
    blocks: u64,
    snr_db: f32,
}

fn decode(path: &std::path::Path, fs: f64) -> Decoded {
    use decdvbt_rx::auto::AutoReceiver;
    let bytes = std::fs::read(path).unwrap();
    let mut rx = AutoReceiver::new(fs, 0.0);
    let (mut ts, mut abs) = (Vec::new(), 0u64);
    for chunk in bytes.chunks(2 * 65_536) {
        let block: Vec<Iq> = chunk
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| Iq::new(f32::from(c[0] as i8) / 128.0, f32::from(c[1] as i8) / 128.0))
            .collect();
        rx.push(&block, abs);
        abs += block.len() as u64;
        rx.take_ts(&mut ts);
    }
    rx.flush();
    rx.take_ts(&mut ts);
    let st = rx.status();
    Decoded {
        ts,
        uncorrectable: st.outer.uncorrectable,
        blocks: st.outer.blocks,
        snr_db: st.snr_db,
    }
}

/// Where `rx` starts in `sent`, by three non-null packets in a row, and then
/// how many packets differ from there on. Returns (compared, mismatches).
fn compare(sent: &[u8], rx: &[u8]) -> (usize, usize) {
    let is_null = |p: &[u8]| p[1] == 0x1F && p[2] == 0xFF;
    let s: Vec<&[u8]> = sent.as_chunks::<188>().0.iter().map(|p| &p[..]).collect();
    let r: Vec<&[u8]> = rx.as_chunks::<188>().0.iter().map(|p| &p[..]).collect();
    let first = (0..r.len() - 3)
        .find(|&i| (0..3).all(|k| !is_null(r[i + k])))
        .expect("the receiver produced programme packets");
    let off = (0..s.len() - 3)
        .find(|&k| s[k] == r[first] && s[k + 1] == r[first + 1] && s[k + 2] == r[first + 2])
        .expect("the received packets are in what was sent")
        - first;
    let mut bad = 0;
    let mut n = 0;
    for (j, rp) in r.iter().enumerate().skip(first) {
        let Some(sp) = s.get(j + off) else { break };
        n += 1;
        bad += usize::from(rp != sp);
    }
    (n, bad)
}

/// T6 and T7: the IQ file the transmitter writes decodes in our receiver with no
/// uncorrectable block and every packet identical to what was sent; the sent
/// stream decodes cleanly and carries its identity.
#[test]
fn loopback_is_bit_exact_and_the_picture_survives() {
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("t6");
    let clip = make_clip(&ff, &dir, "hd.mp4", 1280, 720, 5, false);
    let iq = dir.join("tx_546.000MHz_9.142857Msps.cs8");
    let ts_path = dir.join("sent.ts");
    let mut cfg = null_config(&clip, mode_a());
    cfg.service.name = "Test clip".into();
    // The whole clip, so the stream sent ends cleanly (a cut-off last frame
    // would be a decode error of its own).
    cfg.sink = TxSink::IqFile {
        path: iq.clone(),
        paced: false,
        seconds: None,
    };
    let st = run(cfg, 1e9, Some(&ts_path));
    assert_eq!(st.state, TxState::Finished, "{st:?}");
    assert_eq!(st.underruns, 0);
    assert!(st.sent_s > 5.0, "{}", st.sent_s);

    let d = decode(&iq, 64e6 / 7.0);
    assert!(d.blocks > 1000, "the receiver locked: {} blocks", d.blocks);
    assert_eq!(d.uncorrectable, 0);
    assert!(d.snr_db > 30.0, "SNR {}", d.snr_db);
    let sent = std::fs::read(&ts_path).unwrap();
    let (n, bad) = compare(&sent, &d.ts);
    assert!(n > 40_000, "only {n} packets compared");
    assert_eq!(bad, 0, "{bad} of {n} packets differ");

    // The services the receiver lists.
    let mut an = decdvbt_ts::TsAnalyser::new();
    for p in d.ts.as_chunks::<188>().0 {
        an.packet(p);
    }
    let r = an.report();
    assert_eq!(r.network.name.as_deref(), Some("DecDVB-T"));
    assert_eq!(r.programmes.len(), 1);
    assert_eq!(r.programmes[0].name.as_deref(), Some("Test clip"));
    assert_eq!(r.programmes[0].service_type, Some(0x19));
    assert!(r.utc_unix.is_some());
    let t = &r.network.transponders[0];
    assert_eq!(t.terrestrial.unwrap().centre_hz, 546_000_000);
    assert_eq!(r.bad_sections, 0);

    // T7: the stream we sent decodes with no errors, and has all the frames.
    let out = std::process::Command::new(&ff)
        .args(["-v", "error", "-i"])
        .arg(&ts_path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&out.stderr);
    assert!(errors.trim().is_empty(), "ffmpeg: {errors}");
    let probe = decdvbt_tx::ffmpeg::find_ffprobe(&ff).map(|p| {
        std::process::Command::new(p)
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
            .args([
                "-show_entries",
                "stream=nb_read_frames,codec_name",
                "-of",
                "compact=p=0",
            ])
            .arg(&ts_path)
            .output()
            .unwrap()
    });
    if let Some(p) = probe {
        let text = String::from_utf8_lossy(&p.stdout).to_string();
        let frames: i64 = text
            .lines()
            .flat_map(|l| l.split('|'))
            .find_map(|f| f.trim().strip_prefix("nb_read_frames="))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(-1);
        assert!((frames - 125).abs() <= 1, "{frames} frames: {text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// T8: a looped clip is one continuous stream: no continuity error, no PCR step.
#[test]
fn a_looped_clip_is_continuous() {
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("t8");
    let clip = make_clip(&ff, &dir, "short.mp4", 1280, 720, 3, false);
    let ts_path = dir.join("loop.ts");
    let mut cfg = null_config(&clip, mode_b());
    cfg.loop_input = true;
    cfg.sink = TxSink::Null { paced: true };
    let st = run(cfg, 12.0, Some(&ts_path));
    assert_eq!(st.state, TxState::Transmitting, "{st:?}");
    let ts = std::fs::read(&ts_path).unwrap();
    let a = analyse(&ts, rate_of(&mode_b()));
    assert!(a.cc_errors.is_empty(), "{a:#?}");
    assert_eq!(a.pcr_jumps, 0, "{a:#?}");
    assert!(a.pcr_ppm.abs() < 5.0);
    assert_eq!(st.ts_stuffed, 0);
    // Four passes of a 3 s clip: more than 3 s of programme went through.
    assert!(st.programme_s > 9.0, "{}", st.programme_s);
    if let Some(p) = decdvbt_tx::ffmpeg::find_ffprobe(&ff) {
        // The video's decode times never jump: no gap above two frames.
        let out = std::process::Command::new(p)
            .args(["-v", "error", "-select_streams", "v:0"])
            .args(["-show_entries", "packet=dts_time", "-of", "csv=p=0"])
            .arg(&ts_path)
            .output()
            .unwrap();
        let dts: Vec<f64> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
            .collect();
        assert!(dts.len() > 200, "{} packets", dts.len());
        let mut sorted = dts.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let worst = sorted.windows(2).map(|w| w[1] - w[0]).fold(0.0, f64::max);
        assert!(worst < 0.09, "gap of {worst} s in the video's decode times");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// T10: the fixed 10 MS/s rate decodes just as exactly as the native one.
#[test]
fn a_resampled_rate_decodes_bit_exact_too() {
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("t10");
    let clip = make_clip(&ff, &dir, "hd.mp4", 1280, 720, 4, false);
    let iq = dir.join("tx_546.000MHz_10Msps.cs8");
    let ts_path = dir.join("sent.ts");
    let mut cfg = null_config(&clip, mode_a());
    cfg.rate = TxRate::Fixed(10_000_000);
    cfg.sink = TxSink::IqFile {
        path: iq.clone(),
        paced: false,
        seconds: None,
    };
    let st = run(cfg, 1e9, Some(&ts_path));
    assert_eq!(st.sample_rate_hz, 10_000_000);
    let d = decode(&iq, 10e6);
    assert_eq!(d.uncorrectable, 0);
    let (n, bad) = compare(&std::fs::read(&ts_path).unwrap(), &d.ts);
    assert!(n > 30_000, "{n}");
    assert_eq!(bad, 0, "{bad} of {n}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The paced null sink (a fake radio at the sample clock) never underruns, and
/// the session reports its meters.
#[test]
fn a_paced_run_does_not_underrun() {
    let Some(ff) = ffmpeg() else { return };
    let dir = test_dir("paced");
    let clip = make_clip(&ff, &dir, "hd.mp4", 1280, 720, 6, false);
    let mut cfg = null_config(&clip, mode_b());
    cfg.loop_input = true;
    cfg.sink = TxSink::Null { paced: true };
    let mut s = TxSession::new(cfg).unwrap();
    s.prepare().unwrap();
    wait_for(&mut s, Duration::from_secs(60), |st| {
        st.state == TxState::Ready
    });
    s.transmit().unwrap();
    let st = wait_for(&mut s, Duration::from_secs(30), |st| st.sent_s >= 6.0);
    s.stop();
    assert_eq!(st.underruns, 0, "{st:?}");
    assert_eq!(st.ts_stuffed, 0, "{st:?}");
    assert_eq!(st.dts_pcr_warnings, 0);
    assert!(st.iq_fill > 0.5, "IQ ring {}", st.iq_fill);
    assert!(
        st.level_rms_dbfs > -13.0 && st.level_rms_dbfs < -11.0,
        "{}",
        st.level_rms_dbfs
    );
    assert!(
        (st.ts_in_bps / 1e6 - 12.06).abs() < 1.0,
        "in {}",
        st.ts_in_bps
    );
    assert!(st.process_cpu_pct < 150.0);
    assert!(!st.chain.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
