// made by CasualArclamp
//! Simple mode: the receiver as a set-top box. The expert window's panels go
//! and three screens take the window: the TV (the picture, full size, with a
//! banner on a channel change), the guide (channels down, time across, as a
//! television's) and the channel scan (a sweep of the UHF band, then each
//! multiplex found locked in turn to read its services). A strip along the top
//! switches screens, the look and back to the expert window.
//!
//! Two looks: Native (the expert window's dark panels and amber) and Teletext
//! (Ceefax colours, a monospaced face, Fastext). The mode and the look are
//! remembered; the channel list a scan finds is kept in
//! `%APPDATA%\DecDVB-T\channels.txt`.
//!
//! Keys (the Red Button application, when it shows, has the arrows and OK):
//! TV: ▲▼ or PgUp/PgDn change channel, OK or G the guide, I the banner.
//! Guide: arrows move, OK or red (R) watches, blue (B) back to now, yellow (Y)
//! the scan, Esc, green (G) or Tab the TV. Scan: OK or red starts and stops,
//! green the TV, yellow the guide. Tab opens or closes the guide whatever has
//! the keys. Teletext shows the colour keys as Fastext.
//!
//! Cost: nothing new runs per sample; the screens are painted shapes and
//! text, drawn only when the app repaints anyway.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use decdvbt_core::channel::{uhf_centre_hz, uhf_channel_at};
use decdvbt_rx::auto::Standard;
use eframe::egui::{
    self, Align2, Color32, FontId, Key, Modifiers, Painter, Pos2, Rect, Sense, Stroke, StrokeKind,
    pos2, vec2,
};

use crate::epg_view::{self, Row};
use crate::{App, prefs, services};

