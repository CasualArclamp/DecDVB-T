// made by CasualArclamp
//! Opening what the commands read: an IQ file (the centre and rate taken from
//! its name) or, for a UHF channel (or `hackrf` with a frequency), the HackRF
//! or the Airspy R2 (`--radio`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use decdvbt_core::SampleFormat;
use decdvbt_core::channel::{parse_channel, uhf_centre_hz};
use decdvbt_core::dvbt::Bandwidth;
use decdvbt_engine::DropProbe;
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

/// Which radio a live channel is received on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Radio {
    #[default]
    Hackrf,
    /// Airspy R2 (10 MS/s only).
    Airspy,
}

/// The radio choice and the Airspy's own settings, shared by the commands
/// that can receive live. The HackRF's gains keep their own `--lna/--vga/--amp`
/// flags, so every HackRF command line works as it did.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct RadioArgs {
    /// The radio to receive on.
    #[arg(long, value_enum, default_value_t = Radio::Hackrf)]
    pub radio: Radio,
    /// Airspy gain preset from libairspy: `sensitivity:N` or `linearity:N`
    /// (N 0-21, more is more gain). Default `sensitivity:10`. The three
    /// flags below override single stages of it.
    #[arg(long, value_name = "KIND:N")]
    pub airspy_preset: Option<String>,
    /// Airspy LNA gain index, 0-14.
    #[arg(long)]
    pub airspy_lna: Option<u8>,
    /// Airspy mixer gain index, 0-15.
    #[arg(long)]
    pub airspy_mixer: Option<u8>,
    /// Airspy IF (VGA) gain index, 0-15.
    #[arg(long)]
    pub airspy_vga: Option<u8>,
    /// Airspy packed 12-bit USB transfers (30 instead of 40 MB/s): try this
    /// if the port drops buffers.
    #[arg(long)]
    pub airspy_packed: bool,
}

#[cfg(feature = "airspy")]
impl RadioArgs {
    /// The preset (or the default) with single stages overridden.
    fn airspy_gains(&self) -> Result<decdvbt_io::AirspyGains> {
        use decdvbt_io::{AirspyGains, AirspyPreset};
        let mut g = match &self.airspy_preset {
            Some(p) => p
                .parse::<AirspyPreset>()
                .map_err(anyhow::Error::msg)?
                .gains(),
            None => AirspyGains::default(),
        };
        g.lna = self.airspy_lna.unwrap_or(g.lna);
        g.mixer = self.airspy_mixer.unwrap_or(g.mixer);
        g.vga = self.airspy_vga.unwrap_or(g.vga);
        for (name, v, max) in [
            ("--airspy-lna", g.lna, 14),
            ("--airspy-mixer", g.mixer, 15),
            ("--airspy-vga", g.vga, 15),
        ] {
            anyhow::ensure!(v <= max, "{name} {v} is above {max}");
        }
        Ok(g)
    }
}

/// A live radio at UHF channel `n`: the source and its dropped-buffer count.
/// `rate_msps` is the HackRF's rate (the Airspy has only 10); `gains` are the
/// HackRF's (the radio's defaults if `None`).
pub fn open_live(
    n: u8,
    rate_msps: f64,
    gains: Option<Gains>,
    radio: &RadioArgs,
) -> Result<(Box<dyn IqSource>, Option<DropProbe>)> {
    let center = uhf_centre_hz(n).unwrap();
    open_radio(center, rate_msps, None, gains, radio)
}

