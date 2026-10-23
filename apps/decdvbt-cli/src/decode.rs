// made by CasualArclamp
//! `decdvbt decode`: run the receiver over a capture (or the live HackRF),
//! write the transport stream, and say what came out: the mode, the signal's
//! quality, the Reed–Solomon statistics and the services the stream holds.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use decdvbt_core::channel::{uhf_centre_hz, uhf_channel_at};
use decdvbt_rx::receiver::{Receiver, ReceiverConfig};
use decdvbt_ts::TS_LEN;
use decdvbt_ts::psi::TsAnalyser;

use crate::source;

#[derive(clap::Args)]
pub struct DecodeArgs {
    /// An IQ file, or a UHF channel ("C26") for the HackRF.
    source: String,
    /// Write the transport stream here.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Seconds of signal to decode.
    #[arg(long, default_value_t = 6.0)]
    seconds: f64,
    /// The signal's offset from the radio's centre, Hz (a file's name gives
    /// the centre; the channel is taken as the one it falls in).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    offset: f64,
    /// Sample rate of a raw file, MS/s, when its name does not say.
    #[arg(long, default_value_t = 10.0)]
    file_rate: f64,
    /// Print a line a second while decoding.
    #[arg(long)]
    progress: bool,
    /// Leave packets the Reed–Solomon code could not correct out of the
    /// stream (they are kept, with the error flag set, by default).
    #[arg(long)]
    drop_errors: bool,
}

