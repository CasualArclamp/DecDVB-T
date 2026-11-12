// made by CasualArclamp
//! The radios brought over from DecSAT: the RigExpert Fobos SDR, the SDRplay
//! RSPs, a USRP through UHD, and a radio on another machine behind a DecSAT
//! server. Their settings, how each is opened and the controls drawn for it.
//!
//! All receive only. The first three load their maker's library at run time
//! (`fobos_sdr.dll`/`fobos.dll`, `sdrplay_api.dll`, `uhd.dll`), so DecDVB-T
//! builds and starts without them; the server link is plain TCP.
//!
//! The settings are plain data, whatever the build has: only [`MoreRadios::open`]
//! and the live controls touch the drivers, behind their features.

#[allow(unused_imports)] // the server link's counter
use std::sync::Arc;
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};

use decdvbt_io::IqSource;
use eframe::egui::{self, Color32, RichText};

/// One of the radios here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Extra {
    Fobos,
    Sdrplay,
    Uhd,
    Remote,
}

impl Extra {
    pub const ALL: [Extra; 4] = [Extra::Fobos, Extra::Sdrplay, Extra::Uhd, Extra::Remote];

    pub fn label(self) -> &'static str {
        match self {
            Extra::Fobos => "Fobos SDR",
            Extra::Sdrplay => "SDRplay RSP",
            Extra::Uhd => "USRP (UHD)",
            Extra::Remote => "DecSAT server",
        }
    }

    /// The name the preferences and `--radio` use.
    pub fn key(self) -> &'static str {
        match self {
            Extra::Fobos => "fobos",
            Extra::Sdrplay => "sdrplay",
            Extra::Uhd => "uhd",
            Extra::Remote => "remote",
        }
    }

    pub fn from_key(s: &str) -> Option<Extra> {
        match s {
            "fobos" => Some(Extra::Fobos),
            "sdrplay" | "rsp" | "rspduo" => Some(Extra::Sdrplay),
            "uhd" | "usrp" => Some(Extra::Uhd),
            "remote" | "decsat" => Some(Extra::Remote),
            _ => None,
        }
    }

    /// The lowest and highest frequency it tunes to, Hz. A USRP's depends on
    /// its daughterboard and a server's on its radio: each refuses what it
    /// cannot do.
    #[allow(dead_code)] // not every app clamps its tuning
    pub fn range_hz(self) -> (f64, f64) {
        match self {
            Extra::Fobos => (50e6, 6e9),
            Extra::Sdrplay => (1e3, 2e9),
            Extra::Uhd | Extra::Remote => (1e3, 6e9),
        }
    }
}

/// Sample rates every Fobos offers, S/s (`decdvbt_io::FOBOS_RATES`).
const FOBOS_RATES: [f64; 9] = [8e6, 10e6, 12.5e6, 16e6, 20e6, 25e6, 32e6, 40e6, 50e6];
/// The SDRplay's (`decdvbt_io::SDRPLAY_RATES`).
const SDRPLAY_RATES: [f64; 5] = [2e6, 4e6, 6e6, 8e6, 10e6];
/// The rates the SDRplay, a USRP and a server's radio start at.
const SDRPLAY_DEFAULT_RATE: f64 = 10e6;
const UHD_DEFAULT_RATE: f64 = 10e6;
const REMOTE_DEFAULT_RATE: f64 = 10e6;
/// Sample widths a server packs its IQ to.
const REMOTE_BITS: [u8; 6] = [4, 6, 8, 10, 12, 16];

/// What the settings row changed.
#[derive(Default, Clone, Copy)]
pub struct Changed {
    /// Something the radio must be opened again for (a rate, an address).
    pub reopen: bool,
    /// Something worth saving.
    pub save: bool,
}

/// The settings of all four, as the GUI keeps and saves them.
#[derive(Clone, Debug, PartialEq)]
pub struct MoreRadios {
    pub fobos_rate: f64,
    /// RF LNA step 0-3 and IF VGA step 0-31 (2 dB each).
    pub fobos_lna: u8,
    pub fobos_vga: u8,
    /// Lock to 10 MHz on CLK IN.
    pub fobos_ext_clock: bool,

