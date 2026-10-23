// made by CasualArclamp
//! Opening what the commands read: an IQ file (the centre and rate taken from
//! its name) or, for a UHF channel, the HackRF.

use std::path::PathBuf;

use anyhow::{Context, Result};
use decdvbt_core::SampleFormat;
use decdvbt_core::channel::parse_channel;
#[cfg(feature = "hackrf")]
use decdvbt_core::channel::uhf_centre_hz;
use decdvbt_io::{IqFileReader, IqSource, format_from_path, name_hint};

/// `source` is an IQ file, or a UHF channel ("C26") for the HackRF.
/// `file_rate_msps` is the rate of a raw file whose name does not say.
pub fn open(source: &str, file_rate_msps: f64) -> Result<Box<dyn IqSource>> {
    match parse_channel(source) {
        #[cfg(feature = "hackrf")]
        Some(n) => {
            use decdvbt_io::{HackRfGains, HackRfSettings, HackRfSource};
            Ok(Box::new(HackRfSource::open(HackRfSettings {
                center_hz: uhf_centre_hz(n).unwrap().round() as u64,
                sample_rate: 10_000_000,
                gains: HackRfGains::default(),
            })?))
        }
        #[cfg(not(feature = "hackrf"))]
        Some(_) => anyhow::bail!("built without the hackrf feature"),
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
