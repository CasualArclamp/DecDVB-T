// made by CasualArclamp
//! Opening a source and running the engine on it: the HackRF, the Airspy R2,
//! the Pluto, an RTL-SDR, one of the radios from DecSAT (`more_radios.rs`) or
//! network IQ at a channel, or a recorded IQ file.

use std::path::PathBuf;

use decdvbt_core::SampleFormat;
use decdvbt_engine::{Rx, RxOptions};
use decdvbt_io::{IqFileReader, IqSource, NetIqControl, RtlGains, format_from_path};

use crate::more_radios::{Extra, MoreControl, MoreRadios};
use crate::net_ui::NetUi;

/// The HackRF's three gain stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gains {
    /// RF amplifier, +14 dB.
    pub amp: bool,
    /// 0–40 dB in 8 dB steps.
    pub lna: u16,
    /// 0–62 dB in 2 dB steps.
    pub vga: u16,
}

impl Default for Gains {
    fn default() -> Self {
        // What decodes DVB-T on the author's roof: the RF amplifier on (off,
        // the front end's noise figure leaves 64-QAM undecodable), LNA 32,
        // VGA 8 (peaks near 0.8 of full scale).
        Gains {
            amp: true,
            lna: 32,
            vga: 8,
        }
    }
}

/// The Airspy R2's three gain stages (LNA 0–14, mixer 0–15, VGA 0–15; both
/// AGCs are off). Plain data here so the app and the prefs need not know
/// whether the build has the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AirGains {
    pub lna: u8,
    pub mixer: u8,
    pub vga: u8,
}

impl Default for AirGains {
    /// libairspy's sensitivity preset 10 (untested on a real aerial).
    fn default() -> Self {
        Preset::DEFAULT.gains()
    }
}

/// libairspy's one-knob gain presets, step 0–21.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub linearity: bool,
    pub step: u8,
}

impl Preset {
    pub const DEFAULT: Preset = Preset {
        linearity: false,
        step: 10,
    };

    pub fn gains(self) -> AirGains {
        #[cfg(feature = "airspy")]
        {
            let g = if self.linearity {
                decdvbt_io::AirspyGains::linearity(self.step)
            } else {
                decdvbt_io::AirspyGains::sensitivity(self.step)
            };
            AirGains {
                lna: g.lna,
                mixer: g.mixer,
                vga: g.vga,
            }
        }
        // Sensitivity 10, for a build without the Airspy driver.
        #[cfg(not(feature = "airspy"))]
        AirGains {
            lna: 12,
            mixer: 4,
            vga: 5,
        }
    }

    /// `sensitivity:10` / `linearity:3`, as kept in the prefs.
    pub fn key(self) -> String {
        format!(
            "{}:{}",
            if self.linearity {
                "linearity"
            } else {
                "sensitivity"
            },
            self.step
        )
    }

    pub fn parse(s: &str) -> Option<Preset> {
        let (kind, n) = s.split_once(':')?;
        let step: u8 = n.parse().ok().filter(|n| *n <= 21)?;
        match kind {
            "linearity" => Some(Preset {
                linearity: true,
                step,
            }),
            "sensitivity" => Some(Preset {
                linearity: false,
                step,
            }),
            _ => None,
        }
    }
}

/// The ADALM-PLUTO / Pluto+ as the GUI keeps it: where it is, its gain and
/// whether 8-bit IQ is used when the firmware (Tezuka) has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlutoUi {
    /// `auto`, `usb` or `ip:ADDRESS`.
    pub uri: String,
    /// Manual gain, dB.
    pub gain_db: i32,
    /// The AD9361's slow-attack AGC instead of `gain_db`.
    pub agc: bool,
    pub eight_bit: bool,
}

impl Default for PlutoUi {
    fn default() -> Self {
        PlutoUi {
            uri: "auto".into(),
            gain_db: 50,
            agc: false,
            eight_bit: true,
        }
    }
}

impl PlutoUi {
    /// The AD9361's gain range (about −3 to 71 dB, less at the band edges).
    pub const GAIN_DB: std::ops::RangeInclusive<i32> = -3..=71;

    /// The rate the Pluto runs at for a channel: the 8 MHz mode's OFDM clock
    /// (64/7 MS/s) for 6–8 MHz channels, the least the link must carry;
    /// 2.5 MS/s for the narrow ones.
    pub fn rate_hz(narrow: bool) -> f64 {
        if narrow { 2.5e6 } else { 64e6 / 7.0 }
    }

