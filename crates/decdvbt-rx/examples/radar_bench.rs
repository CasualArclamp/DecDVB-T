// made by CasualArclamp
//! The radar's per-symbol profile alone: µs a symbol (fastest of several
//! runs), 8K 64-QAM, and the share of a core at 8K 1/32's symbol rate.
//!
//! `cargo run --release -p decdvbt-rx --example radar_bench`

use std::time::Instant;

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{FftSize, Modulation};
use decdvbt_ofdm::pilots::SymbolPlan;
use decdvbt_rx::radar::Profiler;

fn main() {
    let (n, k) = (8192, 6817);
    let fs = 64e6 / 7.0;
    let slot = (n + n / 32) as f64 / fs;
    let plan = SymbolPlan::new(FftSize::K8);
    // Cells spread over the 64-QAM grid, a little noise.
    let mut s = 12345u64;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 40) as f32 / (1u64 << 24) as f32 - 0.5
    };
    let z: Vec<Iq> = (0..k).map(|_| Iq::new(rnd() * 2.4, rnd() * 2.4)).collect();
    let raw: Vec<Iq> = z.iter().map(|&c| c * Iq::new(0.7, 0.3)).collect();
    let inv = vec![0.75f32; k];
    let mut p = Profiler::new(n, k, slot, fs);
    let mut out = Vec::new();
    let mut best = f64::MAX;
    for run in 0..7 {
        let t = Instant::now();
        for l in 0..1000u64 {
            p.push(
                run * 1000 + l,
                &raw,
                &z,
                &plan,
                l as usize,
                &inv,
                Modulation::Qam64,
            );
        }
        best = best.min(t.elapsed().as_secs_f64() / 1000.0);
        out.clear();
        p.take(&mut out);
    }
    println!(
        "{:.1} µs a symbol: {:.1} % of a core at {:.0} symbols a second",
        best * 1e6,
        100.0 * best / slot,
        1.0 / slot
    );
}
