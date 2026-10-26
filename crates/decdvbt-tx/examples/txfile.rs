// made by CasualArclamp
//! The zero-RF test tool for the transmit engine: a video file through ffmpeg,
//! the modulator and a *virtual* radio (an IQ file, or nowhere).
//!
//! ```text
//! cargo run --release -p decdvbt-tx --example txfile -- IN.mp4 OUT.cs8
//!     [--mode 64qam-2/3-8k-1/32] [--seconds 30] [--loop] [--rate native|10|20]
//!     [--chain auto|nvenc|x264] [--paced | --flat-out] [--null]
//!     [--save-ts sent.ts] [--stages] [--freq-mhz 546] [--no-preview]
//! ```
//!
//! It builds a `TxConfig` with an `IqFile` (or `Null`) sink, runs `prepare`,
//! `transmit`, prints a status line a second and the totals at the end. The
//! output file is renamed `OUT_<MHz>MHz_<rate>Msps.cs8` unless it already
//! carries its tuning, so the receiver reads the centre and rate from the name.
//!
//! **There is no `--hackrf` option and this program cannot key the radio**: it
//! never calls `arm`, never builds a `HackRf` sink, and sets
//! `DECDVBT_TX_NO_RADIO` for good measure.
//!
//! `--stages` adds a per-stage CPU split of the modulator (outer, inner,
//! pilots/IFFT/guard, cs8 pack) on the test stream, no ffmpeg.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Mode;
use decdvbt_mod::Modulator;
use decdvbt_mod::inner::InnerEncoder;
use decdvbt_mod::outer::OuterEncoder;
use decdvbt_mod::ts::{TS_LEN, TestStream, TsSource};
use decdvbt_tx::config::{EncoderChoice, TxConfig, TxRate, TxSink};
use decdvbt_tx::ring::pack_cs8;
use decdvbt_tx::{TxSession, TxState};

struct Args {
    input: Option<PathBuf>,
    out: Option<PathBuf>,
    mode: Mode,
    seconds: f64,
    looped: bool,
    rate: TxRate,
    chain: EncoderChoice,
    paced: bool,
    null: bool,
    save_ts: Option<PathBuf>,
    stages: bool,
    freq_mhz: f64,
    preview: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: txfile IN.mp4 [OUT.cs8] [--mode 64qam-2/3-8k-1/32] [--seconds 30] [--loop]\n\
         \x20      [--rate native|10|20] [--chain auto|nvenc|x264] [--paced | --flat-out]\n\
         \x20      [--null] [--save-ts sent.ts] [--stages] [--freq-mhz 546] [--no-preview]"
    );
    std::process::exit(2);
}

fn parse() -> Args {
    let mut a = Args {
        input: None,
        out: None,
        mode: Mode::parse("64qam-2/3").unwrap(),
        seconds: 30.0,
        looped: false,
        rate: TxRate::Native,
        chain: EncoderChoice::Auto,
        paced: false,
        null: false,
        save_ts: None,
        stages: false,
        freq_mhz: 546.0,
        preview: true,
    };
    let mut it = std::env::args().skip(1);
    let mut positional = Vec::new();
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--mode" => {
                a.mode = Mode::parse(&value()).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    usage()
                })
            }
            "--seconds" => a.seconds = value().parse().unwrap_or_else(|_| usage()),
            "--loop" => a.looped = true,
            "--rate" => {
                a.rate = match value().as_str() {
                    "native" => TxRate::Native,
                    "10" => TxRate::Fixed(10_000_000),
                    "20" => TxRate::Fixed(20_000_000),
                    _ => usage(),
                }
            }
            "--chain" => {
                a.chain = match value().as_str() {
                    "auto" => EncoderChoice::Auto,
                    "nvenc" => EncoderChoice::Nvenc,
                    "x264" => EncoderChoice::X264,
                    _ => usage(),
                }
            }
            "--paced" => a.paced = true,
            "--flat-out" => a.paced = false,
            "--null" => a.null = true,
            "--save-ts" => a.save_ts = Some(value().into()),
            "--stages" => a.stages = true,
            "--freq-mhz" => a.freq_mhz = value().parse().unwrap_or_else(|_| usage()),
            "--no-preview" => a.preview = false,
            s if s.starts_with("--") => usage(),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let mut p = positional.into_iter();
    a.input = p.next();
    a.out = p.next();
    a
}

