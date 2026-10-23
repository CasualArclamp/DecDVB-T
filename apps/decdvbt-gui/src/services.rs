// made by CasualArclamp
//! The services panel: the multiplex's services as the NIT numbers them
//! (UK Freeview's channel numbers), with what each is, its bitrate and what is
//! on now and next; and where the transport stream goes besides the player.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use decdvbt_engine::{Outputs, TsView};
use decdvbt_ts::{Programme, TsReport, TsTap, service_type_name};
use eframe::egui::{self, Color32, RichText, Ui};

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const GREEN: Color32 = Color32::from_rgb(110, 220, 130);

/// What the panel asks of the app.
pub enum Action {
    /// Play this service in the app.
    Play(u16),
    /// Open this service in VLC (through the HTTP output).
    Vlc(u16),
    OpenTs,
    OpenEpg,
}

/// What a service is, by what it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tv,
    Hd,
    Radio,
    Data,
}

impl Kind {
    /// What the SDT says the service is (EN 300 468 table 87) wins; the
    /// streams in its PMT decide only when it says nothing. A television
    /// service with no picture or sound in its PMT at the moment (BBC THREE
    /// and BBC FOUR share their capacity with the children's channels and are
    /// off air by day) is still a television service.
    pub fn of(p: &Programme) -> Kind {
        match p.service_type {
            Some(0x11 | 0x19..=0x1F) => return Kind::Hd,
            Some(0x01 | 0x16..=0x18) => return Kind::Tv,
            Some(0x02 | 0x0A) => return Kind::Radio,
            Some(0x03 | 0x0C..=0x10) => return Kind::Data,
            _ => {}
        }
        let video = p.streams.iter().any(|e| e.is_video());
        let audio = p.streams.iter().any(|e| e.is_audio());
        match (video, audio) {
            (true, _) => Kind::Tv,
            (false, true) => Kind::Radio,
            _ => Kind::Data,
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Kind::Tv => "TV",
            Kind::Hd => "HD",
            Kind::Radio => "radio",
            Kind::Data => "data",
        }
    }
}

/// Whether the service has a picture or sound to play now: its PMT lists a
/// video or an audio stream.
pub fn on_air(p: &Programme) -> bool {
    p.streams.iter().any(|e| e.is_video() || e.is_audio())
}

/// A service's bitrate, bit/s, from its streams' PIDs.
pub fn service_rate(p: &Programme, rates: &HashMap<u16, f64>) -> f64 {
    p.streams
        .iter()
        .map(|e| e.pid)
        .chain(p.pcr_pid)
        .collect::<std::collections::BTreeSet<_>>()
        .iter()
        .filter_map(|pid| rates.get(pid))
        .sum()
}

/// The services in the order a viewer expects: by channel number, then the
/// ones without a number by service id. Radio and data services come after
/// the television ones when asked.
pub fn ordered(r: &TsReport) -> Vec<&Programme> {
    let mut v: Vec<&Programme> = r
        .programmes
        .iter()
        // A programme the PAT lists but the SDT has not named, and that has
        // no streams yet, is not worth a row.
        .filter(|p| p.name.is_some() || !p.streams.is_empty())
        .collect();
    v.sort_by_key(|p| (p.lcn.unwrap_or(u16::MAX), p.number));
    v
}

