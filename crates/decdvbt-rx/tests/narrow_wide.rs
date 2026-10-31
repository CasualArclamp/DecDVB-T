// made by CasualArclamp
//! The narrow channels received at the Airspy R2's 10 MS/s (its only rate):
//! the receivers take the radio's rate as it is, so a 1 MHz DVB-T or 1.7 MHz
//! DVB-T2 signal at 10 MS/s must decode as it does at the HackRF's 2 MS/s
//! (`narrow.rs`). What comes out must be the packets that went in.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{Bandwidth, Mode};
use decdvbt_mod::channel::{Channel, Path};
use decdvbt_mod::sim::{SimOptions, T2SimOptions, generate, generate_t2};
use decdvbt_mod::t2::T2Config;
use decdvbt_mod::ts::{TS_LEN, TestStream, TsSource, check_test_payload};
use decdvbt_rx::auto::{AutoReceiver, Standard};

/// The Airspy R2's only sample rate.
const RADIO_HZ: f64 = 10e6;

fn sent(n: usize) -> Vec<[u8; TS_LEN]> {
    let mut ts = TestStream::default();
    (0..n)
        .map(|_| {
            let mut p = [0u8; TS_LEN];
            ts.next_packet(&mut p);
            p
        })
        .collect()
}

/// Packets equal to the sent ones from the first test-stream packet on, and
/// the first that is not.
fn matched(rx: &[u8], reference: &[[u8; TS_LEN]]) -> (usize, Option<usize>) {
    let packets = rx.as_chunks::<TS_LEN>().0;
    let Some((i0, counter)) = packets
        .iter()
        .enumerate()
        .find_map(|(i, p)| check_test_payload(p).map(|n| (i, n)))
    else {
        return (0, None);
    };
    let Some(r0) = reference
        .iter()
        .position(|p| check_test_payload(p) == Some(counter))
    else {
        return (0, Some(i0));
    };
    let mut n = 0;
    for (j, p) in packets[i0..].iter().enumerate() {
        match reference.get(r0 + j) {
            Some(want) if want == p => n += 1,
            _ => return (n, Some(i0 + j)),
        }
    }
    (n, None)
}

fn receive(x: &[Iq], bandwidth: Bandwidth) -> (Vec<u8>, AutoReceiver, std::time::Duration) {
    let mut rx = AutoReceiver::with_bandwidth(RADIO_HZ, 0.0, bandwidth);
    let mut ts = Vec::new();
    let t0 = std::time::Instant::now();
    for (k, block) in x.chunks(262_147).enumerate() {
        rx.push(block, (k * 262_147) as u64);
        rx.take_ts(&mut ts);
    }
    rx.flush();
    rx.take_ts(&mut ts);
    (ts, rx, t0.elapsed())
}

/// `x` at 2 MS/s as the Airspy delivers it at 10: zero-stuffed by 5 and
/// low-passed at the old Nyquist (the generator cannot resample the T2
/// 1.7 MHz clock, 1.845 MS/s, to 10 MS/s itself).
fn upsample_5(x: &[Iq]) -> Vec<Iq> {
    const L: usize = 5;
    let taps = decdvbt_dsp::lowpass(0.9 / (2.0 * L as f64), 201);
    let mid = taps.len() / 2;
    let mut out = vec![Iq::new(0.0, 0.0); x.len() * L];
    for (k, &s) in x.iter().enumerate() {
        for (t, &h) in taps.iter().enumerate() {
            // The impulse at k*L reaches out[k*L + t - mid].
            if let Some(o) = out.get_mut((k * L + t).wrapping_sub(mid)) {
                *o += s * (h * L as f32);
            }
        }
    }
    out
}

fn channel(cn_db: f64, cfo_hz: f64, sco_ppm: f64) -> Channel {
    Channel {
        cn_db: Some(cn_db),
        paths: vec![
            Path::echo(0.0, 0.0),
            Path::echo(6.0, -7.0),
            Path::echo(21.0, -13.0),
        ],
        cfo_hz,
        sco_ppm,
        seed: 7,
        ..Default::default()
    }
}

#[test]
fn one_mhz_dvbt_at_10_msps() {
    let text = "qpsk-1/2-2k-1/8-1mhz";
    let mode = Mode::parse(text).unwrap();
    let seconds = 1.6;
    let opts = SimOptions {
        mode,
        cell_id: None,
        ts_file: None,
        seconds,
        out_rate_hz: Some(RADIO_HZ),
        level_dbfs: -18.0,
        channel: channel(14.0, 2_300.0, 12.0),
    };
    let mut x = Vec::new();
    generate(&opts, |b| {
        x.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let (ts, rx, took) = receive(&x, Bandwidth::Mhz1);
    let expected = (mode.bitrate_hp() * seconds / (8.0 * TS_LEN as f64)) as usize;
    let (n, bad) = matched(&ts, &sent(expected + 2000));
    let st = rx.status();
    eprintln!(
        "{text} at 10 MS/s: {:?}, {n} of about {expected}, SNR {:.1} dB, CFO {:.0} Hz, \
         {:.0} % of real time",
        rx.standard(),
        st.snr_db,
        st.cfo_hz,
        100.0 * took.as_secs_f64() / seconds
    );
    assert_eq!(rx.standard(), Standard::Dvbt);
    assert_eq!(bad, None);
    assert!(n * 2 > expected, "{n} of {expected}");
}

#[test]
fn t2_1_7_mhz_at_10_msps() {
    let spec = "8k-1/16-pp4-16qam-3/4-short-ext-1.7mhz";
    let cfg = T2Config::parse(spec).unwrap();
    let rate = cfg.ts_rate_bps();
    let frame_s = cfg.frame_s();
    let frames = 8;
    let opts = T2SimOptions {
        cfg,
        ts_file: None,
        frames,
        out_rate_hz: Some(2e6),
        level_dbfs: -18.0,
        channel: channel(24.0, 3_456.0, 8.0),
    };
    let mut x = Vec::new();
    generate_t2(&opts, |b| {
        x.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let x = upsample_5(&x);
    let (ts, rx, took) = receive(&x, Bandwidth::Mhz1_7);
    let expected = (rate * frame_s * frames as f64 / (8.0 * TS_LEN as f64)) as usize;
    let (n, bad) = matched(&ts, &sent(expected + 2000));
    let st = rx.dvbt2();
    eprintln!(
        "{spec} at 10 MS/s: {:?}, {n} of about {expected}, frames {} ok {} bad, SNR {:.1} dB, \
         {:.0} % of real time",
        rx.standard(),
        st.frames_ok,
        st.frames_bad,
        st.snr_db,
        100.0 * took.as_secs_f64() / (frame_s * frames as f64)
    );
    assert_eq!(rx.standard(), Standard::Dvbt2);
    assert_eq!(bad, None);
    assert!(n * 2 > expected, "{n} of {expected}");
}
