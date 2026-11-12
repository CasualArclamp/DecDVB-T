// made by CasualArclamp
//! Network IQ as the GUI keeps it: where the stream comes from, its sample
//! type and the rate its sender says it sends (`decdvbt_io::net`, from DecLAB:
//! SDR++'s IQ Exporter, GNU Radio's TCP and UDP sinks, an rtl_tcp server).
//!
//! The stream says neither its rate nor its frequency, so both are the
//! user's to give; the sender is tuned at its end (only an rtl_tcp server
//! follows DecDVB-T's tuning).

use decdvbt_io::{IqSource, NetIqControl, NetIqSettings, NetIqSource, NetSample};
use eframe::egui::{self, Color32, RichText};

#[derive(Debug, Clone, PartialEq)]
pub struct NetUi {
    /// `tcp://HOST:PORT`, `listen://ADDR:PORT` or `udp://ADDR:PORT`.
    pub url: String,
    pub sample: NetSample,
    pub rate_hz: f64,
}

impl Default for NetUi {
    fn default() -> Self {
        NetUi {
            url: "tcp://127.0.0.1:7777".into(),
            sample: NetSample::Cs8,
            rate_hz: 10e6,
        }
    }
}

impl NetUi {
    /// From the preferences: `get` looks a key up.
    pub fn load(get: impl Fn(&str) -> Option<String>) -> NetUi {
        let d = NetUi::default();
        NetUi {
            url: get("net_url")
                .filter(|u| !u.trim().is_empty())
                .unwrap_or(d.url),
            sample: get("net_iq")
                .and_then(|t| NetSample::parse(&t))
                .unwrap_or(d.sample),
            rate_hz: get("net_rate")
                .and_then(|r| r.parse().ok())
                .filter(|r: &f64| *r >= 1e3)
                .unwrap_or(d.rate_hz),
        }
    }

    pub fn save(&self) -> Vec<(&'static str, String)> {
        vec![
            ("net_url", self.url.clone()),
            ("net_iq", self.sample.label().into()),
            ("net_rate", format!("{:.0}", self.rate_hz)),
        ]
    }

    /// Open the stream, labelled as centred on `center_hz`.
    pub fn open(&self, center_hz: f64) -> Result<(Box<dyn IqSource>, NetIqControl), String> {
        let (mode, addr) = NetIqSettings::parse_url(self.url.trim()).map_err(|e| e.to_string())?;
        let src = NetIqSource::open(NetIqSettings {
            mode,
            addr,
            sample: self.sample,
            sample_rate: self.rate_hz,
            center_hz,
        })
        .map_err(|e| e.to_string())?;
        let control = src.control();
        Ok((Box::new(src), control))
    }

    /// The settings as one run of controls for a wrapping row. True when
    /// one changed that the stream must be opened again for.
    pub fn ui(&mut self, ui: &mut egui::Ui, live: Option<&NetIqControl>) -> bool {
        let mut reopen = false;
        let r = ui
            .add(egui::TextEdit::singleline(&mut self.url).desired_width(170.0))
            .on_hover_text(
                "tcp://HOST:PORT connects to a sender that listens (SDR++ IQ Exporter: \
                 TCP (Server)); listen://0.0.0.0:PORT waits for one to connect (TCP \
                 (Client)); udp://0.0.0.0:PORT takes UDP. An rtl_tcp server is recognised \
                 and tuned; any other sender is tuned at its end.",
            );
        reopen |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let before = self.sample;
        egui::ComboBox::from_id_salt("net_iq")
            .selected_text(self.sample.label())
            .show_ui(ui, |ui| {
                for t in NetSample::ALL {
                    ui.selectable_value(&mut self.sample, t, t.label());
                }
            });
        reopen |= self.sample != before;
        ui.label(RichText::new("rate").color(Color32::GRAY));
        let mut msps = self.rate_hz / 1e6;
        let r = ui.add(
            egui::DragValue::new(&mut msps)
                .range(0.001..=100.0)
                .speed(0.01)
                .max_decimals(6)
                .suffix(" MS/s"),
        );
        if r.changed() {
            self.rate_hz = msps * 1e6;
        }
        reopen |= r.drag_stopped() || (r.lost_focus() && r.changed());
        if let Some(c) = live {
            let mut t = c.state();
            if let Some(m) = c.measured_rate() {
                t.push_str(&format!(" · {:.3} MS/s arriving", m / 1e6));
            }
            ui.label(RichText::new(t).small().weak());
        }
        reopen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_the_preferences() {
        let n = NetUi {
            url: "udp://0.0.0.0:5000".into(),
            sample: NetSample::Cs16,
            rate_hz: 9_142_857.0,
        };
        let saved = n.save();
        let back = NetUi::load(|k| {
            saved
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        });
        assert_eq!(back, n);
        assert_eq!(NetUi::load(|_| None), NetUi::default());
    }
}
