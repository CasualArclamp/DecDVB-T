// made by CasualArclamp
//! Opening what the commands read: an IQ file (the centre and rate taken from
//! its name) or, for a UHF channel (or `hackrf` with a frequency), the HackRF
//! the Airspy R2 / HydraSDR, the ADALM-PLUTO / Pluto+, an RTL-SDR, a Fobos
//! SDR, an SDRplay RSP, a USRP, a radio behind a DecSAT server or network IQ
//! (`--radio`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use decdvbt_core::SampleFormat;
use decdvbt_core::channel::{centre_hz, parse_channel};
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
    /// Airspy R2 or HydraSDR RFOne (the same protocol; 10 MS/s only).
    #[value(alias = "hydrasdr")]
    Airspy,
    /// ADALM-PLUTO or Pluto+, over USB or the network (64/7 MS/s for 6-8
    /// MHz channels, 2.5 MS/s for 1 and 1.7 MHz).
    #[value(alias = "plutoplus")]
    Pluto,
    /// RTL-SDR (R820T/R828D), 2.048 MS/s: 1 and 1.7 MHz channels only.
    #[value(alias = "rtl")]
    Rtlsdr,
    /// RigExpert Fobos SDR (needs RigExpert's fobos_sdr / fobos library).
    Fobos,
    /// SDRplay RSP (needs the SDRplay API installed).
    #[value(alias = "rsp")]
    Sdrplay,
    /// A USRP through UHD (uhd.dll, Windows).
    #[value(alias = "usrp")]
    Uhd,
    /// A radio behind DecSAT's decsat-server (`--remote HOST[:PORT]`).
    #[value(alias = "decsat")]
    Remote,
    /// Raw IQ over the network (`--net URL --radio-rate MSPS`), tuned at
    /// its sender (an rtl_tcp server is tuned from here).
    #[value(alias = "network")]
    Net,
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
    /// Where the Pluto is: `auto` (USB, else 192.168.2.1), `usb`,
    /// `ip:ADDRESS` (a Pluto+ on Ethernet).
    #[arg(long, default_value = "auto", value_name = "URI")]
    pub pluto_uri: String,
    /// Pluto receive gain, dB (about -3 to 71), or `agc`.
    #[arg(long, default_value = "50", value_name = "DB|agc")]
    pub pluto_gain: String,
    /// Stream 16-bit IQ even when the firmware (Tezuka) offers 8-bit.
    #[arg(long)]
    pub pluto_16bit: bool,
    /// The rate of the Fobos, SDRplay, USRP, remote radio or network
    /// stream, MS/s (10 unless given; a network stream's must be given).
    #[arg(long, value_name = "MSPS")]
    pub radio_rate: Option<f64>,
    /// RTL-SDR tuner gain, dB (0-49.6), or `agc`.
    #[arg(long, default_value = "40.2", value_name = "DB|agc")]
    pub rtl_gain: String,
    /// Fobos gains: LNA step 0-3 and VGA step 0-31 (2 dB each).
    #[arg(long, value_name = "LNA,VGA")]
    pub fobos_gain: Option<String>,
    /// Lock the Fobos to 10 MHz on its CLK IN socket.
    #[arg(long)]
    pub fobos_ext_clock: bool,
    /// SDRplay gain reductions: LNA state (0 = most gain) and IF gain
    /// reduction 20-59 dB.
    #[arg(long, value_name = "LNASTATE,IFGR")]
    pub sdrplay_gain: Option<String>,
    /// Use an RSPduo's Tuner B.
    #[arg(long)]
    pub sdrplay_tuner_b: bool,
    /// UHD device selector (`serial=…`, `type=b200`, `addr=…`); the first
    /// USRP found if not given.
    #[arg(long, default_value = "", value_name = "ARGS")]
    pub uhd_args: String,
    /// USRP receive gain, dB.
    #[arg(long, default_value_t = 35.0, value_name = "DB")]
    pub uhd_gain: f64,
    /// USRP receive port.
    #[arg(long, default_value = "RX2", value_name = "PORT")]
    pub uhd_antenna: String,
    /// A decsat-server to take a radio from: HOST or HOST:PORT.
    #[arg(long, value_name = "HOST[:PORT]")]
    pub remote: Option<String>,
    /// The server's name for the radio (hackrf, pluto, …); its first if not
    /// given.
    #[arg(long, default_value = "", value_name = "KEY")]
    pub remote_radio: String,
    /// A file holding the server's password (else the variable
    /// DECDVBT_REMOTE_PASSWORD), so it stays out of the shell's history.
    #[arg(long, value_name = "FILE")]
    pub remote_password_file: Option<PathBuf>,
    /// Bits a sample component on the link: 4, 6, 8, 10, 12 or 16.
    #[arg(long, default_value_t = 8)]
    pub remote_bits: u8,
    /// Have the server zstd-compress the IQ.
    #[arg(long)]
    pub remote_zstd: bool,
    /// The remote radio's gains, by the server's stage names.
    #[arg(long, value_name = "NAME=V,NAME=V")]
    pub remote_gain: Option<String>,
    /// Network IQ: tcp://HOST:PORT, listen://ADDR:PORT or udp://ADDR:PORT.
    #[arg(long, value_name = "URL")]
    pub net: Option<String>,
    /// Network IQ's sample type: int8, uint8, int16, int32 or float32.
    #[arg(long, default_value = "int8", value_name = "TYPE")]
    pub net_iq: String,
}