/// How long the channel banner stays.
const BANNER: Duration = Duration::from_secs(5);
/// The guide shows two hours.
const SPAN_S: i64 = 2 * 3600;
const HALF_HOUR: i64 = 1800;
/// A multiplex's services are taken once their count has held this long.
const SETTLE: Duration = Duration::from_millis(2500);
/// A channel that gives no services in this long is passed.
const LOCK_GIVE_UP: Duration = Duration::from_secs(14);
/// A channel this far over the band's median level is worth locking to.
const OVER_FLOOR_DB: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    Native,
    Teletext,
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Theme::Native => "Native",
            Theme::Teletext => "Teletext",
        }
    }

    pub fn parse(s: &str) -> Option<Theme> {
        match s.to_ascii_lowercase().as_str() {
            "native" => Some(Theme::Native),
            "teletext" => Some(Theme::Teletext),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Screen {
    #[default]
    Tv,
    Guide,
    Scan,
}

impl Screen {
    pub fn parse(s: &str) -> Option<Screen> {
        match s {
            "tv" => Some(Screen::Tv),
            "guide" => Some(Screen::Guide),
            "scan" => Some(Screen::Scan),
            _ => None,
        }
    }
}

/// A look's colours and type.
struct Look {
    mono: bool,
    bg: Color32,
    panel: Color32,
    line: Color32,
    text: Color32,
    dim: Color32,
    accent: Color32,
    cell: [Color32; 2],
    cell_fg: [Color32; 2],
    past_bg: Color32,
    past_fg: Color32,
    now_bg: Color32,
    focus_bg: Color32,
    focus_fg: Color32,
    now_line: Color32,
    internet: Color32,
}

const TT_BLUE: Color32 = Color32::from_rgb(0, 0, 255);
const TT_CYAN: Color32 = Color32::from_rgb(0, 255, 255);
const TT_YELLOW: Color32 = Color32::from_rgb(255, 255, 0);
const TT_GREEN: Color32 = Color32::from_rgb(0, 255, 0);
const TT_RED: Color32 = Color32::from_rgb(255, 0, 0);
const TT_MAGENTA: Color32 = Color32::from_rgb(255, 0, 255);

impl Look {
    fn of(t: Theme) -> Look {
        match t {
            Theme::Native => Look {
                mono: false,
                bg: Color32::from_rgb(24, 24, 24),
                panel: Color32::from_rgb(30, 30, 30),
                line: Color32::from_rgb(44, 44, 44),
                text: Color32::from_rgb(232, 232, 232),
                dim: Color32::from_rgb(141, 141, 141),
                accent: Color32::from_rgb(240, 192, 64),
                cell: [Color32::from_rgb(27, 27, 27); 2],
                cell_fg: [Color32::from_rgb(228, 228, 228); 2],
                past_bg: Color32::from_rgb(27, 27, 27),
                past_fg: Color32::from_rgb(141, 141, 141),
                now_bg: Color32::from_rgb(38, 38, 38),
                focus_bg: Color32::from_rgb(58, 47, 18),
                focus_fg: Color32::from_rgb(255, 211, 106),
                now_line: Color32::from_rgb(255, 154, 31),
                internet: Color32::from_rgb(184, 163, 255),
            },
            Theme::Teletext => Look {
                mono: true,
                bg: Color32::BLACK,
                panel: Color32::BLACK,
                line: Color32::BLACK,
                text: Color32::WHITE,
                dim: TT_CYAN,
                accent: TT_YELLOW,
                cell: [TT_BLUE, TT_CYAN],
                cell_fg: [Color32::WHITE, Color32::BLACK],
                past_bg: Color32::BLACK,
                past_fg: TT_CYAN,
                now_bg: TT_BLUE,
                focus_bg: TT_YELLOW,
                focus_fg: Color32::BLACK,
                now_line: TT_RED,
                internet: TT_MAGENTA,
            },
        }
    }

    fn font(&self, size: f32) -> FontId {
        if self.mono {
            FontId::monospace(size)
        } else {
            FontId::proportional(size)
        }
    }
}

/// A multiplex a scan found: its channel, standard and services
/// (channel number, service id, name).
#[derive(Clone, Debug)]
pub struct Found {
    pub ch: u8,
    pub standard: String,
    pub services: Vec<(Option<u16>, u16, String)>,
}

enum Phase {
    /// The band scan sweeps the band.
    Sweep,
    /// Locking to each channel with a signal in turn.
    Lock {
        cands: Vec<u8>,
        at: usize,
        since: Instant,
        seen: usize,
        stable: Instant,
    },
}

struct ChanScan {
    phase: Phase,
    /// The channel to go back to if the scan is stopped.
    back: u8,
    log: Vec<String>,
    found: Vec<Found>,
}

pub struct Simple {
    pub on: bool,
    pub theme: Theme,
    pub screen: Screen,
    /// The guide's focused row, the first row shown, and the time focused
    /// (None: now).
    row: usize,
    top: usize,
    at: Option<i64>,
    banner: Option<Instant>,
    scan: Option<ChanScan>,
    /// The last scan's result (the channel list).
    pub found: Vec<Found>,
    /// Something was put on since simple mode came on.
    started: bool,
    /// The last scan's log, kept to show after it ends.
    log: Vec<String>,
    /// `DECDVBT_SIMPLE_SCAN=1` (screenshots, tests) has started its scan.
    env_scan_done: bool,
}

impl Simple {
    pub fn new(on: bool, theme: Theme, screen: Screen) -> Simple {
        Simple {
            on,
            theme,
            screen,
            row: 0,
            top: 0,
            at: None,
            banner: None,
            scan: None,
            found: load_list(),
            started: false,
            log: Vec::new(),
            env_scan_done: false,
        }
    }
}

// ---- the channel list file ----

fn list_path() -> Option<std::path::PathBuf> {
    Some(prefs::dir()?.join("channels.txt"))
}

/// The channel list: a line a service, "C42<TAB>DVB-T<TAB>lcn<TAB>sid<TAB>name".
fn load_list() -> Vec<Found> {
    let Some(text) = list_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    let mut out: Vec<Found> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.splitn(5, '\t').collect();
        let [ch, std, lcn, sid, name] = f[..] else {
            continue;
        };
        let (Some(ch), Ok(sid)) = (
            ch.strip_prefix('C').and_then(|c| c.parse().ok()),
            sid.parse(),
        ) else {
            continue;
        };
        let lcn = lcn.parse().ok();
        match out.iter_mut().find(|m| m.ch == ch) {
            Some(m) => m.services.push((lcn, sid, name.to_string())),
            None => out.push(Found {
                ch,
                standard: std.to_string(),
                services: vec![(lcn, sid, name.to_string())],
            }),
        }
    }
    out
}

fn save_list(found: &[Found]) {
    let Some(p) = list_path() else { return };
    let mut s = String::new();
    for m in found {
        for (lcn, sid, name) in &m.services {
            let lcn = lcn.map_or("-".to_string(), |l| l.to_string());
            s.push_str(&format!(
                "C{}\t{}\t{lcn}\t{sid}\t{name}\n",
                m.ch, m.standard
            ));
        }
    }
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let _ = std::fs::write(p, s);
}

// ---- time ----

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn floor_half(t: i64) -> i64 {
    t - t.rem_euclid(HALF_HOUR)
}

impl App {
    // ---- switching ----

    /// Simple mode on or off (remembered by a live run).
    pub(crate) fn simple_set(&mut self, on: bool) {
        self.simple.on = on;
        self.tv.embedded = on;
        if on {
            self.simple.started = self.tv.service().is_some();
        }
        if self.keeper.is_some() {
            prefs::set("simple", if on { "1" } else { "0" });
        }
    }

    fn simple_theme(&mut self, t: Theme) {
        self.simple.theme = t;
        if self.keeper.is_some() {
            prefs::set("simple_theme", t.name());
        }
    }

    /// Watch service `sid`, on channel `ch` if it is another multiplex's.
    fn simple_watch(&mut self, sid: u16, ch: Option<u8>) {
        self.simple.screen = Screen::Tv;
        self.simple.banner = Some(Instant::now());
        self.simple.started = true;
        match ch.filter(|&c| c != self.channel) {
            Some(c) => {
                self.tune_channel(c);
                // Played once the multiplex's tables are in.
                self.opts.play = Some(sid);
            }
            None => self.watch(sid),
        }
        if self.keeper.is_some() {
            prefs::set("simple_sid", &sid.to_string());
        }
    }

    /// The services to step through with channel up and down: the scan's
    /// list, else this multiplex's and the others the network names.
    fn zap_list(&self) -> Vec<(u16, u16, Option<u8>)> {
        let mut v: Vec<(u16, u16, Option<u8>)> = Vec::new();
        if !self.simple.found.is_empty() {
            for m in &self.simple.found {
                for (lcn, sid, _) in &m.services {
                    v.push((lcn.unwrap_or(u16::MAX), *sid, Some(m.ch)));
                }
            }
        } else {
            let report = &self.ts.report;
            for p in services::ordered(report) {
                if services::on_air(p) || services::has_app(p) {
                    v.push((p.lcn.unwrap_or(u16::MAX), p.number, None));
                }
            }
            for o in &report.others {
                if matches!(
                    o.service_type,
                    Some(0x01 | 0x02 | 0x0A | 0x0C | 0x16 | 0x19 | 0x1F)
                ) {
                    let ch = o.centre_hz.and_then(|f| uhf_channel_at(f as f64));
                    v.push((o.lcn.unwrap_or(u16::MAX), o.number, ch));
                }
            }
        }
        v.sort_by_key(|e| (e.0, e.1));
        v.dedup_by_key(|e| e.1);
        v
    }

    fn zap(&mut self, up: bool) {
        let list = self.zap_list();
        if list.is_empty() {
            return;
        }
        let cur = self.tv.service().or(self.selected);
        let at = cur.and_then(|s| list.iter().position(|e| e.1 == s));
        let next = match (at, up) {
            (Some(i), true) => (i + 1) % list.len(),
            (Some(i), false) => (i + list.len() - 1) % list.len(),
            (None, _) => 0,
        };
        let (_, sid, ch) = list[next];
        self.simple_watch(sid, ch);
    }

    // ---- per frame ----

    /// Simple mode's work each frame: put a channel on, drive a scan.
    pub(crate) fn simple_tick(&mut self) {
        if !self.simple.on {
            return;
        }
        // DECDVBT_SIMPLE_SCAN=1 (screenshots, tests): scan once the radio runs.
        if !self.simple.env_scan_done
            && std::env::var_os("DECDVBT_SIMPLE_SCAN").is_some()
            && self.session.as_ref().is_some_and(|s| s.live)
            && self.tuned_at.elapsed() > Duration::from_secs(3)
        {
            self.simple.env_scan_done = true;
            self.simple_start_scan();
        }
        self.scan_tick();
        // Something on: the last channel watched if this multiplex has it,
        // else the first with a picture.
        if !self.simple.started && self.simple.scan.is_none() && self.opts.play.is_none() {
            let report = std::sync::Arc::clone(&self.ts.report);
            let list = services::ordered(&report);
            let last: Option<u16> = prefs::get("simple_sid").and_then(|s| s.parse().ok());
            let pick = last
                .filter(|s| list.iter().any(|p| p.number == *s && services::on_air(p)))
                .or_else(|| {
                    list.iter()
                        .find(|p| services::on_air(p) && p.streams.iter().any(|e| e.is_video()))
                        .map(|p| p.number)
                });
            if let Some(sid) = pick {
                self.simple.started = true;
                self.simple.banner = Some(Instant::now());
                self.watch(sid);
            }
        }
    }

    pub(crate) fn simple_start_scan(&mut self) {
        if !self.session.as_ref().is_some_and(|s| s.live) {
            self.note =
                "a channel scan needs the radio running (Start in the expert window)".into();
            return;
        }
        if self.simple.scan.is_some() {
            return;
        }
        self.tv.stop();
        self.tap = None;
        self.simple.scan = Some(ChanScan {
            phase: Phase::Sweep,
            back: self.channel,
            log: vec!["sweeping C21–C48…".into()],
            found: Vec::new(),
        });
        self.simple.screen = Screen::Scan;
        if !self.band_scan.running() {
            self.toggle_band_scan();
        }
    }

    fn simple_stop_scan(&mut self) {
        let Some(s) = self.simple.scan.take() else {
            return;
        };
        if self.band_scan.running() {
            self.toggle_band_scan();
        }
        self.simple.log = s.log;
        self.simple.log.push("stopped".into());
        self.tune_channel(s.back);
        self.simple.started = false;
    }

    fn scan_tick(&mut self) {
        let Some(scan) = &mut self.simple.scan else {
            return;
        };
        match &mut scan.phase {
            Phase::Sweep => {
                if self.band_scan.running() {
                    return;
                }
                let mut lv = self.band_scan.levels();
                let mut sorted: Vec<f32> = lv.iter().map(|l| l.1).collect();
                sorted.sort_by(f32::total_cmp);
                let floor = sorted.get(sorted.len() / 2).copied().unwrap_or(-100.0);
                lv.retain(|l| l.1 > floor + OVER_FLOOR_DB);
                let cands: Vec<u8> = lv.iter().map(|l| l.0).collect();
                scan.log.push(format!(
                    "{} channel{} above the floor: {}",
                    cands.len(),
                    if cands.len() == 1 { "" } else { "s" },
                    cands
                        .iter()
                        .map(|c| format!("C{c}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
                let Some(&first) = cands.first() else {
                    self.finish_scan();
                    return;
                };
                scan.phase = Phase::Lock {
                    cands,
                    at: 0,
                    since: Instant::now(),
                    seen: 0,
                    stable: Instant::now(),
                };
                self.tune_channel(first);
            }
            Phase::Lock {
                cands,
                at,
                since,
                seen,
                stable,
            } => {
                let report = std::sync::Arc::clone(&self.ts.report);
                let named: Vec<(Option<u16>, u16, String)> = services::ordered(&report)
                    .iter()
                    .filter_map(|p| p.name.clone().map(|n| (p.lcn, p.number, n)))
                    .collect();
                if named.len() != *seen {
                    *seen = named.len();
                    *stable = Instant::now();
                }
                let settled = !named.is_empty()
                    && stable.elapsed() >= SETTLE
                    && since.elapsed() >= Duration::from_secs(3);
                if !settled && since.elapsed() < LOCK_GIVE_UP {
                    return;
                }
                let ch = cands[*at];
                if named.is_empty() {
                    scan.log.push(format!("C{ch}: no lock"));
                } else {
                    let standard = match self.standard {
                        Standard::Dvbt2 => "DVB-T2",
                        _ => "DVB-T",
                    };
                    scan.log
                        .push(format!("C{ch}: {standard}, {} services", named.len()));
                    scan.found.push(Found {
                        ch,
                        standard: standard.into(),
                        services: named,
                    });
                }
                *at += 1;
                if let Some(&next) = cands.get(*at) {
                    *since = Instant::now();
                    *seen = 0;
                    *stable = Instant::now();
                    self.tune_channel(next);
                } else {
                    self.finish_scan();
                }
            }
        }
    }

    fn finish_scan(&mut self) {
        let Some(mut s) = self.simple.scan.take() else {
            return;
        };
        let n: usize = s.found.iter().map(|m| m.services.len()).sum();
        s.log
            .push(format!("done: {} multiplexes, {n} services", s.found.len()));
        if !s.found.is_empty() {
            save_list(&s.found);
            self.simple.found = s.found;
        }
        self.simple.log = s.log;
        let back = self.simple.found.first().map_or(s.back, |m| m.ch);
        self.tune_channel(back);
        self.simple.started = false;
    }

    // ---- keys ----

    /// Simple mode's keys, taken before the Red Button's.
    pub(crate) fn simple_keys(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let press = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if press(Key::Tab) {
            self.simple.screen = if self.simple.screen == Screen::Guide {
                Screen::Tv
            } else {
                Screen::Guide
            };
            self.simple.at = None;
            return;
        }
        match self.simple.screen {
            Screen::Tv => {
                // The application, when it shows, has the arrows and OK.
                if self.tv.red.shows_app() {
                    return;
                }
                if press(Key::ArrowUp) || press(Key::PageUp) {
                    self.zap(true);
                } else if press(Key::ArrowDown) || press(Key::PageDown) {
                    self.zap(false);
                } else if press(Key::Enter) || press(Key::G) {
                    self.simple.screen = Screen::Guide;
                    self.simple.at = None;
                } else if press(Key::I) {
                    self.simple.banner = Some(Instant::now());
                }
            }
            Screen::Guide => {
                let rows = epg_view::rows(&self.ts.report, &self.epg, true);
                if rows.is_empty() {
                    if press(Key::Escape) || press(Key::G) {
                        self.simple.screen = Screen::Tv;
                    }
                    return;
                }
                let n = rows.len();
                if press(Key::ArrowDown) {
                    self.simple.row = (self.simple.row + 1).min(n - 1);
                } else if press(Key::ArrowUp) {
                    self.simple.row = self.simple.row.saturating_sub(1);
                } else if press(Key::PageDown) {
                    self.simple.row = (self.simple.row + 6).min(n - 1);
                } else if press(Key::PageUp) {
                    self.simple.row = self.simple.row.saturating_sub(6);
                } else if press(Key::ArrowRight) {
                    self.guide_step(&rows, true);
                } else if press(Key::ArrowLeft) {
                    self.guide_step(&rows, false);
                } else if press(Key::Y) {
                    self.simple.screen = Screen::Scan;
                } else if press(Key::B) {
                    self.simple.at = None;
                } else if press(Key::Enter) || press(Key::R) {
                    let r = &rows[self.simple.row.min(n - 1)];
                    if r.playable {
                        self.simple_watch(r.number, None);
                    } else if let Some(ch) = r.elsewhere.flatten() {
                        self.simple_watch(r.number, Some(ch));
                    }
                } else if press(Key::Escape) || press(Key::Backspace) || press(Key::G) {
                    self.simple.screen = Screen::Tv;
                }
            }
            Screen::Scan => {
                if press(Key::G) {
                    self.simple.screen = Screen::Tv;
                } else if press(Key::Y) {
                    self.simple.screen = Screen::Guide;
                } else if press(Key::R) {
                    if self.simple.scan.is_some() {
                        self.simple_stop_scan();
                    } else {
                        self.simple_start_scan();
                    }
                } else if press(Key::Escape) || press(Key::Backspace) {
                    if self.simple.scan.is_some() {
                        self.simple_stop_scan();
                    } else {
                        self.simple.screen = Screen::Tv;
                    }
                } else if press(Key::Enter) && self.simple.scan.is_none() {
                    self.simple_start_scan();
                }
            }
        }
    }

    /// Move the guide's focus to the next or the previous programme.
    fn guide_step(&mut self, rows: &[Row], right: bool) {
        let now = self.ts.report.utc_unix.unwrap_or_else(now_unix);
        let at = self.simple.at.unwrap_or(now);
        let sid = rows[self.simple.row.min(rows.len() - 1)].number;
        let evs = self.epg.between(sid, at - 12 * 3600, at + 8 * 86400);
        let cur = evs.iter().find(|e| e.start <= at && at < e.end());
        let target = if right {
            evs.iter()
                .find(|e| e.start > at && cur.is_none_or(|c| e.start >= c.end()))
                .map(|e| e.start)
                .unwrap_or(at + HALF_HOUR)
        } else {
            let before = cur.map_or(at, |c| c.start);
            evs.iter()
                .rev()
                .find(|e| e.start < before)
                .map(|e| e.start.max(floor_half(now)))
                .unwrap_or(before - HALF_HOUR)
        };
        // Not before now's half hour: the guide is for what is coming.
        self.simple.at = Some(target.max(floor_half(now))).filter(|t| *t > now);
    }

    // ---- drawing ----

    /// The strip along the top: screens, look, and back to the expert window.
    pub(crate) fn simple_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("DecDVB-T")
                    .strong()
                    .color(Color32::from_gray(160)),
            );
            ui.separator();
            for (s, name) in [
                (Screen::Tv, "TV"),
                (Screen::Guide, "Guide"),
                (Screen::Scan, "Channel scan"),
            ] {
                if ui.selectable_label(self.simple.screen == s, name).clicked() {
                    self.simple.screen = s;
                    self.simple.at = None;
                }
            }
            ui.separator();
            ui.label("look");
            let mut t = self.simple.theme;
            egui::ComboBox::from_id_salt("simple_theme")
                .selected_text(t.name())
                .width(90.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut t, Theme::Native, "Native");
                    ui.selectable_value(&mut t, Theme::Teletext, "Teletext");
                });
            if t != self.simple.theme {
                self.simple_theme(t);
            }
            if !self.note.is_empty() {
                ui.label(egui::RichText::new(&self.note).color(Color32::from_rgb(240, 200, 80)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.selectable_label(false, "Expert").clicked() {
                    self.simple_set(false);
                }
                let _ = ui.selectable_label(true, "Simple");
            });
        });
    }

    pub(crate) fn simple_screen(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let look = Look::of(self.simple.theme);
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, look.bg);
        match self.simple.screen {
            Screen::Tv => self.draw_tv(&p, rect, &look),
            Screen::Guide => self.draw_guide(ui, rect, &look),
            Screen::Scan => self.draw_scan(ui, rect, &look),
        }
    }

    fn draw_tv(&mut self, p: &Painter, rect: Rect, look: &Look) {
        self.tv.draw_into(p, rect);
        if self.tv.service().is_none() {
            let msg = if let Some(e) = self.tv.problem() {
                e.to_string()
            } else if self.simple.found.is_empty() && self.ts.report.programmes.is_empty() {
                "No channels yet: open Channel scan".to_string()
            } else {
                "Tuning…".to_string()
            };
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                msg,
                look.font(26.0),
                look.dim,
            );
        }
        let show = self.simple.banner.is_some_and(|t| t.elapsed() < BANNER);
        if show && let Some(sid) = self.tv.service().or(self.selected) {
            self.draw_banner(p, rect, look, sid);
        }
    }

    /// The channel banner: number, name, now and next.
    fn draw_banner(&self, p: &Painter, rect: Rect, look: &Look, sid: u16) {
        let report = &self.ts.report;
        let now = report.utc_unix.unwrap_or_else(now_unix);
        let lt = report.local_time;
        let prog = report.programmes.iter().find(|q| q.number == sid);
        let name = prog.and_then(|q| q.name.clone()).unwrap_or_default();
        let lcn = prog
            .and_then(|q| q.lcn)
            .map_or(String::new(), |l| l.to_string());
        let (ev_now, ev_next) = self.epg.now_next(sid, now);
        let b = Rect::from_min_max(
            pos2(rect.left() + 40.0, rect.bottom() - 150.0),
            pos2(rect.right() - 40.0, rect.bottom() - 30.0),
        );
        let bg = if look.mono {
            TT_BLUE
        } else {
            look.panel.gamma_multiply(0.93)
        };
        p.rect_filled(b, if look.mono { 0.0 } else { 8.0 }, bg);
        let x = b.left() + 24.0;
        p.text(
            pos2(x, b.top() + 16.0),
            Align2::LEFT_TOP,
            &lcn,
            look.font(44.0),
            look.accent,
        );
        p.text(
            pos2(x + 110.0, b.top() + 18.0),
            Align2::LEFT_TOP,
            &name,
            look.font(30.0),
            look.text,
        );
        p.text(
            pos2(b.right() - 24.0, b.top() + 18.0),
            Align2::RIGHT_TOP,
            epg_view::hhmm(now, lt),
            look.font(26.0),
            look.text,
        );
        let mut y = b.top() + 66.0;
        if let Some(e) = ev_now {
            let s = format!(
                "Now  {}  {}–{}",
                e.name,
                epg_view::hhmm(e.start, lt),
                epg_view::hhmm(e.end(), lt)
            );
            p.text(
                pos2(x + 110.0, y),
                Align2::LEFT_TOP,
                s,
                look.font(20.0),
                look.text,
            );
            let frac = ((now - e.start) as f32 / e.duration_s.max(1) as f32).clamp(0.0, 1.0);
            let bar = Rect::from_min_size(pos2(b.right() - 324.0, y + 8.0), vec2(300.0, 6.0));
            p.rect_filled(bar, 2.0, look.line.gamma_multiply(1.6));
            p.rect_filled(
                Rect::from_min_size(bar.min, vec2(300.0 * frac, 6.0)),
                2.0,
                look.accent,
            );
            y += 30.0;
        }
        if let Some(e) = ev_next {
            let s = format!("Next {}  {}", e.name, epg_view::hhmm(e.start, lt));
            p.text(
                pos2(x + 110.0, y),
                Align2::LEFT_TOP,
                s,
                look.font(18.0),
                look.dim,
            );
        }
    }

    fn draw_guide(&mut self, ui: &mut egui::Ui, rect: Rect, look: &Look) {
        let p = ui.painter_at(rect);
        let report = std::sync::Arc::clone(&self.ts.report);
        let epg = std::sync::Arc::clone(&self.epg);
        let rows = epg_view::rows(&report, &epg, true);
        let now = report.utc_unix.unwrap_or_else(now_unix);
        let lt = report.local_time;
        if rows.is_empty() {
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "The guide comes with the first multiplex: tune one or run a channel scan",
                look.font(22.0),
                look.dim,
            );
            return;
        }
        self.simple.row = self.simple.row.min(rows.len() - 1);
        let at = self.simple.at.unwrap_or(now);
        let focus_sid = rows[self.simple.row].number;
        let focus_ev = epg
            .between(focus_sid, at - 12 * 3600, at + 8 * 86400)
            .into_iter()
            .find(|e| e.end() > at)
            .cloned();
        // The window: now's half hour, or the focus's when it is further on.
        let f = focus_ev.as_ref().map_or(at, |e| e.start.max(now));
        let mut w0 = floor_half(now);
        if f >= w0 + SPAN_S - HALF_HOUR {
            w0 = floor_half(f) - HALF_HOUR;
        }

        // Geometry per look.
        let tt = look.mono;
        let (head_h, row_h, name_w, foot_h) = if tt {
            (190.0, 46.0, 300.0, 40.0)
        } else {
            (210.0, 50.0, 260.0, 32.0)
        };
        let head = Rect::from_min_size(rect.min, vec2(rect.width(), head_h));
        let time_r =
            Rect::from_min_size(pos2(rect.left(), head.bottom()), vec2(rect.width(), 30.0));
        let grid = Rect::from_min_max(
            pos2(rect.left(), time_r.bottom()),
            pos2(rect.right(), rect.bottom() - foot_h),
        );
        let foot = Rect::from_min_max(pos2(rect.left(), grid.bottom()), rect.max);
        let cells_x0 = grid.left() + name_w;
        let px_per_s = (grid.right() - cells_x0) / SPAN_S as f32;
        let x_of =
            |t: i64| cells_x0 + ((t - w0) as f32 * px_per_s).clamp(0.0, SPAN_S as f32 * px_per_s);

        // Header.
        if tt {
            self.guide_head_teletext(
                &p,
                head,
                look,
                now,
                focus_ev.as_ref(),
                &rows[self.simple.row],
            );
        } else {
            self.guide_head_native(
                &p,
                head,
                look,
                now,
                focus_ev.as_ref(),
                &rows[self.simple.row],
            );
        }

        // Timeline.
        p.rect_filled(time_r, 0.0, look.panel);
        p.text(
            pos2(time_r.left() + 16.0, time_r.center().y),
            Align2::LEFT_CENTER,
            if tt {
                "CHANNEL".to_string()
            } else {
                format!("ALL · {} CHANNELS", rows.len())
            },
            look.font(if tt { 22.0 } else { 12.0 }),
            if tt { look.accent } else { look.dim },
        );
        for k in 0..4 {
            let t = w0 + k * HALF_HOUR;
            let x = x_of(t);
            p.line_segment(
                [pos2(x, time_r.top()), pos2(x, time_r.bottom())],
                Stroke::new(1.0, look.line),
            );
            p.text(
                pos2(x + 8.0, time_r.center().y),
                Align2::LEFT_CENTER,
                epg_view::hhmm(t, lt),
                look.font(if tt { 22.0 } else { 14.0 }),
                if tt { look.accent } else { look.text },
            );
        }

        // Rows.
        let visible = ((grid.height() / row_h).floor() as usize).max(1);
        if self.simple.row < self.simple.top {
            self.simple.top = self.simple.row;
        } else if self.simple.row >= self.simple.top + visible {
            self.simple.top = self.simple.row + 1 - visible;
        }
        let resp = ui.interact(grid, ui.id().with("simple_guide"), Sense::click());
        let mut clicked: Option<(usize, i64)> = None;
        for (i, r) in rows.iter().enumerate().skip(self.simple.top).take(visible) {
            let y = grid.top() + (i - self.simple.top) as f32 * row_h;
            let rr = Rect::from_min_size(pos2(grid.left(), y), vec2(grid.width(), row_h));
            let name_r = Rect::from_min_size(rr.min, vec2(name_w, row_h));
            p.rect_filled(name_r, 0.0, look.panel);
            let lcn = r.lcn.map_or(String::new(), |l| l.to_string());
            p.text(
                pos2(name_r.left() + 16.0, name_r.center().y),
                Align2::LEFT_CENTER,
                lcn,
                look.font(if tt { 24.0 } else { 16.0 }),
                look.accent,
            );
            let name_col = if r.playable || r.elsewhere.is_some() {
                look.text
            } else {
                look.dim
            };
            let nr = p.text(
                pos2(
                    name_r.left() + if tt { 76.0 } else { 66.0 },
                    name_r.center().y,
                ),
                Align2::LEFT_CENTER,
                &r.name,
                look.font(if tt { 24.0 } else { 17.0 }),
                name_col,
            );
            if r.internet {
                p.text(
                    pos2(nr.right() + 8.0, name_r.center().y),
                    Align2::LEFT_CENTER,
                    if tt { "@NET" } else { "IP" },
                    look.font(if tt { 18.0 } else { 11.0 }),
                    look.internet,
                );
            }
            let evs = epg.between(r.number, w0, w0 + SPAN_S);
            if evs.is_empty() {
                let c = Rect::from_min_max(pos2(cells_x0, y), pos2(grid.right(), y + row_h));
                p.rect_filled(
                    c.shrink2(vec2(0.0, if tt { 2.0 } else { 0.0 })),
                    0.0,
                    look.cell[0],
                );
                p.text(
                    pos2(c.left() + 12.0, c.center().y),
                    Align2::LEFT_CENTER,
                    if r.internet {
                        "Internet channel"
                    } else {
                        "No information"
                    },
                    look.font(if tt { 22.0 } else { 16.0 }),
                    look.dim,
                );
            }
            for (k, e) in evs.iter().enumerate() {
                let (x0, x1) = (x_of(e.start), x_of(e.end()));
                if x1 - x0 < 2.0 {
                    continue;
                }
                let c = Rect::from_min_max(pos2(x0, y), pos2(x1, y + row_h));
                let focus = i == self.simple.row
                    && focus_ev.as_ref().is_some_and(|f| f.event_id == e.event_id);
                let past = e.end() <= now;
                let on_now = e.start <= now && now < e.end();
                let (bg, fg) = if focus {
                    (look.focus_bg, look.focus_fg)
                } else if past {
                    (look.past_bg, look.past_fg)
                } else if on_now && !tt {
                    (look.now_bg, look.cell_fg[0])
                } else {
                    (look.cell[k % 2], look.cell_fg[k % 2])
                };
                let inner = if tt { c.shrink2(vec2(2.0, 2.0)) } else { c };
                p.rect_filled(inner, 0.0, bg);
                if !tt {
                    p.line_segment(
                        [c.right_top(), c.right_bottom()],
                        Stroke::new(1.0, look.line),
                    );
                }
                if focus && !tt {
                    p.rect_stroke(
                        inner,
                        0.0,
                        Stroke::new(2.0, look.accent),
                        StrokeKind::Inside,
                    );
                }
                let clip = p.with_clip_rect(inner.shrink2(vec2(10.0, 0.0)).intersect(rect));
                clip.text(
                    pos2(inner.left() + 10.0, inner.center().y),
                    Align2::LEFT_CENTER,
                    &e.name,
                    look.font(if tt { 22.0 } else { 17.0 }),
                    fg,
                );
                if let Some(pos) = resp.interact_pointer_pos()
                    && resp.clicked()
                    && c.contains(pos)
                {
                    clicked = Some((i, e.start));
                }
            }
            if !tt {
                p.line_segment(
                    [rr.left_bottom(), rr.right_bottom()],
                    Stroke::new(1.0, look.line),
                );
            }
        }
        // Now.
        if now >= w0 && now < w0 + SPAN_S {
            let x = x_of(now);
            p.line_segment(
                [pos2(x, time_r.top()), pos2(x, grid.bottom())],
                Stroke::new(if tt { 4.0 } else { 2.0 }, look.now_line),
            );
        }
        if let Some((i, start)) = clicked {
            if i == self.simple.row && focus_ev.as_ref().is_some_and(|f| f.start == start) {
                // A second click on the focused programme watches it.
                let r = &rows[i];
                if r.playable {
                    self.simple_watch(r.number, None);
                } else if let Some(ch) = r.elsewhere.flatten() {
                    self.simple_watch(r.number, Some(ch));
                }
            } else {
                self.simple.row = i;
                self.simple.at = Some(start).filter(|t| *t > now);
            }
        }

        // Keys.
        if tt {
            let w = foot.width() / 4.0;
            for (k, (s, c)) in [
                ("Watch", TT_RED),
                ("TV", TT_GREEN),
                ("Scan", TT_YELLOW),
                ("Now", TT_CYAN),
            ]
            .iter()
            .enumerate()
            {
                p.text(
                    pos2(foot.left() + w * (k as f32 + 0.5), foot.center().y),
                    Align2::CENTER_CENTER,
                    *s,
                    look.font(28.0),
                    *c,
                );
            }
        } else {
            p.rect_filled(foot, 0.0, look.panel);
            let hints = "OK watch    ◀▶ time    ▲▼ channel    B now    Y scan    G / Esc TV";
            p.text(
                pos2(foot.left() + 16.0, foot.center().y),
                Align2::LEFT_CENTER,
                hints,
                FontId::monospace(12.0),
                look.dim,
            );
        }
    }

    fn guide_head_native(
        &self,
        p: &Painter,
        head: Rect,
        look: &Look,
        now: i64,
        ev: Option<&decdvbt_ts::EpgEvent>,
        row: &Row,
    ) {
        let lt = self.ts.report.local_time;
        p.rect_filled(head, 0.0, look.panel);
        let pic = Rect::from_min_size(head.min, vec2(head.height() * 16.0 / 9.0, head.height()));
        self.tv.draw_into(p, pic);
        let x = pic.right() + 26.0;
        let lcn = row.lcn.map_or(String::new(), |l| format!("{l} · "));
        let mut meta = format!("{lcn}{}", row.name.to_uppercase());
        if let Some(e) = ev {
            meta.push_str(&format!(
                " · {}–{}",
                epg_view::hhmm(e.start, lt),
                epg_view::hhmm(e.end(), lt)
            ));
        }
        p.text(
            pos2(x, head.top() + 22.0),
            Align2::LEFT_TOP,
            meta,
            FontId::monospace(13.0),
            look.accent,
        );
        let title = ev.map_or("No information".to_string(), |e| e.name.clone());
        p.text(
            pos2(x, head.top() + 44.0),
            Align2::LEFT_TOP,
            title,
            look.font(30.0),
            look.text,
        );
        let clock_w = 220.0;
        let text_w = (head.right() - clock_w - x - 20.0).max(100.0);
        if let Some(e) = ev {
            if e.start <= now && now < e.end() {
                let frac = ((now - e.start) as f32 / e.duration_s.max(1) as f32).clamp(0.0, 1.0);
                let bar =
                    Rect::from_min_size(pos2(x, head.top() + 86.0), vec2(text_w.min(420.0), 4.0));
                p.rect_filled(bar, 0.0, look.line);
                p.rect_filled(
                    Rect::from_min_size(bar.min, vec2(bar.width() * frac, 4.0)),
                    0.0,
                    look.accent,
                );
            }
            let g = p.layout(
                e.description(),
                look.font(16.0),
                Color32::from_gray(190),
                text_w,
            );
            let clip = p.with_clip_rect(Rect::from_min_max(
                pos2(x, head.top() + 98.0),
                pos2(x + text_w, head.bottom() - 8.0),
            ));
            clip.galley(pos2(x, head.top() + 98.0), g, look.text);
        }
        let cx = head.right() - clock_w + 20.0;
        p.line_segment(
            [pos2(cx - 20.0, head.top()), pos2(cx - 20.0, head.bottom())],
            Stroke::new(1.0, look.line),
        );
        p.text(
            pos2(cx, head.top() + 20.0),
            Align2::LEFT_TOP,
            epg_view::hhmm(now, lt),
            FontId::monospace(30.0),
            look.text,
        );
        p.text(
            pos2(cx, head.top() + 60.0),
            Align2::LEFT_TOP,
            epg_view::local_stamp(now, lt).to_uppercase(),
            FontId::monospace(11.0),
            look.dim,
        );
        p.text(
            pos2(cx, head.bottom() - 40.0),
            Align2::LEFT_TOP,
            format!(
                "C{} · {}",
                self.channel,
                if self.standard == Standard::Dvbt2 {
                    "DVB-T2"
                } else {
                    "DVB-T"
                }
            ),
            FontId::monospace(12.0),
            look.dim,
        );
    }

    fn guide_head_teletext(
        &self,
        p: &Painter,
        head: Rect,
        look: &Look,
        now: i64,
        ev: Option<&decdvbt_ts::EpgEvent>,
        row: &Row,
    ) {
        let lt = self.ts.report.local_time;
        let mono = |s: f32| FontId::monospace(s);
        // The page header row.
        let y = head.top() + 4.0;
        let mut x = head.left() + 20.0;
        for (s, c) in [
            ("P601".to_string(), Color32::WHITE),
            ("DecDVB-T".to_string(), TT_YELLOW),
            ("601".to_string(), TT_GREEN),
            (
                epg_view::local_stamp(now, lt)[..3].to_string(),
                Color32::WHITE,
            ),
            (
                format!("{}/{:02}", epg_view::hhmm(now, lt), now.rem_euclid(60)),
                TT_YELLOW,
            ),
        ] {
            let r = p.text(pos2(x, y), Align2::LEFT_TOP, s, mono(24.0), c);
            x = r.right() + 22.0;
        }
        // The double-height band.
        let band = Rect::from_min_size(
            pos2(head.left(), head.top() + 38.0),
            vec2(head.width(), 96.0),
        );
        p.rect_filled(band, 0.0, TT_BLUE);
        p.text(
            pos2(band.left() + 30.0, band.center().y),
            Align2::LEFT_CENTER,
            "TV GUIDE",
            mono(76.0),
            TT_YELLOW,
        );
        let lcn = row.lcn.map_or(String::new(), |l| format!("{l} "));
        p.text(
            pos2(band.right() - 30.0, band.top() + 18.0),
            Align2::RIGHT_TOP,
            format!("{lcn}{}", row.name.to_uppercase()),
            mono(24.0),
            TT_CYAN,
        );
        if let Some(e) = ev {
            let s = format!(
                "{}  {}-{}",
                e.name,
                epg_view::hhmm(e.start, lt),
                epg_view::hhmm(e.end(), lt)
            );
            p.text(
                pos2(band.right() - 30.0, band.top() + 52.0),
                Align2::RIGHT_TOP,
                s,
                mono(24.0),
                Color32::WHITE,
            );
            let clip = p.with_clip_rect(Rect::from_min_max(
                pos2(head.left(), band.bottom()),
                head.max,
            ));
            clip.text(
                pos2(head.left() + 30.0, band.bottom() + 12.0),
                Align2::LEFT_TOP,
                e.description(),
                mono(22.0),
                TT_GREEN,
            );
        }
        let _ = look;
    }

    fn draw_scan(&mut self, ui: &mut egui::Ui, rect: Rect, look: &Look) {
        let p = ui.painter_at(rect);
        let tt = look.mono;
        let running = self.simple.scan.is_some();
        let (log, found): (Vec<String>, Vec<Found>) = match &self.simple.scan {
            Some(s) => (s.log.clone(), s.found.clone()),
            None => (self.simple.log.clone(), self.simple.found.clone()),
        };
        let status = match self.simple.scan.as_ref().map(|s| &s.phase) {
            Some(Phase::Sweep) => match self.band_scan.progress() {
                Some((at, all)) => format!("Sweeping the band: {} of {all}", at + 1),
                None => "Sweeping the band…".into(),
            },
            Some(Phase::Lock { cands, at, .. }) => format!(
                "Reading C{} ({} of {}): {:.0} MHz",
                cands[*at],
                at + 1,
                cands.len(),
                uhf_centre_hz(cands[*at]).unwrap_or(0.0) / 1e6
            ),
            None if found.is_empty() => "Press OK (Enter) to find your channels".into(),
            None => "Your channels: press OK to scan again, Esc for TV".into(),
        };
        let services: usize = found.iter().map(|m| m.services.len()).sum();
        let mut y;
        if tt {
            let band = Rect::from_min_size(
                pos2(rect.left(), rect.top() + 10.0),
                vec2(rect.width(), 96.0),
            );
            p.rect_filled(band, 0.0, TT_RED);
            p.text(
                pos2(band.left() + 30.0, band.center().y),
                Align2::LEFT_CENTER,
                if running { "TUNING" } else { "CHANNELS" },
                FontId::monospace(76.0),
                Color32::WHITE,
            );
            p.text(
                pos2(band.right() - 30.0, band.center().y),
                Align2::RIGHT_CENTER,
                format!("{} MUX  {services} SVC", found.len()),
                FontId::monospace(30.0),
                TT_YELLOW,
            );
            y = band.bottom() + 20.0;
            p.text(
                pos2(rect.left() + 30.0, y),
                Align2::LEFT_TOP,
                &status,
                FontId::monospace(26.0),
                TT_CYAN,
            );
            y += 46.0;
            // Mosaic blocks, a channel each.
            let done = self.band_scan.done_so_far();
            let lock_at = match self.simple.scan.as_ref().map(|s| &s.phase) {
                Some(Phase::Lock { cands, at, .. }) => Some(cands[*at]),
                _ => None,
            };
            for (k, n) in (21u8..=48).enumerate() {
                let r = Rect::from_min_size(
                    pos2(rect.left() + 30.0 + k as f32 * 34.0, y),
                    vec2(28.0, 28.0),
                );
                let c = if lock_at == Some(n) {
                    TT_YELLOW
                } else if found.iter().any(|m| m.ch == n) {
                    TT_GREEN
                } else if done.contains(&n)
                    || (!self.band_scan.running() && !self.band_scan.levels().is_empty())
                {
                    TT_BLUE
                } else {
                    Color32::from_gray(34)
                };
                p.rect_filled(r, 0.0, c);
                p.text(
                    pos2(r.center().x, r.bottom() + 4.0),
                    Align2::CENTER_TOP,
                    n.to_string(),
                    FontId::monospace(13.0),
                    Color32::from_gray(170),
                );
            }
            y += 64.0;
            p.text(
                pos2(rect.left() + 30.0, y),
                Align2::LEFT_TOP,
                "FOUND",
                FontId::monospace(26.0),
                TT_YELLOW,
            );
            y += 36.0;
            for m in &found {
                let s = format!(
                    "C{:<4} {:<7} {:>3} services   {}",
                    m.ch,
                    m.standard,
                    m.services.len(),
                    first_names(m)
                );
                p.text(
                    pos2(rect.left() + 30.0, y),
                    Align2::LEFT_TOP,
                    s,
                    FontId::monospace(24.0),
                    Color32::WHITE,
                );
                y += 32.0;
            }
            let foot_y = rect.bottom() - 26.0;
            let w = rect.width() / 4.0;
            for (k, (s, c)) in [
                (if running { "Stop" } else { "Scan" }, TT_RED),
                ("TV", TT_GREEN),
                ("Guide", TT_YELLOW),
                ("", TT_CYAN),
            ]
            .iter()
            .enumerate()
            {
                p.text(
                    pos2(rect.left() + w * (k as f32 + 0.5), foot_y),
                    Align2::CENTER_CENTER,
                    *s,
                    FontId::monospace(28.0),
                    *c,
                );
            }
        } else {
            y = rect.top() + 22.0;
            p.text(
                pos2(rect.left() + 26.0, y),
                Align2::LEFT_TOP,
                "Channel scan",
                look.font(30.0),
                look.text,
            );
            p.text(
                pos2(rect.left() + 250.0, y + 12.0),
                Align2::LEFT_TOP,
                &status,
                FontId::monospace(14.0),
                look.accent,
            );
            y += 56.0;
            // The band, as the expert window's band scan draws it.
            let strip = Rect::from_min_size(
                pos2(rect.left() + 26.0, y),
                vec2(rect.width() - 52.0, 120.0),
            );
            let cur = Some(self.channel);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(strip));
            if let Some(crate::band_scan::Action::Tune(n)) =
                self.band_scan.show(&mut child, 120.0, cur)
            {
                self.tune_channel(n);
            }
            y = strip.bottom() + 20.0;
            let table = Rect::from_min_size(
                pos2(rect.left() + 26.0, y),
                vec2(rect.width() * 0.62, 34.0 + 34.0 * found.len().max(1) as f32),
            );
            p.rect_filled(table, 0.0, look.panel);
            let cols = [0.0, 80.0, 190.0, 320.0];
            for (k, h) in ["CH", "MHz", "STANDARD", "SERVICES"].iter().enumerate() {
                p.text(
                    pos2(table.left() + 16.0 + cols[k], table.top() + 10.0),
                    Align2::LEFT_TOP,
                    *h,
                    FontId::monospace(12.0),
                    look.dim,
                );
            }
            for (i, m) in found.iter().enumerate() {
                let ry = table.top() + 34.0 + i as f32 * 34.0;
                p.line_segment(
                    [pos2(table.left(), ry), pos2(table.right(), ry)],
                    Stroke::new(1.0, look.line),
                );
                let vals = [
                    format!("C{}", m.ch),
                    format!("{:.3}", uhf_centre_hz(m.ch).unwrap_or(0.0) / 1e6),
                    m.standard.clone(),
                    format!("{}  ({})", m.services.len(), first_names(m)),
                ];
                for (k, v) in vals.iter().enumerate() {
                    let c = if k == 0 { look.accent } else { look.text };
                    let clip = p.with_clip_rect(Rect::from_min_max(
                        pos2(table.left(), ry),
                        pos2(table.right() - 10.0, ry + 34.0),
                    ));
                    clip.text(
                        pos2(table.left() + 16.0 + cols[k], ry + 9.0),
                        Align2::LEFT_TOP,
                        v,
                        if k < 2 {
                            FontId::monospace(14.0)
                        } else {
                            look.font(15.0)
                        },
                        c,
                    );
                }
            }
            if found.is_empty() {
                p.text(
                    pos2(table.left() + 16.0, table.top() + 44.0),
                    Align2::LEFT_TOP,
                    "nothing yet",
                    look.font(15.0),
                    look.dim,
                );
            }
            // The log.
            let lr = Rect::from_min_max(
                pos2(table.right() + 18.0, y),
                pos2(rect.right() - 26.0, (y + 260.0).min(rect.bottom() - 50.0)),
            );
            p.rect_filled(lr, 0.0, Color32::from_rgb(20, 20, 20));
            for (i, l) in log.iter().rev().take(10).rev().enumerate() {
                p.text(
                    pos2(lr.left() + 12.0, lr.top() + 10.0 + i as f32 * 20.0),
                    Align2::LEFT_TOP,
                    l,
                    FontId::monospace(12.0),
                    Color32::from_gray(160),
                );
            }
            let total = format!("found {} multiplexes · {services} services", found.len());
            p.text(
                pos2(rect.left() + 26.0, rect.bottom() - 28.0),
                Align2::LEFT_CENTER,
                total,
                FontId::monospace(13.0),
                look.dim,
            );
            p.text(
                pos2(rect.right() - 26.0, rect.bottom() - 28.0),
                Align2::RIGHT_CENTER,
                if running {
                    "Esc stop"
                } else {
                    "OK scan    Esc TV"
                },
                FontId::monospace(13.0),
                look.accent,
            );
        }
        let _: Pos2 = rect.min;
    }
}

