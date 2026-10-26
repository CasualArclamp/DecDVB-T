// made by CasualArclamp
//! Inject a CW tone into a cs8 capture in memory, over a list of tone powers,
//! and decode with the narrowband notch off and on: the numbers for
//! docs/STATUS.md. DVB-T (`t`) or DVB-T2 (`t2`).
//!
//! ```text
//! cargo run --release -p decdvbt-rx --example notch_sweep -- t|t2 capture.cs8 --db -30,-20,-10,0,10 [--hz 1000300] [--drift HZ_PER_S] [--seconds S] [--noise DB (white noise added, dB under the capture's mean power)]
//! ```
//! The tone's power is relative to the capture's mean power. Powers of `none`
//! run the capture as it is.

use decdvbt_core::{Iq, SampleFormat, bytes_to_iq};
use decdvbt_rx::receiver::{Receiver, ReceiverConfig};
use decdvbt_rx::t2::receiver::{T2Config, T2Receiver};

const RATE: f64 = 10e6;

/// 0: everything off; 1: the carrier erasure; 2: erasure and the tone canceller.
fn set_mode(m: u8) {
    unsafe {
        std::env::set_var("DECDVBT_NOTCH", if m >= 1 { "1" } else { "0" });
        std::env::set_var("DECDVBT_TONE", if m >= 2 { "1" } else { "0" });
    }
}

fn inject(data: &[u8], p: f64, db: f64, hz: f64, drift: f64, noise_db: Option<f64>) -> Vec<u8> {
    let amp = (p * 10f64.powf(db / 10.0)).sqrt();
    let rms_total = (p * (1.0 + 10f64.powf(db / 10.0))).sqrt();
    // Headroom in 8 bits: the sum is scaled down when a strong tone needs it.
    let scale = (120.0 / (rms_total * 4.0)).min(1.0);
    let mut out = Vec::with_capacity(data.len());
    let mut ph = 0f64;
    let mut seed = 12345u64;
    let sigma = noise_db.map_or(0.0, |n| (p * 10f64.powf(n / 10.0) / 2.0).sqrt());
    let mut gauss = move || {
        let mut s = 0.0;
        for _ in 0..12 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s += (seed >> 11) as f64 / (1u64 << 53) as f64;
        }
        s - 6.0
    };
    let dt = 1.0 / RATE;
    for (i, c) in data.as_chunks::<2>().0.iter().enumerate() {
        ph += std::f64::consts::TAU * (hz + drift * i as f64 * dt) * dt;
        let (s, co) = ph.sin_cos();
        let re = f64::from(c[0] as i8) + amp * co + sigma * gauss();
        let im = f64::from(c[1] as i8) + amp * s + sigma * gauss();
        out.push((re * scale).round().clamp(-127.0, 127.0) as i8 as u8);
        out.push((im * scale).round().clamp(-127.0, 127.0) as i8 as u8);
    }
    out
}

fn blocks(data: &[u8]) -> impl Iterator<Item = Vec<Iq>> + '_ {
    data.chunks(262_144).map(|c| {
        let mut iq = Vec::new();
        bytes_to_iq(c, SampleFormat::Cs8, &mut iq);
        iq
    })
}

fn run_t(data: &[u8], notch: u8) -> String {
    set_mode(notch);
    let mut rx = Receiver::new(ReceiverConfig::new(RATE, 0.0));
    let mut ts = Vec::new();
    let t0 = std::time::Instant::now();
    let mut pos = 0u64;
    for b in blocks(data) {
        rx.push(&b, pos);
        pos += b.len() as u64;
        ts.clear();
        rx.take_ts(&mut ts);
    }
    let secs = t0.elapsed().as_secs_f64();
    let s = rx.status();
    let o = &s.outer;
    format!(
        "snr {:5.1} eff {:5.1} | RS {} uncorrectable {} ({:.2}%) pre-RS BER {:.1e} packets {} locks {} | notch {} car/{} tones {:.0} dB, canc {} @ {:.0} Hz | {:.2} s",
        s.snr_db,
        s.effective_snr_db,
        o.blocks,
        o.uncorrectable,
        100.0 * o.uncorrectable as f64 / o.blocks.max(1) as f64,
        o.pre_rs_ber(),
        o.packets,
        s.locks,
        s.notch.carriers,
        s.notch.tones,
        s.notch.strongest_db,
        s.tone.tones,
        s.tone.freq_hz,
        secs
    )
}

fn run_t2(data: &[u8], notch: u8) -> String {
    set_mode(notch);
    let mut rx = T2Receiver::new(T2Config::new(RATE, 0.0));
    let mut ts = Vec::new();
    let t0 = std::time::Instant::now();
    let mut pos = 0u64;
    for b in blocks(data) {
        rx.push(&b, pos);
        pos += b.len() as u64;
        ts.clear();
        rx.take_ts(&mut ts);
    }
    rx.flush();
    let secs = t0.elapsed().as_secs_f64();
    let s = rx.status();
    format!(
        "snr {:5.1} eff {:5.1} | frames {}/{} blocks ok {} bad {} LDPC it {:.1} packets {} | notch {} car/{} tones {:.0} dB, canc {} @ {:.0} Hz | {:.2} s",
        s.snr_db,
        s.effective_snr_db,
        s.frames_ok,
        s.frames_ok + s.frames_bad,
        s.blocks_ok,
        s.blocks_bad,
        s.ldpc_iterations as f64 / s.blocks_ok.max(1) as f64,
        s.packets,
        s.notch.carriers,
        s.notch.tones,
        s.notch.strongest_db,
        s.tone.tones,
        s.tone.freq_hz,
        secs
    )
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let t2 = a[1] == "t2";
    let data = std::fs::read(&a[2]).unwrap();
    let get = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
    let dbs = get("--db").unwrap_or_else(|| "none".into());
    let hz: f64 = get("--hz").map_or(1_000_300.0, |v| v.parse().unwrap());
    let drift: f64 = get("--drift").map_or(0.0, |v| v.parse().unwrap());
    let noise: Option<f64> = get("--noise").map(|v| v.parse().unwrap());
    let seconds: f64 = get("--seconds").map_or(1e9, |v| v.parse().unwrap());
    let n = ((seconds * RATE) as usize * 2).min(data.len()) & !1;
    let data = &data[..n];
    let p: f64 = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| f64::from(c[0] as i8).powi(2) + f64::from(c[1] as i8).powi(2))
        .sum::<f64>()
        / (data.len() / 2) as f64;
    for db in dbs.split(',') {
        let d = if db == "none" && noise.is_none() {
            data.to_vec()
        } else {
            inject(data, p, db.parse().unwrap_or(-300.0), hz, drift, noise)
        };
        for notch in [0u8, 1, 2] {
            let r = if t2 {
                run_t2(&d, notch)
            } else {
                run_t(&d, notch)
            };
            println!(
                "tone {db:>5} dB {:9} : {r}",
                ["off", "erase", "cancel+er"][notch as usize]
            );
        }
    }
}
