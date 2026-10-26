// made by CasualArclamp
//! Look for persistent narrowband tones in a capture: the long-term average
//! spectrum (Hann-windowed 8192-point transforms of the whole file or its
//! first seconds), each bin's power against the median of 128 around it.
//! Lists the bins over a threshold, in the channel and outside it, and the
//! DC bin on its own (the radio's local-oscillator leakage sits there).
//!
//! ```text
//! cargo run --release -p decdvbt-rx --example tone_scan -- SECONDS THRESHOLD_DB capture.cs8 [more.cs8 ...]
//! ```
//! 10 Msps. The capture's centre is taken as the channel's (the files are named for
//! their tuning), so the channel is the centre ±3.75 MHz.

use std::io::Read;

use decdvbt_core::{Iq, SampleFormat, bytes_to_iq};
use rustfft::FftPlanner;

const N: usize = 8192;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let seconds: f64 = a[1].parse().unwrap();
    let thr: f32 = a[2].parse().unwrap();
    for path in &a[3..] {
        scan(path, seconds, thr, 10e6);
    }
}

#[allow(clippy::needless_range_loop)]
fn scan(path: &str, seconds: f64, thr: f32, rate: f64) {
    let mut f = std::fs::File::open(path).unwrap();
    let fft = FftPlanner::new().plan_fft_forward(N);
    let win: Vec<f32> = (0..N)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * (i as f32 + 0.5) / N as f32).cos())
        .collect();
    let mut acc = vec![0f64; N];
    let mut blocks = 0u64;
    let mut raw = vec![0u8; N * 2 * 64];
    let mut iq: Vec<Iq> = Vec::new();
    let mut taken = 0u64;
    let limit = (seconds * rate) as u64;
    'outer: loop {
        let n = f.read(&mut raw).unwrap();
        if n < N * 2 {
            break;
        }
        bytes_to_iq(&raw[..n & !1], SampleFormat::Cs8, &mut iq);
        for ch in iq.as_chunks::<N>().0 {
            let mut buf: Vec<Iq> = ch.iter().zip(&win).map(|(&x, &w)| x * w).collect();
            fft.process(&mut buf);
            for (s, b) in acc.iter_mut().zip(&buf) {
                *s += f64::from(b.norm_sqr());
            }
            blocks += 1;
            taken += N as u64;
            if taken >= limit {
                break 'outer;
            }
        }
    }
    let spec: Vec<f32> = acc.iter().map(|&v| (v / blocks as f64) as f32).collect();
    // Median of 128 bins around each (in the order -fs/2..fs/2).
    let shifted: Vec<f32> = (0..N).map(|i| spec[(i + N / 2) % N]).collect();
    let mut med = vec![0f32; N];
    let mut tmp = Vec::new();
    for i in 0..N {
        let a = i.saturating_sub(64).min(N - 128);
        tmp.clear();
        tmp.extend_from_slice(&shifted[a..a + 128]);
        med[i] = *tmp.select_nth_unstable_by(64, f32::total_cmp).1;
    }
    let hz = |i: usize| (i as f64 - (N / 2) as f64) * rate / N as f64;
    println!(
        "{path}: {:.1} s, {blocks} transforms, bin {:.0} Hz",
        taken as f64 / rate,
        rate / N as f64
    );
    let dc = N / 2;
    let dc_over = 10.0
        * (shifted[dc - 1..=dc + 1]
            .iter()
            .cloned()
            .fold(0f32, f32::max)
            / med[dc])
            .log10();
    println!("  DC (centre) bin: {dc_over:+.1} dB over the local median");
    let mut found = 0;
    for i in 2..N - 2 {
        let over = 10.0 * (shifted[i] / med[i]).log10();
        let peak = (i - 2..=i + 2).all(|j| shifted[j] <= shifted[i]);
        if over >= thr && peak && (i as isize - dc as isize).abs() > 2 {
            let inband = hz(i).abs() < 3.75e6;
            println!(
                "  {:+10.0} Hz ({}) {over:5.1} dB over the median",
                hz(i),
                if inband { "IN CHANNEL" } else { "outside" }
            );
            found += 1;
        }
    }
    if found == 0 {
        println!("  no bin over {thr} dB");
    }
}
