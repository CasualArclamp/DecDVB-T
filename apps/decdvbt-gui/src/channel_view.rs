// made by CasualArclamp
//! The channel panels, after DecDRM's: the fading waterfall and the
//! delay–Doppler map, from the sounder's channel estimates.
//!
//! - **Fading**: the channel's gain on every pilot carrier over the last
//!   15 s, a row about every 30 ms, newest at the top. Two paths of different
//!   delay cancel each other at frequencies 1/(difference) apart: the
//!   vertical stripes. A Doppler difference makes them move.
//! - **Delay–Doppler**: each propagation path is a spot at its delay (from
//!   the strongest) and Doppler shift; spread along the Doppler axis is the
//!   path's fading rate. Echoes beyond the guard interval (the right-hand
//!   line) interfere with the next symbol.

use std::sync::Arc;

use decdvbt_engine::SounderUpdate;
use decdvbt_rx::delay_doppler::{DelayDoppler, FLOOR_DB, FreqDoppler};
use decdvbt_rx::sounder::{Geometry, Status};
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Rect, RichText, Sense, Stroke,
    TextureHandle, TextureOptions, Ui, pos2,
};

use crate::errormaps::ErrorMaps;
use crate::waterfall::{History, ROWS, RingImage, palette};

/// The colour range starts this far below the median gain, dB.
const BELOW_MEDIAN_DB: f32 = 20.0;
/// At least this much range, dB.
const MIN_RANGE_DB: f32 = 25.0;
/// Seconds of a fading row (FADE_GROUP looked-at symbols).
const FADE_GROUP: f64 = 4.0;

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const PANEL_BG: Color32 = Color32::from_rgb(8, 8, 10);
const DIM: Color32 = Color32::from_gray(150);

/// The Doppler ranges the map can be shown over, Hz.
pub const DOPPLER_VIEWS: [f64; 4] = [5.0, 15.0, 30.0, 60.0];

/// The fading history: rows of gain per pilot, with colours anchored to the
/// median so the stripes stay in view as the level moves.
#[derive(Default)]
pub struct FadingMap {
    history: History,
    ring: RingImage,
    median: Option<f32>,
    peak: Option<f32>,
}

impl FadingMap {
    fn push(&mut self, row: Vec<f32>) {
        let mut sorted = row.clone();
        sorted.sort_by(f32::total_cmp);
        let q = |p: f32| sorted[((sorted.len() - 1) as f32 * p).round() as usize];
        let smooth = |old: Option<f32>, new: f32| Some(old.map_or(new, |o| o + 0.05 * (new - o)));
        self.median = smooth(self.median, q(0.5));
        self.peak = smooth(self.peak, q(0.99));
        let median = self.median.unwrap_or(0.0);
        let lo = median - BELOW_MEDIAN_DB;
        self.history.auto = false;
        self.history.manual = (
            lo,
            (self.peak.unwrap_or(median) + 3.0).max(lo + MIN_RANGE_DB),
        );
        self.history.push(row);
    }

    fn reset(&mut self) {
        self.history.reset();
        self.median = None;
        self.peak = None;
    }
}

pub struct ChannelView {
    fading: FadingMap,
    map: Option<Arc<DelayDoppler>>,
    map_tex: Option<TextureHandle>,
    freq_map: Option<Arc<FreqDoppler>>,
    freq_tex: Option<TextureHandle>,
    pub status: Status,
    /// Half the Doppler range shown, Hz.
    pub doppler_view: f64,
    /// Rows received, to know when a lock has changed the layout.
    columns: usize,
}

impl Default for ChannelView {
    fn default() -> Self {
        ChannelView {
            fading: FadingMap::default(),
            map: None,
            map_tex: None,
            freq_map: None,
            freq_tex: None,
            status: Status::default(),
            doppler_view: DOPPLER_VIEWS[1],
            columns: 0,
        }
    }
}

impl ChannelView {
    /// Forget the history (a new source, a retune).
    pub fn reset(&mut self) {
        self.fading.reset();
        self.map = None;
        self.map_tex = None;
        self.freq_map = None;
        self.freq_tex = None;
        self.status = Status::default();
        self.columns = 0;
    }

