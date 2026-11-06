// made by CasualArclamp
//! The radar panel: the passive radar's range–Doppler map (see
//! `decdvbt_rx::radar`), Doppler across and bistatic range up, as in the FM
//! and DAB passive radars it follows; colour is dB above the noise floor.
//! Beside it, the settings and the strongest echoes off the clutter ridge,
//! with their bistatic speed. Ported from DecDAB's (same author). From a
//! DVB-T lock only: a DVB-T2 signal's rotated constellations cannot be
//! decided cell by cell.
//!
//! Trails: points that rose [`TRAIL_DB`] over the floor stay on the map,
//! fading, for the time chosen, so a moving echo draws its track. Only
//! those points are kept (a max-hold of everything would raise the noise
//! as well).

use std::sync::Arc;

use decdvbt_engine::RadarAsk;
use decdvbt_rx::radar::{Echo, RangeDoppler, echoes};
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Rect, RichText, Sense, Stroke,
    TextureHandle, TextureOptions, Ui, pos2, vec2,
};

use crate::waterfall::palette;

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const GREEN: Color32 = Color32::from_rgb(110, 220, 130);
const PANEL_BG: Color32 = Color32::from_rgb(8, 8, 10);
const DIM: Color32 = Color32::from_gray(150);

/// Half the Doppler axis shown, Hz (the last: all there is, ±541 Hz in
/// 8K 1/32).
pub const DOPPLER_VIEWS: [f64; 4] = [50.0, 100.0, 200.0, 600.0];
/// The range shown, km (the maps are made this far out and no further).
pub const RANGE_VIEWS: [f64; 4] = [25.0, 50.0, 100.0, 150.0];
/// Coherent intervals, s (each made the nearest power of two of symbols).
pub const CPI_CHOICES: [f64; 3] = [0.5, 1.0, 2.0];
/// The top of the colour scale, dB over the floor.
pub const COLOUR_TOPS: [f32; 2] = [15.0, 25.0];
/// How long trails last, s (0: none).
pub const TRAILS: [f64; 3] = [0.0, 10.0, 60.0];
/// Points this far over the floor leave a trail.
pub const TRAIL_DB: f32 = 12.0;
/// Echoes listed and marked; and how far off 0 Hz they must be.
const ECHOES: usize = 6;
const RIDGE_HZ: f64 = 15.0;
// The strongest noise cell of a map (some 800 000 of them) is about 13 dB
// over the median.
const ECHO_DB: f32 = 16.0;
const C: f64 = 299_792_458.0;

pub struct RadarView {
    map: Option<Arc<RangeDoppler>>,
    tex: Option<TextureHandle>,
    /// Trail levels per map cell (dB over the floor, fading), or empty.
    trail: Vec<f32>,
    trail_dims: (usize, usize),
    echoes: Vec<Echo>,
    pub doppler_view: f64,
    pub range_view: f64,
    pub cpi: f64,
    pub colour_top: f32,
    pub trails_s: f64,
}

impl Default for RadarView {
    fn default() -> Self {
        RadarView {
            map: None,
            tex: None,
            trail: Vec::new(),
            trail_dims: (0, 0),
            echoes: Vec::new(),
            doppler_view: DOPPLER_VIEWS[2],
            range_view: RANGE_VIEWS[2],
            cpi: CPI_CHOICES[1],
            colour_top: COLOUR_TOPS[0],
            trails_s: TRAILS[0],
        }
    }
}

impl RadarView {
    /// Forget the maps (a new source, a retune, the panel closed).
    pub fn reset(&mut self) {
        self.map = None;
        self.tex = None;
        self.trail.clear();
        self.echoes.clear();
    }

    /// What the engine is to be asked for.
    pub fn ask(&self) -> RadarAsk {
        RadarAsk {
            cpi_s: self.cpi,
            max_km: self.range_view,
        }
    }

