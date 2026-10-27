// made by CasualArclamp
//! The signal card: the receiver's headline, at the top of the right-hand
//! panel. Laid out as DecSAT's DVB card (`side_panel::dvb_card`): a title bar
//! in the standard's colour with the frequency and channel, the mode being
//! received big with the margin beside it, and a C/N bar ticked at the C/N
//! that mode needs.
//!
//! The margin is the effective SNR (what the cells carry, as the SNR of a
//! Gaussian channel that carries as much: `decdvbt_rx::capacity`) less the
//! C/N the mode needs for quasi error free reception over a Gaussian
//! channel. The band's mean SNR (the MER row) would flatter a channel with
//! notches. Until the receiver reads the mode and measures, the card shows
//! the spectral C/N estimate (`decdvbt_engine::snr`) and no mode, so no
//! margin.

use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Ui, pos2, vec2};

// Each standard's colour, and a margin's health, in DecSAT's palette: DVB-T
// takes the yellow DecSAT gives DVB-S (the older standard), DVB-T2 the green
// of DVB-S2.
const DVB_T: Color32 = Color32::from_rgb(240, 230, 90);
const DVB_T2: Color32 = Color32::from_rgb(110, 220, 110);
const HEALTH_OK: Color32 = Color32::from_rgb(110, 220, 110);
const HEALTH_WARN: Color32 = Color32::from_rgb(240, 200, 80);
const HEALTH_BAD: Color32 = Color32::from_rgb(230, 110, 110);
/// The bar before there is a mode to give it a health.
const NEUTRAL: Color32 = Color32::from_rgb(100, 170, 255);
/// Clear of the side panel's scroll bar, which egui floats over the content.
const RIGHT_PAD: f32 = 6.0;

/// The C/N scale of the bar, dB: fixed, so it does not jump as the reading
/// moves. 64-QAM 7/8 needs 20.1 dB; 35 leaves room for a strong signal.
const BAR_DB: (f32, f32) = (0.0, 35.0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standard {
    DvbT,
    /// Named by the T2 receiver (M6); the card is ready for it.
    #[allow(dead_code)]
    DvbT2,
}

impl Standard {
    fn name(self) -> &'static str {
        match self {
            Standard::DvbT => "DVB-T",
            Standard::DvbT2 => "DVB-T2",
        }
    }

    fn colour(self) -> Color32 {
        match self {
            Standard::DvbT => DVB_T,
            Standard::DvbT2 => DVB_T2,
        }
    }
}

/// What the card shows.
#[derive(Default, Clone)]
pub struct Card {
    /// `None` until the receiver knows which.
    pub standard: Option<Standard>,
    /// The mode's other parameters, as label / value pairs (FFT size, guard
    /// interval, bit rate…), shown under the bar; empty until they are known.
    pub details: Vec<(String, String)>,
    /// The mode is the user's assumption, not read from the signal.
    pub assumed: bool,
    /// "64-QAM 3/4", the constellation and rate being received.
    pub name: Option<String>,
    /// The C/N that mode needs, dB.
    pub qef_db: Option<f32>,
    pub cn_db: Option<f32>,
    /// Where the C/N came from: "spectrum", or "MER" once measured.
    pub cn_source: &'static str,
    pub channel: Option<u8>,
    pub freq_hz: Option<f64>,
    /// A source is running.
    pub active: bool,
}

/// A margin's colour: red below 0 dB, amber within 1 dB, else green.
fn margin_colour(margin: Option<f32>) -> Color32 {
    match margin {
        Some(m) if m < 0.0 => HEALTH_BAD,
        Some(m) if m < 1.0 => HEALTH_WARN,
        _ => HEALTH_OK,
    }
}

