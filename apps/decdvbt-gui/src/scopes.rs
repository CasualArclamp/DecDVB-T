// made by CasualArclamp
//! The scopes panel, after OnAir's: the cells of the last symbols at four
//! stages of the receiver (the TPS cells, the pilots and the data cells
//! equalised, and what the transform gave before the equaliser), and what the
//! pilots say of the channel: its gain and the signal-to-noise ratio across
//! the band, its impulse response and the echoes in it.
//!
//! The engine makes the cells ten times a second and the measurements four,
//! and only while this panel is shown.

use std::sync::Arc;

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Modulation;
use decdvbt_rx::receiver::ReceiverStatus;
use decdvbt_rx::scope::{Measurements, Scopes};
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Rect, RichText, Sense, Stroke,
    TextureHandle, TextureOptions, Ui, pos2, vec2,
};

const PANEL_BG: Color32 = Color32::from_rgb(8, 8, 10);
const DIM: Color32 = Color32::from_gray(150);
const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const RED: Color32 = Color32::from_rgb(255, 90, 80);
const TRACE: Color32 = Color32::from_rgb(110, 200, 255);
const SNR: Color32 = Color32::from_rgb(110, 220, 130);

/// The scopes' half-range in cell units: a 64-QAM corner is at 1.08, a pilot
/// at 1.33.
const RANGE: f32 = 1.5;
/// A scope's picture, pixels a side.
const TEX: usize = 160;
/// How much of the glow is left after the next symbol's cells are in, and how
/// much a cell adds.
const DECAY: f32 = 0.72;
const GLOW: f32 = 0.45;
/// Symbols' worth of cells waiting for the next draw, at most.
const MAX_PENDING: usize = 8;

/// A scope's picture with a phosphor's memory: each symbol's cells are added
/// to a grid of intensities that fades with the next, and the grid is sent
/// to the graphics card as a texture only when cells have come (ten times a
/// second), not every frame. Drawing a scope is then one quad; drawing every
/// cell as a shape costs more CPU than the receiver's equaliser.
struct Phosphor {
    glow: Vec<f32>,
    tex: Option<TextureHandle>,
    /// Cells have been added since the texture was made.
    dirty: bool,
    /// The half-range of the last picture, cell units.
    range: f32,
    name: &'static str,
}

impl Phosphor {
    fn new(name: &'static str) -> Phosphor {
        Phosphor {
            glow: vec![0.0; TEX * TEX],
            tex: None,
            dirty: true,
            range: RANGE,
            name,
        }
    }

    /// Fade the glow and add `cells`. `fixed`: the half-range as a multiple of
    /// [`RANGE`]; `None` fits the cells' rms radius at 0.55 of the half-side
    /// (smoothed, so the picture does not breathe).
    fn add(&mut self, cells: &[Iq], fixed: Option<f32>) {
        let target = match fixed {
            Some(r) => RANGE * r,
            None if cells.is_empty() => self.range,
            None => {
                let ms = cells.iter().map(|c| c.norm_sqr()).sum::<f32>() / cells.len() as f32;
                let t = ms.sqrt() / 0.55;
                if self.range == RANGE {
                    t
                } else {
                    self.range + 0.3 * (t - self.range)
                }
            }
        };
        self.range = target;
        for g in &mut self.glow {
            *g *= DECAY;
        }
        let half = TEX as f32 / 2.0;
        let k = half / self.range;
        for c in cells {
            let (x, y) = ((half + c.re * k) as isize, (half - c.im * k) as isize);
            // A cell is two pixels square, so a sparse scope is still seen.
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (x, y) = (x + dx, y + dy);
                if (0..TEX as isize).contains(&x) && (0..TEX as isize).contains(&y) {
                    let g = &mut self.glow[y as usize * TEX + x as usize];
                    *g = (*g + GLOW).min(1.0);
                }
            }
        }
        self.dirty = true;
    }

    /// The picture, sent again if cells have come since it was made.
    fn texture(&mut self, ctx: &egui::Context) -> &TextureHandle {
        if self.dirty || self.tex.is_none() {
            let px = self
                .glow
                .iter()
                .map(|&g| Color32::from_rgba_unmultiplied(110, 220, 130, (g.sqrt() * 255.0) as u8))
                .collect();
            let img = ColorImage::new([TEX, TEX], px);
            match &mut self.tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => {
                    self.tex = Some(ctx.load_texture(
                        format!("scope_{}", self.name),
                        img,
                        TextureOptions::LINEAR,
                    ));
                }
            }
            self.dirty = false;
        }
        self.tex.as_ref().expect("made above")
    }
}