pub fn run(a: DecodeArgs) -> Result<()> {
    let mut src = source::open(&a.source, a.file_rate)?;
    let fs = src.sample_rate();
    let centre = src.center_freq();
    let channel_centre = uhf_channel_at(centre).and_then(uhf_centre_hz);
    println!(
        "{} (centre {:.3} MHz{})",
        src.describe(),
        centre / 1e6,
        channel_centre.map_or(String::new(), |c| format!(", channel {:.3} MHz", c / 1e6))
    );
    let mut out = match &a.output {
        Some(p) => Some(std::io::BufWriter::new(
            std::fs::File::create(p).with_context(|| format!("cannot create {}", p.display()))?,
        )),
        None => None,
    };

    let cfg = ReceiverConfig::new(fs, a.offset + channel_centre.map_or(0.0, |c| c - centre));
    let mut rx = Receiver::new(cfg);
    let mut analyser = TsAnalyser::new();
    let (mut abs, mut block, mut ts) = (0u64, Vec::new(), Vec::new());
    let (t0, mut in_rx, mut next_report) = (Instant::now(), Duration::ZERO, 1.0);
    let mut packets_out = 0u64;
    while (abs as f64) < a.seconds * fs {
        if src.read(&mut block)? == 0 {
            break;
        }
        let t = Instant::now();
        rx.push(&block, abs);
        in_rx += t.elapsed();
        abs += block.len() as u64;
        ts.clear();
        rx.take_ts(&mut ts);
        for p in ts.as_chunks::<TS_LEN>().0 {
            if a.drop_errors && p[1] & 0x80 != 0 {
                continue;
            }
            analyser.packet(p);
            if let Some(w) = out.as_mut() {
                w.write_all(p)?;
            }
            packets_out += 1;
        }
        let secs = abs as f64 / fs;
        analyser.tick(block.len() as f64 / fs);
        if a.progress && secs >= next_report {
            next_report += 1.0;
            let st = rx.status();
            println!(
                "{secs:5.1} s: {} symbols, TPS {}, SNR {:.1} dB, RS {} clean / {} fixed / {} lost, {} packets",
                st.symbols,
                if st.tps_synced {
                    "in sync"
                } else {
                    "searching"
                },
                st.snr_db,
                st.outer.clean,
                st.outer.corrected,
                st.outer.uncorrectable,
                packets_out
            );
        }
    }
    if let Some(mut w) = out {
        w.flush()?;
    }
    let signal_s = abs as f64 / fs;
    let took = t0.elapsed().as_secs_f64();
    println!(
        "{signal_s:.1} s of signal in {took:.2} s ({:.1} % of real time); \
         the receiver alone {:.0} ms ({:.1} % of real time)",
        100.0 * took / signal_s,
        in_rx.as_secs_f64() * 1e3,
        100.0 * in_rx.as_secs_f64() / signal_s
    );

    let st = rx.status();
    {
        let p = &st.profile;
        let pct = |d: Duration| 100.0 * d.as_secs_f64() / signal_s;
        println!(
            "where the time went (% of real time, one core): acquire {:.1}, read symbols {:.1},              equalise {:.1}, TPS {:.1}, demap {:.1}, Viterbi {:.1}, outer {:.1}",
            pct(p.acquire),
            pct(p.read),
            pct(p.equalise),
            pct(p.tps),
            pct(p.demap),
            pct(p.viterbi),
            pct(p.outer)
        );
    }
    if !st.tracking {
        println!(
            "no DVB-T lock: {} attempts, best cyclic-prefix contrast {:.1} (a lock needs 5)",
            st.attempts, st.contrast
        );
        return Ok(());
    }
    println!(
        "locked: {} guard {}, {} lock(s), carrier offset {:+.1} Hz from the radio's centre, \
         clock {:+.1} ppm",
        st.fft.map_or("?", |f| f.name()),
        st.guard.map_or("?", |g| g.name()),
        st.locks,
        st.cfo_hz,
        st.clock_ppm
    );
    match &st.tps {
        None => println!("TPS not found ({} frames read)", st.tps_frames_ok),
        Some(t) => {
            println!(
                "TPS: {} {} {}, hierarchy {:?}, {} frames read, {} bad",
                t.modulation.name(),
                t.rate_hp.name(),
                st.guard.map_or("?", |g| g.name()),
                t.alpha,
                st.tps_frames_ok,
                st.tps_frames_bad
            );
        }
    }
    if st.hierarchical {
        println!("hierarchical transmission is not decoded yet");
        return Ok(());
    }
    let o = &st.outer;
    println!(
        "SNR {:.1} dB; Reed-Solomon: {} blocks, {} clean, {} corrected ({} bytes), {} uncorrectable, \
         {} loss(es) of sync; error rate into the code {:.2e}",
        st.snr_db,
        o.blocks,
        o.clean,
        o.corrected,
        o.bytes_fixed,
        o.uncorrectable,
        o.losses,
        o.pre_rs_ber()
    );
    println!(
        "{packets_out} transport packets{}",
        a.output
            .as_ref()
            .map_or(String::new(), |p| format!(" written to {}", p.display()))
    );
    summarise(&analyser);
    Ok(())
}

/// What the PSI/SI of the stream says.
fn summarise(an: &TsAnalyser) {
    let r = an.report();
    let errors: u64 = r.pids.iter().map(|p| p.stats.errors).sum();
    let cc: u64 = r.pids.iter().map(|p| p.stats.cc_errors).sum();
    println!(
        "stream: {} packets, {} with the error flag, {} continuity jumps, {} bad sections{}",
        r.packets,
        errors,
        cc,
        r.bad_sections,
        r.utc
            .as_ref()
            .map_or(String::new(), |t| format!(", clock {t} UTC"))
    );
    if let Some(n) = &r.network.name {
        println!("network: {n}");
    }
    println!("{} service(s):", r.programmes.len());
    for p in &r.programmes {
        let streams: Vec<String> = p.streams.iter().map(|s| s.describe()).collect();
        println!(
            "  {:5} {:<24} {:<12} {}{}",
            p.number,
            p.name.as_deref().unwrap_or("?"),
            p.service_type.map_or(String::new(), |t| format!(
                "{t:#04x} {}",
                decdvbt_ts::service_type_name(t)
            )),
            streams.join(", "),
            if p.free_ca { "  [scrambled]" } else { "" }
        );
    }
}
