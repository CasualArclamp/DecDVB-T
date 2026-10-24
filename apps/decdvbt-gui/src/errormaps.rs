// made by CasualArclamp
//! Maps of what is wrong with the data cells, on the constellation's own axes
//! (nothing here is time or frequency):
//!
//! - **The error cloud**: every data cell's error vector (the cell less the
//!   constellation point it is nearest) in one density map. Noise is a round
//!   cloud; a phase error (phase noise, a bad carrier recovery) a cloud
//!   stretched across the points' radii; an I/Q imbalance or compression an
//!   ellipse; a DC offset or a bias a cloud off the centre.
//! - **MER per point**: the signal-to-noise ratio of the cells at each point of
//!   the constellation. Even when the channel is clean; the outer points
//!   worse than the inner show the radio or the transmitter compressing, the
//!   corners worse than the rest a phase error.
//!
//! Made from the data cells the scopes already carry (a few thousand ten times
//! a second), so they cost the receiver nothing.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Modulation;
use decdvbt_rx::scope::Scopes;
use eframe::egui::{
    self, Align2, Color32, ColorImage, CornerRadius, FontId, Rect, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, pos2, vec2,
};

use crate::waterfall::palette;

const PANEL_BG: Color32 = Color32::from_rgb(8, 8, 10);
const DIM: Color32 = Color32::from_gray(150);
/// The cloud is this many bins across, and spans this many σ either side.
const CLOUD: usize = 48;
const CLOUD_SIGMAS: f32 = 3.5;
/// What is kept of the last snapshots' counts when a new one is added.
const DECAY: f32 = 0.9;

