// made by CasualArclamp
//! The T2 frame equaliser against the modulator's own cells: a frame is made,
//! put through a channel, read, and every equalised cell compared with the
//! cell that was sent.

use decdvbt_core::Iq;
use decdvbt_mod::channel::{Channel, ChannelSim, Path};
use decdvbt_mod::t2::{T2Config, T2Modulator};
use decdvbt_mod::ts::TestStream;
use decdvbt_rx::t2::frame::{Demod, Geometry};

/// The mean error power over the mean power, dB, of cells against what was sent.
fn mer(rx: &[Iq], tx: &[Iq]) -> f32 {
    let e: f32 = rx.iter().zip(tx).map(|(a, b)| (*a - *b).norm_sqr()).sum();
    let p: f32 = tx.iter().map(|b| b.norm_sqr()).sum();
    10.0 * (p / e.max(1e-12)).log10()
}

fn noise(y: &mut [Iq], sigma2: f32, seed: u32) {
    let mut s = seed;
    let mut g = || {
        let mut a = 0.0;
        for _ in 0..4 {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            a += (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        }
        a * 1.7320508
    };
    let k = (sigma2 / 2.0).sqrt();
    for v in y.iter_mut() {
        *v += Iq::new(g(), g()) * k;
    }
}

fn run(cfg: T2Config, channel: Channel) -> (f32, Vec<f32>) {
    run_noisy(cfg, channel, 0.0, 1)
}

fn run_noisy(cfg: T2Config, channel: Channel, sigma2: f32, seed: u32) -> (f32, Vec<f32>) {
    let mut m = T2Modulator::new(cfg.clone(), Box::<TestStream>::default()).unwrap();
    let (mut a, mut b, mut c3) = (Vec::new(), Vec::new(), Vec::new());
    // Three frames: the first through the channel to warm it up, the second
    // read, the third the stretch of samples after it.
    m.next_frame(&mut a);
    m.next_frame(&mut b);
    let tx = m.last_cells.clone();
    m.next_frame(&mut c3);
    let mut sim = ChannelSim::new(&channel, decdvbt_mod::t2::SAMPLE_RATE_HZ, m.mean_power());
    let mut y = Vec::new();
    let mut tmp = Vec::new();
    sim.process(&a, &mut tmp);
    sim.process(&b, &mut y);
    let mut tail = Vec::new();
    sim.process(&c3, &mut tail);
    y.extend_from_slice(&tail[..4096]);
    if sigma2 > 0.0 {
        noise(&mut y, sigma2, seed);
    }
    let geo = Geometry::new(cfg.mode, cfg.data_symbols);
    let mut demod = Demod::new(geo);
    // The second frame's P1 starts at its first sample.
    let timing = demod.timing(&y, 2048.0).expect("timing");
    let mut carriers = demod.all_carriers(&y, &timing);
    let eq = demod.data_cells(&mut carriers).expect("equalised");
    let n_p2_cells = demod.carriers.p2_cells() * geo.n_p2;
    let tx_data = &tx[n_p2_cells..n_p2_cells + eq.cells.len()];
    let total = mer(&eq.cells, tx_data);
    // By symbol.
    let c = demod.carriers.data_cells();
    let per_symbol: Vec<f32> = (0..eq.cells.len() / c)
        .map(|j| mer(&eq.cells[j * c..(j + 1) * c], &tx_data[j * c..(j + 1) * c]))
        .collect();
    (total, per_symbol)
}

#[test]
fn a_clean_frame_is_equalised_to_the_noise_floor() {
    let cfg = T2Config::parse("32k-1/128-pp7-256qam-2/3-ext-rot").unwrap();
    let ch = Channel {
        cn_db: Some(35.0),
        ..Default::default()
    };
    let (total, per) = run(cfg, ch);
    eprintln!(
        "MER {total:.1} dB; worst symbol {:.1}",
        per.iter().cloned().fold(100.0, f32::min)
    );
    assert!(total > 25.0, "{total}");
}

#[test]
fn a_frame_through_two_paths_is_equalised_too() {
    let cfg = T2Config::parse("32k-1/128-pp7-256qam-2/3-ext-rot").unwrap();
    let ch = Channel {
        cn_db: Some(35.0),
        paths: vec![Path::echo(0.0, 0.0), Path::echo(6.0, -0.8)],
        ..Default::default()
    };
    let (total, per) = run(cfg, ch);
    eprintln!("MER {total:.1} dB; by symbol {per:.1?}");
    assert!(total > 20.0, "{total}");
}

#[test]
fn noise_at_30_db_leaves_a_mer_near_30_through_any_noise_draw() {
    let cfg = T2Config::parse("32k-1/128-pp7-256qam-2/3-ext-rot").unwrap();
    let ch = Channel {
        paths: vec![Path::echo(0.0, 0.0), Path::echo(6.0, -0.8)],
        ..Default::default()
    };
    for seed in 1..=6 {
        let (total, per) = run_noisy(cfg.clone(), ch.clone(), 1e-3, seed);
        let worst = per.iter().cloned().fold(100.0, f32::min);
        eprintln!("seed {seed}: MER {total:.1} dB, worst symbol {worst:.1}");
        assert!(total > 22.0, "seed {seed}: {total}");
    }
}

/// The frame's blocks decoded from the equalised cells.
fn decode_blocks(cfg: T2Config, channel: Channel, sigma2: f32, seed: u32) -> (usize, usize, usize) {
    use decdvbt_ofdm::t2::carriers::p2_deal;
    use decdvbt_rx::t2::plp::{PlpDecoder, PlpParams};
    let mut m = T2Modulator::new(cfg.clone(), Box::<TestStream>::default()).unwrap();
    let (mut a, mut b, mut c3) = (Vec::new(), Vec::new(), Vec::new());
    m.next_frame(&mut a);
    m.next_frame(&mut b);
    m.next_frame(&mut c3);
    let mut sim = ChannelSim::new(&channel, decdvbt_mod::t2::SAMPLE_RATE_HZ, m.mean_power());
    let mut y = Vec::new();
    let mut tmp = Vec::new();
    sim.process(&a, &mut tmp);
    sim.process(&b, &mut y);
    let mut tail = Vec::new();
    sim.process(&c3, &mut tail);
    y.extend_from_slice(&tail[..4096]);
    noise(&mut y, sigma2, seed);
    let (pre, post) = {
        let (a, b) = m.l1();
        (a.clone(), b.clone())
    };
    let geo = Geometry::new(cfg.mode, cfg.data_symbols);
    let mut demod = Demod::new(geo);
    let timing = demod.timing(&y, 2048.0).expect("timing");
    let mut carriers = demod.all_carriers(&y, &timing);
    let (dealt, dvar) = demod.p2_cells(&carriers[..geo.n_p2 * geo.c_ps]);
    let (logical, lvar) = demod.p2_logical(&dealt, &dvar, 1840, pre.l1_post_size as usize);
    let _ = p2_deal;
    let eq = demod.data_cells(&mut carriers).expect("equalised");
    let mut cells = logical;
    let mut var = lvar;
    cells.extend_from_slice(&eq.cells);
    var.extend_from_slice(&eq.var);
    let params = PlpParams::from_l1(&post.plps[0]).unwrap();
    let mut dec = PlpDecoder::new(params, 1).unwrap();
    let mut out = Vec::new();
    let st = dec
        .decode_frame(
            &cells,
            &var,
            1840 + pre.l1_post_size as usize,
            cfg.fec_blocks,
            &mut out,
        )
        .unwrap();
    (st.ok, st.blocks, out.len())
}

#[test]
fn every_block_of_a_noisy_two_path_frame_decodes() {
    let cfg = T2Config::parse("32k-1/128-pp7-256qam-2/3-ext-rot").unwrap();
    let ch = Channel {
        paths: vec![Path::echo(0.0, 0.0), Path::echo(6.0, -0.8)],
        ..Default::default()
    };
    let (ok, blocks, packets) = decode_blocks(cfg, ch, 1e-3, 3);
    eprintln!("{ok} of {blocks} blocks, {packets} packets");
    assert_eq!(ok, blocks);
}
