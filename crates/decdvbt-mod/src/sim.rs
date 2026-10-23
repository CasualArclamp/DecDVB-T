// made by CasualArclamp
//! A test signal end to end: the modulator, a channel, a resampler to the
//! radio's rate, a level, and out to a sink in blocks.

use std::path::PathBuf;

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Mode;
use decdvbt_dsp::Resampler;

use crate::channel::{Channel, ChannelSim, RadioSim};
use crate::modulator::Modulator;
use crate::ts::{FileTs, TestStream, TsSource};

/// The occupied bandwidth of an 8 MHz channel's block, Hz (§4.7: 6817 × 1/Tu).
pub const OCCUPIED_8MHZ_HZ: f64 = 7.61e6;

#[derive(Debug, Clone)]
pub struct SimOptions {
    pub mode: Mode,
    pub cell_id: Option<u16>,
    /// A `.ts` file to send, looped; the test stream when `None`.
    pub ts_file: Option<PathBuf>,
    pub seconds: f64,
    /// The output sample rate, Hz; `None` is the native 64/7 MS/s (8 MHz).
    pub out_rate_hz: Option<f64>,
    /// The rms level of the output, dBFS (full scale is a component of ±1).
    pub level_dbfs: f64,
    pub channel: Channel,
}

/// What a run produced.
#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub symbols: usize,
    pub samples: u64,
    pub out_rate_hz: f64,
    /// Samples with a component beyond full scale (clipped by the format).
    pub clipped: u64,
    pub gain: f32,
}

fn source(o: &SimOptions) -> Result<Box<dyn TsSource>, String> {
    Ok(match &o.ts_file {
        Some(p) => Box::new(FileTs::open(p)?),
        None => Box::<TestStream>::default(),
    })
}

/// Build the chain and push `symbols` through it, `each` seeing every block
/// of output (before the level is applied).
fn run_chain(
    o: &SimOptions,
    symbols: usize,
    mut each: impl FnMut(&[Iq]) -> Result<(), String>,
) -> Result<f64, String> {
    let mut m = Modulator::new(o.mode, o.cell_id, source(o)?)?;
    let fs_in = o.mode.bandwidth.sample_rate_hz();
    // Propagation at the signal's rate, the receiver's filter (the
    // resampler), then the receiver's own faults at its rate.
    let mut sim = ChannelSim::new(&o.channel, fs_in, m.mean_power());
    let fs_out = o.out_rate_hz.unwrap_or(fs_in);
    let mut radio = RadioSim::new(
        &o.channel,
        fs_out,
        sim.carrier_power(),
        o.mode.bandwidth.mhz() * 1e6,
    );
    let mut rs = match o.out_rate_hz {
        Some(r) if (r - fs_in).abs() > 1.0 => Some(Resampler::for_rates(
            fs_in,
            r,
            OCCUPIED_8MHZ_HZ * o.mode.bandwidth.mhz() / 8.0,
        )?),
        _ => None,
    };
    let (mut sym, mut chan, mut out, mut rx) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // Start after the byte interleaver has filled: until then data bypass the
    // randomiser as zeros, every carrier carries the same point, and the
    // symbol is one huge pulse.
    for _ in 0..m.warmup_symbols() {
        m.next_symbol(&mut sym);
    }
    let (mut sum, mut n) = (0.0f64, 0u64);
    for _ in 0..symbols {
        m.next_symbol(&mut sym);
        chan.clear();
        sim.process(&sym, &mut chan);
        let filtered: &[Iq] = match &mut rs {
            Some(r) => {
                out.clear();
                r.process(&chan, &mut out);
                &out
            }
            None => &chan,
        };
        rx.clear();
        radio.process(filtered, &mut rx);
        sum += rx.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>();
        n += rx.len() as u64;
        each(&rx)?;
    }
    Ok(sum / n.max(1) as f64)
}

