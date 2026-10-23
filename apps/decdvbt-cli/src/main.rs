// made by CasualArclamp
//! DecDVB-T command line.
//!
//! - `channels`, `channel`: the UHF channel plan.
//! - `scan`: step the HackRF over the UK channels and say which look like
//!   digital TV (a flat 7.6 MHz block with steep shoulders).
//! - `modulate`: a DVB-T test signal as an IQ file, through a channel with
//!   multipath, fading, noise and the radio's faults.
//! - `capture`: run the receive engine on the HackRF (or replay a file) for a
//!   few seconds, print level, drops and CPU, and optionally record the IQ.
//!   The headless check of the front end, and the first CPU benchmark.
//!
//! Decoding arrives with M3.

mod modulate;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use decdvbt_core::SampleFormat;
use decdvbt_core::channel::{parse_channel, uhf_centre_hz, uhf_channel_at, uk_channels};
use decdvbt_engine::{Command as RxCommand, CpuGauge, Rx, RxOptions, Stage, spectral_cn_db};
use decdvbt_io::{IqFileReader, IqSource, format_from_path, name_hint};

#[derive(Parser)]
#[command(name = "decdvbt", version, about = "DVB-T / DVB-T2 receiver")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the UK UHF channels and their centre frequencies.
    Channels,
    /// One channel's centre frequency ("C34", "34"), or the channel holding a
    /// frequency in MHz ("578").
    Channel { which: String },
    /// Step the HackRF over the UK channels and report each one's level and
    /// whether it looks like a DVB-T/T2 block.
    Scan {
        #[arg(long, default_value_t = 24)]
        lna: u16,
        #[arg(long, default_value_t = 20)]
        vga: u16,
        #[arg(long)]
        amp: bool,
        /// Dwell on each channel, seconds.
        #[arg(long, default_value_t = 0.6)]
        dwell: f64,
    },
    /// Make a DVB-T test signal as an IQ file, optionally through a channel.
    Modulate(Box<modulate::ModulateArgs>),
    /// Run the receive engine for a few seconds and report what it sees.
    Capture {
        /// A UHF channel ("C34") for the HackRF, or an IQ file to replay.
        source: String,
        /// HackRF sample rate, MS/s (10 holds one 8 MHz channel).
        #[arg(long, default_value_t = 10.0)]
        rate: f64,
        /// Seconds to run.
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
        /// Record the IQ (cs8) to this file.
        #[arg(long)]
        record: Option<PathBuf>,
        /// HackRF LNA gain, dB (0-40, steps of 8).
        #[arg(long, default_value_t = 24)]
        lna: u16,
        /// HackRF VGA gain, dB (0-62, steps of 2).
        #[arg(long, default_value_t = 20)]
        vga: u16,
        /// HackRF RF amplifier (+14 dB).
        #[arg(long)]
        amp: bool,
        /// Sample rate of a raw IQ file, MS/s, when its name does not say.
        #[arg(long, default_value_t = 10.0)]
        file_rate: f64,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Channels => {
            for n in uk_channels() {
                println!("C{n:<3} {:>7.3} MHz", uhf_centre_hz(n).unwrap() / 1e6);
            }
        }
        Command::Channel { which } => {
            if let Some(n) = parse_channel(&which) {
                println!("C{n} {:.3} MHz", uhf_centre_hz(n).unwrap() / 1e6);
            } else if let Ok(mhz) = which.parse::<f64>() {
                match uhf_channel_at(mhz * 1e6) {
                    Some(n) => println!("C{n} {:.3} MHz", uhf_centre_hz(n).unwrap() / 1e6),
                    None => bail!("{mhz} MHz is not in a UHF channel (21–69)"),
                }
            } else {
                bail!("not a channel or a frequency: {which}");
            }
        }
        Command::Modulate(a) => modulate::run(*a)?,
        Command::Scan {
            lna,
            vga,
            amp,
            dwell,
        } => scan(lna, vga, amp, dwell)?,
        Command::Capture {
            source,
            rate,
            seconds,
            record,
            lna,
            vga,
            amp,
            file_rate,
        } => capture(&source, rate, seconds, record, lna, vga, amp, file_rate)?,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn capture(
    source: &str,
    rate: f64,
    seconds: f64,
    record: Option<PathBuf>,
    lna: u16,
    vga: u16,
    amp: bool,
    file_rate: f64,
) -> Result<()> {
    let (src, drops): (Box<dyn IqSource>, Option<decdvbt_engine::DropProbe>) =
        match parse_channel(source) {
            Some(n) => live(n, rate, lna, vga, amp)?,
            None => {
                let path = PathBuf::from(source);
                let fmt = format_from_path(&path).unwrap_or(SampleFormat::Cs8);
                let hint = name_hint(&path);
                let r = IqFileReader::open(
                    &path,
                    fmt,
                    hint.sample_rate.unwrap_or(file_rate * 1e6),
                    1 << 17,
                )
                .with_context(|| format!("cannot open {source}"))?
                .with_center_freq(hint.center_hz.unwrap_or(0.0));
                (Box::new(r), None)
            }
        };
    println!("{}", src.describe());
    let rx = Rx::start(src, RxOptions::default(), drops);
    if let Some(p) = record {
        rx.send(RxCommand::Record(Some(p)));
    }
    let mut cpu = CpuGauge::default();
    let t0 = Instant::now();
    let mut rows = 0usize;
    // The spectral C/N of the channel under the centre, smoothed over rows.
    let mut cn: Option<f32> = None;
    while t0.elapsed().as_secs_f64() < seconds && rx.running() {
        std::thread::sleep(Duration::from_millis(500));
        for r in rx.take_rows() {
            rows += 1;
            let vfo = uhf_channel_at(r.center_hz).and_then(uhf_centre_hz);
            if let Some(e) = vfo.and_then(|v| spectral_cn_db(&r.db, r.center_hz, r.sample_rate, v))
            {
                cn = Some(cn.map_or(e, |c| c + 0.1 * (e - c)));
            }
        }
        let s = rx.stats();
        let [src_load, rec_load, spec_load] = s.load.percent;
        println!(
            "{:5.1} s  rms {:6.1} dBFS  peak {:.2}  clip {:4.2}%  dropped {}  rows {}  \
             C/N {}  CPU {:5.1}%  [{}: {:.1}%  {}: {:.1}%  {}: {:.1}%]{}",
            t0.elapsed().as_secs_f64(),
            s.rms_dbfs,
            s.peak,
            s.clip_percent,
            s.dropped,
            rows,
            cn.map_or("  - ".into(), |c| format!("{c:4.1}")),
            cpu.poll(),
            Stage::Source.name(),
            src_load,
            Stage::Record.name(),
            rec_load,
            Stage::Spectrum.name(),
            spec_load,
            s.error.map(|e| format!("  ERROR {e}")).unwrap_or_default()
        );
    }
    Ok(())
}

/// What one channel's averaged spectrum says (10 MS/s, 4096 bins).
struct ChannelLook {
    /// Mean power over the middle 7.0 MHz, dBFS per bin.
    inband_db: f32,
    /// Spread of the in-band bins after smoothing over 100 kHz, dB (a
    /// DVB-T/T2 block is flat; a band of narrow carriers is not).
    ripple_db: f32,
    /// In-band mean less the mean of the gaps either side of the 7.61 MHz
    /// occupied band, dB: steep shoulders mean a block.
    shoulder_db: f32,
}

fn look(db: &[f32], rate: f64) -> ChannelLook {
    let bin_hz = rate / db.len() as f64;
    let at = |hz: f64| (db.len() as f64 / 2.0 + hz / bin_hz).round() as usize;
    let mean_db = |r: std::ops::Range<usize>| {
        // Average in power, report in dB.
        let p = r.clone().map(|i| 10f32.powf(db[i] / 10.0)).sum::<f32>() / r.len() as f32;
        10.0 * p.log10()
    };
    let inband = at(-3.4e6)..at(3.4e6);
    // The gap between this block (to ±3.805 MHz) and a neighbour's (from
    // ±4.195 MHz), either side.
    let edges = (mean_db(at(-4.1e6)..at(-3.9e6)) + mean_db(at(3.9e6)..at(4.1e6))) / 2.0;
    let k = (100e3 / bin_hz).round().max(1.0) as usize;
    let smooth: Vec<f32> = db[inband.clone()]
        .chunks(k)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect();
    let (lo, hi) = smooth
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
    let inband_db = mean_db(inband);
    ChannelLook {
        inband_db,
        ripple_db: hi - lo,
        shoulder_db: inband_db - edges,
    }
}

fn scan(lna: u16, vga: u16, amp: bool, dwell: f64) -> Result<()> {
    let first = uk_channels().next().unwrap();
    let (src, drops) = live(first, 10.0, lna, vga, amp)?;
    let rx = Rx::start(src, RxOptions::default(), drops);
    println!("chan   MHz     inband   ripple  shoulder  over floor");
    let mut looks = Vec::new();
    for n in uk_channels() {
        rx.send(RxCommand::Retune(uhf_centre_hz(n).unwrap()));
        std::thread::sleep(Duration::from_millis(150));
        rx.take_rows();
        // Rows keep coming at 20 a second; average what the dwell gives.
        std::thread::sleep(Duration::from_secs_f64(dwell));
        let rows = rx.take_rows();
        let Some(rate) = rows.first().map(|r| r.sample_rate) else {
            bail!("no data from the HackRF");
        };
        let bins = rows[0].db.len();
        let mut avg = vec![0f32; bins];
        for r in &rows {
            for (a, v) in avg.iter_mut().zip(&r.db) {
                *a += 10f32.powf(v / 10.0) / rows.len() as f32;
            }
        }
        let avg: Vec<f32> = avg.iter().map(|p| 10.0 * p.log10()).collect();
        looks.push((n, look(&avg, rate)));
    }
    // The floor: the quietest quarter of the channels (their mean).
    let mut levels: Vec<f32> = looks.iter().map(|(_, l)| l.inband_db).collect();
    levels.sort_by(|a, b| a.total_cmp(b));
    let floor =
        levels[..levels.len().div_ceil(4)].iter().sum::<f32>() / levels.len().div_ceil(4) as f32;
    for (n, l) in looks {
        let over = l.inband_db - floor;
        // Well over the floor with a step at the edges: a block of OFDM.
        let tv = over > 8.0 && l.shoulder_db > 6.0;
        println!(
            "C{n:<4} {:6.1}  {:6.1}  {:6.1}  {:7.1}  {:9.1}  {}",
            uhf_centre_hz(n).unwrap() / 1e6,
            l.inband_db,
            l.ripple_db,
            l.shoulder_db,
            over,
            if tv { "<- digital TV?" } else { "" }
        );
    }
    Ok(())
}

#[cfg(feature = "hackrf")]
fn live(
    n: u8,
    rate: f64,
    lna: u16,
    vga: u16,
    amp: bool,
) -> Result<(Box<dyn IqSource>, Option<decdvbt_engine::DropProbe>)> {
    use decdvbt_io::{HackRfGains, HackRfSettings, HackRfSource};
    let s = HackRfSource::open(HackRfSettings {
        center_hz: uhf_centre_hz(n).unwrap().round() as u64,
        sample_rate: (rate * 1e6).round() as u32,
        gains: HackRfGains {
            amp,
            lna_db: lna,
            vga_db: vga,
        },
    })?;
    let ctl = s.control();
    Ok((Box::new(s), Some(Box::new(move || ctl.overflows()))))
}

#[cfg(not(feature = "hackrf"))]
fn live(
    _: u8,
    _: f64,
    _: u16,
    _: u16,
    _: bool,
) -> Result<(Box<dyn IqSource>, Option<decdvbt_engine::DropProbe>)> {
    bail!("built without the hackrf feature")
}
