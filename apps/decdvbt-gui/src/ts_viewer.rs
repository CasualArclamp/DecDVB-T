// made by CasualArclamp
//! The transport stream analyser window: what the multiplex carries, as the
//! tables say (services and their streams, the network) and as the packets
//! show (PIDs with rates and continuity, the tables heard).

use decdvbt_engine::TsView;
use decdvbt_ts::{Programme, ca_system_name, service_type_name};
use eframe::egui::{self, Color32, RichText, Ui};

use crate::epg_view::local_stamp;
use crate::services;

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const RED: Color32 = Color32::from_rgb(255, 90, 80);

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Services,
    Pids,
    Tables,
    Network,
    Stream,
}

#[derive(Default)]
pub struct TsViewer {
    pub open: bool,
    tab: Tab,
}

impl TsViewer {
    pub fn new(open: bool) -> Self {
        TsViewer {
            open,
            tab: Tab::default(),
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, view: &TsView) {
        let mut open = self.open;
        egui::Window::new("Transport stream")
            .id(egui::Id::new("ts_viewer"))
            .open(&mut open)
            .default_size([760.0, 520.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (t, name) in [
                        (Tab::Services, "Services"),
                        (Tab::Pids, "PIDs"),
                        (Tab::Tables, "Tables"),
                        (Tab::Network, "Network"),
                        (Tab::Stream, "Stream"),
                    ] {
                        ui.selectable_value(&mut self.tab, t, name);
                    }
                });
                ui.separator();
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.tab {
                        Tab::Services => services_tab(ui, view),
                        Tab::Pids => pids_tab(ui, view),
                        Tab::Tables => tables_tab(ui, view),
                        Tab::Network => network_tab(ui, view),
                        Tab::Stream => stream_tab(ui, view),
                    });
            });
        self.open = open;
    }
}

fn services_tab(ui: &mut Ui, view: &TsView) {
    let r = &*view.report;
    if r.programmes.is_empty() {
        ui.label(RichText::new("no services yet").weak());
        return;
    }
    for p in services::ordered(r) {
        let title = format!(
            "{} {}  (service {}, PMT {:#06x}{})",
            p.lcn.map_or(String::new(), |n| format!("[{n}]")),
            p.name.as_deref().unwrap_or("?"),
            p.number,
            p.pmt_pid,
            p.pcr_pid
                .map_or(String::new(), |c| format!(", PCR {c:#06x}")),
        );
        egui::CollapsingHeader::new(RichText::new(title).strong())
            .id_salt(("svc", p.number))
            .default_open(false)
            .show(ui, |ui| service_details(ui, p));
    }
}

fn service_details(ui: &mut Ui, p: &Programme) {
    if let Some(t) = p.service_type {
        ui.label(format!("type: {}", service_type_name(t)));
    }
    if let Some(n) = &p.provider {
        ui.label(format!("provider: {n}"));
    }
    for e in &p.streams {
        ui.label(format!("  {:#06x}  {}", e.pid, e.describe()));
    }
    if !p.ca_systems.is_empty() {
        let names: Vec<String> = p
            .ca_systems
            .iter()
            .map(|&s| format!("{} ({s:#06x})", ca_system_name(s)))
            .collect();
        ui.label(RichText::new(format!("scrambled: {}", names.join(", "))).color(AMBER));
    } else if p.free_ca {
        ui.label(RichText::new("the SDT says it may be scrambled").color(AMBER));
    }
    for (label, e) in [("now", &p.now), ("next", &p.next)] {
        if let Some(e) = e {
            ui.label(format!(
                "{label}: {} ({} min{})",
                e.name,
                e.duration_min,
                e.start
                    .as_ref()
                    .map_or(String::new(), |s| format!(", {s} UTC"))
            ));
        }
    }
}

fn pids_tab(ui: &mut Ui, view: &TsView) {
    let r = &*view.report;
    egui::Grid::new("pids")
        .striped(true)
        .num_columns(8)
        .spacing([12.0, 2.0])
        .show(ui, |ui| {
            for h in [
                "PID",
                "what",
                "service",
                "packets",
                "kbit/s",
                "CC err",
                "TEI",
                "scrambled",
            ] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for row in &r.pids {
                let s = &row.stats;
                ui.label(format!("{:#06x}", row.pid));
                ui.label(&row.kind);
                ui.label(row.service.as_deref().unwrap_or(""));
                ui.label(s.packets.to_string());
                ui.label(format!("{:.0}", s.rate_bps / 1e3));
                let bad = |ui: &mut Ui, n: u64| {
                    if n > 0 {
                        ui.label(RichText::new(n.to_string()).color(RED));
                    } else {
                        ui.label("0");
                    }
                };
                bad(ui, s.cc_errors);
                bad(ui, s.errors);
                ui.label(if s.scrambled > 0 {
                    s.scrambled.to_string()
                } else {
                    String::new()
                });
                ui.end_row();
            }
        });
}

