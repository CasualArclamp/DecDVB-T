// made by CasualArclamp
//! Doppler and channel-change measurements on a capture, window by window:
//! what a moving receiver sees.
//!
//!   cargo run --release -p decdvbt-rx --example doppler -- FILE.cs8 [START_S [DUR_S]]
//!
//! For each window of WIN symbols (env, 256) it reads the symbols with the
//! receiver's own tracker and equaliser (locking on the capture at START_S)
//! and prints:
//!
//! - the **Doppler spectrum** of the continual pilots (each is seen every
//!   symbol, so the span is ±1/(2·Ts), about ±540 Hz in 8k): the pilots' time
//!   series, Hann-windowed and transformed, the power averaged over the
//!   pilots. From it the noise floor (the median of the outer half of the
//!   span), the power above it, its mean and RMS spread, and the width that
//!   holds 99 % of it;
//! - the **ICI** that spread implies: (2π·σ·Tu)²/12 relative to the signal
//!   (the first-order leakage of a carrier whose gain changes over the
//!   useful symbol), a derived figure, not a measurement of the ICI itself;
//! - the data cells' MER against 64-QAM (a cell's error clipped at CLIP, 0.1,
//!   so that the few carriers in the radio's DC notch cannot be the figure),
//!   and at the end by twelfth of the band.
//!
//! The capture goes through the receiver's DC removal first. Env: WIN
//! (symbols), EQA (the IIR's smoothing), MOBILE=0|1 and DENOISE=0|1 (force the
//! time filter and the delay-domain filter, see `mobile.rs`), FIT=1 (print the
//! filter's fit to the correlation), PROFILE=1 (the Doppler spectrum, ±100 Hz),
//! SYMS=1 (a line a symbol), HPROF=1 and HTRACE=1 (|H|² by 24th of the band, and
//! at ten carriers symbol by symbol), FINE=1 and CENTRE=1 (MER by 64 carriers,
//! and carrier by carrier round the centre).

use std::io::{Read, Seek, SeekFrom};

use decdvbt_core::Iq;
use decdvbt_core::dvbt::{Alpha, Bandwidth, Modulation};
use decdvbt_ofdm::mapping::Constellation;
use decdvbt_ofdm::pilots::{SymbolPlan, continual_pilots, prbs};
use decdvbt_rx::chest::Equalizer;
use decdvbt_rx::interp::{Interpolator, Planar};
use decdvbt_rx::lock::lock_on;
use decdvbt_rx::symbol::SymbolReader;
use rustfft::FftPlanner;

