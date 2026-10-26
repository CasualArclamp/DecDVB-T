// made by CasualArclamp
//! A CW tone inside the channel against the whole T2 receiver: a clean signal
//! flags nothing, and a tone a few dB under the signal's power (with which the
//! receiver would otherwise never acquire) is found, taken out and the
//! transport stream comes out right.

use decdvbt_core::Iq;
use decdvbt_mod::channel::Channel;
use decdvbt_mod::sim::{T2SimOptions, generate_t2};
use decdvbt_mod::t2::T2Config;
use decdvbt_mod::ts::check_test_payload;
use decdvbt_rx::t2::receiver::{T2Config as RxConfig, T2Receiver};

const RATE: f64 = 10e6;

/// Decode `samples`: (locks, good test packets, bad ones, tones cancelled,
/// carriers erased).
fn decode(samples: &[Iq]) -> (u32, usize, usize, u32, u32) {
    let mut rx = T2Receiver::new(RxConfig::new(RATE, 0.0));
    let mut ts = Vec::new();
    let mut pos = 0u64;
    let mut tones = 0;
    for block in samples.chunks(100_000) {
        rx.push(block, pos);
        pos += block.len() as u64;
        rx.take_ts(&mut ts);
        tones = tones.max(rx.status().tone.tones);
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
    (s.locks, good, bad, tones, s.notch.carriers)
}

fn signal() -> Vec<Iq> {
    let opts = T2SimOptions {
        cfg: T2Config::parse("8k-1/16-pp4-16qam-3/4-short-ext").unwrap(),
        ts_file: None,
        frames: 14,
        out_rate_hz: Some(RATE),
        level_dbfs: -18.0,
        channel: Channel {
            cn_db: Some(32.0),
            cfo_hz: 12_345.0,
            ..Default::default()
        },
    };
    let mut samples: Vec<Iq> = Vec::new();
    generate_t2(&opts, |b| {
        samples.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    samples
}

fn add_tone(x: &mut [Iq], db: f64, hz: f64) {
    let p = x.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / x.len() as f64;
    let amp = (p * 10f64.powf(db / 10.0)).sqrt() as f32;
    for (n, c) in x.iter_mut().enumerate() {
        // The phase is wrapped in f64 before it is narrowed: an f32 phase of
        // millions of radians has a resolution of a quarter of one.
        let ph = std::f64::consts::TAU * (hz * n as f64 / RATE).fract();
        *c += Iq::from_polar(amp, ph as f32);
    }
}

#[test]
fn a_clean_signal_has_nothing_flagged() {
    let (locks, good, bad, tones, erased) = decode(&signal());
    assert_eq!((locks, bad, tones), (1, 0, 0));
    assert!(good > 100);
    assert_eq!(erased, 0);
}

#[test]
fn strong_tones_are_cancelled_and_the_stream_comes_out_right() {
    for (db, hz) in [
        (-12.0, 1_000_300.0),
        (-4.0, -2_345_678.0),
        (3.0, 3_100_000.0),
    ] {
        let mut x = signal();
        add_tone(&mut x, db, hz);
        let (locks, good, bad, tones, _) = decode(&x);
        eprintln!("{db} dB at {hz} Hz: {locks} locks, good {good} bad {bad}, tones {tones}");
        assert_eq!(locks, 1, "{db} dB");
        assert!(tones >= 1, "{db} dB: no tone found");
        assert!(good > 100 && bad == 0, "{db} dB: good {good} bad {bad}");
    }
}
