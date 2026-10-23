// made by CasualArclamp
//! A hardware test of tuning: opens the HackRF, then moves between channels
//! the way the GUI does (a retune, and a decode request a frame later) and
//! says for each whether the receiver locked and decoded, and how long it took.
//!
//! `cargo run --release -p decdvbt-gui --example retune_soak -- C23 C26 C28 C22 C25 …`
//! (channels as many as you like, tried in order, repeated `--rounds N` times;
//! `--wait SECS` per channel, default 8). Receive only, RF amp on, as the GUI.

use std::time::{Duration, Instant};

use decdvbt_core::channel::{parse_channel, uhf_centre_hz};
use decdvbt_engine::{Command, Rx, RxOptions};
use decdvbt_io::{HackRfGains, HackRfSettings, HackRfSource};

fn main() {
    let (mut channels, mut rounds, mut wait) = (Vec::new(), 1u32, 8.0f64);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rounds" => {
                rounds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--rounds N")
            }
            "--wait" => {
                wait = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--wait SECS")
            }
            c => channels.push(parse_channel(c).unwrap_or_else(|| panic!("not a channel: {c}"))),
        }
    }
    assert!(!channels.is_empty(), "give some channels, e.g. C23 C26");
    let first = uhf_centre_hz(channels[0]).expect("channel");
    let src = HackRfSource::open(HackRfSettings {
        center_hz: first.round() as u64,
        sample_rate: 10_000_000,
        gains: HackRfGains {
            amp: true,
            lna_db: 32,
            vga_db: 8,
        },
    })
    .expect("HackRF");
    let rx = Rx::start(Box::new(src), RxOptions::default(), None);
    let (mut tried, mut failed) = (0, 0);
    let mut at_first = true;
    for round in 0..rounds {
        for &ch in &channels {
            let fc = uhf_centre_hz(ch).expect("channel");
            if !at_first || round > 0 {
                rx.send(Command::Retune(fc));
            }
            at_first = false;
            // The GUI's decode request follows the retune by a frame or so.
            std::thread::sleep(Duration::from_millis(33));
            rx.send(Command::Decode(Some(fc)));
            let t0 = Instant::now();
            let mut done = None;
            let mut last = Default::default();
            while t0.elapsed().as_secs_f64() < wait {
                std::thread::sleep(Duration::from_millis(100));
                let u = rx.take_decoder();
                let st = u.status;
                if st.decoding && st.outer.clean + st.outer.corrected > 200 && st.snr_db > 8.0 {
                    done = Some(t0.elapsed().as_secs_f64());
                    last = st;
                    break;
                }
                last = st;
            }
            tried += 1;
            match done {
                Some(s) => println!(
                    "C{ch}: decoding after {s:.1} s (MER {:.1} dB, {} locks, {} stalled;                      acquisitions failed: {} no prefix, {} not DVB-T)",
                    last.snr_db,
                    last.locks,
                    last.stalled_locks,
                    last.acq_no_prefix,
                    last.acq_not_dvbt
                ),
                None => {
                    failed += 1;
                    println!(
                        "C{ch}: NO DECODE in {wait} s — tracking {} tps {} decoding {} MER {:.1} dB, \
                         RS {} good {} lost, {} locks, {} stalled",
                        last.tracking,
                        last.tps_synced,
                        last.decoding,
                        last.snr_db,
                        last.outer.clean + last.outer.corrected,
                        last.outer.uncorrectable,
                        last.locks,
                        last.stalled_locks
                    );
                    let st = rx.stats();
                    println!(
                        "      the radio: rms {:.1} dBFS, peak {:.0} %, clipping {:.2} %, {} samples, {} dropped;                          receiver: {} acquisition attempts, best cyclic-prefix contrast {:.1}",
                        st.rms_dbfs,
                        st.peak * 100.0,
                        st.clip_percent,
                        st.samples,
                        st.dropped,
                        last.attempts,
                        last.contrast
                    );
                }
            }
        }
    }
    println!("{tried} tunings, {failed} without a decode");
    // Stop the radio properly: `process::exit` skips destructors, and a
    // HackRF left receiving by one run sometimes delivers nothing to the next.
    drop(rx);
    std::thread::sleep(Duration::from_millis(300));
    std::process::exit(i32::from(failed > 0));
}
