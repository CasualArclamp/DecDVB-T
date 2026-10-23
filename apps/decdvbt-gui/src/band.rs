// made by CasualArclamp
//! The band view: the spectrum over the waterfall, one frequency axis for
//! both, the UHF channels marked, and the one VFO (the channel being
//! received) bracketed.
//!
//! Layout follows DecSAT's band view; the channel plan overlay is new here.

use decdvbt_core::channel::{UHF_FIRST, UHF_LAST, uhf_centre_hz, uhf_channel_at};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Mesh, Pos2, Rect, Sense, Shape,
    Stroke, TextureId, pos2,
};

/// Half of a DVB-T/T2 8 MHz channel's occupied bandwidth (7.61 MHz).
const OCCUPIED_HALF_HZ: f64 = 3.805e6;
/// Width reserved left of the plot for the dB scale.
const LEFT: f32 = 48.0;
/// Height of the frequency scale under the spectrum.
const AXIS_H: f32 = 20.0;

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const TRACE: Color32 = Color32::from_rgb(150, 205, 255);

/// What to draw.
pub struct BandInput<'a> {
    /// Smoothed spectrum, dBFS, the most negative frequency first.
    pub spectrum_db: &'a [f32],
    pub center_hz: f64,
    pub span_hz: f64,
    /// Colour range of the waterfall, which the spectrum's scale follows.
    pub levels: (f32, f32),
    pub waterfall: Option<(TextureId, Rect)>,
    /// The channel being received, if one is picked.
    pub vfo_hz: Option<f64>,
}

pub enum Action {
    /// A click on a channel in the view.
    Tune(u8),
}

pub fn show(ui: &mut egui::Ui, inp: &BandInput) -> Option<Action> {
    let (resp, painter) = ui.allocate_painter(ui.available_size(), Sense::click());
    let rect = resp.rect;
    painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(12, 12, 14));
    if rect.width() < 200.0 || rect.height() < 160.0 || inp.span_hz <= 0.0 {
        if inp.span_hz <= 0.0 {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "no signal yet — pick a channel and press Start, or open an IQ file",
                FontId::monospace(13.0),
                Color32::GRAY,
            );
        }
        return None;
    }

    let spec_h = (rect.height() * 0.36).clamp(110.0, 340.0);
    let plot_l = rect.left() + LEFT;
    let plot_r = rect.right() - 6.0;
    let spec = Rect::from_min_max(
        pos2(plot_l, rect.top() + 4.0),
        pos2(plot_r, rect.top() + spec_h),
    );
    let wf = Rect::from_min_max(
        pos2(plot_l, spec.bottom() + AXIS_H),
        pos2(plot_r, rect.bottom() - 4.0),
    );
    let lo_hz = inp.center_hz - inp.span_hz / 2.0;
    let x_of = |hz: f64| plot_l + ((hz - lo_hz) / inp.span_hz) as f32 * (plot_r - plot_l);
    let hz_of = |x: f32| lo_hz + ((x - plot_l) / (plot_r - plot_l)) as f64 * inp.span_hz;
    // The waterfall's colour range, with room below for the notches and the
    // floor outside the channel and above for the peaks.
    let (lo_db, hi_db) = (inp.levels.0 - 10.0, inp.levels.1 + 5.0);
    let y_of = |db: f32| {
        let f = ((db - lo_db) / (hi_db - lo_db).max(1.0)).clamp(0.0, 1.0);
        spec.bottom() - f * spec.height()
    };
    let font = FontId::monospace(11.0);
    let grid = Color32::from_rgba_unmultiplied(255, 255, 255, 22);
    let dim = Color32::from_gray(150);

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

    // The channels in view: a faint band behind each (UK ones brighter), its
    // label at the top, and the one being received picked out.
    let selected = inp
        .vfo_hz
        .and_then(uhf_channel_at)
        .filter(|&n| uhf_centre_hz(n).is_some_and(|c| (c - inp.vfo_hz.unwrap()).abs() < 1e3));
    for n in UHF_FIRST..=UHF_LAST {
        let c = uhf_centre_hz(n).unwrap();
        let (x0, x1) = (
            x_of(c - OCCUPIED_HALF_HZ).max(plot_l),
            x_of(c + OCCUPIED_HALF_HZ).min(plot_r),
        );
        if x1 <= plot_l || x0 >= plot_r || x1 - x0 < 4.0 {
            continue;
        }
        let uk = n <= 48 && n != 38;
        let is_sel = selected == Some(n);
        let alpha = if is_sel {
            26
        } else if uk {
            9
        } else {
            4
        };
        for r in [spec, wf] {
            painter.rect_filled(
                Rect::from_min_max(pos2(x0, r.top()), pos2(x1, r.bottom())),
                CornerRadius::ZERO,
                Color32::from_rgba_unmultiplied(120, 170, 255, alpha),
            );
        }
        let cx = x_of(c);
        if cx > plot_l + 14.0 && cx < plot_r - 14.0 {
            painter.text(
                pos2(cx, spec.top() + 2.0),
                Align2::CENTER_TOP,
                format!("C{n}"),
                font.clone(),
                if is_sel { AMBER } else { dim },
            );
        }
    }

    // Frequency scale.
    let step = if inp.span_hz <= 12e6 { 1e6 } else { 2e6 };
    let mut f = (lo_hz / step).ceil() * step;
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
        f += step;
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
    if n > 1 {
        let cols = (plot_r - plot_l).max(1.0) as usize;
        let mut pts: Vec<Pos2> = Vec::with_capacity(cols + 1);
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
    }

    // The waterfall, newest row at the top.
    if let Some((tex, uv)) = inp.waterfall {
        painter.image(tex, wf, uv, Color32::WHITE);
    }

    // The VFO: the received channel's occupied band.
    if let Some(v) = inp.vfo_hz {
        let edge = Stroke::new(1.4, AMBER);
        for hz in [v - OCCUPIED_HALF_HZ, v + OCCUPIED_HALF_HZ] {
            let x = x_of(hz);
            if (plot_l..=plot_r).contains(&x) {
                painter.line_segment([pos2(x, spec.top()), pos2(x, wf.bottom())], edge);
            }
        }
    }

    // Pointer: a crosshair line and a readout of what is under it.
    let mut action = None;
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
        let level = inp.spectrum_db.get(bin).map(|d| format!("  {d:.1} dBFS"));
        let chan = uhf_channel_at(hz)
            .map(|c| {
                let off = hz - uhf_centre_hz(c).unwrap();
                format!("  C{c} {:+.0} kHz", off / 1e3)
            })
            .unwrap_or_default();
        painter.text(
            pos2(plot_r - 4.0, spec.top() + 16.0),
            Align2::RIGHT_TOP,
            format!("{:.3} MHz{chan}{}", hz / 1e6, level.unwrap_or_default()),
            FontId::monospace(12.0),
            Color32::WHITE,
        );
        if uhf_channel_at(hz).is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.clicked()
            && let Some(c) = uhf_channel_at(hz)
        {
            action = Some(Action::Tune(c));
        }
    }
    action
}