/// `A,B` as two numbers.
#[cfg(any(feature = "fobos", feature = "sdrplay"))]
fn gain_pair(s: &str, what: &str) -> Result<(u8, u8)> {
    let bad = || anyhow::anyhow!("{what}, not {s:?}");
    let (a, b) = s.split_once(',').ok_or_else(bad)?;
    Ok((
        a.trim().parse().map_err(|_| bad())?,
        b.trim().parse().map_err(|_| bad())?,
    ))
}

impl RadioArgs {
    /// The rate the radios without a fixed one run at, S/s: `--radio-rate`,
    /// else 10 MS/s (2 for a 1 or 1.7 MHz channel).
    #[allow(dead_code)] // unused when none of those radios is built in
    fn other_rate(&self, narrow: bool) -> f64 {
        self.radio_rate.unwrap_or(if narrow { 2.0 } else { 10.0 }) * 1e6
    }
}

#[cfg(feature = "pluto")]
impl RadioArgs {
    fn pluto_gain(&self) -> Result<decdvbt_io::PlutoGain> {
        use decdvbt_io::PlutoGain;
        if self.pluto_gain.eq_ignore_ascii_case("agc") {
            return Ok(PlutoGain::Agc);
        }
        let db: f64 = self.pluto_gain.parse().map_err(|_| {
            anyhow::anyhow!("--pluto-gain takes dB or agc, not {:?}", self.pluto_gain)
        })?;
        Ok(PlutoGain::Manual(db))
    }
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
    let center = centre_hz(n).unwrap();
    open_radio(center, rate_msps, None, gains, radio)
}