    pub sdrplay_rate: f64,
    /// LNA state (0 = most gain) and IF gain reduction 20-59 dB.
    pub sdrplay_lna: u8,
    pub sdrplay_gr: u8,
    /// An RSPduo's Tuner B.
    pub sdrplay_tuner_b: bool,

    /// UHD's device selector (`serial=…`, `type=b200`, `addr=…`); empty for
    /// the first USRP found.
    pub uhd_args: String,
    pub uhd_rate: f64,
    pub uhd_gain: f64,
    pub uhd_antenna: String,

    /// `HOST` or `HOST:PORT` of a `decsat-server`.
    pub remote_addr: String,
    /// Never saved.
    pub remote_password: String,
    /// The server's name for the radio (`hackrf`, `pluto`, …); empty for its
    /// first.
    pub remote_radio: String,
    pub remote_rate: f64,
    pub remote_bits: u8,
    pub remote_zstd: bool,
    /// Gain stages set on the server's radio, by the server's names; sent
    /// again at the next start. Forgotten when the radio's name changes.
    pub remote_gains: Vec<(String, f64)>,
}

impl Default for MoreRadios {
    fn default() -> Self {
        MoreRadios {
            fobos_rate: 8e6,
            fobos_lna: 1,
            fobos_vga: 8,
            fobos_ext_clock: false,
            sdrplay_rate: SDRPLAY_DEFAULT_RATE,
            sdrplay_lna: 4,
            sdrplay_gr: 40,
            sdrplay_tuner_b: false,
            uhd_args: String::new(),
            uhd_rate: UHD_DEFAULT_RATE,
            uhd_gain: 35.0,
            uhd_antenna: "RX2".into(),
            remote_addr: "127.0.0.1".into(),
            remote_password: String::new(),
            remote_radio: String::new(),
            remote_rate: REMOTE_DEFAULT_RATE,
            remote_bits: 8,
            remote_zstd: false,
            remote_gains: Vec::new(),
        }
    }
}

/// The live controls of whichever of them is open.
pub enum MoreControl {
    /// A build with none of them still has a type to name.
    #[allow(dead_code)]
    None,
    #[cfg(feature = "fobos")]
    Fobos(decdvbt_io::FobosControl),
    #[cfg(feature = "sdrplay")]
    Sdrplay(decdvbt_io::SdrplayControl),
    #[cfg(all(windows, feature = "uhd"))]
    Uhd(decdvbt_io::UhdControl),
    /// With the count of samples lost on the way.
    #[cfg(feature = "remote")]
    Remote(decdvbt_io::remote::RemoteControl, Arc<AtomicU64>),
}

impl MoreControl {
    /// Buffers (a server: samples) lost because this PC or the link fell
    /// behind.
    #[allow(dead_code)] // not every app shows it
    pub fn dropped(&self) -> u64 {
        match self {
            MoreControl::None => 0,
            #[cfg(feature = "fobos")]
            MoreControl::Fobos(c) => c.overflows(),
            #[cfg(feature = "sdrplay")]
            MoreControl::Sdrplay(c) => c.overflows(),
            #[cfg(all(windows, feature = "uhd"))]
            MoreControl::Uhd(_) => 0,
            #[cfg(feature = "remote")]
            MoreControl::Remote(_, lost) => lost.load(Ordering::Relaxed),
        }
    }

    /// A counter another thread can read (the engine's dropped-buffer probe).
    #[allow(dead_code)]
    pub fn drop_probe(&self) -> Box<dyn Fn() -> u64 + Send> {
        match self {
            MoreControl::None => Box::new(|| 0),
            #[cfg(feature = "fobos")]
            MoreControl::Fobos(c) => {
                let c = c.clone();
                Box::new(move || c.overflows())
            }
            #[cfg(feature = "sdrplay")]
            MoreControl::Sdrplay(c) => {
                let c = c.clone();
                Box::new(move || c.overflows())
            }
            #[cfg(all(windows, feature = "uhd"))]
            MoreControl::Uhd(_) => Box::new(|| 0),
            #[cfg(feature = "remote")]
            MoreControl::Remote(_, lost) => {
                let lost = Arc::clone(lost);
                Box::new(move || lost.load(Ordering::Relaxed))
            }
        }
    }

