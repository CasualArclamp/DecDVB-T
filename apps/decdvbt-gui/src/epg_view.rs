// made by CasualArclamp
//! The programme guide window: services down, time across, from the EIT
//! (present/following and the schedule) in the local time the TOT gives. The
//! other multiplexes' services are there too (every Freeview multiplex sends
//! the whole network's eight days), in channel-number order, dimmer: they
//! cannot be played from this multiplex.

use std::time::{SystemTime, UNIX_EPOCH};

use decdvbt_core::channel::uhf_channel_at;
use decdvbt_ts::epg::{civil, weekday};
use decdvbt_ts::{Epg, EpgEvent, LocalTime, TsReport};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, RichText, ScrollArea, Sense, Stroke, Ui,
    pos2, vec2,
};

use crate::services;

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const ROW_H: f32 = 34.0;
const NAME_W: f32 = 150.0;
const HEADER_H: f32 = 20.0;
const PANEL_BG: Color32 = Color32::from_rgb(12, 12, 15);

/// "Sat 2026-10-23 14:05" for `t` (seconds since 1970, UTC) in local time.
pub fn local_stamp(t: i64, lt: Option<LocalTime>) -> String {
    let local = t + 60 * i64::from(lt.map_or(0, |l| l.offset_at(t)));
    let (y, mo, d, h, mi, _) = civil(local);
    format!(
        "{} {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}",
        DAYS[weekday(local) as usize]
    )
}

fn hhmm(t: i64, lt: Option<LocalTime>) -> String {
    let local = t + 60 * i64::from(lt.map_or(0, |l| l.offset_at(t)));
    let (_, _, _, h, m, _) = civil(local);
    format!("{h:02}:{m:02}")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A muted colour for a content nibble.
fn genre_colour(g: Option<u8>) -> Color32 {
    match g {
        Some(1) => Color32::from_rgb(70, 52, 92),
        Some(2) => Color32::from_rgb(44, 66, 96),
        Some(3) => Color32::from_rgb(92, 72, 40),
        Some(4) => Color32::from_rgb(40, 88, 60),
        Some(5) => Color32::from_rgb(96, 62, 78),
        Some(6) => Color32::from_rgb(88, 44, 52),
        Some(7) => Color32::from_rgb(62, 78, 92),
        Some(8 | 9) => Color32::from_rgb(52, 82, 82),
        Some(10) => Color32::from_rgb(82, 82, 46),
        _ => Color32::from_rgb(44, 48, 56),
    }
}

/// A row of the guide: a service of this multiplex or another.
struct Row {
    number: u16,
    lcn: Option<u16>,
    name: String,
    /// On this multiplex, with a picture or sound: it can be played.
    playable: bool,
    /// On another multiplex: its UHF channel.
    elsewhere: Option<Option<u8>>,
}

/// The guide's rows: this multiplex's services and, if asked, the others' that
/// have events, by channel number.
fn rows(report: &TsReport, epg: &Epg, all: bool) -> Vec<Row> {
    let mut v: Vec<Row> = services::ordered(report)
        .into_iter()
        .map(|p| Row {
            number: p.number,
            lcn: p.lcn,
            name: p.name.clone().unwrap_or_else(|| "?".into()),
            playable: services::on_air(p),
            elsewhere: None,
        })
        .collect();
    if all {
        v.extend(
            report
                .others
                .iter()
                .filter(|o| epg.services.get(&o.number).is_some_and(|m| !m.is_empty()))
                .map(|o| Row {
                    number: o.number,
                    lcn: o.lcn,
                    name: o.name.clone().unwrap_or_else(|| "?".into()),
                    playable: false,
                    elsewhere: Some(o.centre_hz.and_then(|f| uhf_channel_at(f as f64))),
                }),
        );
    }
    v.sort_by_key(|r| (r.lcn.unwrap_or(u16::MAX), r.number));
    v
}

/// The access and picture marks of an event: HD, S(ubtitles), HoH, AD, SL, 5.1.
fn marks(e: &EpgEvent) -> Vec<&'static str> {
    let mut m = Vec::new();
    if e.hd {
        m.push("HD");
    }
    if e.subtitles {
        m.push(if e.hard_of_hearing { "S (HoH)" } else { "S" });
    }
    if e.audio_description {
        m.push("AD");
    }
    if e.signed {
        m.push("SL");
    }
    if e.surround {
        m.push("5.1");
    }
    m
}

