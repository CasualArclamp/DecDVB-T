// made by CasualArclamp
//! Opening a source and running the engine on it: the HackRF at a channel, or
//! a recorded IQ file.

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

/// What to open.
#[derive(Debug, Clone)]
pub enum Spec {
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

/// A running source.
pub struct Session {
    pub rx: Rx,
    pub live: bool,
    set_gains: Option<GainSetter>,
}

impl Session {
    pub fn open(spec: &Spec) -> Result<Session, String> {
        match spec {
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
                let rx = Rx::start(
                    Box::new(src),
                    RxOptions::default(),
                    Some(Box::new(move || probe.overflows())),
                );
                Ok(Session {
                    rx,
                    live: true,
                    set_gains: Some(Box::new(move |g| {
                        ctl.set_gains(decdvbt_io::HackRfGains {
                            amp: g.amp,
                            lna_db: g.lna,
                            vga_db: g.vga,
                        })
                        .map_err(|e| e.to_string())
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
                })
            }
        }
    }

    /// Change the HackRF's gains (no-op for a file).
    pub fn set_gains(&self, g: Gains) -> Result<(), String> {
        match &self.set_gains {
            Some(f) => f(g),
            None => Ok(()),
        }
    }
}