    /// What the radio said about itself, for a status line.
    pub fn info(&self) -> String {
        match self {
            MoreControl::None => String::new(),
            #[cfg(feature = "fobos")]
            MoreControl::Fobos(c) => {
                let i = c.info();
                format!(
                    "Fobos {} · hw {} · fw {} · {} firmware",
                    i.serial,
                    i.hw_revision,
                    i.fw_version,
                    if i.agile { "agile" } else { "stock" }
                )
            }
            #[cfg(feature = "sdrplay")]
            MoreControl::Sdrplay(c) => {
                let i = c.info();
                format!(
                    "{} {} · tuner {} · API {:.2} · ADC overloads {}",
                    i.model,
                    i.serial,
                    i.tuner,
                    i.api_version,
                    c.overloads()
                )
            }
            #[cfg(all(windows, feature = "uhd"))]
            MoreControl::Uhd(_) => "USRP".into(),
            #[cfg(feature = "remote")]
            MoreControl::Remote(c, lost) => {
                let s = c.stats();
                let mut t = format!("link {:.1} Mbit/s", s.bytes_per_sec * 8.0 / 1e6);
                let gone = lost.load(Ordering::Relaxed) + s.iq_dropped;
                if gone > 0 {
                    t.push_str(&format!(" · {gone} samples lost"));
                }
                if let Some(why) = c.ended() {
                    t.push_str(&format!(" · ended: {why}"));
                }
                t
            }
        }
    }
}

fn flag(on: bool) -> String {
    if on { "1" } else { "0" }.into()
}

fn grey(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(Color32::GRAY));
}

fn msps(hz: f64) -> String {
    format!("{} MS/s", hz / 1e6)
}

impl MoreRadios {
    /// From the preferences: `get` looks a key up.
    pub fn load(get: impl Fn(&str) -> Option<String>) -> MoreRadios {
        let d = MoreRadios::default();
        // Rust note: a generic closure cannot be written inline, so a small
        // generic `fn` parses any type that implements `FromStr`.
        fn num<T: std::str::FromStr>(v: Option<String>, or: T) -> T {
            v.and_then(|s| s.parse().ok()).unwrap_or(or)
        }
        let on = |k: &str, or: bool| get(k).map_or(or, |v| v == "1");
        MoreRadios {
            fobos_rate: num(get("fobos_rate"), d.fobos_rate),
            fobos_lna: num(get("fobos_lna"), d.fobos_lna).min(3),
            fobos_vga: num(get("fobos_vga"), d.fobos_vga).min(31),
            fobos_ext_clock: on("fobos_ext_clock", false),
            sdrplay_rate: num(get("sdrplay_rate"), d.sdrplay_rate),
            sdrplay_lna: num(get("sdrplay_lna"), d.sdrplay_lna).min(27),
            sdrplay_gr: num(get("sdrplay_gr"), d.sdrplay_gr).clamp(20, 59),
            sdrplay_tuner_b: on("sdrplay_tuner_b", false),
            uhd_args: get("uhd_args").unwrap_or(d.uhd_args),
            uhd_rate: num(get("uhd_rate"), d.uhd_rate),
            uhd_gain: num(get("uhd_gain"), d.uhd_gain),
            uhd_antenna: get("uhd_antenna")
                .filter(|a| !a.is_empty())
                .unwrap_or(d.uhd_antenna),
            remote_addr: get("remote_addr")
                .filter(|a| !a.is_empty())
                .unwrap_or(d.remote_addr),
            remote_password: String::new(),
            remote_radio: get("remote_radio").unwrap_or(d.remote_radio),
            remote_rate: num(get("remote_rate"), d.remote_rate),
            remote_bits: Some(num(get("remote_bits"), d.remote_bits))
                .filter(|b| REMOTE_BITS.contains(b))
                .unwrap_or(d.remote_bits),
            remote_zstd: on("remote_zstd", false),
            remote_gains: get("remote_gains")
                .map(|g| parse_gains(&g))
                .unwrap_or_default(),
        }
    }