    #[cfg(feature = "pluto")]
    fn gain(&self) -> decdvbt_io::PlutoGain {
        if self.agc {
            decdvbt_io::PlutoGain::Agc
        } else {
            decdvbt_io::PlutoGain::Manual(f64::from(self.gain_db))
        }
    }
}

/// What to open.
#[derive(Debug, Clone)]
pub enum Spec {
    #[cfg_attr(not(feature = "pluto"), allow(dead_code))]
    Pluto {
        center_hz: f64,
        /// A 1 or 1.7 MHz channel (2.5 MS/s and the 1.75 MHz filter).
        narrow: bool,
        ui: PlutoUi,
    },
    #[cfg_attr(not(feature = "airspy"), allow(dead_code))]
    Airspy {
        center_hz: f64,
        gains: AirGains,
        packed: bool,
    },
    HackRf {
        center_hz: f64,
        rate_hz: f64,
        gains: Gains,
    },
    /// An RTL-SDR, at its 2.048 MS/s: the 1 and 1.7 MHz channels.
    #[cfg_attr(not(feature = "rtlsdr"), allow(dead_code))]
    RtlSdr { center_hz: f64, gains: RtlGains },
    /// Raw IQ from another program; `center_hz` labels it (an rtl_tcp
    /// server is tuned to it).
    Net { net: NetUi, center_hz: f64 },
    /// The Fobos, an SDRplay, a USRP or a DecSAT server's radio.
    More {
        kind: Extra,
        center_hz: f64,
        // Rust note: boxed so this one variant does not make every `Spec`
        // as large as the whole settings struct.
        radios: Box<MoreRadios>,
    },
    File {
        path: PathBuf,
        center_hz: f64,
        rate_hz: f64,
    },
}

type GainSetter = Box<dyn Fn(Gains) -> Result<(), String>>;
type Setter<T> = Box<dyn Fn(T) -> Result<(), String>>;
type AirGainSetter = Setter<AirGains>;

/// A running source.
pub struct Session {
    pub rx: Rx,
    pub live: bool,
    /// What the radio said about itself, for the status line (the Pluto's
    /// link, firmware and sample width).
    pub info: Option<String>,
    /// The network stream's counters, when that is the source.
    pub net: Option<NetIqControl>,
    /// The live controls of a radio from DecSAT, when one is the source.
    pub more: Option<MoreControl>,
    set_rtl_gains: Option<Setter<RtlGains>>,
    set_gains: Option<GainSetter>,
    set_pluto: Option<Setter<PlutoUi>>,
    set_air_gains: Option<AirGainSetter>,
    set_filter: Option<Setter<Option<f32>>>,
    set_ppm: Option<Setter<f32>>,
}

