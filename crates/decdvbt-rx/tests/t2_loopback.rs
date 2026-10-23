// made by CasualArclamp
//! The whole T2 receiver against the modulator: a signal is made, put through
//! a channel (multipath, noise, a carrier offset and a clock error), played
//! to the receiver as a radio would, and every transport packet that comes out
//! is checked against what was sent.

use decdvbt_core::Iq;
use decdvbt_mod::channel::{Channel, Path};
use decdvbt_mod::sim::{T2SimOptions, generate_t2};
use decdvbt_mod::t2::T2Config;
use decdvbt_mod::ts::check_test_payload;
use decdvbt_rx::t2::receiver::{T2Config as RxConfig, T2Receiver};

struct Outcome {
    locks: u32,
    frames_ok: u64,
    blocks_ok: u64,
    blocks_bad: u64,
    good: usize,
    bad: usize,
    mode: String,
}

fn run(spec: &str, channel: Channel, frames: usize, rate: f64, vfo: f64) -> Outcome {
    let cfg = T2Config::parse(spec).unwrap_or_else(|e| panic!("{spec}: {e}"));
    let opts = T2SimOptions {
        cfg,
        ts_file: None,
        frames,
        out_rate_hz: Some(rate),
        level_dbfs: -18.0,
        channel,
    };
    let mut samples: Vec<Iq> = Vec::new();
    generate_t2(&opts, |b| {
        samples.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    let mut rx = T2Receiver::new(RxConfig::new(rate, vfo));
    let mut ts = Vec::new();
    let mut pos = 0u64;
    for block in samples.chunks(100_000) {
        rx.push(block, pos);
        pos += block.len() as u64;
        rx.take_ts(&mut ts);
    }
    rx.flush();
    rx.take_ts(&mut ts);
    let (mut good, mut bad) = (0, 0);
    for p in ts.as_chunks::<188>().0 {
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        if pid == 0x101 {
            if check_test_payload(p).is_some() {
                good += 1;
            } else {
                bad += 1;
            }
        }
    }
    let s = rx.status();
    Outcome {
        locks: s.locks,
        frames_ok: s.frames_ok,
        blocks_ok: s.blocks_ok,
        blocks_bad: s.blocks_bad,
        good,
        bad,
        mode: format!("{:?}", s.mode),
    }
}

fn channel() -> Channel {
    Channel {
        cn_db: Some(32.0),
        paths: vec![Path::echo(0.0, 0.0), Path::echo(2.0, -9.0)],
        cfo_hz: 12_345.0,
        sco_ppm: 3.0,
        ..Default::default()
    }
}

fn check(spec: &str, frames: usize, min_frames: u64) {
    let o = run(spec, channel(), frames, 10e6, 0.0);
    eprintln!(
        "{spec}: {} locks, {} frames, blocks {}/{}, test packets good {} bad {} ({})",
        o.locks,
        o.frames_ok,
        o.blocks_ok,
        o.blocks_ok + o.blocks_bad,
        o.good,
        o.bad,
        o.mode
    );
    assert_eq!(o.locks, 1, "{spec}");
    assert!(o.frames_ok >= min_frames, "{spec}: {} frames", o.frames_ok);
    assert!(o.good > 100, "{spec}: only {} good packets", o.good);
    // Nothing that comes out is wrong (the first frames, while the clock is
    // found, are lost, not corrupted).
    assert_eq!(o.bad, 0, "{spec}: {} bad packets", o.bad);
    // After the first frames every block decodes.
    assert!(
        o.blocks_ok as f64 > 0.7 * (o.blocks_ok + o.blocks_bad) as f64,
        "{spec}: blocks {}/{}",
        o.blocks_ok,
        o.blocks_ok + o.blocks_bad
    );
}

#[test]
fn eight_k_with_two_p2_symbols_and_a_short_code() {
    // N_P2 = 2: L1 is dealt over both symbols.
    check("8k-1/16-pp4-16qam-3/4-short-ext", 12, 6);
}

#[test]
fn two_k_with_a_closing_symbol_and_rotated_qpsk() {
    // 2K PP2 with a 1/8 guard ends its frames with a closing symbol.
    check("2k-1/8-pp2-qpsk-1/2-short-rot", 14, 7);
}

#[test]
fn sixteen_k_64_qam_with_a_normal_code() {
    check("16k-1/32-pp4-64qam-3/4-rot-ext", 10, 4);
}

#[test]
fn the_uk_multiplex() {
    check("uk", 10, 4);
}

#[test]
fn it_locks_through_a_vfo_offset_and_at_other_radio_rates() {
    // The radio's centre 150 kHz from the signal's, at 12.5 MS/s.
    let ch = Channel {
        cn_db: Some(32.0),
        cfo_hz: 150_000.0,
        ..Default::default()
    };
    let o = run("8k-1/16-pp4-16qam-3/4-short-ext", ch, 10, 12.5e6, 0.0);
    eprintln!(
        "12.5 MS/s, offset: {} frames, good {} bad {}",
        o.frames_ok, o.good, o.bad
    );
    assert_eq!(o.locks, 1);
    assert!(o.good > 100 && o.bad == 0);
}

#[test]
fn noise_alone_gives_no_lock() {
    let mut rx = T2Receiver::new(RxConfig::new(10e6, 0.0));
    let mut seed = 7u32;
    let mut g = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    };
    let noise: Vec<Iq> = (0..6_000_000).map(|_| Iq::new(g(), g())).collect();
    let mut pos = 0u64;
    for b in noise.chunks(100_000) {
        rx.push(b, pos);
        pos += b.len() as u64;
    }
    assert_eq!(rx.status().locks, 0);
    assert!(rx.status().attempts >= 1);
}