    /// What to keep: everything but the server's password.
    pub fn save(&self) -> Vec<(&'static str, String)> {
        vec![
            ("fobos_rate", format!("{:.0}", self.fobos_rate)),
            ("fobos_lna", self.fobos_lna.to_string()),
            ("fobos_vga", self.fobos_vga.to_string()),
            ("fobos_ext_clock", flag(self.fobos_ext_clock)),
            ("sdrplay_rate", format!("{:.0}", self.sdrplay_rate)),
            ("sdrplay_lna", self.sdrplay_lna.to_string()),
            ("sdrplay_gr", self.sdrplay_gr.to_string()),
            ("sdrplay_tuner_b", flag(self.sdrplay_tuner_b)),
            ("uhd_args", self.uhd_args.clone()),
            ("uhd_rate", format!("{:.0}", self.uhd_rate)),
            ("uhd_gain", format!("{:.1}", self.uhd_gain)),
            ("uhd_antenna", self.uhd_antenna.clone()),
            ("remote_addr", self.remote_addr.clone()),
            ("remote_radio", self.remote_radio.clone()),
            ("remote_rate", format!("{:.0}", self.remote_rate)),
            ("remote_bits", self.remote_bits.to_string()),
            ("remote_zstd", flag(self.remote_zstd)),
            ("remote_gains", format_gains(&self.remote_gains)),
        ]
    }

    /// The rate `kind` is set to run at, S/s.
    #[allow(dead_code)]
    pub fn rate(&self, kind: Extra) -> f64 {
        match kind {
            Extra::Fobos => self.fobos_rate,
            Extra::Sdrplay => self.sdrplay_rate,
            Extra::Uhd => self.uhd_rate,
            Extra::Remote => self.remote_rate,
        }
    }

    #[allow(dead_code)]
    pub fn set_rate(&mut self, kind: Extra, hz: f64) {
        match kind {
            Extra::Fobos => self.fobos_rate = hz,
            Extra::Sdrplay => self.sdrplay_rate = hz,
            Extra::Uhd => self.uhd_rate = hz,
            Extra::Remote => self.remote_rate = hz,
        }
    }

