// made by CasualArclamp
//! Live Airspy R2 soak: decode a DVB-T channel and log, a line a second, what
//! the radio delivered (samples, dropped buffers, gaps between reads) beside
//! what the decoder lost, so a drop can be put on the USB side or the DSP side.
//!
//! `cargo run --release -p decdvbt-cli --example airspy_live -- 618 60 [packed] [lna mixer vga]`
//!
//! Receive only, bias tee off (the driver's own rule).

use std::time::{Duration, Instant};

use decdvbt_core::dvbt::Bandwidth;
use decdvbt_io::IqSource;
use decdvbt_io::airspy::{AirspyGains, AirspySettings, AirspySource};
use decdvbt_rx::auto::AutoReceiver;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mhz: f64 = args.first().map_or(Ok(618.0), |s| s.parse())?;
    let seconds: f64 = args.get(1).map_or(Ok(60.0), |s| s.parse())?;
    let packed = args.iter().any(|a| a == "packed");
    let nums: Vec<u8> = args[2.min(args.len())..]
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    let gains = if nums.len() == 3 {
        AirspyGains {
            lna: nums[0],
            mixer: nums[1],
            vga: nums[2],
        }
    } else {
        AirspyGains {
            lna: 8,
            mixer: 0,
            vga: 10,
        }
    };
    let mut src = AirspySource::open(AirspySettings {
        center_hz: (mhz * 1e6) as u64,
        gains,
        packed,
        ..Default::default()
    })?;
    let control = src.control();
    let fs = src.sample_rate();
    println!("{}", src.describe());
    let mut rx = AutoReceiver::with_bandwidth(fs, 0.0, Bandwidth::parse("8").unwrap());
    rx.set_realtime(true);

    let (mut block, mut ts) = (Vec::new(), Vec::new());
    let t0 = Instant::now();
    let mut last_read = Instant::now();
    let (mut abs, mut sec_samples, mut next) = (0u64, 0u64, 1.0);
    let (mut max_gap, mut max_push) = (Duration::ZERO, Duration::ZERO);
    let (mut lost0, mut fixed0, mut ovf0, mut tps_bad0) = (0u64, 0u64, 0u64, 0u64);
    let mut lost_sec = 0u64;
    // Per buffer: power in dB, to see a level step (a missing or doubled
    // transfer, a gain glitch).
    let (mut pmin, mut pmax) = (f32::MAX, f32::MIN);
    // When the decoder lost blocks, seconds since the start.
    let mut loss_times: Vec<f64> = Vec::new();
    let mut ovf_times: Vec<f64> = Vec::new();
    while t0.elapsed().as_secs_f64() < seconds {
        let n = src.read(&mut block)?;
        let gap = last_read.elapsed();
        max_gap = max_gap.max(gap);
        let p = block.iter().map(|s| s.norm_sqr()).sum::<f32>() / n.max(1) as f32;
        let db = 10.0 * p.max(1e-12).log10();
        pmin = pmin.min(db);
        pmax = pmax.max(db);
        let t = Instant::now();
        rx.push(&block, abs);
        max_push = max_push.max(t.elapsed());
        ts.clear();
        rx.take_ts(&mut ts);
        abs += n as u64;
        sec_samples += n as u64;
        let st = rx.status();
        let now = abs as f64 / fs;
        if st.outer.uncorrectable > lost0 {
            loss_times.push(now);
        }
        let ovf = control.lost();
        if ovf > ovf0 {
            ovf_times.push(now);
        }
        let wall = t0.elapsed().as_secs_f64();
        if wall >= next {
            next += 1.0;
            println!(
                "{wall:5.1} s wall {now:6.2} s stream: {:6.3} MS/s, lost +{}, gap {:4.1} ms, push {:4.1} ms, \
                 pwr {:5.1}..{:5.1} dB, SNR {:4.1}, RS fixed +{} lost +{}, TPS bad +{}, locks {}",
                sec_samples as f64 / 1e6,
                ovf - ovf0,
                max_gap.as_secs_f64() * 1e3,
                max_push.as_secs_f64() * 1e3,
                pmin,
                pmax,
                st.snr_db,
                st.outer.corrected.saturating_sub(fixed0),
                st.outer.uncorrectable.saturating_sub(lost_sec),
                st.tps_frames_bad.saturating_sub(tps_bad0),
                st.locks
            );
            (sec_samples, max_gap, max_push) = (0, Duration::ZERO, Duration::ZERO);
            (pmin, pmax) = (f32::MAX, f32::MIN);
            (fixed0, tps_bad0, lost_sec) = (
                st.outer.corrected,
                st.tps_frames_bad,
                st.outer.uncorrectable,
            );
            ovf0 = ovf;
        }
        lost0 = st.outer.uncorrectable;
        last_read = Instant::now();
    }
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "total: {:.4} MS/s over {wall:.1} s ({:.0} samples short of 10 MS/s), lost (dropped + blanked) {}",
        abs as f64 / wall / 1e6,
        wall * fs - abs as f64,
        control.lost()
    );
    let show = |name: &str, v: &[f64]| {
        let s: Vec<String> = v.iter().map(|t| format!("{t:.2}")).collect();
        println!("{name} at (stream s): {}", s.join(" "));
    };
    show("RS losses", &loss_times);
    show("overflows", &ovf_times);
    drop(src);
    Ok(())
}