/// `out` with the tuning in its name, as the receiver reads it.
fn tuned_name(out: PathBuf, mhz: f64, rate_hz: u32) -> PathBuf {
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("tx");
    let lower = stem.to_ascii_lowercase();
    if lower.contains("mhz") && lower.contains("sps") {
        return out;
    }
    let name = format!("{stem}_{mhz:.3}MHz_{}Msps.cs8", rate_hz as f64 / 1e6);
    out.with_file_name(name)
}

/// Seconds of CPU per second of signal for each stage, on the test stream.
fn stages(mode: Mode, symbols: usize, rate: TxRate) {
    let mut ts = TestStream::default();
    let mut outer = OuterEncoder::default();
    let mut inner = InnerEncoder::new(mode).unwrap();
    let (mut t_outer, mut t_inner) = (Duration::ZERO, Duration::ZERO);
    let mut pkt = [0u8; TS_LEN];
    let mut block = Vec::new();
    let warm = 12;
    for l in 0..symbols + warm {
        while inner.bytes_needed() > 0 {
            ts.next_packet(&mut pkt);
            block.clear();
            let t = Instant::now();
            outer.push_packet(&pkt, &mut block);
            if l >= warm {
                t_outer += t.elapsed();
            }
            inner.feed(&block);
        }
        let t = Instant::now();
        let n = inner.symbol_cells(l % 68).len();
        if l >= warm {
            t_inner += t.elapsed();
        }
        assert!(n > 0);
    }
    // The whole modulator and the pack, on the same stream.
    let mut m = Modulator::new(mode, None, Box::<TestStream>::default()).unwrap();
    let mut sym: Vec<Iq> = Vec::new();
    for _ in 0..m.warmup_symbols() + warm {
        m.next_symbol(&mut sym);
    }
    let t = Instant::now();
    for _ in 0..symbols {
        m.next_symbol(&mut sym);
    }
    let t_full = t.elapsed();
    let mut bytes = Vec::new();
    let t = Instant::now();
    for _ in 0..symbols {
        bytes.clear();
        pack_cs8(&sym, &mut bytes);
    }
    let t_pack = t.elapsed();
    let signal = symbols as f64 * mode.symbol_s();
    let pct = |d: Duration| 100.0 * d.as_secs_f64() / signal;
    if let TxRate::Fixed(hz) = rate {
        let native = mode.bandwidth.sample_rate_hz();
        let occupied = 7.61e6 * mode.bandwidth.mhz() / 8.0;
        let mut r = decdvbt_dsp::Resampler::for_rates(native, f64::from(hz), occupied).unwrap();
        let mut out = Vec::new();
        let t = Instant::now();
        for _ in 0..symbols {
            out.clear();
            r.process(&sym, &mut out);
        }
        println!(
            "resampler to {} MS/s: {:.1} % of one core",
            hz as f64 / 1e6,
            pct(t.elapsed())
        );
    }
    println!(
        "stages (% of one core per second of signal, {symbols} symbols): outer {:.1}  inner {:.1}  \
         pilots/IFFT/guard {:.1}  (modulator total {:.1})  cs8 pack {:.1}",
        pct(t_outer),
        pct(t_inner),
        pct(t_full) - pct(t_outer) - pct(t_inner),
        pct(t_full),
        pct(t_pack)
    );
}