    /// Open `kind` at `center_hz`.
    pub fn open(
        &self,
        kind: Extra,
        center_hz: f64,
    ) -> Result<(Box<dyn IqSource>, MoreControl), String> {
        let _ = center_hz; // unused when none of them is built in
        match kind {
            #[cfg(feature = "fobos")]
            Extra::Fobos => {
                use decdvbt_io::{FobosGains, FobosSettings, FobosSource};
                let src = FobosSource::open(FobosSettings {
                    center_hz: center_hz.round() as u64,
                    sample_rate: self.fobos_rate.round() as u32,
                    gains: FobosGains {
                        lna: self.fobos_lna,
                        vga: self.fobos_vga,
                    }
                    .snapped(),
                    external_clock: self.fobos_ext_clock,
                })
                .map_err(|e| e.to_string())?;
                let c = src.control();
                Ok((Box::new(src), MoreControl::Fobos(c)))
            }
            #[cfg(feature = "sdrplay")]
            Extra::Sdrplay => {
                use decdvbt_io::{SdrplayGains, SdrplaySettings, SdrplaySource};
                let src = SdrplaySource::open(SdrplaySettings {
                    center_hz: center_hz.round() as u64,
                    sample_rate: self.sdrplay_rate.round() as u32,
                    gains: SdrplayGains {
                        lna: self.sdrplay_lna,
                        gr: self.sdrplay_gr,
                    }
                    .snapped(),
                    tuner_b: self.sdrplay_tuner_b,
                })
                .map_err(|e| e.to_string())?;
                let c = src.control();
                Ok((Box::new(src), MoreControl::Sdrplay(c)))
            }
            #[cfg(all(windows, feature = "uhd"))]
            Extra::Uhd => {
                use decdvbt_io::{UhdSettings, UhdSource};
                let src = UhdSource::open(UhdSettings {
                    device_args: self.uhd_args.trim().to_string(),
                    center_hz,
                    rate: self.uhd_rate,
                    gain: self.uhd_gain,
                    channel: 0,
                    antenna: self.uhd_antenna.trim().to_string(),
                    bandwidth: 0.0,
                })
                .map_err(|e| e.to_string())?;
                let c = src.control();
                Ok((Box::new(src), MoreControl::Uhd(c)))
            }
            #[cfg(feature = "remote")]
            Extra::Remote => {
                use decdvbt_io::remote::{ConnectOptions, IqParams, RadioRequest, RemoteIqSource};
                let o = ConnectOptions {
                    addr: self.remote_addr.trim().to_string(),
                    name: "DecDVB-T".into(),
                    password: Some(self.remote_password.clone()).filter(|p| !p.is_empty()),
                    takeover: false,
                };
                let radio = RadioRequest {
                    key: self.remote_radio.trim().to_string(),
                    center_hz,
                    sample_rate: self.remote_rate,
                    gains: self.remote_gains.clone(),
                };
                let p = IqParams {
                    bits: self.remote_bits,
                    zstd: if self.remote_zstd { 1 } else { 0 },
                    slice: None,
                };
                let src = RemoteIqSource::connect(&o, radio, p).map_err(|e| e.to_string())?;
                let c = MoreControl::Remote(src.control(), src.dropped_counter());
                Ok((Box::new(src), c))
            }
            #[allow(unreachable_patterns)]
            k => Err(format!(
                "{} support was not built in{}",
                k.label(),
                if k == Extra::Uhd && !cfg!(windows) {
                    " (UHD is loaded from uhd.dll, on Windows only)"
                } else {
                    ""
                }
            )),
        }
    }

    /// The settings of `kind`, as one run of controls for a wrapping row.
    /// Gains go straight to `live`, the open radio's controls.
    pub fn ui(&mut self, kind: Extra, ui: &mut egui::Ui, live: Option<&MoreControl>) -> Changed {
        match kind {
            Extra::Fobos => self.fobos_ui(ui, live),
            Extra::Sdrplay => self.sdrplay_ui(ui, live),
            Extra::Uhd => self.uhd_ui(ui, live),
            Extra::Remote => self.remote_ui(ui, live),
        }
    }

    fn fobos_ui(&mut self, ui: &mut egui::Ui, live: Option<&MoreControl>) -> Changed {
        let _ = live;
        let mut ch = Changed::default();
        grey(ui, "rate");
        let before = self.fobos_rate;
        egui::ComboBox::from_id_salt("fobos_rate")
            .selected_text(msps(self.fobos_rate))
            .show_ui(ui, |ui| {
                for r in FOBOS_RATES {
                    ui.selectable_value(&mut self.fobos_rate, r, msps(r));
                }
            })
            .response
            .on_hover_text(
                "8-50 MS/s; the board's nearest rate is taken. Needs RigExpert's fobos_sdr.dll \
                 (agile firmware) or fobos.dll (stock) beside DecDVB-T. Receive only, RF input \
                 (50-6000 MHz).",
            );
        ch.reopen |= self.fobos_rate != before;
        let (lna, vga) = (self.fobos_lna, self.fobos_vga);
        ui.menu_button(
            format!(
                "gain LNA {} · VGA {} dB",
                self.fobos_lna,
                u32::from(self.fobos_vga) * 2
            ),
            |ui| {
                ui.add(egui::Slider::new(&mut self.fobos_lna, 0..=3).text("LNA step"))
                    .on_hover_text("The RF preamplifier: 2 is about +16 dB, 3 about +33 dB.");
                let mut db = u32::from(self.fobos_vga) * 2;
                if ui
                    .add(
                        egui::Slider::new(&mut db, 0..=62)
                            .step_by(2.0)
                            .text("VGA dB"),
                    )
                    .changed()
                {
                    self.fobos_vga = (db / 2) as u8;
                }
            },
        );
        if (lna, vga) != (self.fobos_lna, self.fobos_vga) {
            ch.save = true;
            #[cfg(feature = "fobos")]
            if let Some(MoreControl::Fobos(c)) = live {
                let _ = c.set_gains(decdvbt_io::FobosGains {
                    lna: self.fobos_lna,
                    vga: self.fobos_vga,
                });
            }
        }
        if ui
            .checkbox(&mut self.fobos_ext_clock, "ext. 10 MHz")
            .on_hover_text("Lock to a 10 MHz reference on the CLK IN socket.")
            .changed()
        {
            ch.reopen = true;
        }
        ch
    }