pub struct EpgView {
    pub open: bool,
    /// Show the other multiplexes' services too.
    all: bool,
    /// A service whose programme on now is to be selected once the guide has
    /// it (`--service` with `--window epg`, for screenshots).
    pending: Option<u16>,
    selected: Option<(u16, u16)>,
    px_per_min: f32,
    /// Scroll to "now" on the next draw.
    jump: bool,
}

impl EpgView {
    pub fn new(open: bool) -> Self {
        EpgView {
            open,
            all: true,
            pending: None,
            selected: None,
            px_per_min: 4.0,
            jump: true,
        }
    }

    /// Select what is on now on `service` once the guide knows it.
    pub fn select_now(&mut self, service: u16) {
        self.pending = Some(service);
    }

    /// Draw the window; the service to play, if a programme on now was
    /// clicked.
    pub fn show(&mut self, ctx: &egui::Context, epg: &Epg, report: &TsReport) -> Option<u16> {
        let mut open = self.open;
        let mut play = None;
        egui::Window::new("Programme guide")
            .id(egui::Id::new("epg_view"))
            .open(&mut open)
            .default_size([980.0, 560.0])
            .resizable(true)
            .show(ctx, |ui| play = self.body(ui, epg, report));
        if open && !self.open {
            self.jump = true;
        }
        self.open = open;
        play
    }