/// A live radio at `center` Hz. `rate_msps` is the HackRF's rate; `narrow`
/// (a 1 or 1.7 MHz channel) overrides it with the HackRF's lowest, 2 MS/s,
/// and its narrowest filter. The Airspy has one rate, 10 MS/s, whatever the
/// width.
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
            // A narrow channel is decoded from the 10 MS/s stream as it is
            // (tests/narrow_wide.rs): the receivers take the radio's rate.
            let s = AirspySource::open(AirspySettings {
                center_hz: center.round() as u64,
                // The Airspy has one rate; anything else is refused, not
                // quietly changed.
                sample_rate: (rate_msps * 1e6).round() as u32,
                gains: radio.airspy_gains()?,
                packed: radio.airspy_packed,
            })?;
            let ctl = s.control();
            Ok((Box::new(s), Some(Box::new(move || ctl.lost()))))
        }
        #[cfg(not(feature = "airspy"))]
        Radio::Airspy => {
            let _ = (center, rate_msps, narrow, gains);
            anyhow::bail!("built without the airspy feature")
        }
        #[cfg(feature = "pluto")]
        Radio::Pluto => {
            use decdvbt_io::{EightBit, PlutoSettings, PlutoSource};
            // The OFDM clock of an 8 MHz channel (64/7 MS/s) carries 6-8 MHz
            // channels with the least load on the link; narrow ones need far
            // less, and the AD9361's filter then narrows to 1.75 MHz.
            let (rate, bw) = if narrow {
                (2.5e6, Some(1.75e6))
            } else {
                (64e6 / 7.0, None)
            };
            let s = PlutoSource::open(PlutoSettings {
                uri: radio.pluto_uri.parse().map_err(anyhow::Error::msg)?,
                center_hz: center,
                sample_rate: rate,
                bandwidth: bw,
                gain: radio.pluto_gain()?,
                eight_bit: if radio.pluto_16bit {
                    EightBit::Off
                } else {
                    EightBit::Auto
                },
            })?;
            let ctl = s.control();
            Ok((Box::new(s), Some(Box::new(move || ctl.lost()))))
        }
        #[cfg(not(feature = "pluto"))]
        Radio::Pluto => {
            let _ = (center, rate_msps, narrow, gains);
            anyhow::bail!("built without the pluto feature")
        }
        #[cfg(feature = "rtlsdr")]
        Radio::Rtlsdr => {
            use decdvbt_io::{RtlGains, RtlSdrSource, RtlSettings};
            // 2.048 MS/s whatever the width: a 5 to 8 MHz channel does not
            // fit, and the receiver will simply not lock.
            let s = RtlSdrSource::open(RtlSettings {
                center_hz: center.round() as u64,
                gains: RtlGains::parse(&radio.rtl_gain).map_err(anyhow::Error::msg)?,
                ..RtlSettings::default()
            })?;
            let ctl = s.control();
            Ok((Box::new(s), Some(Box::new(move || ctl.overflows()))))
        }
        #[cfg(not(feature = "rtlsdr"))]
        Radio::Rtlsdr => anyhow::bail!("built without the rtlsdr feature"),
        #[cfg(feature = "fobos")]
        Radio::Fobos => {
            use decdvbt_io::{FobosGains, FobosSettings, FobosSource};
            let d = FobosSettings::default();
            let gains = match &radio.fobos_gain {
                Some(g) => {
                    let (lna, vga) = gain_pair(g, "--fobos-gain takes LNA,VGA (0-3, 0-31)")?;
                    FobosGains { lna, vga }.snapped()
                }
                None => d.gains,
            };
            // The Fobos starts at 8 MS/s: a narrow channel is taken from that.
            let s = FobosSource::open(FobosSettings {
                center_hz: center.round() as u64,
                sample_rate: radio.other_rate(false).max(8e6).round() as u32,
                gains,
                external_clock: radio.fobos_ext_clock,
            })?;
            let ctl = s.control();
            Ok((Box::new(s), Some(Box::new(move || ctl.overflows()))))
        }
        #[cfg(not(feature = "fobos"))]
        Radio::Fobos => anyhow::bail!("built without the fobos feature"),
        #[cfg(feature = "sdrplay")]
        Radio::Sdrplay => {
            use decdvbt_io::{SdrplayGains, SdrplaySettings, SdrplaySource};
            let d = SdrplaySettings::default();
            let gains = match &radio.sdrplay_gain {
                Some(g) => {
                    let (lna, gr) =
                        gain_pair(g, "--sdrplay-gain takes LNASTATE,IFGR (0-9, 20-59)")?;
                    SdrplayGains { lna, gr }.snapped()
                }
                None => d.gains,
            };
            let s = SdrplaySource::open(SdrplaySettings {
                center_hz: center.round() as u64,
                sample_rate: radio.other_rate(narrow).round() as u32,
                gains,
                tuner_b: radio.sdrplay_tuner_b,
            })?;
            let ctl = s.control();
            Ok((Box::new(s), Some(Box::new(move || ctl.overflows()))))
        }
        #[cfg(not(feature = "sdrplay"))]
        Radio::Sdrplay => anyhow::bail!("built without the sdrplay feature"),
        #[cfg(all(windows, feature = "uhd"))]
        Radio::Uhd => {
            use decdvbt_io::{UhdSettings, UhdSource};
            let s = UhdSource::open(UhdSettings {
                device_args: radio.uhd_args.clone(),
                center_hz: center,
                rate: radio.other_rate(narrow),
                gain: radio.uhd_gain,
                antenna: radio.uhd_antenna.clone(),
                ..UhdSettings::default()
            })?;
            Ok((Box::new(s), None))
        }
        #[cfg(not(all(windows, feature = "uhd")))]
        Radio::Uhd => {
            anyhow::bail!("built without the uhd feature (uhd.dll is loaded on Windows only)")
        }
        #[cfg(feature = "remote")]
        Radio::Remote => {
            use decdvbt_io::remote::{ConnectOptions, IqParams, RadioRequest, RemoteIqSource};
            let password = match &radio.remote_password_file {
                Some(f) => Some(
                    std::fs::read_to_string(f)
                        .with_context(|| format!("--remote-password-file {}", f.display()))?
                        .trim()
                        .to_string(),
                ),
                None => std::env::var("DECDVBT_REMOTE_PASSWORD").ok(),
            };
            let mut stages = Vec::new();
            for part in radio
                .remote_gain
                .as_deref()
                .unwrap_or("")
                .split(',')
                .filter(|p| !p.is_empty())
            {
                let (name, v) = part
                    .split_once('=')
                    .context("--remote-gain takes NAME=VALUE,NAME=VALUE")?;
                let v: f64 = v.parse().with_context(|| format!("--remote-gain {part}"))?;
                stages.push((name.to_string(), v));
            }
            let s = RemoteIqSource::connect(
                &ConnectOptions {
                    addr: radio
                        .remote
                        .clone()
                        .context("--radio remote: give --remote HOST[:PORT]")?,
                    name: "decdvbt".into(),
                    password,
                    takeover: false,
                },
                RadioRequest {
                    key: radio.remote_radio.clone(),
                    center_hz: center,
                    sample_rate: radio.other_rate(narrow),
                    gains: stages,
                },
                IqParams {
                    bits: radio.remote_bits,
                    zstd: if radio.remote_zstd { 1 } else { 0 },
                    slice: None,
                },
            )?;
            let lost = s.dropped_counter();
            Ok((
                Box::new(s),
                Some(Box::new(move || {
                    lost.load(std::sync::atomic::Ordering::Relaxed)
                })),
            ))
        }
        #[cfg(not(feature = "remote"))]
        Radio::Remote => anyhow::bail!("built without the remote feature"),
        Radio::Net => {
            use decdvbt_io::{NetIqSettings, NetIqSource, NetSample};
            let url = radio
                .net
                .as_deref()
                .context("--radio net: give --net tcp://HOST:PORT (or listen://, udp://)")?;
            let (mode, addr) = NetIqSettings::parse_url(url)?;
            let s = NetIqSource::open(NetIqSettings {
                mode,
                addr,
                sample: NetSample::parse(&radio.net_iq).with_context(|| {
                    format!(
                        "--net-iq {}: int8, uint8, int16, int32 or float32",
                        radio.net_iq
                    )
                })?,
                sample_rate: radio
                    .radio_rate
                    .context("--radio net: give --radio-rate MSPS (the stream does not say)")?
                    * 1e6,
                center_hz: center,
            })?;
            let ctl = s.control();
            Ok((Box::new(s), Some(Box::new(move || ctl.overflows()))))
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
        .and_then(centre_hz)
        // `radio` is whichever radio --radio names, at --freq.
        .or_else(|| {
            (source.eq_ignore_ascii_case("hackrf") || source.eq_ignore_ascii_case("radio"))
                .then_some(0.0)
        })
        .map(|c| tuning.freq_hz.unwrap_or(c));
    match centre {
        Some(c) if c <= 0.0 => anyhow::bail!("{source}: give the frequency (--freq MHZ)"),
        Some(c) => {
            anyhow::ensure!(
                radio.radio == Radio::Hackrf || !source.eq_ignore_ascii_case("hackrf"),
                "`hackrf` names the HackRF: give a channel (C26), or `radio` with --freq, for another --radio"
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
