// made by CasualArclamp
//! Where the time goes decoding one T2 FEC block: the demapper alone, and the
//! whole chain (demap, LDPC, BCH).
//!
//! ```text
//! cargo run --release -p decdvbt-fec --example t2_bench -- [qam] [rate] [short|normal] [rotated] [cn_db]
//! cargo run --release -p decdvbt-fec --example t2_bench -- 256 2/3 normal rot 22
//! ```

use std::time::Instant;

use decdvbt_core::{CodeRate, FecFrame, Iq};
use decdvbt_fec::t2::plp::PlpCoder;
use decdvbt_fec::t2::qam::{Constellation, Qam};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: &str| args.get(i).map_or(d.to_string(), |s| s.clone());
    let qam = match arg(1, "256").as_str() {
        "4" | "qpsk" => Qam::Qpsk,
        "16" => Qam::Qam16,
        "64" => Qam::Qam64,
        _ => Qam::Qam256,
    };
    let rate = {
        let r = arg(2, "2/3");
        let (a, b) = r.split_once('/').expect("a rate like 2/3");
        CodeRate::new(a.parse().unwrap(), b.parse().unwrap())
    };
    let frame = if arg(3, "normal") == "short" {
        FecFrame::Short
    } else {
        FecFrame::Normal
    };
    let rotated = arg(4, "rot") == "rot";
    let cn_db: f64 = arg(5, "22").parse().unwrap();

    ldpc_iteration_time(frame, rate);
    let mut coder = PlpCoder::new(frame, rate, qam, rotated).expect("a T2 code");
    let mut seed = 12345u32;
    let mut rnd = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / (1u32 << 24) as f32
    };
    let bb: Vec<u8> = (0..coder.bbframe_bytes())
        .map(|_| (rnd() * 256.0) as u8)
        .collect();
    let mut cells = Vec::new();
    coder.encode(&bb, &mut cells);
    // Gaussian-ish noise at the C/N asked for (unit-power cells).
    let sigma2 = 10f64.powf(-cn_db / 10.0) as f32;
    let sd = (sigma2 / 2.0).sqrt();
    let mut gauss = || {
        let a: f32 = (0..4).map(|_| rnd() - 0.5).sum::<f32>() * 1.732_050_8;
        a * sd
    };
    let noisy: Vec<Iq> = cells
        .iter()
        .map(|c| *c + Iq::new(gauss(), gauss()))
        .collect();
    let var = vec![sigma2; noisy.len()];

    let n = 20;
    let cst = Constellation::new(qam, rotated);
    let mut llr = vec![0i8; noisy.len() * qam.bits()];
    let t = Instant::now();
    for _ in 0..n {
        cst.demap_block_q(&noisy, &var, 8.0, &mut llr);
        std::hint::black_box(&llr);
    }
    let demap = t.elapsed().as_secs_f64() / n as f64;

    let mut out = vec![0u8; bb.len()];
    let t = Instant::now();
    let mut stats = None;
    for _ in 0..n {
        stats = coder.decode(&noisy, &var, &mut out);
    }
    let whole = t.elapsed().as_secs_f64() / n as f64;
    println!(
        "{} {} {:?}{}: C/N {cn_db} dB, {} cells a block",
        qam.name(),
        rate,
        frame,
        if rotated { " rotated" } else { "" },
        noisy.len()
    );
    println!(
        "demap {:.2} ms ({:.0} ns a cell); whole block {:.2} ms; the rest (LDPC, BCH, gather) {:.2} ms",
        demap * 1e3,
        demap * 1e9 / noisy.len() as f64,
        whole * 1e3,
        (whole - demap) * 1e3
    );
    println!("{:?}, bytes right: {}", stats, out == bb);
}

/// The LDPC decoder alone, on LLRs too noisy to converge, so that every one of
/// the iterations runs: the time of an iteration.
#[allow(dead_code)]
pub fn ldpc_iteration_time(frame: FecFrame, rate: CodeRate) {
    use decdvbt_fec::ldpc::{LdpcCode, LdpcDecoder};
    use decdvbt_fec::params::FecParams;
    let p = FecParams::new_t2(frame, rate).expect("a code");
    let mut dec = LdpcDecoder::new(LdpcCode::new(p.ldpc_table()));
    let mut seed = 99u32;
    let llr: Vec<i8> = (0..p.n_ldpc)
        .map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            ((seed >> 24) as i32 - 128).clamp(-60, 60) as i8
        })
        .collect();
    let mut info = vec![0u8; p.n_bch / 8];
    let iterations = 20;
    let t = Instant::now();
    let reps = 5;
    for _ in 0..reps {
        dec.decode(&llr, &mut info, iterations);
    }
    let per = t.elapsed().as_secs_f64() / (reps * iterations) as f64;
    // Without iterating: the load, one syndrome check and the read-out.
    let t = Instant::now();
    for _ in 0..reps * 4 {
        dec.decode(&llr, &mut info, 0);
    }
    let fixed = t.elapsed().as_secs_f64() / (reps * 4) as f64;
    println!(
        "LDPC {frame:?} {rate}: {:.0} us an iteration; {:.0} us to load, check once and read out",
        per * 1e6,
        fixed * 1e6
    );
}
