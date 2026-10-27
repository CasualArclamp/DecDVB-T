// made by CasualArclamp
//! `decdvbt modulate`: a DVB-T test signal as an IQ file, through a channel
//! with multipath, fading, noise and the radio's own faults.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use decdvbt_core::SampleFormat;
use decdvbt_core::channel::{parse_channel, uhf_centre_hz};
use decdvbt_core::dvbt::{Bandwidth, Mode};
use decdvbt_io::IqFileWriter;
use decdvbt_mod::channel::{Channel, Impulse, PROFILES, Path as ChannelPath, profile};
use decdvbt_mod::sim::{SimOptions, T2SimOptions, generate, generate_t2, symbols_for};
use decdvbt_mod::t2::{T2Config, T2Modulator};

#[derive(clap::Args)]
pub struct ModulateArgs {
    /// Output file (.cs8, .cs16 or .cf32). Default: captures/sim_<mode>_….cs8,
    /// named so the GUI knows its centre and rate.
    out: Option<PathBuf>,
    /// The mode: CONSTELLATION-RATE[-2k|8k][-GUARD][-1mhz|6mhz|7mhz|8mhz],
    /// e.g. 64qam-2/3-8k-1/32 (the UK's DVB-T), 16qam-3/4-2k-1/8, or
    /// qpsk-1/2-2k-1/8-1mhz (8 MHz DVB-T with every clock divided by 8).
    #[arg(long, default_value = "64qam-2/3-8k-1/32")]
    mode: String,
    /// Make a DVB-T2 signal instead: `uk`, or FFT-GUARD-PPn-CONSTELLATION-RATE
    /// with any of ext, rot, short, ti=N, blocks=N, syms=N and a bandwidth
    /// (1.7mhz, 6mhz, 7mhz, 8mhz), e.g. 32k-1/128-pp7-256qam-2/3-ext-rot (which
    /// is `uk`) or 8k-1/16-pp4-16qam-1/2-1.7mhz.
    #[arg(long)]
    t2: Option<String>,
    /// Seconds of signal.
    #[arg(long, default_value_t = 3.0)]
    seconds: f64,
    /// Output sample rate, MS/s; 0 for the signal's native rate (64/7 MS/s
    /// at 8 MHz). Default 10, or 2 (the HackRF's lowest) for 1 and 1.7 MHz.
    #[arg(long)]
    rate: Option<f64>,
    /// Channel the file is labelled with (the centre in its name).
    #[arg(long, default_value = "C30")]
    channel: String,
    /// The centre the file is labelled with, MHz, instead of a UHF channel
    /// (narrow signals sit anywhere).
    #[arg(long)]
    freq: Option<f64>,
    /// RMS level of the output, dBFS.
    #[arg(long, default_value_t = -20.0, allow_hyphen_values = true)]
    level: f64,
    /// Sample format of the file: cs8 (the HackRF's), cs16 or cf32.
    #[arg(long)]
    format: Option<String>,
    /// Send this transport stream file (looped) instead of the test stream.
    #[arg(long)]
    ts: Option<PathBuf>,
    /// Cell identifier to put in the TPS.
    #[arg(long)]
    cell_id: Option<u16>,
    /// Seed for the noise, the fading and the bursts.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// C/N, dB, against the nominal channel bandwidth.
    #[arg(long, allow_hyphen_values = true)]
    cn: Option<f64>,
    /// A named set of paths: tu6, ra6, sfn20, 0db-echo, urban.
    #[arg(long)]
    profile: Option<String>,
    /// A path, DELAY_US:GAIN_DB[:PHASE_DEG[:DOPPLER_HZ]]; repeat for more.
    #[arg(long = "path", allow_hyphen_values = true)]
    paths: Vec<String>,
    /// Rayleigh fading on every path, at this maximum Doppler, Hz.
    #[arg(long)]
    fd: Option<f64>,
    /// With --fd: Rician instead of Rayleigh fading, a steady component this
    /// many dB over the scattered one on every path (a vehicle with a line
    /// of sight).
    #[arg(long, requires = "fd", allow_hyphen_values = true)]
    k_db: Option<f64>,
    /// Carrier frequency offset, Hz.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    cfo: f64,
    /// Sampling clock offset, ppm (positive: the receiver's clock is fast).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    sco: f64,
    /// Impulse bursts, RATE_HZ:REL_DB:WIDTH_US, e.g. 200:10:5.
    #[arg(long, allow_hyphen_values = true)]
    impulse: Option<String>,
    /// DC offset, as a fraction of the signal's rms (the HackRF's centre spike).
    #[arg(long, default_value_t = 0.0)]
    dc: f64,
    /// IQ imbalance, GAIN_DB:PHASE_DEG.
    #[arg(long, allow_hyphen_values = true)]
    iq: Option<String>,
}

