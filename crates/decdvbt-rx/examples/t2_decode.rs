// made by CasualArclamp
//! Decode a DVB-T2 capture with the receiver: feed the file to it in blocks as
//! a radio would, write the transport stream, print what it knows.
//!
//! ```text
//! cargo run --release -p decdvbt-rx --example t2_decode -- captures/c30_546.000MHz_10Msps_amp.cs8 out/c30.ts [rate_msps] [vfo_hz] [seconds]
//! ```

use std::io::{Read, Write};
use std::time::Instant;

use decdvbt_core::{Iq, SampleFormat, bytes_to_iq};
use decdvbt_rx::t2::receiver::{T2Config, T2Receiver};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("a capture");
    let out = args.get(2).expect("an output .ts");
    let rate: f64 = args.get(3).map_or(10.0, |v| v.parse().unwrap()) * 1e6;
    let vfo: f64 = args.get(4).map_or(0.0, |v| v.parse().unwrap());
    let seconds: f64 = args.get(5).map_or(1e9, |v| v.parse().unwrap());

    let mut f = std::fs::File::open(path).unwrap();
    let mut o = std::fs::File::create(out).unwrap();
    let mut cfg = T2Config::new(rate, vfo);
    if let Ok(t) = std::env::var("THREADS") {
        cfg.fec_threads = t.parse().unwrap();
    }
    let mut rx = T2Receiver::new(cfg);
    let block = 131_072usize;
    let mut raw = vec![0u8; block * 2];
    let mut iq: Vec<Iq> = Vec::new();
    let mut pos = 0u64;
    let mut ts = Vec::new();
    let t0 = Instant::now();
    let limit = (seconds * rate) as u64;
    let mut last_report = 0u64;
    let (mut last_frames, mut last_blocks) = (0u64, 0u64);
    while pos < limit {
        let n = f.read(&mut raw).unwrap();
        if n < 2 {
            break;
        }
        bytes_to_iq(&raw[..n & !1], SampleFormat::Cs8, &mut iq);
        rx.push(&iq, pos);
        pos += iq.len() as u64;
        ts.clear();
        rx.take_ts(&mut ts);
        o.write_all(&ts).unwrap();
        {
            let s = rx.status();
            let frames = s.frames_ok + s.frames_bad;
            if frames != last_frames {
                println!(
                    "frame {frames}: ok {} blocks_ok {} (+{}) bad {} snr {:.1} dB, clock {:+.2} ppm cfo {:+.1}",
                    s.frames_ok,
                    s.blocks_ok,
                    s.blocks_ok - last_blocks,
                    s.blocks_bad,
                    s.snr_db,
                    s.clock_ppm,
                    s.cfo_hz
                );
                println!("    {}", s.debug);
                last_frames = frames;
                last_blocks = s.blocks_ok;
            }
        }
        if pos - last_report > (rate * 0.5) as u64 {
            last_report = pos;
            let s = rx.status();
            println!(
                "{:5.2} s: track {} l1 {} dec {} | frames {}/{} blocks {}/{} packets {} snr {:.1} dB cfo {:+.1} Hz clock {:+.2} ppm",
                pos as f64 / rate,
                s.tracking,
                s.l1_ok,
                s.decoding,
                s.frames_ok,
                s.frames_ok + s.frames_bad,
                s.blocks_ok,
                s.blocks_ok + s.blocks_bad,
                s.packets,
                s.snr_db,
                s.cfo_hz,
                s.clock_ppm
            );
        }
    }
    let s = rx.status();
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "done: {:.1} s of signal in {secs:.1} s ({:.2}x real time); {:?}\nprofile {:?}",
        pos as f64 / rate,
        pos as f64 / rate / secs,
        s.mode,
        s.profile
    );
    println!(
        "locks {}, attempts {}, no P1 {}, no L1 {}, frames ok {} bad {}, blocks ok {} bad {}, packets {}, resyncs {}, LDPC iterations {:.1} a block",
        s.locks,
        s.attempts,
        s.acq_no_p1,
        s.acq_no_l1,
        s.frames_ok,
        s.frames_bad,
        s.blocks_ok,
        s.blocks_bad,
        s.packets,
        s.resyncs,
        s.ldpc_iterations as f64 / s.blocks_ok.max(1) as f64
    );
}
