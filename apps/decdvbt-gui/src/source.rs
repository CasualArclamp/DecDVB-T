// made by CasualArclamp
//! Opening a source and running the engine on it: the HackRF or the Airspy R2
//! at a channel, or a recorded IQ file.

use std::path::PathBuf;

use decdvbt_core::SampleFormat;
use decdvbt_engine::{Rx, RxOptions};
use decdvbt_io::{IqFileReader, IqSource, format_from_path};

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

/// What to open.
#[derive(Debug, Clone)]
pub enum Spec {
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
    set_gains: Option<GainSetter>,
    set_air_gains: Option<AirGainSetter>,
    set_filter: Option<Setter<Option<f32>>>,
    set_ppm: Option<Setter<f32>>,
}

impl Session {
    pub fn open(spec: &Spec) -> Result<Session, String> {
        match spec {
            #[cfg(feature = "airspy")]
            Spec::Airspy {
                center_hz,
                gains,
                packed,
            } => {
                use decdvbt_io::{AirspyGains, AirspySettings, AirspySource};
                let src = AirspySource::open(AirspySettings {
                    center_hz: center_hz.round() as u64,
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
                    Some(Box::new(move || probe.overflows())),
                );
                Ok(Session {
                    rx,
                    live: true,
                    set_gains: None,
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