    pub fn update(&mut self, u: SounderUpdate) {
        self.status = u.status;
        for row in u.fading {
            // A different layout (a new lock on another mode) starts afresh.
            if row.len() != self.columns {
                self.fading.reset();
                self.columns = row.len();
            }
            self.fading.push(row);
        }
        if let Some(m) = u.map {
            self.map = Some(m);
            self.map_tex = None; // rebuilt on the next draw
        }
        if let Some(m) = u.freq_map {
            self.freq_map = Some(m);
            self.freq_tex = None;
        }
    }

    /// The header: what the sounder is doing.
    fn header(&self, ui: &mut Ui) {
        let s = &self.status;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Channel").strong());
            match (s.tracking, s.geometry) {
                (true, Some(g)) => {
                    ui.label(
                        RichText::new(format!(
                            "locked · {} {} guard · carrier {:+.2} kHz · clock {:+.1} ppm · \
                             coherence {:.2}",
                            g.fft_name,
                            g.guard_name,
                            s.cfo_hz / 1e3,
                            s.clock_ppm,
                            s.coherence
                        ))
                        .color(Color32::from_rgb(110, 220, 130)),
                    );
                }
                _ if s.attempts > 4 => {
                    ui.label(
                        RichText::new(format!(
                            "no DVB-T here (best cyclic-prefix contrast {:.1}, a lock needs 5): \
                             too weak, or DVB-T2 (sounded by its receiver: turn decoding on)",
                            s.contrast
                        ))
                        .color(AMBER),
                    );
                }
                _ => {
                    ui.label(RichText::new("searching for a signal…").weak());
                }
            }
        });
    }

    /// The maps that are not about time: paths by delay and Doppler, the
    /// Doppler spectrum at each carrier, and what is wrong with the cells.
    pub fn show_maps(&mut self, ui: &mut Ui, errors: &mut ErrorMaps) {
        self.header(ui);
        let avail = ui.available_size();
        let gap = 10.0;
        let height = (avail.y - 4.0).max(60.0);
        // The two Doppler maps are wide; the two constellation ones squarish.
        let wide = ((avail.x - 3.0 * gap) * 0.3).max(100.0);
        let sq = ((avail.x - 3.0 * gap) * 0.2).max(100.0);
        let g = self.status.geometry.filter(|_| self.status.tracking);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui(egui::vec2(wide, height), |ui| self.map_plot(ui, g));
            ui.allocate_ui(egui::vec2(wide, height), |ui| self.freq_plot(ui, g));
            ui.allocate_ui(egui::vec2(sq, height), |ui| {
                errors.cloud(ui, ui.available_size());
            });
            ui.allocate_ui(egui::vec2(sq, height), |ui| {
                errors.points(ui, ui.available_size());
            });
        });
    }

    pub fn show(&mut self, ui: &mut Ui) {
        self.header(ui);
        let avail = ui.available_size();
        let gap = 14.0;
        let half = ((avail.x - gap) / 2.0).max(100.0);
        let height = (avail.y - 4.0).max(60.0);
        let g = self.status.geometry.filter(|_| self.status.tracking);
        ui.horizontal(|ui| {
            ui.allocate_ui(egui::vec2(half, height), |ui| self.fading_plot(ui, g));
            ui.add_space(gap);
            ui.allocate_ui(egui::vec2(half, height), |ui| self.map_plot(ui, g));
        });
    }

    fn fading_plot(&mut self, ui: &mut Ui, g: Option<Geometry>) {
        let (resp, p) = ui.allocate_painter(ui.available_size(), Sense::hover());
        let rect = resp.rect;
        p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
        let plot = Rect::from_min_max(
            pos2(rect.left() + 44.0, rect.top() + 18.0),
            pos2(rect.right() - 6.0, rect.bottom() - 16.0),
        );
        let font = FontId::monospace(11.0);
        p.text(
            pos2(rect.left() + 4.0, rect.top() + 1.0),
            Align2::LEFT_TOP,
            "gain per carrier, last 15 s",
            font.clone(),
            DIM,
        );
        let (Some(g), Some((tex, uv))) =
            (g, self.fading.ring.update(ui.ctx(), &self.fading.history))
        else {
            p.text(
                plot.center(),
                Align2::CENTER_CENTER,
                "waiting for a lock",
                font,
                DIM,
            );
            return;
        };
        p.image(tex, plot, uv, Color32::WHITE);
        // Frequency across: pilots span the carriers from the first to the
        // last; ticks at whole MHz.
        let hz0 = g.column_hz(0);
        let hz1 = g.column_hz(g.columns - 1);
        let x_of = |hz: f64| plot.left() + ((hz - hz0) / (hz1 - hz0)) as f32 * plot.width();
        let mut mhz = (hz0 / 1e6).ceil();
        while mhz * 1e6 <= hz1 {
            let x = x_of(mhz * 1e6);
            p.line_segment(
                [pos2(x, plot.bottom()), pos2(x, plot.bottom() + 3.0)],
                Stroke::new(1.0, DIM),
            );
            p.text(
                pos2(x, plot.bottom() + 3.0),
                Align2::CENTER_TOP,
                format!("{mhz:+.0}"),
                font.clone(),
                DIM,
            );
            mhz += 1.0;
        }
        p.text(
            pos2(plot.right(), plot.bottom() + 3.0),
            Align2::RIGHT_TOP,
            "MHz",
            font.clone(),
            DIM,
        );
        // Time down: newest at the top.
        let span = ROWS as f64 * FADE_GROUP * g.row_s;
        let mut t = 0.0;
        while t <= span {
            let y = plot.top() + (t / span) as f32 * plot.height();
            p.line_segment(
                [pos2(plot.left() - 3.0, y), pos2(plot.left(), y)],
                Stroke::new(1.0, DIM),
            );
            p.text(
                pos2(plot.left() - 5.0, y),
                Align2::RIGHT_CENTER,
                if t == 0.0 {
                    "now".into()
                } else {
                    format!("-{t:.0} s")
                },
                font.clone(),
                DIM,
            );
            t += 5.0;
        }
        p.rect_stroke(
            plot,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(70)),
            egui::StrokeKind::Outside,
        );
    }

    fn map_plot(&mut self, ui: &mut Ui, g: Option<Geometry>) {
        let (resp, p) = ui.allocate_painter(ui.available_size(), Sense::hover());
        let rect = resp.rect;
        p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
        let font = FontId::monospace(11.0);
        let plot = Rect::from_min_max(
            pos2(rect.left() + 48.0, rect.top() + 18.0),
            pos2(rect.right() - 6.0, rect.bottom() - 16.0),
        );
        p.text(
            pos2(rect.left() + 4.0, rect.top() + 1.0),
            Align2::LEFT_TOP,
            "paths: delay and Doppler shift",
            font.clone(),
            DIM,
        );
        let (Some(m), Some(_g)) = (self.map.clone(), g) else {
            p.text(
                plot.center(),
                Align2::CENTER_CENTER,
                "the map appears a few seconds after the lock",
                font,
                DIM,
            );
            return;
        };
        let tex = self.map_tex.get_or_insert_with(|| {
            ui.ctx()
                .load_texture("delay_doppler", map_image(&m), TextureOptions::LINEAR)
        });
        // The part of the Doppler axis shown.
        let (d0, d1) = (m.doppler_hz(0), m.doppler_hz(m.dopplers - 1));
        let view = self.doppler_view.min(d1).max(1.0);
        let v = |d: f64| ((d1 - d) / (d1 - d0)) as f32;
        let uv = Rect::from_min_max(pos2(0.0, v(view)), pos2(1.0, v(-view)));
        p.image(tex.id(), plot, uv, Color32::WHITE);

        let (x0, x1) = (m.delay_us(0), m.delay_us(m.delays - 1));
        let x_of = |us: f64| plot.left() + ((us - x0) / (x1 - x0)) as f32 * plot.width();
        let y_of = |hz: f64| plot.top() + ((view - hz) / (2.0 * view)) as f32 * plot.height();
        // Delay ticks every 5 us.
        let mut us = (x0 / 5.0).ceil() * 5.0;
        while us <= x1 {
            let x = x_of(us);
            p.line_segment(
                [pos2(x, plot.bottom()), pos2(x, plot.bottom() + 3.0)],
                Stroke::new(1.0, DIM),
            );
            p.text(
                pos2(x, plot.bottom() + 3.0),
                Align2::CENTER_TOP,
                format!("{us:.0}"),
                font.clone(),
                DIM,
            );
            us += 5.0;
        }
        p.text(
            pos2(plot.right(), plot.bottom() + 3.0),
            Align2::RIGHT_TOP,
            "us",
            font.clone(),
            DIM,
        );
        doppler_axis(&p, rect, plot, view, &font);
        // The guard interval: later echoes interfere with the next symbol.
        for us in [0.0, m.guard_us] {
            let x = x_of(us);
            if (plot.left()..=plot.right()).contains(&x) {
                p.line_segment(
                    [pos2(x, plot.top()), pos2(x, plot.bottom())],
                    Stroke::new(
                        1.0,
                        AMBER.gamma_multiply(if us == 0.0 { 0.45 } else { 0.9 }),
                    ),
                );
            }
        }
        let y0 = y_of(0.0);
        p.line_segment(
            [pos2(plot.left(), y0), pos2(plot.right(), y0)],
            Stroke::new(1.0, Color32::from_white_alpha(40)),
        );
        p.rect_stroke(
            plot,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(70)),
            egui::StrokeKind::Outside,
        );
        p.text(
            pos2(x_of(m.guard_us) - 3.0, plot.top() + 2.0),
            Align2::RIGHT_TOP,
            "guard",
            font.clone(),
            AMBER,
        );

        // What is under the pointer.
        if let Some(pt) = resp.hover_pos()
            && plot.contains(pt)
        {
            let us = x0 + ((pt.x - plot.left()) / plot.width()) as f64 * (x1 - x0);
            let hz = view - ((pt.y - plot.top()) / plot.height()) as f64 * 2.0 * view;
            let c = ((us - x0) / m.delay_step_us)
                .round()
                .clamp(0.0, (m.delays - 1) as f64) as usize;
            let r = ((hz - d0) / m.doppler_step_hz)
                .round()
                .clamp(0.0, (m.dopplers - 1) as f64) as usize;
            p.text(
                pos2(plot.right() - 4.0, plot.bottom() - 4.0),
                Align2::RIGHT_BOTTOM,
                format!("{us:+.2} us  {hz:+.1} Hz  {:.0} dB", m.at(r, c)),
                FontId::monospace(12.0),
                Color32::WHITE,
            );
        }
    }

    /// The Doppler spectrum at each carrier: frequency across, Doppler shift
    /// up. Two paths that move differently swell and shrink against each
    /// other from carrier to carrier; a fast fade that is only at some
    /// carriers (a narrow echo cluster) shows as patches, one that is
    /// everywhere as a flat band.
    fn freq_plot(&mut self, ui: &mut Ui, g: Option<Geometry>) {
        let (resp, p) = ui.allocate_painter(ui.available_size(), Sense::hover());
        let rect = resp.rect;
        p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
        let font = FontId::monospace(11.0);
        let plot = Rect::from_min_max(
            pos2(rect.left() + 48.0, rect.top() + 18.0),
            pos2(rect.right() - 6.0, rect.bottom() - 16.0),
        );
        p.text(
            pos2(rect.left() + 4.0, rect.top() + 1.0),
            Align2::LEFT_TOP,
            "fading by carrier: frequency and Doppler shift",
            font.clone(),
            DIM,
        );
        let (Some(m), Some(_g)) = (self.freq_map.clone(), g) else {
            p.text(
                plot.center(),
                Align2::CENTER_CENTER,
                "the map appears a few seconds after the lock",
                font,
                DIM,
            );
            return;
        };
        let tex = self.freq_tex.get_or_insert_with(|| {
            ui.ctx()
                .load_texture("freq_doppler", freq_image(&m), TextureOptions::LINEAR)
        });
        let (d0, d1) = (m.doppler_start_hz, -m.doppler_start_hz);
        let view = self.doppler_view.min(d1).max(1.0);
        let v = |d: f64| ((d1 - d) / (d1 - d0)) as f32;
        let uv = Rect::from_min_max(pos2(0.0, v(view)), pos2(1.0, v(-view)));
        p.image(tex.id(), plot, uv, Color32::WHITE);

        let (x0, x1) = (m.freq_mhz(0), m.freq_mhz(m.cols - 1));
        let x_of = |mhz: f64| plot.left() + ((mhz - x0) / (x1 - x0)) as f32 * plot.width();
        let mut mhz = x0.ceil();
        while mhz <= x1 {
            let x = x_of(mhz);
            p.line_segment(
                [pos2(x, plot.bottom()), pos2(x, plot.bottom() + 3.0)],
                Stroke::new(1.0, DIM),
            );
            p.text(
                pos2(x, plot.bottom() + 3.0),
                Align2::CENTER_TOP,
                format!("{mhz:+.0}"),
                font.clone(),
                DIM,
            );
            mhz += 1.0;
        }
        p.text(
            pos2(plot.right(), plot.bottom() + 3.0),
            Align2::RIGHT_TOP,
            "MHz",
            font.clone(),
            DIM,
        );
        doppler_axis(&p, rect, plot, view, &font);
        let y0 = plot.top() + (plot.height() / 2.0);
        p.line_segment(
            [pos2(plot.left(), y0), pos2(plot.right(), y0)],
            Stroke::new(1.0, Color32::from_white_alpha(40)),
        );
        p.rect_stroke(
            plot,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(70)),
            egui::StrokeKind::Outside,
        );
        if let Some(pt) = resp.hover_pos()
            && plot.contains(pt)
        {
            let mhz = x0 + ((pt.x - plot.left()) / plot.width()) as f64 * (x1 - x0);
            let hz = view - ((pt.y - plot.top()) / plot.height()) as f64 * 2.0 * view;
            let c = ((mhz - x0) / m.freq_step_mhz)
                .round()
                .clamp(0.0, (m.cols - 1) as f64) as usize;
            let r = ((hz - d0) / m.doppler_step_hz)
                .round()
                .clamp(0.0, (m.dopplers - 1) as f64) as usize;
            p.text(
                pos2(plot.right() - 4.0, plot.bottom() - 4.0),
                Align2::RIGHT_BOTTOM,
                format!("{mhz:+.2} MHz  {hz:+.1} Hz  {:.0} dB", m.at(r, c)),
                FontId::monospace(12.0),
                Color32::WHITE,
            );
        }
    }
}