    fn body(&mut self, ui: &mut Ui, epg: &Epg, report: &TsReport) -> Option<u16> {
        let mut play: Option<u16> = None;
        let lt = report.local_time;
        let now = report.utc_unix.unwrap_or_else(now_unix);
        let services = rows(report, epg, self.all);
        if let Some(sid) = self.pending
            && let (Some(e), _) = epg.now_next(sid, now)
        {
            self.selected = Some((sid, e.event_id));
            self.pending = None;
        }
        ui.horizontal(|ui| {
            if ui.button("Now").clicked() {
                self.jump = true;
            }
            ui.checkbox(&mut self.all, "all multiplexes");
            ui.label("zoom");
            ui.add(egui::Slider::new(&mut self.px_per_min, 1.5..=10.0).show_value(false));
            ui.label(
                RichText::new(format!(
                    "{} events · {} · click a programme that is on now to watch it",
                    epg.events(),
                    local_stamp(now, lt)
                ))
                .weak(),
            );
        });
        if epg.events() == 0 {
            ui.label(
                RichText::new(
                    "No guide yet. The EIT comes with the stream; present/following within a \
                     few seconds, the schedule over the next minutes.",
                )
                .weak(),
            );
            return None;
        }
        // The time range: an hour back to the last event heard.
        let t_min = (now - 3600) / 1800 * 1800;
        let t_max = epg
            .services
            .values()
            .flat_map(|m| m.values())
            .map(EpgEvent::end)
            .max()
            .unwrap_or(now + 3600)
            .max(now + 4 * 3600);
        let width = ((t_max - t_min) as f32 / 60.0 * self.px_per_min).max(200.0);
        let height = HEADER_H + ROW_H * services.len() as f32;
        let x_of = |t: i64| (t - t_min) as f32 / 60.0 * self.px_per_min;
        let detail_h = 170.0;
        let rows_h = (ui.available_height() - detail_h).max(120.0);
        ScrollArea::vertical()
            .id_salt("epg_v")
            .max_height(rows_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    // The names, fixed at the left.
                    let (names, p) = ui.allocate_painter(vec2(NAME_W, height), Sense::click());
                    // A click on a service's name plays it, if it has a
                    // picture or sound.
                    if names.clicked()
                        && let Some(pt) = names.interact_pointer_pos()
                    {
                        let row = ((pt.y - names.rect.top() - HEADER_H) / ROW_H).floor();
                        if row >= 0.0
                            && let Some(s) = services.get(row as usize)
                            && s.playable
                        {
                            play = Some(s.number);
                        }
                    }
                    p.rect_filled(names.rect, CornerRadius::ZERO, PANEL_BG);
                    for (i, s) in services.iter().enumerate() {
                        let y = names.rect.top() + HEADER_H + ROW_H * i as f32;
                        p.text(
                            pos2(names.rect.left() + 4.0, y + ROW_H / 2.0),
                            Align2::LEFT_CENTER,
                            format!(
                                "{:>3} {}",
                                s.lcn.map_or(String::new(), |n| n.to_string()),
                                s.name.chars().take(18).collect::<String>()
                            ),
                            FontId::monospace(12.0),
                            if s.elsewhere.is_some() {
                                Color32::GRAY
                            } else {
                                Color32::LIGHT_GRAY
                            },
                        );
                    }
                    let mut area = ScrollArea::horizontal()
                        .id_salt("epg_h")
                        .auto_shrink([false, false]);
                    if std::mem::take(&mut self.jump) {
                        area = area.horizontal_scroll_offset((x_of(now) - 60.0).max(0.0));
                    }
                    area.show(ui, |ui| {
                        let (resp, p) = ui.allocate_painter(vec2(width, height), Sense::hover());
                        let rect = resp.rect;
                        let clip = ui.clip_rect();
                        p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
                        // The hours.
                        let first = (t_min + 3599) / 3600 * 3600;
                        let mut t = first;
                        while t <= t_max {
                            let x = rect.left() + x_of(t);
                            if x >= clip.left() - 60.0 && x <= clip.right() + 60.0 {
                                p.line_segment(
                                    [pos2(x, rect.top()), pos2(x, rect.bottom())],
                                    Stroke::new(1.0, Color32::from_white_alpha(18)),
                                );
                                p.text(
                                    pos2(x + 3.0, rect.top() + 2.0),
                                    Align2::LEFT_TOP,
                                    hhmm(t, lt),
                                    FontId::monospace(11.0),
                                    Color32::GRAY,
                                );
                            }
                            t += 3600;
                        }
                        let (tx0, tx1) = (
                            t_min + ((clip.left() - rect.left()) / self.px_per_min * 60.0) as i64
                                - 60,
                            t_min
                                + ((clip.right() - rect.left()) / self.px_per_min * 60.0) as i64
                                + 60,
                        );
                        for (i, s) in services.iter().enumerate() {
                            let y = rect.top() + HEADER_H + ROW_H * i as f32;
                            for e in epg.between(s.number, tx0, tx1) {
                                let r = Rect::from_min_max(
                                    pos2(rect.left() + x_of(e.start) + 1.0, y + 2.0),
                                    pos2(rect.left() + x_of(e.end()) - 1.0, y + ROW_H - 2.0),
                                );
                                let id = ui.id().with(("ev", s.number, e.event_id));
                                let hit = ui.interact(r.intersect(clip), id, Sense::click());
                                let on_now = e.start <= now && now < e.end();
                                // What is on now, on a service with something to
                                // show, plays when clicked (marked ▶).
                                let playable = on_now && s.playable;
                                if hit.clicked() {
                                    self.selected = Some((s.number, e.event_id));
                                    if playable {
                                        play = Some(s.number);
                                    }
                                }
                                if playable && hit.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                let sel = self.selected == Some((s.number, e.event_id));
                                let mut fill = genre_colour(e.genre);
                                if s.elsewhere.is_some() {
                                    fill = fill.gamma_multiply(0.75);
                                }
                                if on_now {
                                    fill = fill.gamma_multiply(1.5);
                                }
                                if hit.hovered() {
                                    fill = fill.gamma_multiply(1.25);
                                }
                                p.rect_filled(r, CornerRadius::same(3), fill);
                                if sel {
                                    p.rect_stroke(
                                        r,
                                        CornerRadius::same(3),
                                        Stroke::new(1.5, Color32::WHITE),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                p.with_clip_rect(r.shrink2(vec2(3.0, 0.0)).intersect(clip))
                                    .text(
                                        pos2(r.left().max(clip.left()) + 4.0, r.center().y),
                                        Align2::LEFT_CENTER,
                                        if playable {
                                            format!("▶ {}", e.name)
                                        } else {
                                            e.name.clone()
                                        },
                                        FontId::proportional(12.0),
                                        Color32::WHITE,
                                    );
                            }
                        }
                        let x = rect.left() + x_of(now);
                        p.line_segment(
                            [pos2(x, rect.top()), pos2(x, rect.bottom())],
                            Stroke::new(1.5, Color32::from_rgb(255, 90, 80)),
                        );
                    });
                });
            });
        ui.separator();
        self.detail(ui, epg, report);
        play
    }

    fn detail(&self, ui: &mut Ui, epg: &Epg, report: &TsReport) {
        let lt = report.local_time;
        let ev = self
            .selected
            .and_then(|(s, e)| Some((s, epg.services.get(&s)?.get(&e)?)));
        let Some((sid, e)) = ev else {
            ui.label(RichText::new("Click an event for its description.").weak());
            return;
        };
        let name_of = |sid: u16| {
            report
                .programmes
                .iter()
                .find(|p| p.number == sid)
                .and_then(|p| p.name.clone())
                .or_else(|| {
                    report
                        .others
                        .iter()
                        .find(|o| o.number == sid)
                        .and_then(|o| o.name.clone())
                })
                .unwrap_or_else(|| "?".into())
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&e.name).strong());
            for m in marks(e) {
                ui.label(
                    RichText::new(m)
                        .small()
                        .color(Color32::BLACK)
                        .background_color(Color32::from_rgb(170, 190, 210)),
                );
            }
        });
        let mut meta = vec![
            name_of(sid),
            format!("{} – {}", local_stamp(e.start, lt), hhmm(e.end(), lt)),
            format!("{} min", e.duration_s / 60),
        ];
        meta.extend(e.kinds().iter().map(|k| k.to_string()));
        if let Some(a) = e.min_age {
            meta.push(format!("{a}+"));
        }
        if let Some(Some(ch)) = report
            .others
            .iter()
            .find(|o| o.number == sid)
            .map(|o| o.centre_hz.and_then(|f| uhf_channel_at(f as f64)))
        {
            meta.push(format!("on C{ch}"));
        }
        ui.label(RichText::new(meta.join(" · ")).small().weak());
        if let Some(g) = &e.guidance {
            ui.label(
                RichText::new(g)
                    .small()
                    .color(Color32::from_rgb(240, 190, 90)),
            );
        }
        ScrollArea::vertical()
            .id_salt("epg_detail")
            .max_height(110.0)
            .show(ui, |ui| {
                ui.label(e.description());
                for (what, value) in &e.items {
                    ui.label(RichText::new(format!("{what}: {value}")).small());
                }
                // The same episode at other times, and the series' events.
                let other_times = |list: Vec<(u16, &EpgEvent)>| {
                    list.into_iter()
                        .filter(|(s, x)| !(*s == sid && x.event_id == e.event_id))
                        .take(6)
                        .map(|(s, x)| format!("{} {}", local_stamp(x.start, lt), name_of(s)))
                        .collect::<Vec<_>>()
                };
                if let Some(c) = &e.episode_crid {
                    let full = epg.full_crid(sid, c);
                    let also = other_times(epg.showings(&full));
                    if !also.is_empty() {
                        ui.label(
                            RichText::new(format!("Also shown: {}", also.join(" · "))).small(),
                        );
                    }
                }
                if let Some(c) = &e.series_crid {
                    let full = epg.full_crid(sid, c);
                    let series = epg.series_events(&full);
                    let later: Vec<String> = series
                        .iter()
                        .filter(|(_, x)| x.start > e.start)
                        .take(6)
                        .map(|(s, x)| {
                            format!("{} {} ({})", local_stamp(x.start, lt), x.name, name_of(*s))
                        })
                        .collect();
                    ui.label(
                        RichText::new(format!(
                            "Series: {} events in the guide{}",
                            series.len(),
                            if later.is_empty() {
                                String::new()
                            } else {
                                format!("; next: {}", later.join(" · "))
                            }
                        ))
                        .small(),
                    );
                }
                let crids: Vec<String> = [
                    ("episode", &e.episode_crid),
                    ("series", &e.series_crid),
                    ("recommended", &e.recommendation_crid),
                ]
                .iter()
                .filter_map(|(k, c)| c.as_ref().map(|c| format!("{k} {}", epg.full_crid(sid, c))))
                .collect();
                if !crids.is_empty() {
                    ui.label(RichText::new(crids.join(" · ")).small().weak());
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_shown_in_the_streams_local_time() {
        let t = 1_677_326_400; // 2023-02-25 12:00:00 UTC, a Saturday
        assert_eq!(local_stamp(t, None), "Sat 2023-02-25 12:00");
        let bst = LocalTime {
            offset_min: 60,
            change: t + 86_400,
            next_offset_min: 120,
        };
        assert_eq!(local_stamp(t, Some(bst)), "Sat 2023-02-25 13:00");
        assert_eq!(hhmm(t + 2 * 86_400, Some(bst)), "14:00");
    }
}