/// A live radio at `center` Hz. `rate_msps` is the HackRF's rate; `narrow`
/// (a 1 or 1.7 MHz channel) overrides it with the HackRF's lowest, 2 MS/s,
/// and its narrowest filter. The Airspy has one rate, 10 MS/s, and no
/// narrow-channel mode.
fn open_radio(
    center: f64,
    rate_msps: f64,
    narrow: Option<bool>,
    gains: Option<Gains>,
    radio: &RadioArgs,
) -> Result<(Box<dyn IqSource>, Option<DropProbe>)> {
    let narrow = narrow.unwrap_or(false);
    match radio.radio {
        #[cfg(feature = "hackrf")]
        Radio::Hackrf => {
            use decdvbt_io::{HackRfGains, HackRfSettings, HackRfSource};
            let rate = if narrow { 2.0 } else { rate_msps };
            let s = HackRfSource::open(HackRfSettings {
                center_hz: center.round() as u64,
                sample_rate: (rate * 1e6).round() as u32,
                gains: gains.map_or_else(HackRfGains::default, |g| HackRfGains {
                    amp: g.amp,
                    lna_db: g.lna,
                    vga_db: g.vga,
                }),
            })?;
            let ctl = s.control();
            if narrow {
                // The driver's own (0.75 of 2 MS/s) is its 1.75 MHz too; say so.
                ctl.set_filter_bandwidth(Some(1_750_000))?;
            }
            Ok((Box::new(s), Some(Box::new(move || ctl.overflows()))))
        }
        #[cfg(not(feature = "hackrf"))]
        Radio::Hackrf => {
            let _ = (center, rate_msps, narrow, gains);
            anyhow::bail!("built without the hackrf feature")
        }
        #[cfg(feature = "airspy")]
        Radio::Airspy => {
            use decdvbt_io::{AirspySettings, AirspySource};
            anyhow::ensure!(
                !narrow,
                "the Airspy R2 has no narrow-channel mode (its lowest rate is 10 MS/s): use the HackRF"
            );
            let s = AirspySource::open(AirspySettings {
                center_hz: center.round() as u64,
                // The Airspy has one rate; anything else is refused, not
                // quietly changed.
                sample_rate: (rate_msps * 1e6).round() as u32,
                gains: radio.airspy_gains()?,
                packed: radio.airspy_packed,
            })?;
            let ctl = s.control();
            Ok((Box::new(s), Some(Box::new(move || ctl.overflows()))))
        }
        #[cfg(not(feature = "airspy"))]
        Radio::Airspy => {
            let _ = (center, rate_msps, narrow, gains);
            anyhow::bail!("built without the airspy feature")
        }
    }
}

/// `source` is an IQ file, a UHF channel ("C26") for the radio `radio` names,
/// or `hackrf` for the HackRF at `tuning.freq_hz`; the HackRF takes `gains`
/// (its defaults if `None`). `file_rate_msps` is the rate of a raw file whose
/// name does not say.
pub fn open(
    source: &str,
    file_rate_msps: f64,
    gains: Option<Gains>,
    tuning: Tuning,
    radio: &RadioArgs,
) -> Result<Box<dyn IqSource>> {
    let centre = parse_channel(source)
        .and_then(uhf_centre_hz)
        .or_else(|| source.eq_ignore_ascii_case("hackrf").then_some(0.0))
        .map(|c| tuning.freq_hz.unwrap_or(c));
    match centre {
        Some(c) if c <= 0.0 => anyhow::bail!("hackrf: give the frequency (--freq MHZ)"),
        Some(c) => {
            anyhow::ensure!(
                radio.radio == Radio::Hackrf || !source.eq_ignore_ascii_case("hackrf"),
                "`hackrf` names the HackRF: give a channel (C26) for --radio airspy"
            );
            Ok(open_radio(c, 10.0, Some(tuning.bandwidth.narrow()), gains, radio)?.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "airspy")]
    #[test]
    fn airspy_gain_flags_override_the_preset() {
        use decdvbt_io::AirspyGains;
        let a = RadioArgs::default();
        assert_eq!(a.airspy_gains().unwrap(), AirspyGains::sensitivity(10));
        let a = RadioArgs {
            airspy_preset: Some("linearity:5".into()),
            airspy_vga: Some(3),
            ..Default::default()
        };
        let g = a.airspy_gains().unwrap();
        assert_eq!(g.vga, 3);
        assert_eq!(g.lna, AirspyGains::linearity(5).lna);
        assert!(
            RadioArgs {
                airspy_lna: Some(15),
                ..Default::default()
            }
            .airspy_gains()
            .is_err()
        );
        assert!(
            RadioArgs {
                airspy_preset: Some("nonsense".into()),
                ..Default::default()
            }
            .airspy_gains()
            .is_err()
        );
    }
}
