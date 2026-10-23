// made by CasualArclamp
//! Diagnostics of the front end on a capture: timing, the equaliser's fits and
//! the quality of the data and TPS cells, symbol by symbol and by region of the
//! band. For finding out why a real signal does not decode (it found that a
//! HackRF with its RF amplifier off is noise-limited, and that the slope fit
//! was failing at low pilot SNR).
//!
//!   cargo run --release -p decdvbt-rx --example diag -- FILE.cs8 [options]
//!
//! Options are environment variables (a number unless said):
//!
//! - SECONDS: how much of the file to read (1); EVERY: print a symbol line
//!   every so many symbols (20).
//! - FINE=0: no pilot timing feedback; ACP, BCP: the cyclic-prefix loop's
//!   gains; EQA: the grid smoothing; CPE=0: no common-phase fit.
//! - NATIVE=1: the file is complex f32 already at the signal's own rate (made
//!   with scipy's resample_poly, say), to take the interpolator out of it.
//! - PSD=1: also print the capture's spectrum.
//! - DUMP=path (with DUMP_FROM, DUMP_TO): write the equalised data cells of
//!   those symbols as f32 records (symbol, carrier, re, im, |H|²); DUMPH=path
//!   (with DUMPH_AT): write the channel estimate of one symbol.

use std::io::{Read, Write};

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{Alpha, Bandwidth, Modulation};
use decdvbt_ofdm::mapping::Constellation;
use decdvbt_ofdm::pilots::{SymbolPlan, continual_pilots, tps_carriers};
use decdvbt_rx::chest::Equalizer;
use decdvbt_rx::interp::{Interpolator, Planar};
use decdvbt_rx::lock::lock_on;
use decdvbt_rx::symbol::SymbolReader;

fn env(name: &str, d: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

/// What is kept of each symbol, for the summary.
struct Sym {
    mer: f64,
    phase: usize,
    tau: f32,
}

fn read_samples(path: &str, seconds: f64, native: bool) -> Vec<Iq> {
    let mut raw = Vec::new();
    let mut f = std::fs::File::open(path).expect("cannot open the file");
    if native {
        f.read_to_end(&mut raw).unwrap();
        raw.as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                Iq::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect()
    } else {
        raw.resize(2 * (seconds * 10e6) as usize, 0);
        let got = f.read(&mut raw).unwrap();
        raw.truncate(got);
        raw.as_chunks::<2>()
            .0
            .iter()
            .map(|c| Iq::new(f32::from(c[0] as i8) / 128.0, f32::from(c[1] as i8) / 128.0))
            .collect()
    }
}

/// A Welch estimate: 2048-point Hann segments, 64 bins a line.
fn print_psd(x: &[Iq]) {
    use rustfft::FftPlanner;
    let nfft = 2048;
    let fft = FftPlanner::<f32>::new().plan_fft_forward(nfft);
    let win: Vec<f32> = (0..nfft)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / nfft as f32).cos())
        .collect();
    let mut acc = vec![0.0f64; nfft];
    let mut segs = 0;
    for seg in x.chunks_exact(nfft).step_by(7) {
        let mut b: Vec<Iq> = seg.iter().zip(&win).map(|(v, w)| v * *w).collect();
        fft.process(&mut b);
        for (a, v) in acc.iter_mut().zip(&b) {
            *a += f64::from(v.norm_sqr());
        }
        segs += 1;
    }
    println!("PSD over {segs} segments (dB, relative; -5 .. +5 MHz at 10 MS/s):");
    let step = 64;
    for g in 0..nfft / step {
        let bin0 = nfft / 2 + g * step;
        let m: f64 = (0..step).map(|j| acc[(bin0 + j) % nfft]).sum::<f64>() / (step * segs) as f64;
        let f_mhz =
            ((g * step) as f64 - (nfft / 2) as f64 + step as f64 / 2.0) * 10.0 / nfft as f64;
        let db = 10.0 * m.max(1e-12).log10();
        println!(
            "{f_mhz:+6.2} MHz {db:6.1} {}",
            "#".repeat(((db + 70.0).max(0.0) / 1.5) as usize)
        );
    }
}

fn dump_cells(path: &str, i: usize, plan: &SymbolPlan, phase: usize, eq: &Equalizer) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for &k in plan.data_carriers(phase) {
        let z = eq.z[k as usize];
        for v in [i as f32, k as f32, z.re, z.im, eq.w[k as usize]] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
    }
}