pub struct ScopeView {
    scopes: [Phosphor; 4],
    /// Symbols' cells in, to be added at the next draw.
    pending: Vec<Arc<Scopes>>,
    /// The last symbol's constellation, for the ideal points and the header.
    modulation: Option<Modulation>,
    seen: bool,
    measurements: Option<Arc<Measurements>>,
}

impl Default for ScopeView {
    fn default() -> Self {
        ScopeView {
            scopes: [
                Phosphor::new("tps"),
                Phosphor::new("pilots"),
                Phosphor::new("data"),
                Phosphor::new("raw"),
            ],
            pending: Vec::new(),
            modulation: None,
            seen: false,
            measurements: None,
        }
    }
}

impl ScopeView {
    pub fn reset(&mut self) {
        *self = ScopeView::default();
    }

    pub fn update(&mut self, scopes: Option<Arc<Scopes>>, m: Option<Arc<Measurements>>) {
        if let Some(s) = scopes {
            if self.pending.len() == MAX_PENDING {
                self.pending.remove(0);
            }
            self.pending.push(s);
        }
        if m.is_some() {
            self.measurements = m;
        }
    }

    pub fn show(&mut self, ui: &mut Ui, st: &ReceiverStatus) {
        for s in std::mem::take(&mut self.pending) {
            self.scopes[0].add(&s.tps, Some(1.0));
            self.scopes[1].add(&s.pilots, Some(1.0));
            self.scopes[2].add(&s.data, Some(1.0));
            self.scopes[3].add(&s.raw, None);
            self.modulation = s.modulation;
            self.seen = true;
        }
        let modulation = self.modulation;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Scopes").strong());
            if !self.seen {
                ui.label(RichText::new("waiting for a lock").weak());
            } else {
                let mut bits = Vec::new();
                bits.extend(modulation.map(|m| m.name().to_string()));
                if st.decoding && st.snr_db.is_finite() {
                    bits.push(format!("MER {:.1} dB", st.snr_db));
                }
                if st.decoding {
                    bits.push(format!("before RS {:.1e}", st.outer.pre_rs_ber()));
                    let o = &st.outer;
                    let total = (o.clean + o.corrected + o.uncorrectable).max(1);
                    bits.push(format!(
                        "after RS {:.1e} blocks lost",
                        o.uncorrectable as f64 / total as f64
                    ));
                }
                ui.label(RichText::new(bits.join(" · ")).color(SNR));
            }
        });
        let avail = ui.available_size();
        let height = (avail.y - 4.0).max(80.0);
        let gap = 8.0;
        let side = ((avail.x * 0.52 - 3.0 * gap) / 4.0).clamp(80.0, height);
        let plots_w = ((avail.x - 4.0 * side - 4.0 * gap - 8.0) / 3.0).max(80.0);
        let size = vec2(side, height);
        let seen = self.seen;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            let marks: [Vec<Iq>; 4] = [
                vec![Iq::new(1.0, 0.0), Iq::new(-1.0, 0.0)],
                vec![Iq::new(4.0 / 3.0, 0.0), Iq::new(-4.0 / 3.0, 0.0)],
                ideal(modulation),
                Vec::new(),
            ];
            for (ph, marks) in self.scopes.iter_mut().zip(&marks) {
                scope(ui, size, ph, seen, marks);
            }
            let m = self.measurements.as_deref();
            response_plot(ui, vec2(plots_w, height), m);
            snr_plot(ui, vec2(plots_w, height), m);
            impulse_plot(ui, vec2(plots_w, height), m);
        });
    }
}

/// The ideal points of a constellation at unit mean power (α = 1).
fn ideal(m: Option<Modulation>) -> Vec<Iq> {
    let (levels, norm): (&[f32], f32) = match m {
        Some(Modulation::Qpsk) => (&[-1.0, 1.0], 2f32.sqrt()),
        Some(Modulation::Qam16) => (&[-3.0, -1.0, 1.0, 3.0], 10f32.sqrt()),
        Some(Modulation::Qam64) => (&[-7.0, -5.0, -3.0, -1.0, 1.0, 3.0, 5.0, 7.0], 42f32.sqrt()),
        None => return Vec::new(),
    };
    levels
        .iter()
        .flat_map(|&i| levels.iter().map(move |&q| Iq::new(i / norm, q / norm)))
        .collect()
}

