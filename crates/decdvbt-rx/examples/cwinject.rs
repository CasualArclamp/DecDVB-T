// made by CasualArclamp
//! Add a CW tone to a cs8 capture (for testing the narrowband-interference
//! notch, see `decdvbt_rx::notch`).
//!
//! ```text
//! cargo run --release -p decdvbt-rx --example cwinject -- in.cs8 out.cs8 --db -20 --hz 1.0e6 [--drift HZ_PER_S] [--rate 10e6] [--second HZ:DB]
//! ```
//!
//! `--db` is the tone's power over the capture's own mean power (signal and
//! noise); `--hz` its offset from the capture's centre; `--drift` how fast it
//! moves; `--second` adds another tone. The sum is scaled to leave headroom in
//! 8 bits (the whole file, signal and tone alike), so a strong tone costs the
//! signal some of its quantisation resolution, as it would an 8-bit radio.
//! Prints the scale used.

use std::io::Write;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (inp, outp) = (&a[1], &a[2]);
    let get = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
    let db: f64 = get("--db").map_or(-20.0, |v| v.parse().unwrap());
    let hz: f64 = get("--hz").map_or(1.0e6, |v| v.parse().unwrap());
    let drift: f64 = get("--drift").map_or(0.0, |v| v.parse().unwrap());
    let rate: f64 = get("--rate").map_or(10e6, |v| v.parse().unwrap());
    let second: Option<(f64, f64)> = get("--second").map(|v| {
        let (h, d) = v.split_once(':').unwrap();
        (h.parse().unwrap(), d.parse().unwrap())
    });

    // Pass 1: the mean power and the peak.
    let data = std::fs::read(inp).unwrap();
    let n = data.len() / 2;
    let p: f64 = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| f64::from(c[0] as i8).powi(2) + f64::from(c[1] as i8).powi(2))
        .sum::<f64>()
        / n as f64;
    let amp = (p * 10f64.powf(db / 10.0)).sqrt();
    let amp2 = second.map(|(_, d)| (p * 10f64.powf(d / 10.0)).sqrt());
    // A complex Gaussian-ish sum: peak is about rms · 4.5; keep it in ±120.
    let rms_total = (p * (1.0 + 10f64.powf(db / 10.0))).sqrt();
    let scale = (120.0 / (rms_total * 4.0)).min(1.0);
    eprintln!(
        "mean power {p:.1} (rms {:.1}), tone amplitude {amp:.1}, scale {scale:.3}",
        p.sqrt()
    );
    let mut out = Vec::with_capacity(data.len());
    let (mut ph, mut ph2) = (0f64, 0f64);
    let dt = 1.0 / rate;
    for (i, c) in data.as_chunks::<2>().0.iter().enumerate() {
        let f = hz + drift * i as f64 * dt;
        ph += std::f64::consts::TAU * f * dt;
        let (s, co) = ph.sin_cos();
        let mut re = f64::from(c[0] as i8) + amp * co;
        let mut im = f64::from(c[1] as i8) + amp * s;
        if let (Some((h2, _)), Some(a2)) = (second, amp2) {
            ph2 += std::f64::consts::TAU * h2 * dt;
            let (s2, c2) = ph2.sin_cos();
            re += a2 * c2;
            im += a2 * s2;
        }
        out.push((re * scale).round().clamp(-127.0, 127.0) as i8 as u8);
        out.push((im * scale).round().clamp(-127.0, 127.0) as i8 as u8);
    }
    let mut f = std::fs::File::create(outp).unwrap();
    f.write_all(&out).unwrap();
}
