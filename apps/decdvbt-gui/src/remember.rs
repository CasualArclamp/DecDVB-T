// made by CasualArclamp
//! What the app keeps from one run to the next, beyond the SDR window's own
//! settings and the learned carrier offsets: the radio's channel and
//! frequency, gains and sample rate, the channel's width, the capture last
//! opened, the panel and frame rate in use, the window's size.
//!
//! The app compares a [`Snapshot`] of these with the one last written, once a
//! frame, and writes when they have held still for a moment (a gain slider
//! dragged across a hundred values writes once) and again on exit.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::prefs;
use crate::source::{AirGains, Gains, Preset};

/// How long a change has to hold still before it is written.
const SETTLE: Duration = Duration::from_millis(800);
/// The window is never remembered smaller than this, or larger than a screen.
const MIN_WINDOW: (u32, u32) = (900, 560);
const MAX_WINDOW: u32 = 8000;

/// Everything kept, in the form it is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The radio's channel and frequency (not changed by moving the VFO in a
    /// capture).
    pub channel: u8,
    pub tune_hz: i64,
    pub file_source: bool,
    /// The radio is the Airspy R2 (else the HackRF, when not a file).
    pub airspy: bool,
    pub rate_khz: i64,
    pub gains: Gains,
    /// The Airspy's stages, the libairspy preset they were set from (`None`:
    /// set by hand) and whether it uses the packed transfers.
    pub air: AirGains,
    pub air_preset: Option<Preset>,
    pub air_packed: bool,
    /// The capture last opened, its centre and rate.
    pub file: String,
    pub file_centre_hz: i64,
    pub file_rate_khz: i64,
    pub decode: bool,
    pub bottom: String,
    pub fps: u32,
    pub doppler_hz: u32,
    pub window: (u32, u32),
    /// The channel's width ("8mhz", "1.7mhz").
    pub bandwidth: String,
}

impl Snapshot {
    fn write(&self) {
        let b = |v: bool| if v { "1" } else { "0" };
        prefs::set_many(&[
            ("channel", &self.channel.to_string()),
            ("tune_hz", &self.tune_hz.to_string()),
            (
                "source",
                if self.file_source {
                    "file"
                } else if self.airspy {
                    "airspy"
                } else {
                    "hackrf"
                },
            ),
            ("rate_khz", &self.rate_khz.to_string()),
            ("lna", &self.gains.lna.to_string()),
            ("vga", &self.gains.vga.to_string()),
            ("amp", b(self.gains.amp)),
            ("air_lna", &self.air.lna.to_string()),
            ("air_mixer", &self.air.mixer.to_string()),
            ("air_vga", &self.air.vga.to_string()),
            (
                "air_preset",
                &self.air_preset.map_or(String::new(), Preset::key),
            ),
            ("air_packed", b(self.air_packed)),
            ("file", &self.file),
            ("file_centre_hz", &self.file_centre_hz.to_string()),
            ("file_rate_khz", &self.file_rate_khz.to_string()),
            ("view_decode", b(self.decode)),
            ("view_bottom", &self.bottom),
            ("view_fps", &self.fps.to_string()),
            ("view_doppler", &self.doppler_hz.to_string()),
            ("window", &format!("{}x{}", self.window.0, self.window.1)),
            ("bandwidth", &self.bandwidth),
        ]);
    }
}

/// What was kept, as far as it is there: any key may be missing, or garbled.
#[derive(Debug, Default)]
pub struct Saved {
    pub channel: Option<u8>,
    pub tune_hz: Option<f64>,
    pub file_source: bool,
    pub airspy: bool,
    pub rate_msps: Option<f64>,
    pub gains: Option<Gains>,
    pub air: Option<AirGains>,
    pub air_preset: Option<Preset>,
    pub air_packed: Option<bool>,
    pub file: Option<PathBuf>,
    pub file_centre_mhz: Option<f64>,
    pub file_rate_msps: Option<f64>,
    pub decode: Option<bool>,
    pub bottom: Option<String>,
    pub fps: Option<f64>,
    pub doppler_hz: Option<f64>,
    pub window: Option<(u32, u32)>,
    pub bandwidth: Option<decdvbt_core::dvbt::Bandwidth>,
}