fn numbers(s: &str, n: usize, what: &str) -> Result<Vec<f64>> {
    let v: Vec<f64> = s
        .split(':')
        .map(|p| p.trim().parse::<f64>())
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| anyhow::anyhow!("{what}: not numbers: {s}"))?;
    anyhow::ensure!(v.len() == n, "{what}: expected {n} values, got {s}");
    Ok(v)
}

/// "10", "12.5": a rate in MS/s for a file name.
fn rate_text(out_rate_hz: f64) -> String {
    let m = out_rate_hz / 1e6;
    if m.fract().abs() < 1e-9 {
        format!("{m:.0}")
    } else {
        format!("{m:.4}")
    }
}

/// The output rate, Hz, for a signal whose native rate is `native` and
/// whose width is `bw`.
fn out_rate_of(a: &ModulateArgs, native: f64, bw: Bandwidth) -> f64 {
    match a.rate {
        Some(0.0) => native,
        Some(r) => r * 1e6,
        None if bw.narrow() => 2e6,
        None => 10e6,
    }
}

/// The label for the file name: "C30_546.000MHz" or "433.250MHz", and the
/// centre, MHz.
fn centre_of(a: &ModulateArgs) -> Result<(String, f64)> {
    if let Some(mhz) = a.freq {
        return Ok((format!("{mhz:.3}MHz"), mhz));
    }
    let chan_no =
        parse_channel(&a.channel).with_context(|| format!("not a UHF channel: {}", a.channel))?;
    let centre_mhz = uhf_centre_hz(chan_no).unwrap() / 1e6;
    Ok((format!("C{chan_no}_{centre_mhz:.3}MHz"), centre_mhz))
}

fn channel_of(a: &ModulateArgs) -> Result<Channel> {
    let mut ch = Channel {
        cfo_hz: a.cfo,
        sco_ppm: a.sco,
        cn_db: a.cn,
        dc_offset: a.dc,
        seed: a.seed,
        ..Default::default()
    };
    if let Some(p) = &a.profile {
        ch.paths =
            profile(p).with_context(|| format!("no profile {p}; try {}", PROFILES.join(", ")))?;
    }
    for p in &a.paths {
        ch.paths
            .push(ChannelPath::parse(p).map_err(anyhow::Error::msg)?);
    }
    if let Some(fd) = a.fd {
        if ch.paths.is_empty() {
            ch.paths.push(ChannelPath::echo(0.0, 0.0));
        }
        ch = match a.k_db {
            Some(k_db) => ch.with_rician(k_db, fd),
            None => ch.with_rayleigh(fd),
        };
    }
    if let Some(i) = &a.impulse {
        let v = numbers(i, 3, "--impulse")?;
        ch.impulse = Some(Impulse {
            rate_hz: v[0],
            rel_db: v[1],
            width_us: v[2],
        });
    }
    if let Some(i) = &a.iq {
        let v = numbers(i, 2, "--iq")?;
        (ch.iq_gain_db, ch.iq_phase_deg) = (v[0], v[1]);
    }
    Ok(ch)
}

pub fn run(a: ModulateArgs) -> Result<()> {
    if a.t2.is_some() {
        return run_t2(a);
    }
    let mode = Mode::parse(&a.mode).map_err(anyhow::Error::msg)?;
    let (label, _) = centre_of(&a)?;
    let out_rate = out_rate_of(&a, mode.bandwidth.sample_rate_hz(), mode.bandwidth);

    let ch = channel_of(&a)?;

    let fmt_name = a
        .format
        .clone()
        .or_else(|| {
            a.out
                .as_deref()
                .and_then(|p| p.extension())
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_else(|| "cs8".into());
    let fmt = match fmt_name.as_str() {
        "cs8" => SampleFormat::Cs8,
        "cs16" => SampleFormat::Cs16,
        "cf32" => SampleFormat::Cf32,
        o => bail!("format {o}: use cs8, cs16 or cf32"),
    };
    let out = a.out.clone().unwrap_or_else(|| {
        let mut tags = String::new();
        if let Some(cn) = a.cn {
            tags += &format!("_cn{cn:.0}");
        }
        if let Some(p) = &a.profile {
            tags += &format!("_{p}");
        }
        PathBuf::from(format!(
            "captures/sim_{}_{label}_{}Msps{tags}.{fmt_name}",
            a.mode.replace('/', "-"),
            rate_text(out_rate)
        ))
    });
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }

    let opts = SimOptions {
        mode,
        cell_id: a.cell_id,
        ts_file: a.ts,
        seconds: a.seconds,
        out_rate_hz: Some(out_rate),
        level_dbfs: a.level,
        channel: ch.clone(),
    };
    println!(
        "{} {} {} {} guard {}: {:.3} Mbit/s, symbol {:.0} µs, C/N needed {}",
        mode.bandwidth,
        mode.fft.name(),
        mode.modulation.name(),
        mode.rate_hp.name(),
        mode.guard.name(),
        mode.bitrate_hp() / 1e6,
        mode.symbol_s() * 1e6,
        mode.qef_cn_db()
            .map_or("-".into(), |q| format!("{q:.1} dB")),
    );
    if !ch.is_clean() {
        println!("channel: {ch:?}");
    }
    let mut w = IqFileWriter::create(&out, fmt)?;
    let t0 = Instant::now();
    let s = generate(&opts, |block| w.write(block).map_err(|e| e.to_string()))
        .map_err(anyhow::Error::msg)?;
    w.finish()?;
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "{}: {} symbols, {:.2} s at {:.3} MS/s, {} samples, level {:.1} dBFS (gain {:.3}), \
         clipped {} ({:.4} %), made in {secs:.1} s ({:.1}x real time)",
        out.display(),
        s.symbols,
        s.samples as f64 / s.out_rate_hz,
        s.out_rate_hz / 1e6,
        s.samples,
        a.level,
        s.gain,
        s.clipped,
        100.0 * s.clipped as f64 / s.samples.max(1) as f64,
        symbols_for(&opts) as f64 * mode.symbol_s() / secs,
    );
    if s.clipped > 0 {
        eprintln!("warning: {} samples clipped: lower --level", s.clipped);
    }
    Ok(())
}