pub fn show(ui: &mut Ui, c: &Card) {
    let grey = ui.visuals().weak_text_color();
    let strong = ui.visuals().strong_text_color();
    let colour = c.standard.map_or(grey, Standard::colour);
    // To the tenth shown (and + 0.0 turns a −0.0 into 0.0), so that the colour
    // agrees with the figure.
    let margin = c
        .cn_db
        .zip(c.qef_db)
        .map(|(cn, q)| ((cn - q) * 10.0).round() / 10.0 + 0.0);
    let health = margin.map(|m| margin_colour(Some(m)));

    // ---- the title bar: standard · mode, and where it is
    let title = match c.standard {
        Some(s) if c.assumed => format!("{} · assumed", s.name()),
        Some(s) => s.name().to_string(),
        None => "DVB-T / T2".to_string(),
    };
    let bar = egui::Frame::new()
        .fill(colour.gamma_multiply(0.15))
        .corner_radius(CornerRadius {
            nw: 4,
            ne: 4,
            sw: 0,
            se: 0,
        })
        .inner_margin(egui::Margin {
            left: 8,
            right: 8 + RIGHT_PAD as i8,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                // The right-hand figures are laid out first (right to left)
                // in what is left, so the title is the one to give way.
                let right = ui.available_width().min(190.0);
                ui.scope(|ui| {
                    ui.set_max_width((ui.available_width() - right).max(40.0));
                    ui.add(
                        egui::Label::new(
                            RichText::new(title)
                                .monospace()
                                .size(14.0)
                                .strong()
                                .color(colour),
                        )
                        .truncate(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ch) = c.channel {
                        ui.label(
                            RichText::new(format!("C{ch}"))
                                .monospace()
                                .size(20.0)
                                .color(strong),
                        );
                    }
                    if let Some(f) = c.freq_hz {
                        ui.add_space(10.0);
                        ui.label(RichText::new("MHz").size(11.0).weak());
                        ui.label(
                            RichText::new(format!("{:.3}", f / 1e6))
                                .monospace()
                                .size(16.0)
                                .color(if c.name.is_some() { strong } else { grey }),
                        );
                    }
                });
            });
        })
        .response;
    ui.painter().hline(
        bar.rect.x_range(),
        bar.rect.bottom(),
        egui::Stroke::new(2.0, colour),
    );
    ui.add_space(4.0);

    // ---- the mode and its margin
    let resp = ui
        .horizontal(|ui| {
            ui.label(
                RichText::new(c.name.as_deref().unwrap_or("–"))
                    .monospace()
                    .size(26.0)
                    .color(if c.name.is_some() { colour } else { grey }),
            );
            if let (Some(m), Some(health)) = (margin, health) {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(RIGHT_PAD);
                    ui.label(
                        RichText::new(format!("{m:+5.1} dB"))
                            .monospace()
                            .size(18.0)
                            .color(health),
                    );
                });
            }
        })
        .response;
    resp.on_hover_text(
        "Margin: the effective SNR less the C/N this mode needs to be quasi error \
         free over a Gaussian channel (EN 300 744 Table A.1 for DVB-T). The \
         effective SNR is what a Gaussian channel would need to carry what the \
         cells carry, carrier by carrier: on a channel with notches it is below \
         the mean (the MER row), because the faded carriers' bits are where the \
         errors are. Below 0 dB the FEC fails; within 1 dB a fade drops \
         packets.\n\nUntil the receiver reads \
         the mode, there is no margin: the bar shows a C/N estimated from the \
         spectrum (the block against the empty guard beside it).",
    );

    // ---- the C/N bar, ticked at what the mode needs
    let (lo, hi) = BAR_DB;
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width() - RIGHT_PAD, 10.0), Sense::hover());
    let x_of = |db: f32| rect.left() + rect.width() * ((db - lo) / (hi - lo)).clamp(0.0, 1.0);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), ui.visuals().extreme_bg_color);
    if let Some(cn) = c.cn_db {
        let fill = egui::Rect::from_min_max(rect.min, pos2(x_of(cn), rect.max.y));
        p.rect_filled(
            fill,
            CornerRadius::same(2),
            health.unwrap_or(NEUTRAL.gamma_multiply(0.8)),
        );
    }
    if let Some(q) = c.qef_db {
        let x = x_of(q);
        p.rect_filled(
            egui::Rect::from_min_max(
                pos2(x - 1.0, rect.top() - 3.0),
                pos2(x + 1.0, rect.bottom() + 3.0),
            ),
            CornerRadius::ZERO,
            strong,
        );
    }
    ui.horizontal(|ui| {
        let left = match (c.cn_db, c.active) {
            (Some(cn), _) => format!("C/N {cn:.1} dB ({})", c.cn_source),
            (None, true) => "no signal in this channel".into(),
            (None, false) => "no source running".into(),
        };
        ui.label(RichText::new(left).size(12.0).weak());
        if let Some(q) = c.qef_db {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(RIGHT_PAD);
                ui.label(RichText::new(format!("QEF {q:.1} dB")).size(12.0).weak());
            });
        }
    });

    // ---- the rest of the mode: pairs of label and value, two pairs a row
    ui.add_space(6.0);
    if c.details.is_empty() {
        if c.active {
            ui.label(
                RichText::new(
                    "FFT size, guard interval and code rate are read from the TPS once                      the receiver locks.",
                )
                .size(12.0)
                .weak(),
            );
        }
        return;
    }
    egui::Grid::new("card_details")
        .num_columns(4)
        .spacing([10.0, 3.0])
        .show(ui, |ui| {
            for pair in c.details.chunks(2) {
                for (k, v) in pair {
                    ui.label(RichText::new(k).size(12.0).weak());
                    ui.label(RichText::new(v).size(12.0).color(strong));
                }
                ui.end_row();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_colours_follow_decsat() {
        assert_eq!(margin_colour(Some(-0.1)), HEALTH_BAD);
        assert_eq!(margin_colour(Some(0.0)), HEALTH_WARN);
        assert_eq!(margin_colour(Some(0.9)), HEALTH_WARN);
        assert_eq!(margin_colour(Some(1.0)), HEALTH_OK);
        assert_eq!(margin_colour(None), HEALTH_OK);
    }
}