/// The power mean of the MER of the symbols `keep` selects, in dB (a plain mean
/// of dB would hide the bad symbols).
fn power_mean(stats: &[Sym], keep: impl Fn(&Sym) -> bool) -> (usize, f64) {
    let v: Vec<f64> = stats.iter().filter(|s| keep(s)).map(|s| s.mer).collect();
    let p = v.iter().map(|m| 10f64.powf(-m / 10.0)).sum::<f64>() / v.len().max(1) as f64;
    (v.len(), -10.0 * p.log10())
}

fn main() {
    let path = std::env::args().nth(1).expect("a .cs8 file");
    let native = env("NATIVE", 0.0) > 0.5;
    let x = read_samples(&path, env("SECONDS", 1.0), native);
    let mut planar = Planar::default();
    planar.set(&x);
    let mut head = Planar::default();
    head.set(&x[..70_000]);
    if env("PSD", 0.0) > 0.5 {
        print_psd(&x);
    }

    let interp = Interpolator::new();
    let fs_native = Bandwidth::Mhz8.sample_rate_hz();
    let rho = if native { 1.0 } else { 10e6 / fs_native };
    let l = lock_on(&interp, &head, 0, rho, fs_native, 0.0, 0.0).expect("lock");
    println!("{l:?}");
    let k_total = l.fft.active_carriers();
    let plan = SymbolPlan::new(l.fft);
    let cons = Constellation::new(Modulation::Qam64, Alpha::NonHierarchical);
    let tps: Vec<usize> = tps_carriers(l.fft).iter().map(|&k| k as usize).collect();
    let mut reader = SymbolReader::new(l.fft, l.guard, l.apex, l.f_total, rho, fs_native, 1);
    reader.alpha_cp = env("ACP", 0.05);
    reader.beta_cp = env("BCP", 0.0005);
    let fine = env("FINE", 1.0) > 0.5;
    let every = env("EVERY", 20.0) as usize;
    let mut eq = Equalizer::new(l.fft);
    eq.alpha = env("EQA", 0.5) as f32;
    eq.use_cpe = env("CPE", 1.0) > 0.5;

    let dump = std::env::var("DUMP").ok();
    let dump_range = (
        env("DUMP_FROM", 100.0) as usize,
        env("DUMP_TO", 140.0) as usize,
    );
    let dumph = std::env::var("DUMPH").ok();
    let n_cps = continual_pilots(l.fft).len();

    let mut carriers = Vec::new();
    let mut phase = l.phase;
    let mut i = 0usize;
    let mut prev_apex = l.apex;
    let mut stats: Vec<Sym> = Vec::new();
    let mut bands = [(0.0f64, 0usize); 12];
    let mut gains = [0.0f64; 12];
    let mut cp_err = vec![0.0f64; n_cps];
    let mut cp_n = 0usize;
    let mut cpe_a: Vec<f64> = Vec::new();

    while let Ok(info) = reader.read(&interp, &planar, 0, &mut carriers) {
        eq.process(&mut carriers, phase);
        let tau = eq.timing_error();
        if fine {
            reader.correct(f64::from(tau));
        }
        // The data cells against 64-QAM, and the TPS cells against BPSK.
        let (mut e, mut c) = (0.0f64, 0usize);
        for &k in plan.data_carriers(phase) {
            let z = eq.z[k as usize];
            let err = f64::from((z - cons.point(cons.nearest(z))).norm_sqr());
            e += err;
            c += 1;
            if i >= 30 {
                let b = (k as usize * 12 / k_total).min(11);
                bands[b].0 += err;
                bands[b].1 += 1;
            }
        }
        let mer = 10.0 * (c as f64 / e).log10();
        let te: f64 = tps
            .iter()
            .map(|&k| f64::from(eq.z[k].im.powi(2) + (eq.z[k].re.abs() - 1.0).powi(2)))
            .sum();
        let tps_mer = 10.0 * (tps.len() as f64 / te).log10();
        if i >= 30 {
            stats.push(Sym {
                mer,
                phase,
                tau: tau.abs(),
            });
            cpe_a.push(f64::from(eq.cpe.0));
            for (b, g) in gains.iter_mut().enumerate() {
                let (lo, hi) = (b * k_total / 12, (b + 1) * k_total / 12);
                *g += eq.w[lo..hi].iter().map(|&v| f64::from(v)).sum::<f64>() / (hi - lo) as f64;
            }
            if eq.cp_resid.len() == n_cps {
                for (j, r) in eq.cp_resid.iter().enumerate() {
                    cp_err[j] += f64::from((r - Iq::new(1.0, 0.0)).norm_sqr());
                }
                cp_n += 1;
            }
        }
        if i.is_multiple_of(every) {
            println!(
                "sym {i:4} ph {phase}: MER {mer:5.1} dB, TPS {tps_mer:5.1} dB, CP SNR {:5.1} dB, \
                 coherence {:.2}, |H|^2 {:5.1} dB, cpe ({:+.2} rad, {:+.2} samples), \
                 apex step {:+.2}, rate {:+.1} ppm, cfo {:+.1} Hz",
                -10.0 * eq.cp_nsr.max(1e-9).log10(),
                eq.coherence,
                10.0 * (eq.w.iter().sum::<f32>() / eq.w.len() as f32).log10(),
                eq.cpe.0,
                tau,
                info.apex - prev_apex - reader.per as f64,
                reader.rate * 1e6,
                reader.f_total,
            );
        }
        if let Some(p) = &dump
            && (dump_range.0..dump_range.1).contains(&i)
        {
            dump_cells(p, i, &plan, phase, &eq);
        }
        if let Some(p) = &dumph
            && i == env("DUMPH_AT", 200.0) as usize
        {
            let mut f = std::fs::File::create(p).unwrap();
            for h in &eq.h {
                f.write_all(&h.re.to_le_bytes()).unwrap();
                f.write_all(&h.im.to_le_bytes()).unwrap();
            }
        }
        prev_apex = info.apex;
        phase = (phase + 1) % 4;
        i += 1;
    }

    // ---- the summary
    println!("{i} symbols read");
    for ph in 0..4 {
        let (n, m) = power_mean(&stats, |s| s.phase == ph);
        println!("MER (power mean) at phase {ph}: {m:.1} dB over {n} symbols");
    }
    for (lo, hi) in [(0.0, 0.2), (0.2, 0.5), (0.5, 1.0), (1.0, 9.0)] {
        let (n, m) = power_mean(&stats, |s| s.tau >= lo && s.tau < hi);
        println!("MER by |timing error| in [{lo}, {hi}) samples: {m:.1} dB over {n} symbols");
    }
    println!(
        "MER by twelfth of the band: {}",
        bands
            .iter()
            .map(|b| format!("{:.1}", 10.0 * (b.1 as f64 / b.0.max(1e-9)).log10()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let n = stats.len().max(1) as f64;
    println!(
        "|H|^2 by twelfth (dB): {}",
        gains
            .iter()
            .map(|g| format!("{:.1}", 10.0 * (g / n).log10()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if cp_n > 0 {
        let cps = continual_pilots(l.fft);
        println!("continual pilots' error against their reference (dB), by group of 12:");
        for g in 0..cps.len() / 12 {
            let (lo, hi) = (g * 12, (g + 1) * 12);
            let one = |j: usize| 10.0 * (cp_err[j] / cp_n as f64).max(1e-12).log10();
            let all: f64 = cp_err[lo..hi].iter().sum::<f64>() / (12 * cp_n) as f64;
            println!(
                "  carriers {:4}..{:4}: {:6.1}   ({})",
                cps[lo],
                cps[hi - 1],
                10.0 * all.max(1e-12).log10(),
                (lo..hi)
                    .map(|j| format!("{:.0}", one(j)))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }
    // The common phase's second difference is what a smooth carrier-offset
    // error cannot explain: phase noise, and noise in the fit.
    let mut un = cpe_a.clone();
    for j in 1..un.len() {
        let mut d = un[j] - un[j - 1];
        d -= (d / std::f64::consts::TAU).round() * std::f64::consts::TAU;
        un[j] = un[j - 1] + d;
    }
    let d2: Vec<f64> = (2..un.len())
        .map(|j| un[j] - 2.0 * un[j - 1] + un[j - 2])
        .collect();
    let rms = (d2.iter().map(|v| v * v).sum::<f64>() / d2.len().max(1) as f64).sqrt() / 6f64.sqrt();
    println!(
        "common phase jitter {rms:.3} rad ({:.1} dB)",
        20.0 * rms.max(1e-9).log10()
    );
}