/// The services list; `selected` is the service id picked.
pub fn list(ui: &mut Ui, view: &TsView, selected: &mut Option<u16>) -> Option<Action> {
    let r = &*view.report;
    let rates: HashMap<u16, f64> = r.pids.iter().map(|p| (p.pid, p.stats.rate_bps)).collect();
    let services = ordered(r);
    let mut action = None;
    if services.is_empty() {
        ui.label(
            RichText::new(if view.packets == 0 {
                "No transport stream yet. Tune a multiplex the receiver can lock to."
            } else {
                "Reading the tables (the PAT and SDT come within a second or two)…"
            })
            .weak(),
        );
        return None;
    }
    egui::ScrollArea::vertical()
        .id_salt("services_scroll")
        .max_height((ui.available_height() - 112.0).max(120.0))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for p in services {
                let kind = Kind::of(p);
                let scrambled = p.free_ca || !p.ca_systems.is_empty();
                let name = p
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("service {}", p.number));
                let name: String = name.chars().take(17).collect();
                let lcn = p.lcn.map_or("   ".into(), |n| format!("{n:>3}"));
                let on_air = on_air(p);
                let kbps = service_rate(p, &rates) / 1e3;
                // Off air: what is flowing is its data carousel, not a picture.
                let rate = if !on_air {
                    "off air".to_string()
                } else if kbps > 0.5 {
                    format!("{kbps:.0}k")
                } else {
                    String::new()
                };
                let line = format!(
                    "{lcn} {name:<17} {:<5} {rate:>7}{}",
                    kind.tag(),
                    if scrambled { " 🔒" } else { "" },
                );
                let is_sel = *selected == Some(p.number);
                let resp = ui.selectable_label(is_sel, line).on_hover_text(describe(p));
                // Click a service and it plays, as on a television (one with
                // nothing on air says so instead).
                if resp.clicked() {
                    *selected = Some(p.number);
                    action = Some(Action::Play(p.number));
                }
                resp.context_menu(|ui| {
                    if ui.button("▶ Play").clicked() {
                        action = Some(Action::Play(p.number));
                        ui.close();
                    }
                    if ui.button("Open in VLC").clicked() {
                        action = Some(Action::Vlc(p.number));
                        ui.close();
                    }
                });
                if let Some(now) = &p.now
                    && !now.name.is_empty()
                {
                    ui.label(RichText::new(format!("      {}", now.name)).small().weak());
                }
            }
        });
    action
}

/// The buttons under the list.
pub fn buttons(ui: &mut Ui, selected: Option<u16>) -> Option<Action> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(selected.is_some(), |ui| {
            if ui.button("▶ Play").clicked() {
                action = selected.map(Action::Play);
            }
            if ui
                .button("VLC")
                .on_hover_text("Open the service in VLC through the HTTP output")
                .clicked()
            {
                action = selected.map(Action::Vlc);
            }
        });
        if ui.button("TS analyser").clicked() {
            action = Some(Action::OpenTs);
        }
        if ui.button("Guide").clicked() {
            action = Some(Action::OpenEpg);
        }
    });
    action
}

/// Where the stream goes besides the player: a file, UDP, an HTTP server.
pub struct OutputsUi {
    pub record: bool,
    pub dir: String,
    pub udp_on: bool,
    pub udp: String,
    pub http_on: bool,
    pub http: String,
    pub error: Option<String>,
}

impl Default for OutputsUi {
    fn default() -> Self {
        OutputsUi {
            record: false,
            dir: "recordings".into(),
            udp_on: false,
            udp: "127.0.0.1:1234".into(),
            http_on: false,
            http: "127.0.0.1:8001".into(),
            error: None,
        }
    }
}

impl OutputsUi {
    /// Draw the controls; true if anything changed.
    pub fn show(&mut self, ui: &mut Ui, view: &TsView) -> bool {
        let mut changed = false;
        egui::Grid::new("outputs")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                changed |= ui.checkbox(&mut self.record, "record .ts").changed();
                if let Some((p, n)) = &view.file {
                    ui.label(
                        RichText::new(format!(
                            "{} · {:.1} MB",
                            p.file_name()
                                .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
                            *n as f64 * 188.0 / 1e6
                        ))
                        .small()
                        .color(if view.file_active {
                            GREEN
                        } else {
                            Color32::GRAY
                        }),
                    );
                } else {
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut self.dir).desired_width(150.0))
                        .changed();
                }
                ui.end_row();
                changed |= ui.checkbox(&mut self.udp_on, "UDP").changed();
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut self.udp).desired_width(150.0))
                        .changed();
                    if let Some((_, n)) = view.udp {
                        ui.label(RichText::new(format!("{n} datagrams")).small().weak());
                    }
                });
                ui.end_row();
                changed |= ui.checkbox(&mut self.http_on, "HTTP").changed();
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut self.http).desired_width(150.0))
                        .changed();
                    if let Some((a, clients)) = &view.tcp {
                        ui.label(
                            RichText::new(format!("http://{a}/ · {} players", clients.len()))
                                .small()
                                .weak(),
                        );
                    }
                });
                ui.end_row();
            });
        if let Some(e) = view.error.as_ref().or(self.error.as_ref()) {
            ui.label(RichText::new(e).small().color(AMBER));
        }
        changed
    }

    /// The outputs as the hub takes them, with the player's tap.
    pub fn outputs(&mut self, name: &str, tap: Option<TsTap>, http_wanted: bool) -> Outputs {
        self.error = None;
        let mut addr = |text: &str, what: &str| match text.trim().parse::<SocketAddr>() {
            Ok(a) => Some(a),
            Err(_) => {
                self.error = Some(format!("{what}: not an address (127.0.0.1:1234)"));
                None
            }
        };
        let udp = if self.udp_on {
            addr(&self.udp.clone(), "UDP")
        } else {
            None
        };
        // The HTTP server also runs while VLC is to be opened on it.
        let tcp = if self.http_on || http_wanted {
            addr(&self.http.clone(), "HTTP")
        } else {
            None
        };
        Outputs {
            record: self.record,
            dir: PathBuf::from(self.dir.trim()),
            name: name.to_string(),
            udp,
            tcp,
            tap,
        }
    }
}

