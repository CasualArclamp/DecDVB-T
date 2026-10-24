// made by CasualArclamp
//! The T2 frame equaliser's time on one UK frame (32K, PP7, two paths), run
//! again and again: the fastest of the runs, which other work on the machine
//! can only make slower, and the parts of it.
//!
//! `cargo run --release -p decdvbt-rx --example t2_eq_bench -- [runs]`

use std::time::{Duration, Instant};

use decdvbt_mod::channel::{Channel, ChannelSim, Path};
use decdvbt_mod::t2::{T2Config, T2Modulator};
use decdvbt_mod::ts::TestStream;
use decdvbt_rx::t2::frame::{Demod, DemodProfile, Geometry};

fn main() {
    let runs: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(40);
    let cfg = T2Config::uk();
    let mut m = T2Modulator::new(cfg.clone(), Box::<TestStream>::default()).unwrap();
    let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
    m.next_frame(&mut a);
    m.next_frame(&mut b);
    m.next_frame(&mut c);
    let ch = Channel {
        cn_db: Some(30.0),
        paths: vec![Path::echo(0.0, 0.0), Path::echo(6.0, -6.0)],
        ..Default::default()
    };
    let mut sim = ChannelSim::new(&ch, decdvbt_mod::t2::SAMPLE_RATE_HZ, m.mean_power());
    let (mut tmp, mut y, mut tail) = (Vec::new(), Vec::new(), Vec::new());
    sim.process(&a, &mut tmp);
    sim.process(&b, &mut y);
    sim.process(&c, &mut tail);
    y.extend_from_slice(&tail[..4096]);

    let mut demod = Demod::new(Geometry::new(cfg.mode, cfg.data_symbols));
    let timing = demod.timing(&y, 2048.0).expect("timing");
    let carriers = demod.all_carriers(&y, &timing);
    let (mut cells, mut var) = (Vec::new(), Vec::new());
    let mut best = (Duration::MAX, DemodProfile::default());
    for _ in 0..runs {
        let mut work = carriers.clone();
        cells.clear();
        var.clear();
        demod.prof = DemodProfile::default();
        let t0 = Instant::now();
        let eq = demod.data_cells_into(&mut work, false, &mut cells, &mut var);
        let took = t0.elapsed();
        assert!(eq.is_some());
        if took < best.0 {
            best = (took, demod.prof);
        }
    }
    println!(
        "fastest of {runs}: {:.2} ms a frame, {} cells; {:?}",
        best.0.as_secs_f64() * 1e3,
        cells.len(),
        best.1
    );
}