/// Symbols to make for `seconds`.
pub fn symbols_for(o: &SimOptions) -> usize {
    let m = Modulator::new(o.mode, o.cell_id, Box::<TestStream>::default());
    let ts = m.map_or(1e-3, |m| m.mode().symbol_s());
    ((o.seconds / ts).ceil() as usize).max(1)
}

/// Make the signal, `sink` taking each block with the level applied.
pub fn generate(
    o: &SimOptions,
    mut sink: impl FnMut(&[Iq]) -> Result<(), String>,
) -> Result<Summary, String> {
    // A first pass over a few symbols finds the power the level is set
    // against (noise, echoes and the resampler all move it); the chain is
    // seeded and the modulator deterministic, so the second pass is the same
    // signal.
    let probe = run_chain(o, 24.min(symbols_for(o)), |_| Ok(()))?;
    let gain = (10f64.powf(o.level_dbfs / 20.0) / probe.sqrt()) as f32;
    let symbols = symbols_for(o);
    let mut s = Summary {
        symbols,
        gain,
        out_rate_hz: o
            .out_rate_hz
            .unwrap_or_else(|| o.mode.bandwidth.sample_rate_hz()),
        ..Default::default()
    };
    let mut scaled = Vec::new();
    run_chain(o, symbols, |block| {
        scaled.clear();
        scaled.extend(block.iter().map(|c| *c * gain));
        s.samples += scaled.len() as u64;
        s.clipped += scaled
            .iter()
            .filter(|c| c.re.abs() >= 1.0 || c.im.abs() >= 1.0)
            .count() as u64;
        sink(&scaled)
    })?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::dvbt::{Modulation, Rate};

    fn opts(mode: &str) -> SimOptions {
        SimOptions {
            mode: Mode::parse(mode).unwrap(),
            cell_id: None,
            ts_file: None,
            seconds: 0.02,
            out_rate_hz: Some(10e6),
            level_dbfs: -20.0,
            channel: Channel::default(),
        }
    }

    fn power(v: &[Iq]) -> f64 {
        v.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / v.len() as f64
    }

    #[test]
    fn the_output_has_the_asked_level_and_rate() {
        let o = opts("64qam-2/3-8k-1/32");
        let mut all = Vec::new();
        let s = generate(&o, |b| {
            all.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        // 20 ms: 22 symbols of 924 µs, at 10 MS/s.
        assert_eq!(s.symbols, 22);
        assert!(
            (all.len() as f64 / (22.0 * 924e-6 * 10e6) - 1.0).abs() < 0.01,
            "{}",
            all.len()
        );
        let rms_db = 10.0 * power(&all[2000..]).log10();
        assert!((rms_db + 20.0).abs() < 0.3, "{rms_db}");
        assert_eq!(s.clipped, 0);
        assert!((s.out_rate_hz - 10e6).abs() < 1.0);
        // No start-up pulse: the peak is an ordinary OFDM one, under 12 dB
        // above the rms (component peak 0.35 for an rms of 0.1).
        let peak = all[2000..]
            .iter()
            .map(|c| c.re.abs().max(c.im.abs()))
            .fold(0.0, f32::max);
        assert!(peak < 0.5, "{peak}");
        let early = all[..2000]
            .iter()
            .map(|c| c.re.abs().max(c.im.abs()))
            .fold(0.0, f32::max);
        assert!(early < 0.5, "{early}");
    }

    #[test]
    fn the_same_options_make_the_same_signal() {
        let mut o = opts("qpsk-1/2-2k-1/8");
        o.channel = Channel {
            cn_db: Some(15.0),
            paths: vec![crate::channel::Path::echo(1.5, -4.0)],
            ..Default::default()
        };
        let make = || {
            let mut v = Vec::new();
            generate(&o, |b| {
                v.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
            v
        };
        assert_eq!(make(), make());
    }

    #[test]
    fn a_wider_signal_than_the_output_is_refused() {
        let mut o = opts("qpsk-1/2-2k-1/8");
        o.out_rate_hz = Some(5e6);
        assert!(generate(&o, |_| Ok(())).is_err());
        let _ = (Modulation::Qpsk, Rate::R1_2);
    }
}