impl Session {
    pub fn open(spec: &Spec) -> Result<Session, String> {
        match spec {
            #[cfg(feature = "pluto")]
            Spec::Pluto {
                center_hz,
                narrow,
                ui,
            } => {
                use decdvbt_io::{EightBit, PlutoSettings, PlutoSource};
                let src = PlutoSource::open(PlutoSettings {
                    uri: ui.uri.parse()?,
                    center_hz: *center_hz,
                    sample_rate: PlutoUi::rate_hz(*narrow),
                    bandwidth: narrow.then_some(1.75e6),
                    gain: ui.gain(),
                    eight_bit: if ui.eight_bit {
                        EightBit::Auto
                    } else {
                        EightBit::Off
                    },
                })
                .map_err(|e| e.to_string())?;
                let ctl = src.control();
                let i = ctl.info().clone();
                let info = format!(
                    "Pluto on {}: {} {}, {}-bit IQ",
                    i.link,
                    i.model,
                    i.firmware,
                    if i.eight_bit { 8 } else { 16 }
                );
                let probe = ctl.clone();
                let rx = Rx::start(
                    Box::new(src),
                    RxOptions::default(),
                    Some(Box::new(move || probe.lost())),
                );
                Ok(Session {
                    rx,
                    live: true,
                    net: None,
                    more: None,
                    set_rtl_gains: None,
                    info: Some(info),
                    set_gains: None,
                    set_air_gains: None,
                    set_pluto: Some(Box::new(move |u: PlutoUi| {
                        ctl.set_gain(u.gain()).map_err(|e| e.to_string())
                    })),
                    set_filter: None,
                    set_ppm: None,
                })
            }
            #[cfg(not(feature = "pluto"))]
            Spec::Pluto { .. } => Err("built without the pluto feature".into()),
            #[cfg(feature = "airspy")]
            Spec::Airspy {
                center_hz,
                gains,
                packed,
            } => {
                use decdvbt_io::{AirspyGains, AirspySettings, AirspySource};
                let src = AirspySource::open(AirspySettings {
                    center_hz: center_hz.round() as u64,
                    // The board's nearest: a Mini runs at 6 MS/s.
                    sample_rate: 10_000_000,
                    gains: AirspyGains {
                        lna: gains.lna,
                        mixer: gains.mixer,
                        vga: gains.vga,
                    },
                    packed: *packed,
                })
                .map_err(|e| e.to_string())?;
                let ctl = src.control();
                let probe = ctl.clone();
                let rx = Rx::start(
                    Box::new(src),
                    RxOptions::default(),
                    Some(Box::new(move || probe.lost())),
                );
                Ok(Session {
                    rx,
                    live: true,
                    net: None,
                    more: None,
                    set_rtl_gains: None,
                    info: None,
                    set_gains: None,
                    set_pluto: None,
                    set_air_gains: Some(Box::new(move |g| {
                        ctl.set_gains(AirspyGains {
                            lna: g.lna,
                            mixer: g.mixer,
                            vga: g.vga,
                        })
                        .map_err(|e| e.to_string())
                    })),
                    // The HackRF's baseband filter and clock trim do not
                    // exist on this radio.
                    set_filter: None,
                    set_ppm: None,
                })
            }
            #[cfg(not(feature = "airspy"))]
            Spec::Airspy { .. } => Err("built without the airspy feature".into()),
            #[cfg(feature = "hackrf")]
            Spec::HackRf {
                center_hz,
                rate_hz,
                gains,
            } => {
                use decdvbt_io::{HackRfGains, HackRfSettings, HackRfSource};
                let src = HackRfSource::open(HackRfSettings {
                    center_hz: center_hz.round() as u64,
                    sample_rate: rate_hz.round() as u32,
                    gains: HackRfGains {
                        amp: gains.amp,
                        lna_db: gains.lna,
                        vga_db: gains.vga,
                    },
                })
                .map_err(|e| e.to_string())?;
                let ctl = src.control();
                let probe = ctl.clone();
                let (filter_ctl, ppm_ctl) = (ctl.clone(), ctl.clone());
                let rx = Rx::start(
                    Box::new(src),
                    RxOptions::default(),
                    Some(Box::new(move || probe.overflows())),
                );
                Ok(Session {
                    rx,
                    live: true,
                    net: None,
                    more: None,
                    set_rtl_gains: None,
                    info: None,
                    set_pluto: None,
                    set_air_gains: None,
                    set_gains: Some(Box::new(move |g| {
                        ctl.set_gains(decdvbt_io::HackRfGains {
                            amp: g.amp,
                            lna_db: g.lna,
                            vga_db: g.vga,
                        })
                        .map_err(|e| e.to_string())
                    })),
                    set_filter: Some(Box::new(move |mhz| {
                        filter_ctl
                            .set_filter_bandwidth(mhz.map(|f| (f * 1e6) as u32))
                            .map_err(|e| e.to_string())
                    })),
                    set_ppm: Some(Box::new(move |ppm| {
                        ppm_ctl.set_ppm(f64::from(ppm)).map_err(|e| e.to_string())
                    })),
                })
            }
            #[cfg(not(feature = "hackrf"))]
            Spec::HackRf { .. } => Err("built without the hackrf feature".into()),
            #[cfg(feature = "rtlsdr")]
            Spec::RtlSdr { center_hz, gains } => {
                use decdvbt_io::{RtlSdrSource, RtlSettings};
                let src = RtlSdrSource::open(RtlSettings {
                    center_hz: center_hz.round() as u64,
                    gains: *gains,
                    ..RtlSettings::default()
                })
                .map_err(|e| e.to_string())?;
                let ctl = src.control();
                let probe = ctl.clone();
                let rx = Rx::start(
                    Box::new(src),
                    RxOptions::default(),
                    Some(Box::new(move || probe.overflows())),
                );
                Ok(Session {
                    rx,
                    live: true,
                    net: None,
                    more: None,
                    set_rtl_gains: Some(Box::new(move |g| {
                        ctl.set_gains(g).map_err(|e| e.to_string())
                    })),
                    info: None,
                    set_gains: None,
                    set_pluto: None,
                    set_air_gains: None,
                    set_filter: None,
                    set_ppm: None,
                })
            }
            #[cfg(not(feature = "rtlsdr"))]
            Spec::RtlSdr { .. } => Err("built without the rtlsdr feature".into()),
            Spec::Net { net, center_hz } => {
                let (src, ctl) = net.open(*center_hz)?;
                let probe = ctl.clone();
                let rx = Rx::start(
                    src,
                    RxOptions::default(),
                    Some(Box::new(move || probe.overflows())),
                );
                Ok(Session {
                    rx,
                    live: true,
                    net: Some(ctl),
                    more: None,
                    set_rtl_gains: None,
                    info: None,
                    set_gains: None,
                    set_pluto: None,
                    set_air_gains: None,
                    set_filter: None,
                    set_ppm: None,
                })
            }
            Spec::More {
                kind,
                center_hz,
                radios,
            } => {
                let (src, control) = radios.open(*kind, *center_hz)?;
                let rx = Rx::start(src, RxOptions::default(), Some(control.drop_probe()));
                Ok(Session {
                    rx,
                    live: true,
                    net: None,
                    info: Some(control.info()).filter(|i| !i.is_empty()),
                    more: Some(control),
                    set_rtl_gains: None,
                    set_gains: None,
                    set_pluto: None,
                    set_air_gains: None,
                    set_filter: None,
                    set_ppm: None,
                })
            }
            Spec::File {
                path,
                center_hz,
                rate_hz,
            } => {
                let fmt = format_from_path(path).unwrap_or(SampleFormat::Cs8);
                let src = IqFileReader::open(path, fmt, *rate_hz, 1 << 17)
                    .map_err(|e| format!("cannot open {}: {e}", path.display()))?
                    .with_center_freq(*center_hz);
                let src: Box<dyn IqSource> = Box::new(src);
                Ok(Session {
                    rx: Rx::start(src, RxOptions::default(), None),
                    live: false,
                    net: None,
                    more: None,
                    set_rtl_gains: None,
                    info: None,
                    set_pluto: None,
                    set_gains: None,
                    set_air_gains: None,
                    set_filter: None,
                    set_ppm: None,
                })
            }
        }
    }

