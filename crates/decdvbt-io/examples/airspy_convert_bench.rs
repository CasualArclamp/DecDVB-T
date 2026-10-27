// made by CasualArclamp
//! Times the Airspy real-to-IQ converter on synthetic transfers.
//! `cargo run --release -p decdvbt-io --features airspy --example airspy_convert_bench`
use std::time::Instant;

use decdvbt_io::airspy::convert::RealToIq;

fn main() {
    // A noisy 4 MHz tone, as 262 144-byte unpacked transfers.
    let mut x = 1u32;
    let bytes: Vec<u8> = (0..131_072usize)
        .flat_map(|k| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = (x >> 24) as f64 - 128.0;
            let v = 2048.0 + 800.0 * (2.0 * std::f64::consts::PI * 0.2 * k as f64).cos() + n;
            (v as u16).to_le_bytes()
        })
        .collect();
    let mut conv = RealToIq::new();
    let mut out = Vec::new();
    for packed in [false, true] {
        let mut data = bytes.clone();
        if packed {
            // Same samples, packed (only the timing matters here).
            data.truncate(147_456);
        }
        for _ in 0..20 {
            out.clear();
            run(&mut conv, packed, &data, &mut out);
        }
        let reps = 600;
        let t = Instant::now();
        let mut n = 0usize;
        for _ in 0..reps {
            out.clear();
            run(&mut conv, packed, &data, &mut out);
            n += out.len();
        }
        let el = t.elapsed().as_secs_f64();
        let ns = el * 1e9 / n as f64;
        println!(
            "{}: {ns:.2} ns per output sample = {:.2} % of one core at 10 MS/s out ({:.1} MS/s out)",
            if packed { "packed  " } else { "unpacked" },
            ns * 1e-9 * 10e6 * 100.0,
            n as f64 / el / 1e6
        );
    }
}

fn run(conv: &mut RealToIq, packed: bool, data: &[u8], out: &mut Vec<decdvbt_core::Iq>) {
    if packed {
        conv.process_packed(data, out);
    } else {
        conv.process_unpacked(data, out);
    }
}
