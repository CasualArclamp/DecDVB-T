// made by CasualArclamp
//! `decdvbt sound`: run the channel sounder over a capture (or the live
//! HackRF) and say what it finds: the mode, the carrier and clock offsets,
//! and the strongest paths of the delay–Doppler map.

use std::time::Instant;

use anyhow::Result;
use decdvbt_core::channel::{uhf_centre_hz, uhf_channel_at};
use decdvbt_rx::delay_doppler::DelayDoppler;
use decdvbt_rx::sounder::{Sounder, SounderConfig};

use crate::source;

#[derive(clap::Args)]
pub struct SoundArgs {
    /// An IQ file, or a UHF channel ("C26") for the HackRF.
    source: String,
    /// Seconds of signal to look at.
    #[arg(long, default_value_t = 6.0)]
    seconds: f64,
    /// The signal's offset from the radio's centre, Hz (a file's name gives
    /// the centre; the channel is taken as the one it falls in).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    offset: f64,
    /// Symbols between those looked at (a multiple of 4).
    #[arg(long, default_value_t = 8)]
    step: usize,
    /// Sample rate of a raw file, MS/s, when its name does not say.
    #[arg(long, default_value_t = 10.0)]
    file_rate: f64,
    /// Paths to list.
    #[arg(long, default_value_t = 10)]
    paths: usize,
}

/// Local maxima of the map above `floor` dB, strongest first.
fn peaks(m: &DelayDoppler, floor: f32, max: usize) -> Vec<(f64, f64, f32)> {
    let mut v = Vec::new();
    for r in 1..m.dopplers.saturating_sub(1) {
        for c in 1..m.delays.saturating_sub(1) {
            let x = m.at(r, c);
            if x < floor {
                continue;
            }
            let is_max = (r - 1..=r + 1)
                .flat_map(|rr| (c - 1..=c + 1).map(move |cc| (rr, cc)))
                .all(|(rr, cc)| (rr, cc) == (r, c) || m.at(rr, cc) < x);
            if is_max {
                v.push((m.delay_us(c), m.doppler_hz(r), x));
            }
        }
    }
    v.sort_by(|a, b| b.2.total_cmp(&a.2));
    v.truncate(max);
    v
}

pub fn run(a: SoundArgs) -> Result<()> {
    let mut src = source::open(&a.source, a.file_rate, None)?;
    let fs = src.sample_rate();
    let centre = src.center_freq();
    let channel_centre = uhf_channel_at(centre).and_then(uhf_centre_hz);
    println!(
        "{} (centre {:.3} MHz{})",
        src.describe(),
        centre / 1e6,
        channel_centre.map_or(String::new(), |c| format!(", channel {:.3} MHz", c / 1e6))
    );

    let mut cfg = SounderConfig::new(fs, a.offset + channel_centre.map_or(0.0, |c| c - centre));
    cfg.step = a.step;
    let mut s = Sounder::new(cfg);
    let (mut abs, mut block) = (0u64, Vec::new());
    let t0 = Instant::now();
    let mut in_sounder = std::time::Duration::ZERO;
    while (abs as f64) < a.seconds * fs {
        if src.read(&mut block)? == 0 {
            break;
        }
        let t = Instant::now();
        s.push(&block, abs);
        in_sounder += t.elapsed();
        abs += block.len() as u64;
    }
    let took = t0.elapsed().as_secs_f64();
    let st = s.status();
    println!(
        "{:.1} s of signal looked at in {took:.2} s ({:.1} % of real time); \
         the sounder alone {:.0} ms ({:.1} % of real time), the rest is reading or waiting for samples",
        abs as f64 / fs,
        100.0 * took / (abs as f64 / fs),
        in_sounder.as_secs_f64() * 1e3,
        100.0 * in_sounder.as_secs_f64() / (abs as f64 / fs)
    );
    let Some(g) = st.geometry.filter(|_| st.tracking) else {
        println!(
            "no DVB-T lock: {} attempts, best cyclic-prefix contrast {:.1} (a lock needs 5)",
            st.attempts, st.contrast
        );
        return Ok(());
    };
    println!(
        "locked: {} guard {}, {} pilots a row, {} rows ({:.0} a second), {} lock(s)",
        g.fft.name(),
        g.guard.name(),
        g.columns,
        st.rows,
        1.0 / g.row_s,
        st.locks
    );
    println!(
        "carrier offset {:+.1} Hz from the radio's centre ({:+.1} Hz from the channel's), \
         clock {:+.1} ppm, pilot coherence {:.2}",
        st.cfo_hz,
        st.cfo_hz - cfg.vfo_offset_hz,
        st.clock_ppm,
        st.coherence
    );
    match s.delay_doppler() {
        None => println!("no delay-Doppler map yet (needs {} s of rows)", 2),
        Some(m) => {
            println!(
                "delay-Doppler map: {:.0} s of rows, {} delays x {} Dopplers, guard {:.0} us",
                m.window_s, m.delays, m.dopplers, m.guard_us
            );
            println!("   delay us   Doppler Hz   dB below the strongest");
            let p = peaks(m, -30.0, a.paths);
            // Delays relative to the strongest path, as a receiver shows them.
            for (d, h, db) in &p {
                println!("   {d:8.2}   {h:10.2}   {db:6.1}");
            }
        }
    }
    Ok(())
}