fn table_name(tid: u8) -> String {
    match tid {
        0x00 => "PAT".into(),
        0x01 => "CAT".into(),
        0x02 => "PMT".into(),
        0x40 => "NIT (this network)".into(),
        0x41 => "NIT (other network)".into(),
        0x42 => "SDT (this stream)".into(),
        0x46 => "SDT (other stream)".into(),
        0x4A => "BAT".into(),
        0x4E => "EIT present/following (this stream)".into(),
        0x4F => "EIT present/following (other stream)".into(),
        0x50..=0x5F => format!("EIT schedule (this stream) {tid:#04x}"),
        0x60..=0x6F => format!("EIT schedule (other stream) {tid:#04x}"),
        0x70 => "TDT".into(),
        0x73 => "TOT".into(),
        t => format!("table {t:#04x}"),
    }
}

fn tables_tab(ui: &mut Ui, view: &TsView) {
    let r = &*view.report;
    egui::Grid::new("tables")
        .striped(true)
        .num_columns(3)
        .spacing([14.0, 2.0])
        .show(ui, |ui| {
            for h in ["PID", "table", "sections"] {
                ui.label(RichText::new(h).strong());
            }
            ui.end_row();
            for &(pid, tid, n) in &r.tables {
                ui.label(format!("{pid:#06x}"));
                ui.label(table_name(tid));
                ui.label(n.to_string());
                ui.end_row();
            }
        });
    if r.bad_sections > 0 {
        ui.label(RichText::new(format!("{} sections failed their CRC", r.bad_sections)).color(RED));
    }
}

fn network_tab(ui: &mut Ui, view: &TsView) {
    let n = &view.report.network;
    ui.label(format!(
        "network {} {}",
        n.network_id.map_or("?".into(), |i| format!("{i:#06x}")),
        n.name.as_deref().unwrap_or("")
    ));
    if n.transponders.is_empty() {
        ui.label(RichText::new("the NIT has not been heard").weak());
        return;
    }
    ui.add_space(4.0);
    for t in &n.transponders {
        let this = Some(t.ts_id) == view.report.ts_id;
        let head = match t.terrestrial {
            Some(ter) => format!(
                "{:.3} MHz  {}  ts {:#06x}{}",
                ter.centre_hz as f64 / 1e6,
                ter.describe(),
                t.ts_id,
                if this { "  ← this" } else { "" }
            ),
            None => format!("ts {:#06x}  onid {:#06x}", t.ts_id, t.onid),
        };
        egui::CollapsingHeader::new(RichText::new(head).strong())
            .id_salt(("ts", t.ts_id, t.onid))
            .default_open(this)
            .show(ui, |ui| {
                for &(sid, ty) in &t.services {
                    let lcn = t.lcns.iter().find(|l| l.0 == sid);
                    let name = view
                        .report
                        .programmes
                        .iter()
                        .find(|p| p.number == sid)
                        .and_then(|p| p.name.as_deref())
                        .unwrap_or("");
                    ui.label(format!(
                        "  {:>3} {sid:#06x} {:<20} {}{}",
                        lcn.map_or(String::new(), |l| l.1.to_string()),
                        name,
                        service_type_name(ty),
                        if lcn.is_some_and(|l| !l.2) {
                            "  (hidden)"
                        } else {
                            ""
                        },
                    ));
                }
            });
    }
}

fn stream_tab(ui: &mut Ui, view: &TsView) {
    let r = &*view.report;
    egui::Grid::new("stream")
        .num_columns(2)
        .spacing([14.0, 3.0])
        .show(ui, |ui| {
            let mut row = |k: &str, v: String| {
                ui.label(RichText::new(k).weak());
                ui.label(v);
                ui.end_row();
            };
            row(
                "transport stream id",
                r.ts_id.map_or("—".into(), |i| format!("{i:#06x}")),
            );
            row("packets", view.packets.to_string());
            row("rate", format!("{:.2} Mbit/s", view.rate_bps / 1e6));
            row("UTC", r.utc.clone().unwrap_or_else(|| "—".into()));
            row(
                "local time",
                match (r.utc_unix, r.local_time) {
                    (Some(t), lt) => local_stamp(t, lt),
                    _ => "—".into(),
                },
            );
            row(
                "offset from UTC",
                r.local_time.map_or("—".into(), |l| {
                    format!(
                        "{:+} min now, {:+} min after the change",
                        l.offset_min, l.next_offset_min
                    )
                }),
            );
            row("bad sections", r.bad_sections.to_string());
            row("packets lost before the hub", view.dropped.to_string());
            row("packets lost to the player", view.tap_dropped.to_string());
        });
}
