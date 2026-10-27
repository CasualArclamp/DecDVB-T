// made by CasualArclamp
//! The narrow channels end to end: DVB-T scaled to 1 MHz (every clock of the
//! 8 MHz mode divided by 8, DecDVB-T's own non-standard mode) and DVB-T2's
//! 1.7 MHz (EN 302 755 Table 66). Each is modulated, put through a channel
//! (white noise, multipath, a carrier offset and a sampling clock error),
//! resampled to the HackRF's lowest rate (2 MS/s) as a radio would deliver it,
//! and decoded by the receiver that is told the width
//! ([`AutoReceiver::with_bandwidth`]). What comes out must be the very packets
//! that went in: from the first one received, every later packet equal to the
//! modulator's, byte for byte, none missing.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{Bandwidth, Mode};
use decdvbt_mod::channel::{Channel, Path};
use decdvbt_mod::sim::{SimOptions, T2SimOptions, generate, generate_t2};
use decdvbt_mod::t2::T2Config;
use decdvbt_mod::ts::{TS_LEN, TestStream, TsSource, check_test_payload};
use decdvbt_rx::auto::{AutoReceiver, Standard};

/// The HackRF's lowest sample rate, which the narrow channels are received at.
const RADIO_HZ: f64 = 2e6;

/// The first `n` packets the test stream sends (what the modulators were fed).
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

/// How the received stream compares with the sent one.
#[derive(Debug)]
struct Compared {
    /// Packets received.
    received: usize,
    /// From the first test-stream data packet received on: packets equal to
    /// the sent ones in order, and the first that is not (if any).
    matched: usize,
    first_mismatch: Option<usize>,
}

/// Line the received packets up with the sent ones (by the first data
/// packet's counter, which is unique) and compare every one after it.
fn compare(rx: &[u8], reference: &[[u8; TS_LEN]]) -> Compared {
    let packets = rx.as_chunks::<TS_LEN>().0;
    let mut c = Compared {
        received: packets.len(),
        matched: 0,
        first_mismatch: None,
    };
    let Some((i0, counter)) = packets
        .iter()
        .enumerate()
        .find_map(|(i, p)| check_test_payload(p).map(|n| (i, n)))
    else {
        return c;
    };
    let Some(r0) = reference
        .iter()
        .position(|p| check_test_payload(p) == Some(counter))
    else {
        c.first_mismatch = Some(i0);
        return c;
    };
    for (j, p) in packets[i0..].iter().enumerate() {
        match reference.get(r0 + j) {
            Some(want) if want == p => c.matched += 1,
            _ => {
                c.first_mismatch = Some(i0 + j);
                break;
            }
        }
    }
    c
}

/// Feed `x` (at [`RADIO_HZ`]) to a receiver for `bandwidth` in blocks of an
/// awkward size; the transport stream and the receiver.
fn receive(x: &[Iq], bandwidth: Bandwidth) -> (Vec<u8>, AutoReceiver) {
    let mut rx = AutoReceiver::with_bandwidth(RADIO_HZ, 0.0, bandwidth);
    let mut ts = Vec::new();
    for (k, block) in x.chunks(65_537).enumerate() {
        rx.push(block, (k * 65_537) as u64);
        rx.take_ts(&mut ts);
    }
    rx.flush();
    rx.take_ts(&mut ts);
    (ts, rx)
}

/// A channel for a narrow signal: noise, a static echo profile well inside
/// the guard (delays in µs, as on air: the 1 MHz guard intervals are 56 to
/// 1792 µs), a carrier offset of several carriers and a clock error.
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