#[derive(Default)]
pub struct ErrorMaps {
    /// The constellation's grid: points a side, and its name.
    modulation: Option<(usize, &'static str)>,
    /// Per constellation point (row-major, Q from the top): Σ|e|² and the
    /// cells, decayed.
    sum: Vec<f32>,
    cnt: Vec<f32>,
    cloud: Vec<f32>,
    cloud_tex: Option<TextureHandle>,
    dirty: bool,
    /// The error's RMS per component, smoothed: the cloud's scale.
    sigma: f32,
}

/// The side of the constellation's grid, and the factor that takes a unit-
/// power cell to the odd-integer grid: √(2(side² − 1)/3) for square QAM.
fn grid(m: (usize, &'static str)) -> (usize, f32) {
    let side = m.0;
    (side, ((2 * (side * side - 1)) as f32 / 3.0).sqrt())
}

/// The grid a snapshot's data cells are on: DVB-T's constellation, or a T2
/// one (a rotated one only once put back together and turned back).
fn grid_of(s: &Scopes) -> Option<(usize, &'static str)> {
    if let Some(m) = s.modulation {
        return Some(match m {
            Modulation::Qpsk => (2, "QPSK"),
            Modulation::Qam16 => (4, "16-QAM"),
            Modulation::Qam64 => (8, "64-QAM"),
        });
    }
    let (bits, rotated) = s.t2_qam?;
    if rotated && !s.derotated {
        return None;
    }
    Some(match bits {
        2 => (2, "QPSK"),
        4 => (4, "16-QAM"),
        6 => (8, "64-QAM"),
        8 => (16, "256-QAM"),
        _ => return None,
    })
}

/// The index (0..side) of the grid level nearest `v` (in cell units).
fn nearest(v: f32, side: usize, norm: f32) -> usize {
    // Levels are the odd integers −(side−1)..(side−1), scaled by 1/norm.
    let x = v * norm;
    (((x + (side as f32 - 1.0)) / 2.0).round().max(0.0) as usize).min(side - 1)
}

fn level(i: usize, side: usize, norm: f32) -> f32 {
    (2.0 * i as f32 - (side as f32 - 1.0)) / norm
}

impl ErrorMaps {
    pub fn reset(&mut self) {
        *self = ErrorMaps::default();
    }

    /// Add a snapshot's data cells.
    pub fn add(&mut self, s: &Scopes) {
        let Some(m) = grid_of(s) else { return };
        if s.data.is_empty() {
            return;
        }
        let (side, norm) = grid(m);
        if self.modulation != Some(m) {
            *self = ErrorMaps {
                modulation: Some(m),
                sum: vec![0.0; side * side],
                cnt: vec![0.0; side * side],
                cloud: vec![0.0; CLOUD * CLOUD],
                sigma: 0.0,
                ..ErrorMaps::default()
            };
        }
        for v in self
            .sum
            .iter_mut()
            .chain(&mut self.cnt)
            .chain(&mut self.cloud)
        {
            *v *= DECAY;
        }
        // The cloud's scale from what has been seen (a first guess before).
        let scale = if self.sigma > 0.0 { self.sigma } else { 0.15 };
        let half = CLOUD as f32 / 2.0;
        let k = half / (CLOUD_SIGMAS * scale);
        let (mut err2, mut n) = (0.0f32, 0u32);
        for &c in &s.data {
            let (i, q) = (nearest(c.re, side, norm), nearest(c.im, side, norm));
            let e = c - Iq::new(level(i, side, norm), level(q, side, norm));
            let p = e.norm_sqr();
            // A cell far from every point (a bad symbol) is not an error of
            // the point.
            if p > 0.25 {
                continue;
            }
            // Q is up: row 0 is the highest.
            let idx = (side - 1 - q) * side + i;
            self.sum[idx] += p;
            self.cnt[idx] += 1.0;
            err2 += p;
            n += 1;
            let (bx, by) = ((half + e.re * k) as isize, (half - e.im * k) as isize);
            if (0..CLOUD as isize).contains(&bx) && (0..CLOUD as isize).contains(&by) {
                self.cloud[by as usize * CLOUD + bx as usize] += 1.0;
            }
        }
        if n > 0 {
            let s_now = (err2 / n as f32 / 2.0).sqrt();
            self.sigma = if self.sigma > 0.0 {
                self.sigma + 0.2 * (s_now - self.sigma)
            } else {
                s_now
            };
        }
        self.dirty = true;
    }

    /// The MER at each point, dB against unit cell power (`None` where no cell
    /// has landed).
    fn mer(&self) -> Vec<Option<f32>> {
        self.sum
            .iter()
            .zip(&self.cnt)
            .map(|(s, n)| (*n > 3.0).then(|| -10.0 * (s / n).max(1e-9).log10()))
            .collect()
    }

    /// The error cloud, as a square.
    pub fn cloud(&mut self, ui: &mut Ui, size: egui::Vec2) {
        let (resp, p) = ui.allocate_painter(size, Sense::hover());
        let rect = resp.rect;
        p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
        let font = FontId::monospace(11.0);
        p.text(
            rect.left_top() + vec2(4.0, 1.0),
            Align2::LEFT_TOP,
            "error cloud",
            font.clone(),
            DIM,
        );
        let side = (rect.width().min(rect.height() - 14.0) - 4.0).max(40.0);
        let sq = Rect::from_center_size(rect.center() + vec2(0.0, 6.0), vec2(side, side));
        if self.modulation.is_none() || self.sigma <= 0.0 {
            p.text(sq.center(), Align2::CENTER_CENTER, "—", font, DIM);
            return;
        }
        if self.dirty || self.cloud_tex.is_none() {
            let lut = palette();
            let peak = self.cloud.iter().copied().fold(1e-3, f32::max);
            let px = self
                .cloud
                .iter()
                .map(|&c| {
                    let t = (c / peak).sqrt();
                    lut[(t * 255.0) as usize]
                })
                .collect();
            let img = ColorImage::new([CLOUD, CLOUD], px);
            match &mut self.cloud_tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => {
                    self.cloud_tex = Some(ui.ctx().load_texture(
                        "error_cloud",
                        img,
                        TextureOptions::LINEAR,
                    ));
                }
            }
            self.dirty = false;
        }
        if let Some(t) = &self.cloud_tex {
            p.image(
                t.id(),
                sq,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        let grid = Stroke::new(1.0, Color32::from_white_alpha(40));
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
        // The 1σ and 2σ circles: a round cloud sits on them.
        for sigmas in [1.0, 2.0] {
            let r = sigmas / CLOUD_SIGMAS * side / 2.0;
            p.circle_stroke(
                sq.center(),
                r,
                Stroke::new(1.0, Color32::from_white_alpha(70)),
            );
        }
        p.rect_stroke(
            sq,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(60)),
            egui::StrokeKind::Outside,
        );
        p.text(
            pos2(sq.right() - 3.0, sq.top() + 2.0),
            Align2::RIGHT_TOP,
            format!("σ {:.3}", self.sigma),
            font,
            Color32::WHITE,
        );
    }

    /// The MER at each point of the constellation, as a grid.
    pub fn points(&self, ui: &mut Ui, size: egui::Vec2) {
        let (resp, p) = ui.allocate_painter(size, Sense::hover());
        let rect = resp.rect;
        p.rect_filled(rect, CornerRadius::ZERO, PANEL_BG);
        let font = FontId::monospace(11.0);
        let title = self.modulation.map_or("MER per point".to_string(), |m| {
            format!("MER per point, dB ({})", m.1)
        });
        p.text(
            rect.left_top() + vec2(4.0, 1.0),
            Align2::LEFT_TOP,
            title,
            font.clone(),
            DIM,
        );
        let side_px = (rect.width().min(rect.height() - 14.0) - 4.0).max(40.0);
        let sq = Rect::from_center_size(rect.center() + vec2(0.0, 6.0), vec2(side_px, side_px));
        let Some(m) = self.modulation else {
            p.text(sq.center(), Align2::CENTER_CENTER, "—", font, DIM);
            return;
        };
        let (n, _) = grid(m);
        let mer = self.mer();
        let seen: Vec<f32> = mer.iter().flatten().copied().collect();
        if seen.is_empty() {
            p.text(sq.center(), Align2::CENTER_CENTER, "—", font, DIM);
            return;
        }
        // Colours: five dB either side of the middle point's figure.
        let mean = seen.iter().sum::<f32>() / seen.len() as f32;
        let lut = palette();
        let cell = side_px / n as f32;
        for (idx, v) in mer.iter().enumerate() {
            let (cx, cy) = ((idx % n) as f32, (idx / n) as f32);
            let r = Rect::from_min_size(sq.min + vec2(cx * cell, cy * cell), vec2(cell, cell))
                .shrink(0.5);
            let Some(v) = v else {
                p.rect_filled(r, CornerRadius::ZERO, Color32::from_gray(20));
                continue;
            };
            // Better is brighter.
            let t = ((v - (mean - 5.0)) / 10.0).clamp(0.0, 1.0);
            let col = lut[(t * 255.0) as usize];
            p.rect_filled(r, CornerRadius::ZERO, col);
            if cell >= 24.0 {
                let ink = if t > 0.6 {
                    Color32::BLACK
                } else {
                    Color32::WHITE
                };
                p.text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    format!("{v:.0}"),
                    FontId::monospace(10.0),
                    ink,
                );
            }
        }
        p.rect_stroke(
            sq,
            CornerRadius::ZERO,
            Stroke::new(1.0, Color32::from_gray(60)),
            egui::StrokeKind::Outside,
        );
        p.text(
            pos2(sq.right() - 3.0, rect.top() + 1.0),
            Align2::RIGHT_TOP,
            format!("mean {mean:.1}"),
            font,
            DIM,
        );
        if let Some(pt) = resp.hover_pos()
            && sq.contains(pt)
        {
            let i = (((pt.x - sq.left()) / cell) as usize).min(n - 1);
            let j = (((pt.y - sq.top()) / cell) as usize).min(n - 1);
            if let Some(v) = mer[j * n + i] {
                p.text(
                    sq.left_bottom() + vec2(3.0, -3.0),
                    Align2::LEFT_BOTTOM,
                    format!("{v:.1} dB"),
                    FontId::monospace(11.0),
                    Color32::WHITE,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(m: Modulation, cells: Vec<Iq>) -> Scopes {
        Scopes {
            data: cells,
            modulation: Some(m),
            ..Scopes::default()
        }
    }

    #[test]
    fn the_nearest_level_and_its_value_round_trip() {
        let (side, norm) = grid((8, "64-QAM"));
        for i in 0..side {
            assert_eq!(nearest(level(i, side, norm), side, norm), i);
        }
        // Beyond the corner is the corner.
        assert_eq!(nearest(5.0, side, norm), side - 1);
        assert_eq!(nearest(-5.0, side, norm), 0);
    }

    #[test]
    fn noise_that_grows_with_amplitude_shows_in_the_outer_points() {
        let (side, norm) = grid((4, "16-QAM"));
        let mut maps = ErrorMaps::default();
        // Cells at each point with an error 0.02 at the inner four, 0.08 at the
        // outer twelve, in a fixed direction.
        let mut cells = Vec::new();
        for i in 0..side {
            for q in 0..side {
                let inner = (1..=2).contains(&i) && (1..=2).contains(&q);
                let e = if inner { 0.02 } else { 0.08 };
                for k in 0..10 {
                    let ang = k as f32 * 0.6;
                    cells.push(
                        Iq::new(level(i, side, norm), level(q, side, norm))
                            + Iq::from_polar(e, ang),
                    );
                }
            }
        }
        maps.add(&snapshot(Modulation::Qam16, cells));
        let mer = maps.mer();
        let at = |i: usize, q: usize| mer[(side - 1 - q) * side + i].unwrap();
        // 20·log10(0.08/0.02) = 12 dB apart.
        assert!(
            (at(1, 1) - at(0, 0) - 12.0).abs() < 0.1,
            "{} {}",
            at(1, 1),
            at(0, 0)
        );
        // The cloud is round: its mass is centred.
        assert!(maps.sigma > 0.0);
    }

    #[test]
    fn a_change_of_constellation_starts_the_maps_afresh() {
        let mut maps = ErrorMaps::default();
        maps.add(&snapshot(Modulation::Qpsk, vec![Iq::new(0.7, 0.7); 20]));
        assert_eq!(maps.sum.len(), 4);
        maps.add(&snapshot(Modulation::Qam64, vec![Iq::new(0.1, 0.1); 20]));
        assert_eq!(maps.sum.len(), 64);
    }
}
