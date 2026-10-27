// made by CasualArclamp
//! Opening what the commands read: an IQ file (the centre and rate taken from
//! its name) or, for a UHF channel or `hackrf` with a frequency, the HackRF.

use std::path::PathBuf;

use anyhow::{Context, Result};
use decdvbt_core::SampleFormat;
use decdvbt_core::channel::{parse_channel, uhf_centre_hz};
use decdvbt_core::dvbt::Bandwidth;
use decdvbt_io::{IqFileReader, IqSource, format_from_path, name_hint};

/// The HackRF's gains: the RF amplifier (+14 dB), LNA and VGA, dB (read
/// only by a build with the radio).
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "hackrf"), allow(dead_code))]
pub struct Gains {
    pub amp: bool,
    pub lna: u16,
    pub vga: u16,
}

/// How a live radio is tuned beyond the channel: a free centre frequency
/// (for `hackrf` as the source) and the channel's width, which picks the
/// radio's rate and filter (10 MS/s and the driver's filter for 6 to 8 MHz;
/// 2 MS/s, its lowest, and the 1.75 MHz filter, its narrowest, for 1 and
/// 1.7 MHz).
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "hackrf"), allow(dead_code))]
pub struct Tuning {
    pub freq_hz: Option<f64>,
    pub bandwidth: Bandwidth,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            freq_hz: None,
            bandwidth: Bandwidth::Mhz8,
        }
    }
}

/// `source` is an IQ file, a UHF channel ("C26") for the HackRF, or `hackrf`
/// for the HackRF at `tuning.freq_hz`; the radio takes `gains` (its defaults
/// if `None`). `file_rate_msps` is the rate of a raw file whose name does not
/// say.
pub fn open(
    source: &str,
    file_rate_msps: f64,
    gains: Option<Gains>,
    tuning: Tuning,
) -> Result<Box<dyn IqSource>> {
    let centre = parse_channel(source)
        .and_then(uhf_centre_hz)
        .or_else(|| source.eq_ignore_ascii_case("hackrf").then_some(0.0))
        .map(|c| tuning.freq_hz.unwrap_or(c));
    match centre {
        Some(c) if c <= 0.0 => anyhow::bail!("hackrf: give the frequency (--freq MHZ)"),
        #[cfg(feature = "hackrf")]
        Some(c) => {
            use decdvbt_io::{HackRfGains, HackRfSettings, HackRfSource};
            let narrow = tuning.bandwidth.narrow();
            let src = HackRfSource::open(HackRfSettings {
                center_hz: c.round() as u64,
                sample_rate: if narrow { 2_000_000 } else { 10_000_000 },
                gains: gains.map_or_else(HackRfGains::default, |g| HackRfGains {
                    amp: g.amp,
                    lna_db: g.lna,
                    vga_db: g.vga,
                }),
            })?;
            if narrow {
                // The driver's own (0.75 of 2 MS/s) is its 1.75 MHz too; say so.
                src.control().set_filter_bandwidth(Some(1_750_000))?;
            }
            Ok(Box::new(src))
        }
        #[cfg(not(feature = "hackrf"))]
        Some(_) => {
            let _ = (gains, tuning);
            anyhow::bail!("built without the hackrf feature")
        }
        None => {
            let path = PathBuf::from(source);
            let fmt = format_from_path(&path).unwrap_or(SampleFormat::Cs8);
            let hint = name_hint(&path);
            Ok(Box::new(
                IqFileReader::open(
                    &path,
                    fmt,
                    hint.sample_rate.unwrap_or(file_rate_msps * 1e6),
                    1 << 17,
                )
                .with_context(|| format!("cannot open {source}"))?
                .with_center_freq(hint.center_hz.unwrap_or(0.0)),
            ))
        }
    }
}