/// The Doppler axis down the left of a plot: ticks at a step that suits the
/// range shown, and the unit under them.
fn doppler_axis(p: &egui::Painter, rect: Rect, plot: Rect, view: f64, font: &FontId) {
    let step = if view <= 6.0 {
        2.0
    } else if view <= 20.0 {
        5.0
    } else {
        10.0
    };
    let y_of = |hz: f64| plot.top() + ((view - hz) / (2.0 * view)) as f32 * plot.height();
    let mut hz = (-view / step).ceil() * step;
    while hz <= view {
        let y = y_of(hz);
        p.line_segment(
            [pos2(plot.left() - 3.0, y), pos2(plot.left(), y)],
            Stroke::new(1.0, DIM),
        );
        p.text(
            pos2(plot.left() - 5.0, y),
            Align2::RIGHT_CENTER,
            format!("{hz:+.0}"),
            font.clone(),
            DIM,
        );
        hz += step;
    }
    p.text(
        pos2(rect.left() + 4.0, plot.bottom() + 3.0),
        Align2::LEFT_TOP,
        "Hz",
        font.clone(),
        DIM,
    );
}

/// The frequency–Doppler map as an image: carriers across, the highest Doppler
/// shift at the top.
fn freq_image(m: &FreqDoppler) -> ColorImage {
    let lut = palette();
    let scale = (lut.len() - 1) as f32 / -FLOOR_DB;
    let mut pixels = Vec::with_capacity(m.cols * m.dopplers);
    for r in (0..m.dopplers).rev() {
        for c in 0..m.cols {
            let i = ((m.at(r, c) - FLOOR_DB) * scale).clamp(0.0, (lut.len() - 1) as f32);
            pixels.push(lut[i as usize]);
        }
    }
    ColorImage::new([m.cols, m.dopplers], pixels)
}

/// The map as an image: delays across, the highest Doppler shift at the top;
/// colours from the floor to the strongest path.
fn map_image(m: &DelayDoppler) -> ColorImage {
    let lut = palette();
    let scale = (lut.len() - 1) as f32 / -FLOOR_DB;
    let mut pixels = Vec::with_capacity(m.delays * m.dopplers);
    for r in (0..m.dopplers).rev() {
        for c in 0..m.delays {
            let i = ((m.at(r, c) - FLOOR_DB) * scale).clamp(0.0, (lut.len() - 1) as f32);
            pixels.push(lut[i as usize]);
        }
    }
    ColorImage::new([m.delays, m.dopplers], pixels)
}
