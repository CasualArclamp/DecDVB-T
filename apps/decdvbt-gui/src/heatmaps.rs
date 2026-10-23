// made by CasualArclamp
//! Heat maps of the channel over time, from the receiver's own estimate (no
//! sounder needed): rows made 20 or 60 times a second, newest at the top.
//!
//! - **Gain**: |H|² at each carrier, against the band's mean. Two paths cancel
//!   each other at carriers 1/(delay difference) apart: stripes, that slide
//!   when an echo moves.
//! - **SNR**: the signal-to-noise ratio at each carrier, from how far the
//!   pilots stray from their estimate. Narrow-band interference (a spur, a
//!   neighbouring service leaking in, a mobile's burst) is a bright-to-dark
//!   stripe that stays where the interferer is.
//! - **Echoes**: the channel's impulse response, delay across, against the
//!   strongest path; an echo is a line at its delay, a moving one a slanting
//!   line, one past the guard interval (the right-hand marker) interferes with
//!   the next symbol.

use std::sync::Arc;

use decdvbt_rx::scope::Measurements;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Rect, Sense, Stroke, Ui, pos2, vec2,
};

use crate::waterfall::{History, RingImage};

const PANEL_BG: Color32 = Color32::from_rgb(8, 8, 10);
const DIM: Color32 = Color32::from_gray(150);
const AMBER: Color32 = Color32::from_rgb(255, 176, 40);

/// Colour ranges, dB: the gain against the band's mean, the signal-to-noise
/// ratio, and the impulse response against its strongest tap.
const GAIN_RANGE: (f32, f32) = (-24.0, 6.0);
const SNR_RANGE: (f32, f32) = (0.0, 34.0);
const ECHO_RANGE: (f32, f32) = (-60.0, 0.0);

struct Map {
    history: History,
    ring: RingImage,
}

impl Map {
    fn new(range: (f32, f32)) -> Map {
        let mut history = History::default();
        (history.auto, history.manual, history.track) = (false, range, false);
        Map {
            history,
            ring: RingImage::default(),
        }
    }
}

pub struct HeatView {
    gain: Map,
    snr: Map,
    echo: Map,
    /// The last row's delay axis and guard interval, for the labels.
    last: Option<Arc<Measurements>>,
}

impl Default for HeatView {
    fn default() -> Self {
        HeatView {
            gain: Map::new(GAIN_RANGE),
            snr: Map::new(SNR_RANGE),
            echo: Map::new(ECHO_RANGE),
            last: None,
        }
    }
}

impl HeatView {
    pub fn reset(&mut self) {
        *self = HeatView::default();
    }

    /// Rows more than the maps hold are not worth drawing: a slow frame's
    /// worth at most is kept.
    pub fn update(&mut self, rows: Vec<Arc<Measurements>>) {
        for r in rows {
            self.gain.history.push(r.gain_db.clone());
            self.snr.history.push(r.snr_db.clone());
            self.echo.history.push(r.impulse_db.clone());
            self.last = Some(r);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, rows_per_s: f32) {
        // Twenty-five seconds of rows, whatever the rate.
        let rows = (25.0 * rows_per_s) as usize;
        for m in [&mut self.gain, &mut self.snr, &mut self.echo] {
            m.history.set_capacity(rows);
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Channel heat maps").strong());
            match &self.last {
                None => {
                    ui.label(egui::RichText::new("waiting for a lock").weak());
                }
                Some(m) => {
                    ui.label(
                        egui::RichText::new(format!(
                            "mean SNR {:.1} dB · {} echoes · guard {:.0} µs · {rows_per_s:.1} rows a second",
                            m.mean_snr_db,
                            m.echoes.len(),
                            m.guard_us
                        ))
                        .color(Color32::from_rgb(110, 220, 130)),
                    );
                }
            }
        });
        let avail = ui.available_size();
        let gap = 10.0;
        let w = ((avail.x - 2.0 * gap) / 3.0).max(120.0);
        let h = (avail.y - 4.0).max(60.0);
        let (last, ctx) = (self.last.clone(), ui.ctx().clone());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (title, map, axis) in [
                ("gain per carrier, dB", &mut self.gain, Axis::Freq),
                ("SNR per carrier, dB", &mut self.snr, Axis::Freq),
                ("echoes: delay µs, dB", &mut self.echo, Axis::Delay),
            ] {
                ui.allocate_ui(vec2(w, h), |ui| {
                    plot(ui, &ctx, title, map, axis, last.as_deref(), rows_per_s);
                });
            }
        });
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Axis {
    Freq,
    Delay,
}