fn dvbt(mode_text: &str, ch: Channel, seconds: f64) {
    let mode = Mode::parse(mode_text).unwrap();
    assert_eq!(mode.bandwidth, Bandwidth::Mhz1, "{mode_text}");
    let opts = SimOptions {
        mode,
        cell_id: None,
        ts_file: None,
        seconds,
        out_rate_hz: Some(RADIO_HZ),
        level_dbfs: -18.0,
        channel: ch,
    };
    let mut x = Vec::new();
    generate(&opts, |b| {
        x.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let (ts, rx) = receive(&x, Bandwidth::Mhz1);
    // The rate the mode carries for the seconds given: what a whole decode
    // would deliver.
    let expected = (mode.bitrate_hp() * seconds / (8.0 * TS_LEN as f64)) as usize;
    let c = compare(&ts, &sent(expected + 2000));
    let st = rx.status();
    eprintln!(
        "{mode_text}: {:?}, {c:?} of about {expected}, SNR {:.1} dB, CFO {:.0} Hz, clock {:+.1} ppm",
        rx.standard(),
        st.snr_db,
        st.cfo_hz,
        st.clock_ppm
    );
    assert_eq!(rx.standard(), Standard::Dvbt, "{mode_text}: {st:?}");
    assert_eq!(c.first_mismatch, None, "{mode_text}: {c:?}");
    // At least half the stream (the rest is the lock time: acquisition, a TPS
    // frame of 68 symbols, the deinterleavers filling).
    assert!(
        c.matched * 2 > expected,
        "{mode_text}: {} of {expected} packets",
        c.matched
    );
    assert!(c.received >= c.matched);
    assert_eq!(st.outer.uncorrectable, 0, "{mode_text}: {:?}", st.outer);
}

#[test]
fn one_mhz_dvbt_2k_qpsk() {
    // 2k at 1 MHz: 558 Hz carriers, 1.79 ms symbols. +2.3 kHz is four
    // carriers and a fraction.
    dvbt("qpsk-1/2-2k-1/8-1mhz", channel(14.0, 2_300.0, 12.0), 1.6);
}

#[test]
fn one_mhz_dvbt_8k_16qam() {
    // 8k: 139.5 Hz carriers, 7.2 ms symbols, a TPS frame every half second.
    dvbt(
        "16qam-3/4-8k-1/32-1mhz",
        channel(22.0, -5_100.0, -15.0),
        3.2,
    );
}

#[test]
fn one_mhz_dvbt_2k_64qam_long_guard() {
    dvbt("64qam-2/3-2k-1/4-1mhz", channel(27.0, 731.0, 25.0), 1.6);
}

#[test]
fn one_mhz_dvbt_8k_64qam_high_rate() {
    dvbt("64qam-5/6-8k-1/16-1mhz", channel(30.0, 12_000.0, 5.0), 3.2);
}

fn t2(spec: &str, ch: Channel, frames: usize) {
    let cfg = T2Config::parse(spec).unwrap_or_else(|e| panic!("{spec}: {e}"));
    assert_eq!(cfg.bandwidth, Bandwidth::Mhz1_7, "{spec}");
    let rate = cfg.ts_rate_bps();
    let frame_s = cfg.frame_s();
    let opts = T2SimOptions {
        cfg,
        ts_file: None,
        frames,
        out_rate_hz: Some(RADIO_HZ),
        level_dbfs: -18.0,
        channel: ch,
    };
    let mut x = Vec::new();
    generate_t2(&opts, |b| {
        x.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let (ts, rx) = receive(&x, Bandwidth::Mhz1_7);
    let expected = (rate * frame_s * frames as f64 / (8.0 * TS_LEN as f64)) as usize;
    let c = compare(&ts, &sent(expected + 2000));
    let st = rx.dvbt2();
    eprintln!(
        "{spec}: {:?}, {c:?} of about {expected}, frames {} ok {} bad, blocks {}/{}, SNR {:.1} dB, \
         CFO {:.0} Hz, clock {:+.1} ppm",
        rx.standard(),
        st.frames_ok,
        st.frames_bad,
        st.blocks_ok,
        st.blocks_ok + st.blocks_bad,
        st.snr_db,
        st.cfo_hz,
        st.clock_ppm
    );
    assert_eq!(rx.standard(), Standard::Dvbt2, "{spec}");
    assert_eq!(st.locks, 1, "{spec}");
    assert_eq!(c.first_mismatch, None, "{spec}: {c:?}");
    assert!(
        c.matched * 2 > expected,
        "{spec}: {} of {expected} packets",
        c.matched
    );
}

#[test]
fn t2_1_7_mhz_8k_16qam() {
    // 8K at 1.7 MHz: 225 Hz carriers, 4.7 ms symbols; frames of 217 ms.
    t2(
        "8k-1/16-pp4-16qam-3/4-short-ext-1.7mhz",
        channel(24.0, 3_456.0, 8.0),
        8,
    );
}

#[test]
fn t2_1_7_mhz_2k_rotated_qpsk_with_a_closing_symbol() {
    t2(
        "2k-1/8-pp2-qpsk-1/2-short-rot-1.7mhz",
        channel(12.0, -1_800.0, -10.0),
        10,
    );
}

#[test]
fn t2_1_7_mhz_4k_64qam_normal_code() {
    t2(
        "4k-1/32-pp7-64qam-2/3-rot-1.7mhz",
        channel(26.0, 650.0, 20.0),
        8,
    );
}

/// The receiver is told the width: 1 MHz runs DVB-T only, 1.7 MHz T2 only.
#[test]
fn only_the_standards_sent_in_the_width_look() {
    let one = AutoReceiver::with_bandwidth(RADIO_HZ, 0.0, Bandwidth::Mhz1);
    assert_eq!(one.dvbt2().locks, 0);
    assert_eq!(one.bandwidth(), Bandwidth::Mhz1);
    let t2 = AutoReceiver::with_bandwidth(RADIO_HZ, 0.0, Bandwidth::Mhz1_7);
    assert_eq!(t2.dvbt2().bandwidth, Some(Bandwidth::Mhz1_7));
}