    /// Take a new map, if there is one.
    pub fn update(&mut self, map: Option<Arc<RangeDoppler>>) {
        let Some(m) = map else { return };
        // The trails: each cell fades by the colour range over the trail's
        // time, and is lifted again by a new point over TRAIL_DB.
        if self.trails_s > 0.0 {
            let dims = (m.ranges, m.dopplers);
            if self.trail_dims != dims {
                self.trail = vec![f32::MIN; m.db.len()];
                self.trail_dims = dims;
            }
            let maps = (self.trails_s / (m.cpi_s / 2.0)).max(1.0);
            let fade = (self.colour_top / maps as f32).max(0.01);
            for (t, &v) in self.trail.iter_mut().zip(&m.db) {
                let held = if v >= TRAIL_DB { t.max(v) } else { *t };
                *t = held - fade;
            }
        } else if !self.trail.is_empty() {
            self.trail.clear();
            self.trail_dims = (0, 0);
        }
        self.echoes = echoes(&m, RIDGE_HZ, self.range_view, ECHO_DB, ECHOES);
        self.map = Some(m);
        self.tex = None; // rebuilt on the next draw
    }

    /// `lock`: `Ok` while a DVB-T lock is decoding, `Err` with why there is
    /// no map otherwise; `rf_hz`: the multiplex's frequency (for the echoes'
    /// speeds).
    pub fn show(&mut self, ui: &mut Ui, lock: Result<(), &str>, rf_hz: Option<f64>) {
        let locked = lock.is_ok();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Radar").strong());
            match (lock, &self.map) {
                (Ok(()), Some(m)) => ui.label(RichText::new(format!(
                    "reference: the decided signal · CPI {:.2} s, a map every {:.2} s · floor {:.0} dB under the direct path · strongest echo {:.0} dB over it",
                    m.cpi_s, m.cpi_s / 2.0, m.floor_db, m.max_snr_db
                )).color(GREEN)),
                (Ok(()), None) => ui.label(RichText::new(format!(
                    "filling the first coherent interval ({:.1} s)",
                    self.cpi
                )).color(GREEN)),
                (Err(why), _) => ui.label(RichText::new(why).weak()),
            };
        });
        let avail = ui.available_size();
        let side = 330.0;
        let height = (avail.y - 4.0).max(60.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.allocate_ui(vec2((avail.x - side - 12.0).max(200.0), height), |ui| {
                self.map_plot(ui, locked, rf_hz)
            });
            ui.allocate_ui(vec2(side, height), |ui| {
                ui.vertical(|ui| self.side(ui, rf_hz));
            });
        });
    }

    /// The settings and the echo list.
    fn side(&mut self, ui: &mut Ui, rf_hz: Option<f64>) {
        let row = |ui: &mut Ui, name: &str, f: &mut dyn FnMut(&mut Ui)| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [60.0, 18.0],
                    egui::Label::new(RichText::new(name).color(DIM)),
                );
                f(ui);
            });
        };
        row(ui, "Doppler", &mut |ui| {
            for d in DOPPLER_VIEWS {
                let label = if d >= 541.0 {
                    "all".to_string()
                } else {
                    format!("±{d:.0}")
                };
                ui.selectable_value(&mut self.doppler_view, d, label);
            }
        });
        let mut range_changed = false;
        row(ui, "range", &mut |ui| {
            for r in RANGE_VIEWS {
                range_changed |= ui
                    .selectable_value(&mut self.range_view, r, format!("{r:.0} km"))
                    .changed();
            }
        });
        row(ui, "CPI", &mut |ui| {
            for c in CPI_CHOICES {
                ui.selectable_value(&mut self.cpi, c, format!("{c:.1} s"))
                    .on_hover_text("the coherent interval: longer sees weaker, steadier echoes, finer in Doppler");
            }
        });
        let mut look_changed = false;
        row(ui, "colour", &mut |ui| {
            for t in COLOUR_TOPS {
                look_changed |= ui
                    .selectable_value(&mut self.colour_top, t, format!("0–{t:.0} dB"))
                    .changed();
            }
        });
        row(ui, "trails", &mut |ui| {
            for t in TRAILS {
                let label = if t == 0.0 {
                    "off".into()
                } else {
                    format!("{t:.0} s")
                };
                look_changed |= ui
                    .selectable_value(&mut self.trails_s, t, label)
                    .on_hover_text(
                        "keep moving echoes on the map a while, fading, to see their tracks",
                    )
                    .changed();
            }
        });
        if look_changed {
            self.tex = None;
        }
        if range_changed && let Some(m) = &self.map {
            self.echoes = echoes(m, RIDGE_HZ, self.range_view, ECHO_DB, ECHOES);
        }
        ui.add_space(6.0);
        ui.label(RichText::new(format!("echoes beyond ±{RIDGE_HZ:.0} Hz")).color(DIM));
        let font = FontId::monospace(12.0);
        ui.label(
            RichText::new("    range    Doppler    speed    level")
                .font(font.clone())
                .color(DIM),
        );
        if self.echoes.is_empty() {
            ui.label(RichText::new("    none").font(font.clone()).color(DIM));
        }
        for (i, e) in self.echoes.iter().enumerate() {
            let speed = rf_hz.map_or(String::from("     "), |f| {
                format!("{:+5.0}", -e.doppler_hz * C / f)
            });
            ui.label(
                RichText::new(format!(
                    "{} {:6.1} km {:+6.1} Hz {} m/s {:4.1} dB",
                    letter(i),
                    e.range_km,
                    e.doppler_hz,
                    speed,
                    e.db
                ))
                .font(font.clone())
                .color(AMBER),
            );
        }
    }

    fn map_plot(&mut self, ui: &mut Ui, locked: bool, rf_hz: Option<f64>) {
        let (resp, p) = ui.allocate_painter(ui.available_size(), Sense::hover());
        let rect = resp.rect;
        p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
        let font = FontId::monospace(11.0);
        let plot = Rect::from_min_max(
            pos2(rect.left() + 40.0, rect.top() + 16.0),
            pos2(rect.right() - 70.0, rect.bottom() - 18.0),
        );
        let (Some(m), true) = (self.map.clone(), locked) else {
            p.text(
                plot.center(),
                Align2::CENTER_CENTER,
                "the map appears a second or two after the lock",
                font,
                DIM,
            );
            return;
        };
        // The cells shown: Doppler within the view, ranges up to it.
        let view = self.doppler_view.min(-m.doppler_start_hz);
        let d_of = |hz: f64| ((hz - m.doppler_start_hz) / m.doppler_step_hz).round() as isize;
        let (da, db) = (
            d_of(-view).max(0) as usize,
            (d_of(view) as usize).min(m.dopplers - 1),
        );
        let rows = ((self.range_view / m.range_step_km).ceil() as usize).clamp(1, m.ranges);
        let trail = (!self.trail.is_empty()).then_some(&self.trail);
        let top = self.colour_top;
        let tex = self.tex.get_or_insert_with(|| {
            ui.ctx().load_texture(
                "radar",
                map_image(&m, trail.map(|t| t.as_slice()), (da, db), rows, top),
                TextureOptions::LINEAR,
            )
        });
        p.image(
            tex.id(),
            plot,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        // Axes: Doppler across, range up.
        let (hz0, hz1) = (m.doppler_hz(da), m.doppler_hz(db));
        let km1 = rows as f64 * m.range_step_km;
        let x_of = |hz: f64| plot.left() + ((hz - hz0) / (hz1 - hz0)) as f32 * plot.width();
        let y_of = |km: f64| plot.bottom() - (km / km1) as f32 * plot.height();
        let hz_step = match view {
            v if v <= 60.0 => 10.0,
            v if v <= 120.0 => 25.0,
            v if v <= 250.0 => 50.0,
            _ => 100.0,
        };
        let mut hz = (hz0 / hz_step).ceil() * hz_step;
        while hz <= hz1 {
            let x = x_of(hz);
            p.line_segment(
                [pos2(x, plot.bottom()), pos2(x, plot.bottom() + 3.0)],
                Stroke::new(1.0, DIM),
            );
            p.text(
                pos2(x, plot.bottom() + 3.0),
                Align2::CENTER_TOP,
                format!("{hz:+.0}"),
                font.clone(),
                DIM,
            );
            hz += hz_step;
        }
        p.text(
            pos2(rect.left() + 4.0, plot.bottom() + 3.0),
            Align2::LEFT_TOP,
            "Hz",
            font.clone(),
            DIM,
        );
        let km_step = match self.range_view {
            r if r <= 30.0 => 5.0,
            r if r <= 60.0 => 10.0,
            _ => 25.0,
        };
        let mut km = 0.0;
        while km <= km1 {
            let y = y_of(km);
            p.line_segment(
                [pos2(plot.left() - 3.0, y), pos2(plot.left(), y)],
                Stroke::new(1.0, DIM),
            );
            p.text(
                pos2(plot.left() - 5.0, y),
                Align2::RIGHT_CENTER,
                format!("{km:.0}"),
                font.clone(),
                DIM,
            );
            km += km_step;
        }
        p.text(
            pos2(rect.left() + 4.0, rect.top() + 1.0),
            Align2::LEFT_TOP,
            "km",
            font.clone(),
            DIM,
        );
        // 0 Hz: the direct signal's frequency.
        let x0 = x_of(0.0);
        p.line_segment(
            [pos2(x0, plot.top()), pos2(x0, plot.bottom())],
            Stroke::new(1.0, Color32::from_white_alpha(30)),
        );
        p.rect_stroke(
            plot,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(70)),
            egui::StrokeKind::Outside,
        );
        // The echoes, marked.
        for (i, e) in self.echoes.iter().enumerate() {
            if e.doppler_hz.abs() > view || e.range_km > km1 {
                continue;
            }
            let c = pos2(x_of(e.doppler_hz), y_of(e.range_km));
            p.circle_stroke(c, 7.0, Stroke::new(1.5, AMBER));
            p.text(
                c + vec2(9.0, -8.0),
                Align2::LEFT_TOP,
                letter(i),
                font.clone(),
                AMBER,
            );
        }
        // The colour scale.
        let bar = Rect::from_min_max(
            pos2(plot.right() + 10.0, plot.top()),
            pos2(plot.right() + 20.0, plot.bottom()),
        );
        let lut = palette();
        let n = 64;
        for i in 0..n {
            let y0 = bar.bottom() - bar.height() * i as f32 / n as f32;
            let y1 = bar.bottom() - bar.height() * (i + 1) as f32 / n as f32;
            p.rect_filled(
                Rect::from_min_max(pos2(bar.left(), y1), pos2(bar.right(), y0)),
                CornerRadius::ZERO,
                lut[i * (lut.len() - 1) / (n - 1)],
            );
        }
        for v in [0.0, top / 3.0, 2.0 * top / 3.0, top] {
            let y = bar.bottom() - bar.height() * v / top;
            p.text(
                pos2(bar.right() + 3.0, y),
                Align2::LEFT_CENTER,
                format!("{v:.0}"),
                font.clone(),
                DIM,
            );
        }
        p.text(
            pos2(bar.left(), plot.bottom() + 3.0),
            Align2::LEFT_TOP,
            "dB",
            font.clone(),
            DIM,
        );
        // What is under the pointer.
        if let Some(pt) = resp.hover_pos()
            && plot.contains(pt)
        {
            let hz = hz0 + ((pt.x - plot.left()) / plot.width()) as f64 * (hz1 - hz0);
            let km = ((plot.bottom() - pt.y) / plot.height()) as f64 * km1;
            let d = d_of(hz).clamp(0, m.dopplers as isize - 1) as usize;
            let r = ((km / m.range_step_km) as usize).min(m.ranges - 1);
            let speed = rf_hz.map_or(String::new(), |f| format!("  {:+.0} m/s", -hz * C / f));
            p.text(
                pos2(plot.right() - 4.0, plot.top() + 4.0),
                Align2::RIGHT_TOP,
                format!("{km:.2} km  {hz:+.1} Hz{speed}  {:.1} dB", m.at(r, d)),
                FontId::monospace(12.0),
                Color32::WHITE,
            );
        }
    }
}

/// A, B, C … for the echoes.
fn letter(i: usize) -> String {
    char::from(b'A' + (i % 26) as u8).to_string()
}

/// The shown part of the map as an image: Doppler cells `ds` across, the
/// first `rows` ranges up (the nearest at the bottom); colours from the
/// floor (0 dB) to `top`; trails, where brighter, over the map.
fn map_image(
    m: &RangeDoppler,
    trail: Option<&[f32]>,
    ds: (usize, usize),
    rows: usize,
    top: f32,
) -> ColorImage {
    let lut = palette();
    let scale = (lut.len() - 1) as f32 / top;
    let w = ds.1 - ds.0 + 1;
    let mut pixels = Vec::with_capacity(w * rows);
    for r in (0..rows).rev() {
        for d in ds.0..=ds.1 {
            let i = r * m.dopplers + d;
            let mut v = m.db[i];
            if let Some(t) = trail {
                v = v.max(t[i]);
            }
            pixels.push(lut[(v * scale).clamp(0.0, (lut.len() - 1) as f32) as usize]);
        }
    }
    ColorImage::new([w, rows], pixels)
}