    fn sdrplay_ui(&mut self, ui: &mut egui::Ui, live: Option<&MoreControl>) -> Changed {
        let _ = live;
        let mut ch = Changed::default();
        grey(ui, "rate");
        let before = self.sdrplay_rate;
        egui::ComboBox::from_id_salt("sdrplay_rate")
            .selected_text(msps(self.sdrplay_rate))
            .show_ui(ui, |ui| {
                for r in SDRPLAY_RATES {
                    ui.selectable_value(&mut self.sdrplay_rate, r, msps(r));
                }
            })
            .response
            .on_hover_text(
                "2-10 MS/s, zero-IF; the analogue bandwidth follows the rate (up to 8 MHz). \
                 Needs the SDRplay API installed (sdrplay.com), or sdrplay_api.dll beside \
                 DecDVB-T. Receive only; the bias tee stays off.",
            );
        ch.reopen |= self.sdrplay_rate != before;
        let (lna, gr) = (self.sdrplay_lna, self.sdrplay_gr);
        ui.menu_button(
            format!(
                "gain LNA state {} · IF −{} dB",
                self.sdrplay_lna, self.sdrplay_gr
            ),
            |ui| {
                ui.label(
                    RichText::new("Both are reductions: a lower number is more gain.")
                        .small()
                        .weak(),
                );
                ui.add(egui::Slider::new(&mut self.sdrplay_lna, 0..=9).text("LNA state"))
                    .on_hover_text(
                        "0 = most gain. An RSPduo has 0-9 (0-8 above 1 GHz); a state the \
                         radio lacks becomes its last.",
                    );
                ui.add(egui::Slider::new(&mut self.sdrplay_gr, 20..=59).text("IF reduction dB"))
                    .on_hover_text("20 = most IF gain. Raise it if the overload count climbs.");
            },
        );
        if (lna, gr) != (self.sdrplay_lna, self.sdrplay_gr) {
            ch.save = true;
            #[cfg(feature = "sdrplay")]
            if let Some(MoreControl::Sdrplay(c)) = live {
                let _ = c.set_gains(decdvbt_io::SdrplayGains {
                    lna: self.sdrplay_lna,
                    gr: self.sdrplay_gr,
                });
            }
        }
        grey(ui, "tuner");
        let before = self.sdrplay_tuner_b;
        ui.selectable_value(&mut self.sdrplay_tuner_b, false, "A");
        ui.selectable_value(&mut self.sdrplay_tuner_b, true, "B")
            .on_hover_text("RSPduo only: Tuner 2. Ignored by other RSPs.");
        ch.reopen |= self.sdrplay_tuner_b != before;
        ch
    }

