// made by CasualArclamp
//! The signal view: the spectrum of what the radio is being given, over a
//! waterfall, one frequency axis for both. The occupied band of the mode is
//! shaded and the sample rate's span is the whole width.
//!
//! The layout follows the receiver GUI's `band.rs` (same author), without its
//! UHF channel plan: this view has one signal and its tuning.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, TextureId,
    pos2,
};

use crate::{AMBER, RED};

/// Width reserved left of the plot for the dB scale.
const LEFT: f32 = 48.0;
/// Height of the frequency scale under the spectrum.
const AXIS_H: f32 = 20.0;
/// Height of the readout line at the bottom.
const READOUT_H: f32 = 20.0;

const TRACE: Color32 = Color32::from_rgb(150, 205, 255);

/// The numbers under the plot.
#[derive(Debug, Clone, Copy)]
pub struct Readout {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    pub clipped: u64,
    /// Samples are being clipped now (the clipped count is rising).
    pub clipping: bool,
}

/// What to draw.
pub struct SignalInput<'a> {
    /// The newest spectrum row, dBFS, the most negative frequency first.
    pub spectrum_db: &'a [f32],
    pub centre_hz: f64,
    /// The span: the radio's sample rate, Hz.
    pub span_hz: f64,
    /// The signal's occupied bandwidth, Hz (7.61 MHz for an 8 MHz channel).
    pub occupied_hz: f64,
    /// Colour range of the waterfall, which the spectrum's scale follows.
    pub levels: (f32, f32),
    pub waterfall: Option<(TextureId, Rect)>,
    pub readout: Option<Readout>,
    /// Shown in the middle while there is no spectrum.
    pub empty_text: &'a str,
}

fn spectrum_height(height: f32) -> f32 {
    (height * 0.42).clamp(100.0, 300.0)
}

/// The waterfall's height in whole physical pixels for a view `height` points
/// tall at `ppp` pixels a point: the app keeps exactly this many rows and the
/// layout draws them on exactly this many pixels, so a row is a pixel and the
/// scroll moves by whole pixels (from DecDAB, same author).
pub fn waterfall_rows(height: f32, ppp: f32) -> usize {
    ((height - spectrum_height(height) - AXIS_H - READOUT_H - 4.0).max(0.0) * ppp).floor() as usize
}

