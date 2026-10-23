// made by CasualArclamp
//! The C/N one T2 mode needs, over a grid of decoder tunings: the soft bits'
//! scale, the min-sum normalization and the iteration limit.
//!
//! ```text
//! cargo run --release -p decdvbt-fec --example t2_tune -- 256 2/3 normal rot
//! ```

use decdvbt_core::{CodeRate, FecFrame, Iq};
use decdvbt_fec::t2::plp::PlpCoder;
use decdvbt_fec::t2::qam::Qam;

struct Rng(u32);

impl Rng {
    fn uniform(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        (self.0 >> 8) as f32 / (1u32 << 24) as f32
    }

    fn gauss(&mut self) -> f32 {
        (0..4).map(|_| self.uniform() - 0.5).sum::<f32>() * 1.732_050_8
    }
}

fn decodes(coder: &mut PlpCoder, cn_db: f64, blocks: usize, rng: &mut Rng) -> (bool, f64) {
    let sigma2 = 10f64.powf(-cn_db / 10.0) as f32;
    let sd = (sigma2 / 2.0).sqrt();
    let mut cells = Vec::new();
    let mut iterations = 0usize;
    for _ in 0..blocks {
        let bb: Vec<u8> = (0..coder.bbframe_bytes())
            .map(|_| (rng.uniform() * 256.0) as u8)
            .collect();
        coder.encode(&bb, &mut cells);
        let noisy: Vec<Iq> = cells
            .iter()
            .map(|c| *c + Iq::new(rng.gauss() * sd, rng.gauss() * sd))
            .collect();
        let var = vec![sigma2; noisy.len()];
        let mut out = vec![0u8; bb.len()];
        match coder.decode(&noisy, &var, &mut out) {
            Some(st) if st.ldpc.converged && out == bb => iterations += st.ldpc.iterations,
            _ => return (false, 0.0),
        }
    }
    (true, iterations as f64 / blocks as f64)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: &str| args.get(i).cloned().unwrap_or(d.to_string());
    let qam = match arg(1, "256").as_str() {
        "4" => Qam::Qpsk,
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
    let mut rng = Rng(7);
    println!("{} {} {:?} rotated {}", qam.name(), rate, frame, rotated);
    println!("scale shift iterations -> threshold dB (iterations a block there + 1 dB)");
    for &(scale, shift, its) in &[
        (4.0, 3, 50),
        (8.0, 3, 50),
        (8.0, 4, 50),
        (8.0, 5, 50),
        (8.0, 4, 100),
        (16.0, 4, 50),
        (2.0, 4, 50),
        (1.0, 4, 50),
        (1.0, 3, 50),
        (0.5, 4, 50),
        (0.5, 3, 50),
        (0.25, 4, 50),
    ] {
        let mut coder = PlpCoder::new(frame, rate, qam, rotated).expect("a code");
        coder.set_tuning(scale, shift, its);
        let (mut lo, mut hi) = (-3.0f64, 32.0f64);
        while hi - lo > 0.1 {
            let mid = 0.5 * (lo + hi);
            if decodes(&mut coder, mid, 6, &mut rng).0 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        let (_, its_a) = decodes(&mut coder, hi + 1.0, 6, &mut rng);
        println!("{scale:5} {shift} {its:4} -> {hi:.1} dB ({its_a:.1})");
    }
}