/// One scope: its picture on a square with the ideal points marked.
fn scope(ui: &mut Ui, size: egui::Vec2, ph: &mut Phosphor, seen: bool, marks: &[Iq]) {
    let (resp, p) = ui.allocate_painter(size, Sense::hover());
    let rect = resp.rect;
    p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
    let font = FontId::monospace(11.0);
    let title = if ph.name == "raw" {
        "raw (scaled to fit)"
    } else {
        ph.name
    };
    p.text(
        rect.left_top() + vec2(4.0, 1.0),
        Align2::LEFT_TOP,
        title,
        font.clone(),
        DIM,
    );
    let side = (rect.width().min(rect.height() - 14.0) - 4.0).max(40.0);
    let sq = Rect::from_center_size(rect.center() + vec2(0.0, 6.0), vec2(side, side));
    let grid = Stroke::new(1.0, Color32::from_white_alpha(28));
    p.line_segment(
        [
            pos2(sq.left(), sq.center().y),
            pos2(sq.right(), sq.center().y),
        ],
        grid,
    );
    p.line_segment(
        [
            pos2(sq.center().x, sq.top()),
            pos2(sq.center().x, sq.bottom()),
        ],
        grid,
    );
    p.rect_stroke(
        sq,
        CornerRadius::ZERO,
        Stroke::new(1.0, Color32::from_gray(60)),
        egui::StrokeKind::Outside,
    );
    if !seen {
        p.text(sq.center(), Align2::CENTER_CENTER, "—", font, DIM);
        return;
    }
    let range = ph.range;
    let tex = ph.texture(ui.ctx());
    p.image(
        tex.id(),
        sq,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    for &m in marks {
        let c = pos2(
            sq.center().x + m.re / range * side / 2.0,
            sq.center().y - m.im / range * side / 2.0,
        );
        let k = Stroke::new(1.0, Color32::from_white_alpha(80));
        p.line_segment([c - vec2(3.0, 0.0), c + vec2(3.0, 0.0)], k);
        p.line_segment([c - vec2(0.0, 3.0), c + vec2(0.0, 3.0)], k);
    }
}

/// A plot area: the panel, the title and the axes' rectangle.
fn frame(ui: &mut Ui, size: egui::Vec2, title: &str) -> (egui::Response, egui::Painter, Rect) {
    let (resp, p) = ui.allocate_painter(size, Sense::hover());
    let rect = resp.rect;
    p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
    p.text(
        rect.left_top() + vec2(4.0, 1.0),
        Align2::LEFT_TOP,
        title,
        FontId::monospace(11.0),
        DIM,
    );
    let plot = Rect::from_min_max(
        pos2(rect.left() + 34.0, rect.top() + 18.0),
        pos2(rect.right() - 6.0, rect.bottom() - 16.0),
    );
    (resp, p, plot)
}

fn border(p: &egui::Painter, plot: Rect) {
    p.rect_stroke(
        plot,
        CornerRadius::ZERO,
        Stroke::new(1.0, Color32::from_gray(70)),
        egui::StrokeKind::Outside,
    );
}

/// Ticks and labels on an axis from `lo` to `hi` every `step`, on the
/// horizontal (`x`) or vertical axis of `plot`.
fn ticks(
    p: &egui::Painter,
    plot: Rect,
    x: bool,
    lo: f32,
    hi: f32,
    step: f32,
    fmt: impl Fn(f32) -> String,
) {
    let font = FontId::monospace(10.0);
    let mut v = (lo / step).ceil() * step;
    while v <= hi + step * 1e-3 {
        let f = (v - lo) / (hi - lo);
        if x {
            let px = plot.left() + f * plot.width();
            p.line_segment(
                [pos2(px, plot.bottom()), pos2(px, plot.bottom() + 3.0)],
                Stroke::new(1.0, DIM),
            );
            p.text(
                pos2(px, plot.bottom() + 3.0),
                Align2::CENTER_TOP,
                fmt(v),
                font.clone(),
                DIM,
            );
        } else {
            let py = plot.bottom() - f * plot.height();
            p.line_segment(
                [pos2(plot.left() - 3.0, py), pos2(plot.left(), py)],
                Stroke::new(1.0, DIM),
            );
            p.line_segment(
                [pos2(plot.left(), py), pos2(plot.right(), py)],
                Stroke::new(1.0, Color32::from_white_alpha(14)),
            );
            p.text(
                pos2(plot.left() - 4.0, py),
                Align2::RIGHT_CENTER,
                fmt(v),
                font.clone(),
                DIM,
            );
        }
        v += step;
    }
}

/// A "nice" tick step for a range: 1, 2 or 5 times a power of ten giving
/// about five ticks.
fn nice_step(range: f32) -> f32 {
    let raw = (range / 5.0).max(1e-6);
    let mag = 10f32.powf(raw.log10().floor());
    let r = raw / mag;
    mag * if r < 1.5 {
        1.0
    } else if r < 3.5 {
        2.0
    } else if r < 7.5 {
        5.0
    } else {
        10.0
    }
}

/// A trace over the frequency axis (MHz) from the measurements.
fn across_band(
    ui: &mut Ui,
    size: egui::Vec2,
    title: &str,
    m: Option<&Measurements>,
    y: impl Fn(&Measurements) -> &[f32],
    colour: Color32,
    floor: f32,
) {
    let (_resp, p, plot) = frame(ui, size, title);
    let Some(m) = m.filter(|m| !m.freq_mhz.is_empty()) else {
        p.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "—",
            FontId::monospace(11.0),
            DIM,
        );
        return;
    };
    let ys = y(m);
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for &v in ys {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let lo = lo.max(floor);
    let step = nice_step((hi - lo).max(6.0));
    let (lo, hi) = (
        (lo / step).floor() * step,
        ((hi / step).ceil() * step).max(lo + step),
    );
    let (x0, x1) = (m.freq_mhz[0], m.freq_mhz[m.freq_mhz.len() - 1]);
    let to = |f: f32, v: f32| {
        pos2(
            plot.left() + (f - x0) / (x1 - x0) * plot.width(),
            plot.bottom() - (v.clamp(lo, hi) - lo) / (hi - lo) * plot.height(),
        )
    };
    ticks(&p, plot, false, lo, hi, step, |v| format!("{v:.0}"));
    ticks(&p, plot, true, x0.ceil(), x1.floor(), 1.0, |v| {
        format!("{v:+.0}")
    });
    p.add(egui::Shape::line(
        decimate(
            plot.width(),
            m.freq_mhz.iter().zip(ys).map(|(&f, &v)| to(f, v)),
        ),
        Stroke::new(1.2, colour),
    ));
    border(&p, plot);
}

/// A trace's points cut to about two a pixel column of a plot `width` wide:
/// the lowest and the highest of each stretch, in the order they came, so a
/// narrow notch is kept and the line costs the tessellator a few hundred
/// points rather than a thousand.
fn decimate(width: f32, pts: impl ExactSizeIterator<Item = egui::Pos2>) -> Vec<egui::Pos2> {
    let pts: Vec<egui::Pos2> = pts.collect();
    let stride = (pts.len() as f32 / width.max(1.0)).floor().max(1.0) as usize;
    if stride < 2 {
        return pts;
    }
    let mut out = Vec::with_capacity(pts.len() * 2 / stride + 2);
    for chunk in pts.chunks(stride) {
        let (mut lo, mut hi) = (chunk[0], chunk[0]);
        let (mut lo_i, mut hi_i) = (0, 0);
        for (i, p) in chunk.iter().enumerate() {
            if p.y < lo.y {
                (lo, lo_i) = (*p, i);
            }
            if p.y > hi.y {
                (hi, hi_i) = (*p, i);
            }
        }
        if lo_i <= hi_i {
            out.extend([lo, hi]);
        } else {
            out.extend([hi, lo]);
        }
    }
    out
}

fn response_plot(ui: &mut Ui, size: egui::Vec2, m: Option<&Measurements>) {
    across_band(
        ui,
        size,
        "gain, dB (MHz from centre)",
        m,
        |m| &m.gain_db,
        TRACE,
        -40.0,
    );
}

fn snr_plot(ui: &mut Ui, size: egui::Vec2, m: Option<&Measurements>) {
    let title = match m {
        Some(m) => format!("SNR per carrier, mean {:.1} dB", m.mean_snr_db),
        None => "SNR per carrier".into(),
    };
    across_band(ui, size, &title, m, |m| &m.snr_db, SNR, 0.0);
}

/// The impulse response: level against delay from the strongest path, the
/// guard interval marked (later echoes fall into the next symbol).
fn impulse_plot(ui: &mut Ui, size: egui::Vec2, m: Option<&Measurements>) {
    let (resp, p, plot) = frame(ui, size, "impulse response, dB (µs)");
    let font = FontId::monospace(10.0);
    let Some(m) = m.filter(|m| m.delay_us.len() > 2) else {
        p.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "—",
            FontId::monospace(11.0),
            DIM,
        );
        return;
    };
    let (x0, x1) = (m.delay_us[0], m.delay_us[m.delay_us.len() - 1]);
    let (lo, hi) = (-60.0f32, 0.0f32);
    let to = |d: f32, v: f32| {
        pos2(
            plot.left() + (d - x0) / (x1 - x0) * plot.width(),
            plot.bottom() - (v.clamp(lo, hi) - lo) / (hi - lo) * plot.height(),
        )
    };
    // Past the guard: shaded.
    let gx = to(m.guard_us, 0.0).x;
    if gx < plot.right() {
        p.rect_filled(
            Rect::from_min_max(pos2(gx, plot.top()), plot.right_bottom()),
            CornerRadius::ZERO,
            RED.gamma_multiply(0.12),
        );
    }
    for d in [0.0, m.guard_us] {
        let x = to(d, 0.0).x;
        if (plot.left()..=plot.right()).contains(&x) {
            p.line_segment(
                [pos2(x, plot.top()), pos2(x, plot.bottom())],
                Stroke::new(1.0, AMBER.gamma_multiply(if d == 0.0 { 0.4 } else { 0.9 })),
            );
        }
    }
    ticks(&p, plot, false, lo, hi, 20.0, |v| format!("{v:.0}"));
    ticks(&p, plot, true, x0, x1, nice_step(x1 - x0), |v| {
        format!("{v:.0}")
    });
    // Filled under the curve, so a lone spike still shows.
    p.add(egui::Shape::line(
        decimate(
            plot.width(),
            m.delay_us
                .iter()
                .zip(&m.impulse_db)
                .map(|(&d, &v)| to(d, v)),
        ),
        Stroke::new(1.2, TRACE),
    ));
    for (i, e) in m.echoes.iter().enumerate() {
        // The strongest path is at delay 0 by construction of the echo list.
        let at = to(e.delay_us, e.db);
        p.circle_stroke(at, 3.0, Stroke::new(1.0, AMBER));
        if i < 4 {
            p.text(
                pos2(plot.right() - 3.0, plot.top() + 2.0 + 11.0 * i as f32),
                Align2::RIGHT_TOP,
                format!("{:+.2} µs {:.0} dB", e.delay_us, e.db),
                font.clone(),
                AMBER,
            );
        }
    }
    border(&p, plot);
    if let Some(pt) = resp.hover_pos()
        && plot.contains(pt)
    {
        let d = x0 + (pt.x - plot.left()) / plot.width() * (x1 - x0);
        p.text(
            plot.left_bottom() + vec2(4.0, -3.0),
            Align2::LEFT_BOTTOM,
            format!("{d:+.2} µs"),
            font,
            Color32::WHITE,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ideal_points_are_the_constellations_at_unit_power() {
        for (m, n) in [
            (Modulation::Qpsk, 4),
            (Modulation::Qam16, 16),
            (Modulation::Qam64, 64),
        ] {
            let pts = ideal(Some(m));
            assert_eq!(pts.len(), n);
            let power = pts.iter().map(|c| c.norm_sqr()).sum::<f32>() / n as f32;
            assert!((power - 1.0).abs() < 1e-5, "{m:?}: {power}");
        }
        assert!(ideal(None).is_empty());
    }

    #[test]
    fn a_cell_glows_where_it_is_and_fades_with_the_next_symbol() {
        let mut ph = Phosphor::new("data");
        ph.add(&[Iq::new(0.0, 0.0)], Some(1.0));
        let centre = TEX / 2 * TEX + TEX / 2;
        assert!(ph.glow[centre] >= GLOW - 1e-6);
        let first = ph.glow[centre];
        ph.add(&[], Some(1.0));
        assert!((ph.glow[centre] - first * DECAY).abs() < 1e-6);
        // A cell off the scope is not drawn (and does not panic).
        ph.add(&[Iq::new(100.0, -100.0)], Some(1.0));
    }

    #[test]
    fn a_raw_scope_fits_its_cells() {
        let mut ph = Phosphor::new("raw");
        let cells: Vec<Iq> = (0..100).map(|k| Iq::from_polar(50.0, k as f32)).collect();
        ph.add(&cells, None);
        // The rms radius lands at 0.55 of the half-side.
        assert!((ph.range - 50.0 / 0.55).abs() < 0.01, "{}", ph.range);
    }

    #[test]
    fn a_decimated_trace_keeps_its_notch() {
        let pts: Vec<egui::Pos2> = (0..1000)
            .map(|i| pos2(i as f32, if i == 503 { 90.0 } else { 10.0 }))
            .collect();
        let d = decimate(200.0, pts.iter().copied());
        assert!(d.len() <= 2 * 200 + 2, "{}", d.len());
        assert!(d.iter().any(|p| p.y == 90.0));
        assert_eq!(decimate(2000.0, pts.iter().copied()).len(), 1000);
    }
}