pub fn show(ui: &mut egui::Ui, inp: &SignalInput) {
    let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(12, 12, 14));
    let font = FontId::monospace(11.0);
    let dim = Color32::from_gray(150);
    if rect.width() < 200.0 || rect.height() < 160.0 || inp.spectrum_db.len() < 2 {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            inp.empty_text,
            FontId::monospace(13.0),
            Color32::GRAY,
        );
        return;
    }

    let spec_h = spectrum_height(rect.height());
    let plot_l = rect.left() + LEFT;
    let plot_r = rect.right() - 6.0;
    let spec = Rect::from_min_max(
        pos2(plot_l, rect.top() + 4.0),
        pos2(plot_r, rect.top() + spec_h),
    );
    // The waterfall on whole physical pixels (see `waterfall_rows`).
    let ppp = ui.ctx().pixels_per_point();
    let wf_top = ((spec.bottom() + AXIS_H) * ppp).round() / ppp;
    let wf_px = waterfall_rows(rect.height(), ppp) as f32;
    let wf = Rect::from_min_max(pos2(plot_l, wf_top), pos2(plot_r, wf_top + wf_px / ppp));
    let lo_hz = inp.centre_hz - inp.span_hz / 2.0;
    let x_of = |hz: f64| plot_l + ((hz - lo_hz) / inp.span_hz) as f32 * (plot_r - plot_l);
    let hz_of = |x: f32| lo_hz + ((x - plot_l) / (plot_r - plot_l)) as f64 * inp.span_hz;
    let (lo_db, hi_db) = (inp.levels.0 - 10.0, inp.levels.1 + 5.0);
    let y_of = |db: f32| {
        let f = ((db - lo_db) / (hi_db - lo_db).max(1.0)).clamp(0.0, 1.0);
        spec.bottom() - f * spec.height()
    };
    let grid = Color32::from_rgba_unmultiplied(255, 255, 255, 22);

    // dB grid and scale.
    let mut db = (lo_db / 10.0).ceil() * 10.0;
    while db < hi_db {
        let y = y_of(db);
        painter.line_segment([pos2(plot_l, y), pos2(plot_r, y)], Stroke::new(1.0, grid));
        painter.text(
            pos2(plot_l - 4.0, y),
            Align2::RIGHT_CENTER,
            format!("{db:.0}"),
            font.clone(),
            dim,
        );
        db += 10.0;
    }
    painter.text(
        pos2(rect.left() + 2.0, spec.top()),
        Align2::LEFT_TOP,
        "dBFS",
        font.clone(),
        dim,
    );

    // The occupied band, behind the trace and the waterfall.
    let (x0, x1) = (
        x_of(inp.centre_hz - inp.occupied_hz / 2.0).max(plot_l),
        x_of(inp.centre_hz + inp.occupied_hz / 2.0).min(plot_r),
    );
    if x1 > x0 {
        for r in [spec, wf] {
            painter.rect_filled(
                Rect::from_min_max(pos2(x0, r.top()), pos2(x1, r.bottom())),
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(120, 170, 255, 14),
            );
        }
    }

    // Frequency scale: a line every MHz.
    let mut f = (lo_hz / 1e6).ceil() * 1e6;
    while f < lo_hz + inp.span_hz {
        let x = x_of(f);
        painter.line_segment(
            [pos2(x, spec.top()), pos2(x, spec.bottom())],
            Stroke::new(1.0, grid),
        );
        painter.text(
            pos2(x, spec.bottom() + 3.0),
            Align2::CENTER_TOP,
            format!("{:.0}", f / 1e6),
            font.clone(),
            dim,
        );
        f += 1e6;
    }
    painter.text(
        pos2(rect.left() + 2.0, spec.bottom() + 3.0),
        Align2::LEFT_TOP,
        "MHz",
        font.clone(),
        dim,
    );

    // The trace: the strongest bin under each pixel column, filled below.
    let n = inp.spectrum_db.len();
    let cols = (plot_r - plot_l).max(1.0) as usize;
    let mut pts: Vec<Pos2> = Vec::with_capacity(cols);
    for c in 0..cols {
        let (k0, k1) = (
            c * n / cols,
            ((c + 1) * n / cols).max(c * n / cols + 1).min(n),
        );
        let m = inp.spectrum_db[k0..k1]
            .iter()
            .copied()
            .fold(f32::MIN, f32::max);
        pts.push(pos2(plot_l + c as f32 + 0.5, y_of(m)));
    }
    let fill = Color32::from_rgba_unmultiplied(70, 150, 255, 38);
    let mut mesh = Mesh::default();
    for p in &pts {
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(*p, fill);
        mesh.colored_vertex(pos2(p.x, spec.bottom()), fill);
        if i >= 2 {
            mesh.add_triangle(i - 2, i - 1, i);
            mesh.add_triangle(i - 1, i + 1, i);
        }
    }
    painter.add(Shape::mesh(mesh));
    painter.add(Shape::line(pts, Stroke::new(1.2, TRACE)));

    // The waterfall, newest row at the top.
    if let Some((tex, uv)) = inp.waterfall {
        painter.image(tex, wf, uv, Color32::WHITE);
    }

    // The occupied band's edges, and the tuned centre.
    let edge = Stroke::new(1.0, AMBER.gamma_multiply(0.7));
    for hz in [
        inp.centre_hz - inp.occupied_hz / 2.0,
        inp.centre_hz + inp.occupied_hz / 2.0,
    ] {
        let x = x_of(hz);
        if (plot_l..=plot_r).contains(&x) {
            painter.line_segment([pos2(x, spec.top()), pos2(x, wf.bottom())], edge);
        }
    }
    let cx = x_of(inp.centre_hz);
    painter.line_segment(
        [pos2(cx, spec.top()), pos2(cx, spec.bottom())],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)),
    );

    // Pointer: a crosshair and what is under it.
    if let Some(p) = resp.hover_pos()
        && (plot_l..=plot_r).contains(&p.x)
        && (spec.top()..=wf.bottom()).contains(&p.y)
    {
        let hz = hz_of(p.x);
        painter.line_segment(
            [pos2(p.x, spec.top()), pos2(p.x, wf.bottom())],
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 90)),
        );
        let bin = (((hz - lo_hz) / inp.span_hz) * n as f64) as usize;
        let level = inp
            .spectrum_db
            .get(bin)
            .map_or(String::new(), |d| format!("  {d:.1} dBFS"));
        painter.text(
            pos2(plot_r - 4.0, spec.top() + 4.0),
            Align2::RIGHT_TOP,
            format!("{:.3} MHz{level}", hz / 1e6),
            FontId::monospace(12.0),
            Color32::WHITE,
        );
    }

    // The readout under the plot.
    if let Some(r) = inp.readout {
        let y = rect.bottom() - READOUT_H / 2.0 - 1.0;
        let text = format!(
            "rms {:.1} dBFS   peak {:.1} dBFS   clipped {}",
            r.rms_dbfs, r.peak_dbfs, r.clipped
        );
        painter.text(
            pos2(plot_l, y),
            Align2::LEFT_CENTER,
            text,
            FontId::monospace(12.0),
            if r.clipping { RED } else { dim },
        );
    }
}