    /// The HackRF's baseband filter, MHz (`None`: automatic); no-op for a file.
    pub fn set_filter_mhz(&self, mhz: Option<f32>) -> Result<(), String> {
        self.set_filter.as_ref().map_or(Ok(()), |f| f(mhz))
    }

    /// The HackRF's frequency error, ppm; no-op for a file.
    pub fn set_ppm(&self, ppm: f32) -> Result<(), String> {
        self.set_ppm.as_ref().map_or(Ok(()), |f| f(ppm))
    }

    /// Change the RTL-SDR's gain (no-op for any other source).
    pub fn set_rtl_gains(&self, g: RtlGains) -> Result<(), String> {
        self.set_rtl_gains.as_ref().map_or(Ok(()), |f| f(g))
    }

    /// Change the Pluto's gain or AGC (no-op for any other source).
    pub fn set_pluto(&self, u: &PlutoUi) -> Result<(), String> {
        self.set_pluto.as_ref().map_or(Ok(()), |f| f(u.clone()))
    }

    /// Change the Airspy's gains (no-op for any other source).
    pub fn set_air_gains(&self, g: AirGains) -> Result<(), String> {
        self.set_air_gains.as_ref().map_or(Ok(()), |f| f(g))
    }

    /// Change the HackRF's gains (no-op for a file or the Airspy).
    pub fn set_gains(&self, g: Gains) -> Result<(), String> {
        match &self.set_gains {
            Some(f) => f(g),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip_through_their_key() {
        for p in [
            Preset::DEFAULT,
            Preset {
                linearity: true,
                step: 0,
            },
            Preset {
                linearity: false,
                step: 21,
            },
        ] {
            assert_eq!(Preset::parse(&p.key()), Some(p));
        }
        assert_eq!(Preset::parse("sensitivity:22"), None);
        assert_eq!(Preset::parse("loud:1"), None);
        assert_eq!(Preset::parse(""), None);
    }

    #[test]
    fn the_default_is_sensitivity_ten() {
        assert_eq!(
            AirGains::default(),
            AirGains {
                lna: 12,
                mixer: 4,
                vga: 5
            }
        );
    }
}