fn main() {
    // This tool never keys a radio; this makes the library refuse one too.
    // SAFETY: set at the start of `main`, before any other thread exists.
    unsafe { std::env::set_var(decdvbt_tx::radio::NO_RADIO_ENV, "1") };
    let a = parse();
    if a.stages {
        stages(a.mode, 600, a.rate);
        if a.input.is_none() {
            return;
        }
    }
    let Some(input) = a.input.clone() else {
        usage()
    };
    let mut cfg = TxConfig::new(input);
    cfg.mode = a.mode;
    cfg.loop_input = a.looped;
    cfg.rate = a.rate;
    cfg.video.encoder = a.chain;
    cfg.centre_hz = a.freq_mhz * 1e6;
    cfg.preview = a.preview;
    let rate_hz = cfg.sample_rate_hz();
    let mut out_path = None;
    cfg.sink = if a.null || a.out.is_none() {
        TxSink::Null { paced: a.paced }
    } else {
        let p = tuned_name(a.out.clone().unwrap(), a.freq_mhz, rate_hz);
        out_path = Some(p.clone());
        TxSink::IqFile {
            path: p,
            paced: a.paced,
            seconds: Some(a.seconds),
        }
    };
    let mut s = TxSession::new(cfg).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    s.set_ts_dump(a.save_ts.clone());
    println!(
        "txfile: {} {} {:.3} MHz, {} MS/s, {}",
        a.mode.modulation.name(),
        a.mode.rate_hp.name(),
        a.freq_mhz,
        rate_hz as f64 / 1e6,
        if a.paced { "paced" } else { "flat out" }
    );
    let t0 = Instant::now();
    if let Err(e) = s.prepare() {
        eprintln!("prepare: {e}");
        std::process::exit(1);
    }
    loop {
        let st = s.status();
        match st.state {
            TxState::Ready => break,
            TxState::Failed => fail(&st),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
        if t0.elapsed() > Duration::from_secs(60) {
            eprintln!("not ready after 60 s");
            std::process::exit(1);
        }
    }
    let st = s.status();
    println!(
        "ready in {:.2} s: chain {}, TS ring {:.0} %, IQ ring {:.0} %",
        t0.elapsed().as_secs_f64(),
        st.chain,
        st.ts_fill * 100.0,
        st.iq_fill * 100.0
    );
    if let Some(i) = s.video_info() {
        println!(
            "input: {}x{} {:.2} fps {} + {}, {:?}{}",
            i.width,
            i.height,
            i.fps,
            i.video_codec,
            i.audio_codec.as_deref().unwrap_or("no audio"),
            i.duration,
            if i.interlaced { ", interlaced" } else { "" }
        );
    }
    s.transmit().unwrap_or_else(|e| {
        eprintln!("transmit: {e}");
        std::process::exit(1);
    });
    let started = Instant::now();
    let cpu0 = decdvbt_tx::cpu::process_cpu_seconds();
    let (mut cpu_samples, mut ff_samples) = (Vec::new(), Vec::new());
    let mut last_print = Instant::now();
    let mut worst_iq_fill: f32 = 1.0;
    let end = loop {
        std::thread::sleep(Duration::from_millis(50));
        let st = s.status();
        if started.elapsed() > Duration::from_secs(2) {
            worst_iq_fill = worst_iq_fill.min(st.iq_fill);
        }
        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = Instant::now();
            if started.elapsed() > Duration::from_secs(3) {
                cpu_samples.push(st.process_cpu_pct);
                ff_samples.push(st.ffmpeg_cpu_pct);
            }
            println!(
                "{:>5.1}s sent {:>6.2}s prog {:>6.2}s | ts {:>3.0}% iq {:>3.0}% stuffed {} under {} | \
                 mod {:>5.1}% ({:.2}x) app {:>5.1}% ff {:>5.1}% ({:.2}x) | in {:.2} Mb/s dts {} \
                 level {:.1} dBFS peak {:.1} clip {}",
                started.elapsed().as_secs_f64(),
                st.sent_s,
                st.programme_s,
                st.ts_fill * 100.0,
                st.iq_fill * 100.0,
                st.ts_stuffed,
                st.underruns,
                st.modulator_load_pct,
                st.modulator_speed_x,
                st.process_cpu_pct,
                st.ffmpeg_cpu_pct,
                st.ffmpeg_speed_x,
                st.ts_in_bps / 1e6,
                st.dts_pcr_warnings,
                st.level_rms_dbfs,
                st.peak_dbfs,
                st.clipped,
            );
        }
        match st.state {
            TxState::Failed => fail(&st),
            TxState::Finished => break st,
            _ => {}
        }
        // The null sink has no end of its own: stop at `--seconds` of signal.
        if a.null && st.sent_s >= a.seconds {
            break st;
        }
    };
    let wall = started.elapsed().as_secs_f64();
    let cpu = decdvbt_tx::cpu::process_cpu_seconds() - cpu0;
    let avg = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
    println!("--- totals");
    println!(
        "signal {:.2} s in {:.2} s wall: speed {:.2}x; this process {:.1} % of a core (steady {:.1} %), \
         ffmpeg {:.1} % (steady)",
        end.sent_s,
        wall,
        end.sent_s / wall,
        100.0 * cpu / wall,
        avg(&cpu_samples),
        avg(&ff_samples)
    );
    println!(
        "ts stuffed {} | underruns {} | dts<pcr {} | clipped {} | lowest IQ ring {:.0} % | chain {}",
        end.ts_stuffed,
        end.underruns,
        end.dts_pcr_warnings,
        end.clipped,
        worst_iq_fill * 100.0,
        end.chain
    );
    s.stop();
    if let Some(p) = out_path {
        println!("wrote {}", p.display());
    }
}

fn fail(st: &decdvbt_tx::TxStatus) -> ! {
    eprintln!("FAILED: {}", st.error.as_deref().unwrap_or("unknown"));
    for l in &st.ffmpeg_log {
        eprintln!("  ffmpeg: {l}");
    }
    std::process::exit(1);
}