    fn uhd_ui(&mut self, ui: &mut egui::Ui, live: Option<&MoreControl>) -> Changed {
        let _ = live;
        let mut ch = Changed::default();
        let enter = |r: &egui::Response, ui: &egui::Ui| {
            r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
        };
        let r = ui
            .add(
                egui::TextEdit::singleline(&mut self.uhd_args)
                    .hint_text("first USRP")
                    .desired_width(120.0),
            )
            .on_hover_text(
                "UHD's device selector: serial=…, type=b200 or addr=…; empty for the first \
                 USRP found. Needs UHD installed (uhd.dll in C:\\Program Files\\UHD\\bin, or \
                 its path in the UHD_DLL environment variable). Receive only.",
            );
        ch.reopen |= enter(&r, ui);
        grey(ui, "rate");
        let mut m = self.uhd_rate / 1e6;
        let r = ui.add(
            egui::DragValue::new(&mut m)
                .range(0.2..=50.0)
                .speed(0.01)
                .max_decimals(6)
                .suffix(" MS/s"),
        );
        if r.changed() {
            self.uhd_rate = m * 1e6;
        }
        ch.reopen |= r.drag_stopped() || (r.lost_focus() && r.changed());
        grey(ui, "gain");
        let r = ui.add(
            egui::DragValue::new(&mut self.uhd_gain)
                .range(0.0..=90.0)
                .speed(0.5)
                .max_decimals(1)
                .suffix(" dB"),
        );
        if r.changed() {
            ch.save = true;
            #[cfg(all(windows, feature = "uhd"))]
            if let Some(MoreControl::Uhd(c)) = live {
                c.gain(self.uhd_gain);
            }
        }
        grey(ui, "antenna");
        let r = ui
            .add(egui::TextEdit::singleline(&mut self.uhd_antenna).desired_width(44.0))
            .on_hover_text("The receive port's name: RX2 or TX/RX on a B200/B210.");
        ch.reopen |= enter(&r, ui);
        ch
    }

    fn remote_ui(&mut self, ui: &mut egui::Ui, live: Option<&MoreControl>) -> Changed {
        let _ = live;
        let mut ch = Changed::default();
        let enter = |r: &egui::Response, ui: &egui::Ui| {
            r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
        };
        let r = ui
            .add(egui::TextEdit::singleline(&mut self.remote_addr).desired_width(130.0))
            .on_hover_text(
                "HOST or HOST:PORT of a decsat-server (DecSAT's remote server; port 5262). \
                 It opens one of its radios and sends the IQ, which is decoded here. The \
                 link is not encrypted: over the internet, use a VPN or an SSH tunnel.",
            );
        ch.reopen |= enter(&r, ui);
        let r = ui
            .add(
                egui::TextEdit::singleline(&mut self.remote_password)
                    .password(true)
                    .hint_text("password")
                    .desired_width(70.0),
            )
            .on_hover_text("Only if the server asks for one. Never saved.");
        ch.reopen |= enter(&r, ui);
        grey(ui, "radio");
        let before = self.remote_radio.clone();
        let r = ui
            .add(
                egui::TextEdit::singleline(&mut self.remote_radio)
                    .hint_text("first")
                    .desired_width(56.0),
            )
            .on_hover_text(
                "The server's name for the radio: hackrf, airspy, fobos, sdrplay, pluto, \
                 file, net. Empty for the first it offers.",
            );
        if self.remote_radio != before {
            // Another radio's stages have other names.
            self.remote_gains.clear();
        }
        ch.reopen |= enter(&r, ui);
        grey(ui, "rate");
        let mut m = self.remote_rate / 1e6;
        let r = ui.add(
            egui::DragValue::new(&mut m)
                .range(0.2..=61.44)
                .speed(0.01)
                .max_decimals(6)
                .suffix(" MS/s"),
        );
        if r.changed() {
            self.remote_rate = m * 1e6;
        }
        ch.reopen |= r.drag_stopped() || (r.lost_focus() && r.changed());
        let before = (self.remote_bits, self.remote_zstd);
        egui::ComboBox::from_id_salt("remote_bits")
            .selected_text(format!("{} bit", self.remote_bits))
            .width(56.0)
            .show_ui(ui, |ui| {
                for b in REMOTE_BITS {
                    ui.selectable_value(&mut self.remote_bits, b, format!("{b} bit"));
                }
            })
            .response
            .on_hover_text(
                "Bits a sample component on the link: 8 suits most signals; fewer saves \
                 bandwidth at the cost of dynamic range.",
            );
        ui.checkbox(&mut self.remote_zstd, "zstd")
            .on_hover_text("Compress the IQ (helps when the band is mostly empty).");
        ch.reopen |= before != (self.remote_bits, self.remote_zstd);
        #[cfg(feature = "remote")]
        if let Some(MoreControl::Remote(c, _)) = live {
            let stages = c.gains();
            if !stages.is_empty() {
                ui.menu_button("gain", |ui| {
                    for g in stages {
                        let mut v = g.value;
                        let slider = egui::Slider::new(&mut v, g.min..=g.max)
                            .step_by(g.step.max(0.0))
                            .text(g.name.as_str());
                        if ui.add(slider).changed() {
                            c.set_gain(&g.name, v);
                            self.remote_gains.retain(|(n, _)| *n != g.name);
                            self.remote_gains.push((g.name.clone(), v));
                            ch.save = true;
                        }
                    }
                });
            }
        }
        ch.save |= ch.reopen;
        ch
    }
}