impl App {
    /// The whole window in simple mode: the strip, then the screen.
    pub(crate) fn simple_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("simple_bar").show(ui, |ui| self.simple_bar(ui));
        let ctx = ui.ctx().clone();
        // Simple mode's keys first, then the TV's own work (the Red Button
        // takes what is left).
        self.simple_keys(&ctx);
        let report = std::sync::Arc::clone(&self.ts.report);
        let list = services::ordered(&report);
        match self.tv.show(&ctx, &list, self.ts.tap_dropped) {
            Some(crate::player::Request::Play(sid, audio, subs)) => self.play(sid, audio, subs),
            Some(crate::player::Request::Close) => self.tap = None,
            None => {}
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.simple_screen(ui));
    }
}

/// A multiplex's first few service names, for the scan's lists.
fn first_names(m: &Found) -> String {
    let mut v: Vec<&(Option<u16>, u16, String)> = m.services.iter().collect();
    v.sort_by_key(|s| s.0.unwrap_or(u16::MAX));
    let names: Vec<&str> = v.iter().take(4).map(|s| s.2.as_str()).collect();
    let more = if m.services.len() > 4 { ", …" } else { "" };
    format!("{}{more}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_hours() {
        assert_eq!(floor_half(1800 * 7 + 59), 1800 * 7);
        assert_eq!(floor_half(1800 * 7), 1800 * 7);
    }

    #[test]
    fn themes_by_name() {
        assert_eq!(Theme::parse("Teletext"), Some(Theme::Teletext));
        assert_eq!(Theme::parse("native"), Some(Theme::Native));
        assert_eq!(Theme::parse("x"), None);
        assert_eq!(Screen::parse("guide"), Some(Screen::Guide));
    }
}
