// made by CasualArclamp
//! The T2 receiver's channel sounder: a T2 signal through a channel of two
//! paths, one of them moving, and its delay–Doppler map against them.

use decdvbt_mod::channel::{Channel, Path};
use decdvbt_mod::sim::{T2SimOptions, generate_t2};
use decdvbt_mod::t2::T2Config;
use decdvbt_rx::delay_doppler::DelayDoppler;
use decdvbt_rx::t2::receiver::{T2Config as RxConfig, T2Receiver};

/// The strongest value within ±`dd` µs and ±`dh` Hz of a point.
fn near(m: &DelayDoppler, us: f64, hz: f64, dd: f64, dh: f64) -> f32 {
    let mut best = f32::MIN;
    for r in 0..m.dopplers {
        if (m.doppler_hz(r) - hz).abs() > dh {
            continue;
        }
        for c in 0..m.delays {
            if (m.delay_us(c) - us).abs() <= dd {
                best = best.max(m.at(r, c));
            }
        }
    }
    best
}

#[test]
fn the_t2_map_shows_a_moving_echo() {
    // 8K, guard 1/16, PP4: pilots every 12th carrier a symbol in two.
    let cfg = T2Config::parse("8k-1/16-pp4-16qam-1/2-short").unwrap();
    let ch = Channel {
        paths: vec![
            Path::echo(0.0, 0.0),
            Path {
                doppler_hz: 8.0,
                ..Path::echo(6.0, -6.0)
            },
        ],
        cfo_hz: 30_000.0,
        cn_db: Some(28.0),
        ..Default::default()
    };
    let opts = T2SimOptions {
        cfg,
        ts_file: None,
        frames: 16,
        out_rate_hz: Some(10e6),
        level_dbfs: -20.0,
        channel: ch,
    };
    let mut rx = T2Receiver::new(RxConfig::new(10e6, 0.0));
    rx.set_sounding(true);
    let mut abs = 0u64;
    generate_t2(&opts, |b| {
        rx.push(b, abs);
        abs += b.len() as u64;
        Ok(())
    })
    .unwrap();
    let s = rx.sounder().expect("sounding");
    let st = s.status();
    let g = st.geometry.expect("a lock");
    assert!(st.tracking && st.coherence > 0.3, "{st:?}");
    assert_eq!((g.fft_name, g.guard_name, g.pilot_step), ("8K", "1/16", 12));
    let mut fading = Vec::new();
    s.take_fading(&mut fading);
    assert!(fading.len() > 100 && fading[0].len() == g.columns, "{}", fading.len());
    let m = s.delay_doppler().expect("a map").clone();
    // The main path at 0 µs and 0 Hz, the echo 6 dB down 6 µs on and 8 Hz
    // above, nothing at its mirror.
    let main = near(&m, 0.0, 0.0, 0.4, 1.0);
    assert!(main > -1.0, "main path {main} dB");
    let echo = near(&m, 6.0, 8.0, 0.5, 1.5);
    assert!((echo + 6.0).abs() < 2.5, "echo at {echo} dB");
    let mirror = near(&m, 6.0, -8.0, 0.5, 1.5);
    assert!(mirror < -20.0, "{mirror} dB at the mirror");
    assert!(near(&m, 25.0, 0.0, 2.0, 3.0) < -25.0);
}