/// `lna=16,vga=20` as the stages it names.
fn parse_gains(s: &str) -> Vec<(String, f64)> {
    s.split(',')
        .filter_map(|p| {
            let (name, v) = p.split_once('=')?;
            Some((name.trim().to_string(), v.trim().parse().ok()?))
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

fn format_gains(g: &[(String, f64)]) -> String {
    g.iter()
        .map(|(n, v)| format!("{n}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_the_preferences_but_the_password_does_not() {
        let m = MoreRadios {
            fobos_rate: 12.5e6,
            fobos_lna: 3,
            fobos_vga: 20,
            fobos_ext_clock: true,
            sdrplay_rate: 6e6,
            sdrplay_lna: 2,
            sdrplay_gr: 33,
            sdrplay_tuner_b: true,
            uhd_args: "serial=ABC,type=b200".into(),
            uhd_rate: 5.5e6,
            uhd_gain: 42.5,
            uhd_antenna: "TX/RX".into(),
            remote_addr: "shack.example:5262".into(),
            remote_password: "secret".into(),
            remote_radio: "hackrf".into(),
            remote_rate: 4e6,
            remote_bits: 12,
            remote_zstd: true,
            remote_gains: vec![("lna".into(), 24.0), ("amp".into(), 1.0)],
        };
        let saved = m.save();
        assert!(saved.iter().all(|(_, v)| !v.contains("secret")));
        let back = MoreRadios::load(|k| {
            saved
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        });
        assert_eq!(
            back,
            MoreRadios {
                remote_password: String::new(),
                ..m
            }
        );
    }

    #[test]
    fn nonsense_in_the_preferences_falls_back_to_the_defaults() {
        let m = MoreRadios::load(|k| {
            let v = match k {
                "fobos_lna" => "9",
                "sdrplay_gr" => "5",
                "remote_bits" => "7",
                "uhd_rate" => "fast",
                "remote_gains" => "lna=x,=3,vga=20",
                _ => return None,
            };
            Some(v.to_string())
        });
        assert_eq!(m.fobos_lna, 3);
        assert_eq!(m.sdrplay_gr, 20);
        assert_eq!(m.remote_bits, 8);
        assert_eq!(m.uhd_rate, MoreRadios::default().uhd_rate);
        assert_eq!(m.remote_gains, [("vga".to_string(), 20.0)]);
    }

    #[test]
    fn keys_name_each_radio_once() {
        for k in Extra::ALL {
            assert_eq!(Extra::from_key(k.key()), Some(k));
        }
        assert_eq!(Extra::from_key("usrp"), Some(Extra::Uhd));
        assert_eq!(Extra::from_key("hackrf"), None);
    }
}
