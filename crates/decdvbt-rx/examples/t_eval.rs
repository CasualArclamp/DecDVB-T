// made by CasualArclamp
//! Run the DVB-T receiver over a capture and print how it did: the Reed-Solomon
//! blocks, the bit error rate before them, the SNR, the tones the notch found,
//! and the CPU time (for before/after comparisons of the notch; set
//! `DECDVBT_NOTCH=0` to switch it off).
//!
//! ```text
//! cargo run --release -p decdvbt-rx --example t_eval -- capture.cs8 [rate_msps] [vfo_hz] [seconds]
//! ```

use std::io::Read;
use std::time::Instant;

use decdvbt_core::{Iq, SampleFormat, bytes_to_iq};
use decdvbt_rx::receiver::{Receiver, ReceiverConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("a capture");
    let rate: f64 = args.get(2).map_or(10.0, |v| v.parse().unwrap()) * 1e6;
    let vfo: f64 = args.get(3).map_or(0.0, |v| v.parse().unwrap());
    let seconds: f64 = args.get(4).map_or(1e9, |v| v.parse().unwrap());
    let mut f = std::fs::File::open(path).unwrap();
    let mut rx = Receiver::new(ReceiverConfig::new(rate, vfo));
    let mut raw = vec![0u8; 262_144];
    let mut iq: Vec<Iq> = Vec::new();
    let mut ts = Vec::new();
    let (mut pos, limit) = (0u64, (seconds * rate) as u64);
    let t0 = Instant::now();
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
    }
    let secs = t0.elapsed().as_secs_f64();
    let s = rx.status();
    let o = &s.outer;
    println!(
        "{:.1} s in {secs:.2} s ({:.1}x); locks {} fft {:?} snr {:.1} eff {:.1} dB | RS blocks {} clean {} fixed {} UNCORRECTABLE {} pre-RS BER {:.2e} packets {} | notch {:?} tone {:?}",
        pos as f64 / rate,
        pos as f64 / rate / secs,
        s.locks,
        s.fft,
        s.snr_db,
        s.effective_snr_db,
        o.blocks,
        o.clean,
        o.corrected,
        o.uncorrectable,
        o.pre_rs_ber(),
        o.packets,
        s.notch,
        s.tone
    );
    println!("profile {:?}", s.profile);
}