fn env(name: &str, d: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = &a[1];
    let start_s: f64 = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let dur_s: f64 = a.get(3).and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let win = env("WIN", 256.0) as usize;
    let mut f = std::fs::File::open(path).unwrap();
    f.seek(SeekFrom::Start((start_s * 10e6) as u64 * 2))
        .unwrap();
    let mut raw = vec![0u8; 2 * (dur_s * 10e6) as usize];
    let got = f.read(&mut raw).unwrap();
    raw.truncate(got);
    let x: Vec<Iq> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| Iq::new(f32::from(c[0] as i8) / 128.0, f32::from(c[1] as i8) / 128.0))
        .collect();
    // The receiver's DC removal (a HackRF's centre spike would otherwise
    // spoil the carriers around it).
    let mut x = x;
    decdvbt_dsp::dc::DcBlocker::new(10e6).process(&mut x);
    let mut planar = Planar::default();
    planar.set(&x);
    let interp = Interpolator::new();
    let fs_native = Bandwidth::Mhz8.sample_rate_hz();
    let rho = 10e6 / fs_native;
    let mut head = Planar::default();
    head.set(&x[..70_000]);
    let l = lock_on(&interp, &head, 0, rho, fs_native, 0.0, 200.0).expect("lock");
    println!("{l:?}");
    let k_total = l.fft.active_carriers();
    let plan = SymbolPlan::new(l.fft);
    let cons = Constellation::new(Modulation::Qam64, Alpha::NonHierarchical);
    let mut reader = SymbolReader::new(l.fft, l.guard, l.apex, l.f_total, rho, fs_native, 1);
    reader.alpha_cp = 0.05;
    reader.beta_cp = 0.0005;
    let mut eq = Equalizer::new(l.fft);
    eq.alpha = env("EQA", 0.5) as f32;
    eq.diagnostics = env("HPROF", 0.0) > 0.5;
    eq.set_guard(l.guard);
    eq.set_denoise(env("DENOISE", 0.0) > 0.5);
    if let Ok(v) = std::env::var("MOBILE") {
        eq.forced = Some(v == "1");
    }
    let cps: Vec<usize> = continual_pilots(l.fft)
        .iter()
        .map(|&k| k as usize)
        .collect();
    let inv: Vec<f32> = prbs(k_total)
        .iter()
        .map(|&w| 0.75 * (1.0 - 2.0 * f32::from(w)))
        .collect();
    let ts = (l.fft.n() + l.fft.n() / l.guard.divisor()) as f64 / fs_native;
    let tu = l.fft.n() as f64 / fs_native;
    let fft = FftPlanner::<f32>::new().plan_fft_forward(win);
    let hann: Vec<f32> = (0..win)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * (i as f32 + 0.5) / win as f32).cos())
        .collect();
    let hann_pow: f32 = hann.iter().map(|v| v * v).sum();
    let mut series: Vec<Vec<Iq>> = vec![Vec::new(); cps.len()];
    let (mut mer_e, mut mer_c) = (0.0f64, 0usize);
    let mut carriers = Vec::new();
    let mut phase = l.phase;
    let mut i = 0usize;
    let mut wi = 0usize;
    // A cell whose error is beyond this is a hard error whatever it is: it is
    // counted at the clip, so that a few carriers where the channel is nearly
    // nothing (the radio's DC notch) cannot be the whole of a band's figure.
    let err_clip = env("CLIP", 0.1);
    let mut bands = [(0.0f64, 0usize); 12];
    let mut centre = vec![(0.0f64, 0usize); 81];
    let mut fine = vec![(0.0f64, 0usize); k_total / 64 + 1];
    let mut eq_time = std::time::Duration::ZERO;
    // As the receiver reads: straight from the radio's samples when the rate
    // allows (10 MS/s does), else through the interpolator (env INTERP=1).
    let direct = reader.is_direct() && env("INTERP", 0.0) < 0.5;
    loop {
        let ok = if direct {
            reader.read_direct(&x, 0, &mut carriers).is_ok()
        } else {
            reader.read(&interp, &planar, 0, &mut carriers).is_ok()
        };
        if !ok {
            break;
        }
        let raw_cp: Vec<Iq> = cps.iter().map(|&k| carriers[k] * inv[k]).collect();
        let t0 = std::time::Instant::now();
        eq.process(&mut carriers, phase);
        eq_time += t0.elapsed();
        reader.correct(f64::from(eq.timing_error()));
        if env("HTRACE", 0.0) > 0.5 && i >= 300 && i < 300 + env("HN", 120.0) as usize {
            let ks = [
                200usize, 1000, 2000, 2800, 3300, 3500, 3700, 3900, 4500, 6000,
            ];
            let mut line = format!("h {i:4} ph{phase}");
            for k in ks {
                line += &format!(" {:5.1}", 10.0 * eq.h[k].norm_sqr().max(1e-12).log10());
            }
            println!("{line}");
        }
        if i >= 30 {
            for (s, v) in series.iter_mut().zip(&raw_cp) {
                s.push(*v);
            }
            let mut e = 0.0f64;
            for &k in plan.data_carriers(phase) {
                let z = eq.z[k as usize];
                let err = f64::from((z - cons.point(cons.nearest(z))).norm_sqr()).min(err_clip);
                e += err;
                mer_c += 1;
                let b = (k as usize * 12 / k_total).min(11);
                bands[b].0 += err;
                bands[b].1 += 1;
                if (k as usize).abs_diff(3408) <= 40 {
                    let j = k as usize + 40 - 3408;
                    centre[j].0 += err;
                    centre[j].1 += 1;
                }
                fine[k as usize / 64].0 += err;
                fine[k as usize / 64].1 += 1;
            }
            mer_e += e;
            if env("SYMS", 0.0) > 0.5 {
                println!(
                    "sym {i} MER {:.1} coh {:.2} cpe {:+.2}",
                    10.0 * (plan.data_carriers(phase).len() as f64 / e.max(1e-12)).log10(),
                    eq.coherence,
                    eq.cpe.0
                );
            }
        }
        if series[0].len() == win {
            let mut psd = vec![0.0f64; win];
            for s in &series {
                let mut b: Vec<Iq> = s.iter().zip(&hann).map(|(v, w)| *v * *w).collect();
                fft.process(&mut b);
                for (p, v) in psd.iter_mut().zip(&b) {
                    *p += f64::from(v.norm_sqr());
                }
            }
            let norm = f64::from(cps.len() as f32 * hann_pow);
            for p in &mut psd {
                *p /= norm;
            }
            let df = 1.0 / (win as f64 * ts);
            let fq = |b: usize| {
                (if b <= win / 2 {
                    b as f64
                } else {
                    b as f64 - win as f64
                }) * df
            };
            let mut outer: Vec<f64> = (0..win)
                .filter(|&b| fq(b).abs() > 0.25 / ts)
                .map(|b| psd[b])
                .collect();
            outer.sort_by(|a, b| a.total_cmp(b));
            let noise = outer[outer.len() / 2];
            let sig: f64 = psd.iter().map(|p| (p - noise).max(0.0)).sum();
            let noise_all = noise * win as f64;
            let mean_f: f64 = (0..win)
                .map(|b| fq(b) * (psd[b] - noise).max(0.0))
                .sum::<f64>()
                / sig.max(1e-30);
            let rms: f64 = ((0..win)
                .map(|b| (fq(b) - mean_f).powi(2) * (psd[b] - noise).max(0.0))
                .sum::<f64>()
                / sig.max(1e-30))
            .sqrt();
            let ici = rms * rms * (std::f64::consts::TAU * tu).powi(2) / 12.0;
            let mut dev: Vec<(f64, f64)> = (0..win)
                .map(|b| ((fq(b) - mean_f).abs(), (psd[b] - noise).max(0.0)))
                .collect();
            dev.sort_by(|a, b| a.0.total_cmp(&b.0));
            let (mut acc, mut w99) = (0.0, 0.0);
            for (d, p) in &dev {
                acc += p;
                w99 = *d;
                if acc >= 0.99 * sig {
                    break;
                }
            }
            let near: f64 = dev
                .iter()
                .filter(|(d, _)| *d < 2.0 * df)
                .map(|(_, p)| p)
                .sum();
            if env("PROFILE", 0.0) > 0.5 {
                // power per Doppler bin in dB against the noise median
                let mut line = String::new();
                for b in (win - 24..win).chain(0..25) {
                    line += &format!("{:+.0}:{:.0} ", fq(b), 10.0 * (psd[b] / noise).log10());
                }
                println!("  profile {line}");
            }
            if let Some(d) = eq.doppler_hz(ts) {
                println!(
                    "  tracker: Doppler {d:.1} Hz, time filter {}, modelled estimate error (IIR, Wiener) {:?} dB",
                    eq.mobile(),
                    eq.estimate_error_db()
                );
            }
            if env("HPROF", 0.0) > 0.5 {
                let mut line = String::new();
                for b in 0..24 {
                    let (lo, hi) = (b * k_total / 24, (b + 1) * k_total / 24);
                    let p: f32 =
                        eq.h[lo..hi].iter().map(|v| v.norm_sqr()).sum::<f32>() / (hi - lo) as f32;
                    line += &format!("{:.0} ", 10.0 * p.max(1e-12).log10());
                }
                println!("  |H|^2 dB per 24th: {line}");
            }
            if env("FIT", 0.0) > 0.5 {
                println!("  fit {:?}", eq.filter_fit());
            }
            let mer = 10.0 * (mer_c as f64 / mer_e.max(1e-12)).log10();
            println!(
                "t {:6.2}s  pilot SNR {:5.1} dB  mean Doppler {:+6.1} Hz  rms spread {:5.1} Hz  \
                 99% half-width {:5.1} Hz  power within 2 bins {:4.1}%  ICI {:6.1} dB  data MER {:5.1} dB",
                start_s + i as f64 * ts,
                10.0 * (sig / noise_all.max(1e-30)).max(1e-9).log10(),
                mean_f,
                rms,
                w99,
                100.0 * near / sig.max(1e-30),
                10.0 * ici.max(1e-12).log10(),
                mer,
            );
            for s in &mut series {
                s.clear();
            }
            mer_e = 0.0;
            mer_c = 0;
            wi += 1;
        }
        phase = (phase + 1) % 4;
        i += 1;
    }
    if env("CENTRE", 0.0) > 0.5 {
        println!(
            "MER by carrier around the centre (3368..3448): {}",
            centre
                .iter()
                .map(|b| format!("{:.0}", 10.0 * (b.1.max(1) as f64 / b.0.max(1e-9)).log10()))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    if env("FINE", 0.0) > 0.5 {
        println!(
            "MER by 64 carriers: {}",
            fine.iter()
                .map(|b| format!("{:.0}", 10.0 * (b.1 as f64 / b.0.max(1e-9)).log10()))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    println!(
        "MER by twelfth of the band: {}",
        bands
            .iter()
            .map(|b| format!("{:.1}", 10.0 * (b.1 as f64 / b.0.max(1e-9)).log10()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!(
        "{i} symbols, {wi} windows; equaliser {:.1} us a symbol",
        eq_time.as_secs_f64() * 1e6 / i.max(1) as f64
    );
}