/// The type of a service by its number, for a tooltip.
pub fn describe(p: &Programme) -> String {
    let ty = p.service_type.map_or("", service_type_name);
    let streams: Vec<String> = p
        .streams
        .iter()
        .map(|e| format!("{:#06x} {}", e.pid, e.describe()))
        .collect();
    format!("{ty}\n{}", streams.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_ts::{EsInfo, TsReport};

    fn service(number: u16, lcn: Option<u16>, streams: &[(u16, u8)], ty: Option<u8>) -> Programme {
        Programme {
            number,
            lcn,
            name: Some(format!("svc {number}")),
            service_type: ty,
            pmt_pid: 0x100 + number,
            streams: streams
                .iter()
                .map(|&(pid, t)| EsInfo::new(pid, t))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_service_is_what_its_streams_are() {
        let tv = service(1, None, &[(0x21, 0x02), (0x22, 0x03)], Some(0x01));
        let hd = service(2, None, &[(0x31, 0x1B), (0x32, 0x0F)], Some(0x19));
        // A television service with nothing but a data carousel in its PMT
        // (BBC FOUR by day) is television, and off air.
        let off = service(5, None, &[(0x61, 0x0B)], Some(0x01));
        assert_eq!(Kind::of(&off), Kind::Tv);
        assert!(!on_air(&off) && on_air(&hd));
        // The SDT says radio: radio, even before its sound is in the PMT.
        assert_eq!(Kind::of(&service(6, None, &[], Some(0x02))), Kind::Radio);
        let radio = service(3, None, &[(0x41, 0x03)], Some(0x02));
        let data = service(4, None, &[(0x51, 0x0B)], None);
        let kinds: Vec<Kind> = [&tv, &hd, &radio, &data]
            .iter()
            .map(|p| Kind::of(p))
            .collect();
        assert_eq!(kinds, [Kind::Tv, Kind::Hd, Kind::Radio, Kind::Data]);
    }

    #[test]
    fn the_list_is_in_channel_number_order_and_skips_the_unnamed() {
        let r = TsReport {
            programmes: vec![
                service(10, Some(30), &[], None),
                service(11, None, &[], None),
                service(12, Some(2), &[], None),
                Programme {
                    number: 13,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let order: Vec<u16> = ordered(&r).iter().map(|p| p.number).collect();
        // 12 (channel 2), 10 (30), 11 (none: last); 13 has no name or streams.
        assert_eq!(order, [12, 10, 11]);
    }

    #[test]
    fn a_services_rate_is_its_pids_rates_once_each() {
        let mut p = service(1, None, &[(0x21, 0x02), (0x22, 0x03)], None);
        p.pcr_pid = Some(0x21);
        let rates = HashMap::from([(0x21, 3e6), (0x22, 2e5), (0x99, 1e6)]);
        assert_eq!(service_rate(&p, &rates), 3.2e6);
    }
}