impl Saved {
    pub fn load() -> Saved {
        let flag = |k: &str| prefs::get(k).map(|v| v == "1");
        let gains = match (
            prefs::number::<u16>("lna"),
            prefs::number::<u16>("vga"),
            flag("amp"),
        ) {
            // The converter's steps: what the HackRF would take.
            (Some(lna), Some(vga), Some(amp)) if lna <= 40 && vga <= 62 => Some(Gains {
                amp,
                lna: lna / 8 * 8,
                vga: vga / 2 * 2,
            }),
            _ => None,
        };
        // The Airspy's stages, as far as they are in range.
        let air = match (
            prefs::number::<u8>("air_lna"),
            prefs::number::<u8>("air_mixer"),
            prefs::number::<u8>("air_vga"),
        ) {
            (Some(lna), Some(mixer), Some(vga)) if lna <= 14 && mixer <= 15 && vga <= 15 => {
                Some(AirGains { lna, mixer, vga })
            }
            _ => None,
        };
        let window = prefs::get("window").and_then(|v| {
            let (w, h) = v.split_once('x')?;
            let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
            Some((
                w.clamp(MIN_WINDOW.0, MAX_WINDOW),
                h.clamp(MIN_WINDOW.1, MAX_WINDOW),
            ))
        });
        Saved {
            channel: prefs::number("channel").filter(|c| (21..=69).contains(c)),
            tune_hz: prefs::number("tune_hz").filter(|h: &f64| *h >= 1e6),
            file_source: prefs::get("source").is_some_and(|v| v == "file"),
            airspy: prefs::get("source").is_some_and(|v| v == "airspy"),
            rate_msps: prefs::number::<i64>("rate_khz").map(|k| k as f64 / 1e3),
            gains,
            air,
            air_preset: prefs::get("air_preset").and_then(|v| Preset::parse(&v)),
            air_packed: flag("air_packed"),
            file: prefs::get("file")
                .filter(|f| !f.is_empty())
                .map(PathBuf::from),
            file_centre_mhz: prefs::number::<i64>("file_centre_hz").map(|h| h as f64 / 1e6),
            file_rate_msps: prefs::number::<i64>("file_rate_khz").map(|k| k as f64 / 1e3),
            decode: flag("view_decode"),
            bottom: prefs::get("view_bottom"),
            fps: prefs::number("view_fps"),
            doppler_hz: prefs::number("view_doppler"),
            window,
            bandwidth: prefs::get("bandwidth")
                .and_then(|b| decdvbt_core::dvbt::Bandwidth::parse(&b)),
        }
    }
}

/// Keeps the last snapshot written and writes a new one when it has settled.
pub struct Keeper {
    /// What the prefs hold, once this run has written (before, they hold
    /// whatever the last run left, which this run may differ from at once: a
    /// channel given on the command line is the last channel used).
    written: Option<Snapshot>,
    pending: Option<(Snapshot, Instant)>,
}

impl Keeper {
    pub fn new() -> Keeper {
        Keeper {
            written: None,
            pending: None,
        }
    }

    /// One look at the app's present state: written once it has been the same
    /// for [`SETTLE`]. True while a change waits to be written (the app is to
    /// look again after [`SETTLE`], whether or not anything else wakes it).
    pub fn look(&mut self, now: Snapshot, at: Instant) -> Option<Duration> {
        if self.written.as_ref() == Some(&now) {
            self.pending = None;
            return None;
        }
        match &self.pending {
            Some((p, since)) if *p == now => {
                if at.saturating_duration_since(*since) >= SETTLE {
                    now.write();
                    self.written = Some(now);
                    self.pending = None;
                    return None;
                }
            }
            _ => self.pending = Some((now, at)),
        }
        Some(SETTLE)
    }

    /// The app is closing: whatever has changed is written now.
    pub fn flush(&mut self, now: Snapshot) {
        if self.written.as_ref() != Some(&now) {
            now.write();
            self.written = Some(now);
            self.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(lna: u16) -> Snapshot {
        Snapshot {
            channel: 26,
            tune_hz: 514_000_000,
            file_source: false,
            airspy: false,
            rate_khz: 10_000,
            gains: Gains {
                amp: true,
                lna,
                vga: 8,
            },
            air: AirGains::default(),
            air_preset: Some(Preset::DEFAULT),
            air_packed: false,
            file: String::new(),
            file_centre_hz: 0,
            file_rate_khz: 10_000,
            decode: true,
            bottom: "scopes".into(),
            fps: 15,
            doppler_hz: 15,
            window: (1400, 860),
            bandwidth: "8mhz".into(),
        }
    }

    #[test]
    fn a_change_is_written_only_once_it_has_held_still() {
        let t0 = Instant::now();
        let mut k = Keeper::new();
        k.written = Some(snap(32));
        // Dragging: every look differs; nothing settles.
        for (i, lna) in [8u16, 16, 24, 32, 40].iter().enumerate() {
            k.look(snap(*lna), t0 + Duration::from_millis(100 * i as u64));
            assert_eq!(k.written, Some(snap(32)));
        }
        // Held at 40: not yet at 700 ms after it began (writing goes to the
        // prefs file, which a test leaves alone).
        k.look(snap(40), t0 + Duration::from_millis(400 + 700));
        assert_eq!(k.written, Some(snap(32)));
        assert!(k.pending.is_some());
    }

    #[test]
    fn going_back_to_what_was_written_cancels_the_change() {
        let t0 = Instant::now();
        let mut k = Keeper::new();
        k.written = Some(snap(32));
        k.look(snap(40), t0);
        assert!(k.pending.is_some());
        k.look(snap(32), t0 + Duration::from_millis(100));
        assert!(k.pending.is_none());
    }
}