fn plot(
    ui: &mut Ui,
    ctx: &egui::Context,
    title: &str,
    map: &mut Map,
    axis: Axis,
    last: Option<&Measurements>,
    rows_per_s: f32,
) {
    let (resp, p) = ui.allocate_painter(ui.available_size(), Sense::hover());
    let rect = resp.rect;
    p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
    let font = FontId::monospace(11.0);
    p.text(
        rect.left_top() + vec2(4.0, 1.0),
        Align2::LEFT_TOP,
        title,
        font.clone(),
        DIM,
    );
    let plot = Rect::from_min_max(
        pos2(rect.left() + 36.0, rect.top() + 18.0),
        pos2(rect.right() - 6.0, rect.bottom() - 16.0),
    );
    let (Some(m), Some((tex, uv))) = (last, map.ring.update(ctx, &map.history)) else {
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
    let tick = Stroke::new(1.0, DIM);
    // Across.
    let (x0, x1, unit): (f32, f32, &str) = match axis {
        Axis::Freq => (
            m.freq_mhz.first().copied().unwrap_or(-3.8),
            m.freq_mhz.last().copied().unwrap_or(3.8),
            "MHz",
        ),
        Axis::Delay => (
            m.delay_us.first().copied().unwrap_or(-14.0),
            m.delay_us.last().copied().unwrap_or(42.0),
            "µs",
        ),
    };
    let step = match axis {
        Axis::Freq => 1.0,
        Axis::Delay => 10.0,
    };
    let x_of = |v: f32| plot.left() + (v - x0) / (x1 - x0).max(1e-3) * plot.width();
    let mut v = (x0 / step).ceil() * step;
    while v <= x1 {
        let x = x_of(v);
        p.line_segment([pos2(x, plot.bottom()), pos2(x, plot.bottom() + 3.0)], tick);
        p.text(
            pos2(x, plot.bottom() + 3.0),
            Align2::CENTER_TOP,
            format!("{v:+.0}"),
            font.clone(),
            DIM,
        );
        v += step;
    }
    p.text(
        pos2(plot.right(), plot.bottom() + 3.0),
        Align2::RIGHT_TOP,
        unit,
        font.clone(),
        DIM,
    );
    if axis == Axis::Delay {
        // The strongest path, and the guard interval after it.
        for d in [0.0, m.guard_us] {
            let x = x_of(d);
            if (plot.left()..=plot.right()).contains(&x) {
                p.line_segment(
                    [pos2(x, plot.top()), pos2(x, plot.bottom())],
                    Stroke::new(1.0, AMBER.gamma_multiply(if d == 0.0 { 0.4 } else { 0.9 })),
                );
            }
        }
    }
    // Time down: newest at the top.
    let span = map.history.capacity() as f32 / rows_per_s.max(1.0);
    let step_s = if span > 40.0 { 10.0 } else { 5.0 };
    let mut t = 0.0;
    while t <= span {
        let y = plot.top() + t / span * plot.height();
        p.line_segment([pos2(plot.left() - 3.0, y), pos2(plot.left(), y)], tick);
        p.text(
            pos2(plot.left() - 5.0, y),
            Align2::RIGHT_CENTER,
            if t == 0.0 {
                "now".to_string()
            } else {
                format!("-{t:.0}s")
            },
            font.clone(),
            DIM,
        );
        t += step_s;
    }
    p.rect_stroke(
        plot,
        CornerRadius::ZERO,
        Stroke::new(1.0, Color32::from_gray(70)),
        egui::StrokeKind::Outside,
    );
}