/// A DVB-T2 test signal.
fn run_t2(a: ModulateArgs) -> Result<()> {
    let spec = a.t2.clone().expect("checked by the caller");
    let mut cfg = T2Config::parse(&spec).map_err(anyhow::Error::msg)?;
    let (label, centre_mhz) = centre_of(&a)?;
    cfg.frequency_hz = (centre_mhz * 1e6).round() as u32;
    let out_rate = out_rate_of(&a, cfg.sample_rate_hz(), cfg.bandwidth);
    let ch = channel_of(&a)?;
    let fmt_name = a
        .format
        .clone()
        .or_else(|| {
            a.out
                .as_deref()
                .and_then(|p| p.extension())
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_else(|| "cs8".into());
    let fmt = match fmt_name.as_str() {
        "cs8" => SampleFormat::Cs8,
        "cs16" => SampleFormat::Cs16,
        "cf32" => SampleFormat::Cf32,
        o => bail!("format {o}: use cs8, cs16 or cf32"),
    };
    let out = a.out.clone().unwrap_or_else(|| {
        let mut tags = String::new();
        if let Some(cn) = a.cn {
            tags += &format!("_cn{cn:.0}");
        }
        if let Some(p) = &a.profile {
            tags += &format!("_{p}");
        }
        PathBuf::from(format!(
            "captures/sim_t2_{}_{label}_{}Msps{tags}.{fmt_name}",
            spec.replace('/', "-"),
            rate_text(out_rate)
        ))
    });
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    // Frames for the seconds asked.
    let probe = T2Modulator::new(cfg.clone(), Box::<decdvbt_mod::ts::TestStream>::default())
        .map_err(anyhow::Error::msg)?;
    let frame_s = probe.frame_samples() as f64 / cfg.sample_rate_hz();
    let frames = ((a.seconds / frame_s).ceil() as usize).max(1);
    let layout = *probe.layout();
    println!(
        "{} {} {} {} {} guard {} {}: {} FEC blocks of {} cells in {} frames of {} symbols \
         ({:.0} ms), {:.3} Mbit/s",
        cfg.bandwidth,
        cfg.mode.fft,
        cfg.qam.name(),
        cfg.rate,
        cfg.mode.pp,
        cfg.mode.guard,
        if cfg.rotated {
            "rotated"
        } else {
            "not rotated"
        },
        cfg.fec_blocks,
        cfg.cells_per_block(),
        frames,
        layout.symbols,
        frame_s * 1e3,
        cfg.ts_rate_bps() / 1e6,
    );
    if !ch.is_clean() {
        println!("channel: {ch:?}");
    }
    let opts = T2SimOptions {
        cfg,
        ts_file: a.ts,
        frames,
        out_rate_hz: Some(out_rate),
        level_dbfs: a.level,
        channel: ch,
    };
    let mut w = IqFileWriter::create(&out, fmt)?;
    let t0 = Instant::now();
    let s = generate_t2(&opts, |block| w.write(block).map_err(|e| e.to_string()))
        .map_err(anyhow::Error::msg)?;
    w.finish()?;
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "{}: {:.2} s at {:.3} MS/s, {} samples, level {:.1} dBFS (gain {:.3}), clipped {}          ({:.4} %), made in {secs:.1} s",
        out.display(),
        s.samples as f64 / s.out_rate_hz,
        s.out_rate_hz / 1e6,
        s.samples,
        a.level,
        s.gain,
        s.clipped,
        100.0 * s.clipped as f64 / s.samples.max(1) as f64,
    );
    if s.clipped > 0 {
        eprintln!("warning: {} samples clipped: lower --level", s.clipped);
    }
    Ok(())
}
