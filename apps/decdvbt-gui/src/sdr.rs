// made by CasualArclamp
//! The SDR settings window: the radio's gains and a button that finds the best
//! ones, the corrections made to its samples (DC offset, I/Q imbalance), the
//! baseband filter, the frequency correction and the automatic centring on a
//! carrier that is not where the channel plan puts it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use decdvbt_engine::FrontEndReading;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, RichText, Sense, Stroke, Ui, pos2, vec2,
};

use crate::prefs;
use crate::source::{AirGains, Gains, Preset};

/// Baseband filter bandwidths the radio has that are worth offering, MHz.
pub const FILTERS_MHZ: [f32; 13] = [
    2.5, 3.5, 5.0, 5.5, 6.0, 7.0, 8.0, 9.0, 10.0, 12.0, 14.0, 15.0, 20.0,
];

/// What is set besides the gains. Remembered between runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SdrSettings {
    /// Take the DC offset off the samples (it costs nothing).
    pub dc_removal: bool,
    /// Take the I/Q image off the samples (a pass over every sample).
    pub iq_balance: bool,
    /// The radio's baseband filter, MHz; `None`: automatic, 0.75 of the
    /// sample rate.
    pub filter_mhz: Option<f32>,
    /// The radio's frequency error, ppm (positive: it runs high).
    pub ppm: f32,
    /// Retune to a carrier found off its channel's centre, and remember the
    /// offset for the channel.
    pub auto_centre: bool,
}

impl Default for SdrSettings {
    fn default() -> Self {
        SdrSettings {
            dc_removal: true,
            iq_balance: false,
            filter_mhz: None,
            ppm: 0.0,
            auto_centre: true,
        }
    }
}

impl SdrSettings {
    pub fn load() -> SdrSettings {
        let d = SdrSettings::default();
        let flag = |k: &str, default: bool| prefs::get(k).map_or(default, |v| v == "1");
        SdrSettings {
            dc_removal: flag("sdr_dc", d.dc_removal),
            iq_balance: flag("sdr_iq", d.iq_balance),
            filter_mhz: prefs::get("sdr_filter").and_then(|v| v.parse().ok()),
            ppm: prefs::get("sdr_ppm")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.ppm),
            auto_centre: flag("auto_centre", d.auto_centre),
        }
    }

    pub fn save(&self) {
        let b = |v: bool| if v { "1" } else { "0" };
        prefs::set("sdr_dc", b(self.dc_removal));
        prefs::set("sdr_iq", b(self.iq_balance));
        prefs::set(
            "sdr_filter",
            &self.filter_mhz.map_or(String::new(), |f| f.to_string()),
        );
        prefs::set("sdr_ppm", &format!("{:.2}", self.ppm));
        prefs::set("auto_centre", b(self.auto_centre));
    }
}

// ---- the gain optimiser -------------------------------------------------

/// Time given to a setting to take hold (the radio's buffers, the equaliser)
/// and then to measure it, per point.
const SETTLE: Duration = Duration::from_millis(800);
const MEASURE: Duration = Duration::from_millis(1300);
/// Total gain (LNA + VGA), dB: where the sweep stops, its step and the most
/// points it makes.
const MAX_TOTAL: u16 = 90;
const STEP: u16 = 4;
const MAX_SWEEP_POINTS: usize = 24;
/// A point this far under the best so far, twice running, is past the peak.
const PAST_PEAK_DB: f32 = 3.0;
/// Clipping over this share of samples, or a level over this, spoils a point.
/// An OFDM signal's peaks are about 10 dB over its rms: at -12 dBFS they stay
/// off the converter's limit, with a little over for the signal to fade up.
const CLIP_PERCENT: f32 = 0.05;
const TOO_HOT_DBFS: f32 = -12.0;
/// Points this close to the best figure are as good as it (the measure
/// wanders by about this much); of them the coolest is chosen.
const NEAR_BEST_DB: f32 = 0.3;

/// What the optimiser looks at, each frame.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    pub now: Instant,
    /// The receiver's MER, while it is decoding.
    pub mer: Option<f32>,
    /// The spectrum's C/N, when it has one (for a signal the receiver cannot
    /// lock to).
    pub cn: Option<f32>,
    pub rms_dbfs: f32,
    pub clip_percent: f32,
}

/// One gain setting tried.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub gains: Gains,
    pub mer: Option<f32>,
    pub cn: Option<f32>,
    pub rms_dbfs: f32,
    /// Clipped or too hot: not a candidate.
    pub spoiled: bool,
}

impl Point {
    pub fn total(&self) -> u16 {
        self.gains.lna + self.gains.vga
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Total gain up in steps from well under the present one.
    Sweep,
    /// Other splits of the best total between the LNA and the VGA.
    Split,
}

pub enum Action {
    Wait,
    /// Set these gains (the optimiser is waiting on them).
    Apply(Gains),
    /// Done: the best gains found, or `None` if nothing was measurable (the
    /// original ones are to be put back).
    Finish(Option<Gains>),
}

/// Finds the gains that give the best signal-to-noise ratio on the signal the
/// radio is tuned to: a sweep of the total gain, then the way it is split
/// between the LNA (a better noise figure, but it overloads sooner) and the
/// VGA (adds noise as well as gain). Each point takes 2 s; a sweep is about
/// twenty.
pub struct Optimiser {
    pub original: Gains,
    pub points: Vec<Point>,
    pub current: Gains,
    stage: Stage,
    queue: Vec<Gains>,
    next_total: u16,
    changed_at: Option<Instant>,
    mer: (f32, u32),
    cn: (f32, u32),
    samples: u32,
    settle: Duration,
    measure: Duration,
}

/// The LNA first (to 40 dB), the VGA for the rest.
fn split(amp: bool, total: u16) -> Gains {
    let lna = (total / 8 * 8).min(40);
    Gains {
        amp,
        lna,
        vga: (total - lna).min(62) / 2 * 2,
    }
}

impl Optimiser {
    pub fn new(original: Gains) -> Optimiser {
        Optimiser::with_times(original, SETTLE, MEASURE)
    }

    fn with_times(original: Gains, settle: Duration, measure: Duration) -> Optimiser {
        let start = (original.lna + original.vga)
            .saturating_sub(20)
            .clamp(8, 70)
            / STEP
            * STEP;
        Optimiser {
            original,
            points: Vec::new(),
            current: split(original.amp, start),
            stage: Stage::Sweep,
            queue: Vec::new(),
            next_total: start,
            changed_at: None,
            mer: (0.0, 0),
            cn: (0.0, 0),
            samples: 0,
            settle,
            measure,
        }
    }

    /// Points measured so far.
    pub fn done(&self) -> usize {
        self.points.len()
    }

    /// The point with the best figure by the one measure that most points
    /// have (the MER if the receiver read one anywhere, else the C/N); of
    /// those within [`NEAR_BEST_DB`] of it, the one with the most headroom.
    pub fn best(&self) -> Option<Point> {
        let scored: Vec<(f32, Point)> = self
            .points
            .iter()
            .filter_map(|p| Some((self.figure(p)?, *p)))
            .collect();
        let top = scored.iter().map(|(v, _)| *v).max_by(f32::total_cmp)?;
        scored
            .into_iter()
            .filter(|(v, _)| *v >= top - NEAR_BEST_DB)
            .min_by(|a, b| a.1.rms_dbfs.total_cmp(&b.1.rms_dbfs))
            .map(|(_, p)| p)
    }

    fn figure(&self, p: &Point) -> Option<f32> {
        let use_mer = self.points.iter().any(|p| !p.spoiled && p.mer.is_some());
        if p.spoiled {
            None
        } else if use_mer {
            p.mer
        } else {
            p.cn
        }
    }

    pub fn tick(&mut self, r: &Reading) -> Action {
        let Some(t0) = self.changed_at else {
            self.changed_at = Some(r.now);
            return Action::Apply(self.current);
        };
        let elapsed = r.now.saturating_duration_since(t0);
        if elapsed < self.settle {
            return Action::Wait;
        }
        if elapsed < self.settle + self.measure {
            if let Some(m) = r.mer {
                self.mer = (self.mer.0 + m, self.mer.1 + 1);
            }
            if let Some(c) = r.cn {
                self.cn = (self.cn.0 + c, self.cn.1 + 1);
            }
            self.samples += 1;
            return Action::Wait;
        }
        // The point is measured. The level and the clipping are the last
        // second's, all of it after the change.
        let mean = |(s, n): (f32, u32)| (n > 0).then(|| s / n as f32);
        let locked = self.mer.1 * 2 >= self.samples.max(1);
        let point = Point {
            gains: self.current,
            mer: mean(self.mer).filter(|_| locked),
            cn: mean(self.cn),
            rms_dbfs: r.rms_dbfs,
            spoiled: r.clip_percent > CLIP_PERCENT || r.rms_dbfs > TOO_HOT_DBFS,
        };
        self.points.push(point);
        self.mer = (0.0, 0);
        self.cn = (0.0, 0);
        self.samples = 0;
        self.changed_at = None;
        self.next(point)
    }

    fn next(&mut self, last: Point) -> Action {
        match self.stage {
            Stage::Sweep => {
                let best = self.best();
                let past_peak = {
                    let v: Vec<f32> = self.points.iter().filter_map(|p| self.figure(p)).collect();
                    let top = best.and_then(|b| self.figure(&b));
                    match (top, v.len()) {
                        (Some(top), n) if n >= 3 => {
                            v[n - 2..].iter().all(|x| *x < top - PAST_PEAK_DB)
                        }
                        _ => false,
                    }
                };
                let done = last.spoiled
                    || past_peak
                    || self.next_total + STEP > MAX_TOTAL
                    || self.points.len() >= MAX_SWEEP_POINTS;
                if !done {
                    self.next_total += STEP;
                    self.current = split(self.original.amp, self.next_total);
                    return Action::Wait;
                }
                // On to the splits of the best total.
                let Some(best) = best else {
                    return Action::Finish(None);
                };
                let total = best.total();
                self.queue = [16u16, 24, 32, 40]
                    .into_iter()
                    .filter(|&lna| lna != best.gains.lna && lna <= total && total - lna <= 62)
                    .map(|lna| Gains {
                        amp: self.original.amp,
                        lna,
                        vga: (total - lna) / 2 * 2,
                    })
                    .collect();
                self.stage = Stage::Split;
                self.next_split()
            }
            Stage::Split => self.next_split(),
        }
    }

    fn next_split(&mut self) -> Action {
        match self.queue.pop() {
            Some(g) => {
                self.current = g;
                Action::Wait
            }
            None => Action::Finish(self.best().map(|p| p.gains)),
        }
    }
}

/// The Airspy's gain controls: the libairspy preset (kind and step) and the
/// three stages it sets, which can then be changed by hand. True if a stage
/// changed.
pub fn airspy_gain_row(ui: &mut Ui, air: &mut AirGains, preset: &mut Option<Preset>) -> bool {
    let before = *air;
    // Choosing or stepping a preset sets the stages; changing a stage makes
    // the setting "custom".
    let mut p = preset.unwrap_or(Preset::DEFAULT);
    let mut from_preset = false;
    ui.label("preset");
    egui::ComboBox::from_id_salt("air_preset_kind")
        .selected_text(match (*preset, p.linearity) {
            (None, _) => "custom",
            (_, true) => "linearity",
            (_, false) => "sensitivity",
        })
        .width(96.0)
        .show_ui(ui, |ui| {
            from_preset |= ui
                .selectable_value(&mut p.linearity, false, "sensitivity")
                .changed();
            from_preset |= ui
                .selectable_value(&mut p.linearity, true, "linearity")
                .changed();
        })
        .response
        .on_hover_text(
            "libairspy's gain presets: sensitivity favours weak signals, linearity strong ones",
        );
    from_preset |= ui
        .add(egui::DragValue::new(&mut p.step).range(0..=21).speed(0.1))
        .on_hover_text("preset step, 0 (least gain) to 21 (most)")
        .changed();
    if from_preset {
        *preset = Some(p);
        *air = p.gains();
    }
    ui.label("LNA");
    ui.add(egui::DragValue::new(&mut air.lna).range(0..=14).speed(0.1));
    ui.label("mixer");
    ui.add(
        egui::DragValue::new(&mut air.mixer)
            .range(0..=15)
            .speed(0.1),
    );
    ui.label("VGA");
    ui.add(egui::DragValue::new(&mut air.vga).range(0..=15).speed(0.1));
    if !from_preset && *air != before {
        *preset = None;
    }
    *air != before
}

// ---- the window -----------------------------------------------------------

/// What the window edits and shows.
pub struct SdrView<'a> {
    pub settings: &'a mut SdrSettings,
    pub gains: &'a mut Gains,
    /// The Airspy's stages, its preset and packing; shown instead of the
    /// HackRF's gains when `airspy`.
    pub air: &'a mut AirGains,
    pub air_preset: &'a mut Option<Preset>,
    pub air_packed: &'a mut bool,
    pub airspy: bool,
    pub live: bool,
    pub front_end: FrontEndReading,
    pub optimiser: Option<&'a Optimiser>,
    /// The last optimisation's result, for display.
    pub last_result: Option<&'a str>,
    pub offsets: &'a HashMap<u8, f64>,
}

/// What the window asks for.
#[derive(Default)]
pub struct SdrAsks {
    pub gains_changed: bool,
    /// The stream has to be started again (the Airspy's transfer format).
    pub restart: bool,
    pub optimise: bool,
    pub cancel: bool,
    pub clear_offsets: bool,
    pub reset: bool,
}

pub fn show(ctx: &egui::Context, open: &mut bool, v: SdrView) -> SdrAsks {
    let mut asks = SdrAsks::default();
    egui::Window::new("SDR settings")
        .id(egui::Id::new("sdr_window"))
        .open(open)
        .default_width(430.0)
        .default_pos(pos2(12.0, 96.0))
        .resizable(true)
        .show(ctx, |ui| body(ui, v, &mut asks));
    asks
}

fn body(ui: &mut Ui, v: SdrView, asks: &mut SdrAsks) {
    let SdrView {
        settings,
        gains,
        air,
        air_preset,
        air_packed,
        airspy,
        live,
        front_end,
        optimiser,
        last_result,
        offsets,
    } = v;
    if airspy {
        ui.add_enabled_ui(live, |ui| {
            ui.label(RichText::new("Gain").strong());
            ui.horizontal_wrapped(|ui| {
                asks.gains_changed |= airspy_gain_row(ui, air, air_preset);
            });
            ui.label(
                RichText::new(
                    "Both AGCs are off. The presets are libairspy's; sensitivity 10, the default, is a first guess not yet tried on a roof.",
                )
                .small()
                .weak(),
            );
            if ui
                .checkbox(air_packed, "packed transfers (30 MB/s instead of 40)")
                .on_hover_text("Try this if the USB port drops buffers. Restarts the stream.")
                .changed()
            {
                asks.restart = true;
            }
        });
        if !live {
            ui.label(RichText::new("The radio's settings apply to a live Airspy.").weak());
        }
    }
    if !airspy {
        ui.add_enabled_ui(live, |ui| {
        ui.label(RichText::new("Gain").strong());
        egui::Grid::new("sdr_gain").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            let before = *gains;
            ui.label("LNA");
            ui.add(egui::DragValue::new(&mut gains.lna).range(0..=40).speed(0.2).suffix(" dB"));
            ui.end_row();
            ui.label("VGA");
            ui.add(egui::DragValue::new(&mut gains.vga).range(0..=62).speed(0.2).suffix(" dB"));
            ui.end_row();
            ui.label("RF amp");
            ui.checkbox(&mut gains.amp, "+14 dB (needed for UHF TV)");
            ui.end_row();
            gains.lna = gains.lna / 8 * 8;
            gains.vga = gains.vga / 2 * 2;
            asks.gains_changed |= *gains != before;
        });
        ui.horizontal(|ui| {
            match optimiser {
                None => {
                    if ui
                        .button("Optimise gain")
                        .on_hover_text(
                            "Steps the gain up through its range on the carrier you are tuned \\n                             to, reads the signal-to-noise ratio at each setting (the receiver's \\n                             MER, or the spectrum's C/N if it cannot lock), then tries other \\n                             splits between the LNA and the VGA, and sets the best. About \\n                             half a minute.",
                        )
                        .clicked()
                    {
                        asks.optimise = true;
                    }
                    if let Some(r) = last_result {
                        ui.label(RichText::new(r).small());
                    }
                }
                Some(o) => {
                    if ui.button("Cancel").clicked() {
                        asks.cancel = true;
                    }
                    let done = o.done();
                    ui.label(
                        RichText::new(format!(
                            "optimising: point {}, LNA {} VGA {}",
                            done + 1,
                            o.current.lna,
                            o.current.vga
                        ))
                        .color(Color32::from_rgb(255, 176, 40)),
                    );
                }
            }
        });
        if let Some(o) = optimiser {
            sweep_plot(ui, &o.points, None);
        }
    });
        if !live {
            ui.label(RichText::new("The radio's settings apply to a live HackRF.").weak());
        }
    }

    ui.add_space(6.0);
    ui.label(RichText::new("Samples").strong());
    ui.checkbox(&mut settings.dc_removal, "DC removal")
        .on_hover_text(
            "Takes the radio's DC offset (the spike at the centre of the band) off the samples. \\n             It costs nothing.",
        );
    ui.label(
        RichText::new(format!("    offset now {:.0} dBFS", front_end.dc_dbfs))
            .small()
            .weak(),
    );
    ui.checkbox(&mut settings.iq_balance, "I/Q balance")
        .on_hover_text(
            "Takes the mirror image the radio's I and Q paths leave across the centre of the \\n             band off the samples (blind, from the signal's circularity). A pass over every \\n             sample, about 1 % of a core.",
        );
    ui.label(
        RichText::new(format!(
            "    image {:.0} dB under the signal{}",
            front_end.image_rejection_db,
            if front_end.image_rejection_db < 40.0 && !settings.iq_balance {
                " — worth turning on"
            } else {
                ""
            }
        ))
        .small()
        .weak(),
    );

    ui.add_space(6.0);
    // The baseband filter and clock trim are the HackRF's.
    ui.add_enabled_ui(live && !airspy, |ui| {
        ui.label(RichText::new(if airspy { "Radio (HackRF only)" } else { "Radio" }).strong());
        egui::Grid::new("sdr_radio").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
            ui.label("baseband filter");
            let label = settings.filter_mhz.map_or("auto".to_string(), |f| format!("{f} MHz"));
            egui::ComboBox::from_id_salt("sdr_filter")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut settings.filter_mhz, None, "auto (0.75 of the rate)");
                    for f in FILTERS_MHZ {
                        ui.selectable_value(&mut settings.filter_mhz, Some(f), format!("{f} MHz"));
                    }
                })
                .response
                .on_hover_text(
                    "The radio's analogue filter ahead of the converter. Auto is 7.5 MHz at 10 MS/s, \\n                     which leaves the edges of an 8 MHz channel on its slope; 9 or 10 MHz keeps them \\n                     flat but lets more of the neighbours' edges in.",
                );
            ui.end_row();
            ui.label("frequency correction");
            ui.add(
                egui::DragValue::new(&mut settings.ppm)
                    .range(-200.0..=200.0)
                    .speed(0.05)
                    .max_decimals(2)
                    .suffix(" ppm"),
            )
            .on_hover_text(
                "The radio's clock error: positive if it runs high. The receiver follows the \\n                 carrier whatever this is; it makes the frequency shown true.",
            );
            ui.end_row();
        });
    });

    ui.add_space(6.0);
    ui.label(RichText::new("Carriers off their channel").strong());
    ui.checkbox(&mut settings.auto_centre, "centre on the carrier")
        .on_hover_text(
            "UK muxes are transmitted up to a third of a megahertz off their channel's centre. \\n             After tuning, the occupied band is found in the spectrum and the radio retuned to \\n             its middle; the offset is remembered for the channel.",
        );
    if offsets.is_empty() {
        ui.label(RichText::new("    none learned yet").small().weak());
    } else {
        let mut list: Vec<_> = offsets.iter().collect();
        list.sort_by_key(|(c, _)| **c);
        let text: Vec<String> = list
            .iter()
            .map(|(c, o)| format!("C{c} {:+.0} kHz", **o / 1e3))
            .collect();
        ui.label(RichText::new(format!("    {}", text.join("  "))).small());
        if ui.small_button("Forget them").clicked() {
            asks.clear_offsets = true;
        }
    }
    ui.add_space(6.0);
    if ui.button("Defaults").clicked() {
        asks.reset = true;
    }
}

/// Figure of merit against total gain: the points, a line through them and the
/// best marked.
pub fn sweep_plot(ui: &mut Ui, points: &[Point], best: Option<Point>) {
    let (resp, p) =
        ui.allocate_painter(vec2(ui.available_width().min(420.0), 110.0), Sense::hover());
    let rect = resp.rect;
    p.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(8, 8, 10));
    let plot = egui::Rect::from_min_max(
        pos2(rect.left() + 36.0, rect.top() + 8.0),
        pos2(rect.right() - 8.0, rect.bottom() - 16.0),
    );
    let font = FontId::monospace(10.0);
    let val = |q: &Point| if q.spoiled { None } else { q.mer.or(q.cn) };
    let valid: Vec<(f32, f32)> = points
        .iter()
        .filter_map(|q| val(q).map(|v| (f32::from(q.total()), v)))
        .collect();
    if valid.is_empty() {
        p.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "measuring…",
            font,
            Color32::GRAY,
        );
        return;
    }
    let (x0, x1) = (
        valid.iter().map(|v| v.0).fold(f32::MAX, f32::min) - 2.0,
        valid.iter().map(|v| v.0).fold(f32::MIN, f32::max) + 2.0,
    );
    let (y0, y1) = (
        valid.iter().map(|v| v.1).fold(f32::MAX, f32::min) - 1.0,
        valid.iter().map(|v| v.1).fold(f32::MIN, f32::max) + 1.0,
    );
    let to = |x: f32, y: f32| -> Pos2 {
        pos2(
            plot.left() + (x - x0) / (x1 - x0).max(1.0) * plot.width(),
            plot.bottom() - (y - y0) / (y1 - y0).max(1.0) * plot.height(),
        )
    };
    p.text(
        rect.left_top() + vec2(36.0, -1.0),
        Align2::LEFT_TOP,
        "dB vs total gain",
        font.clone(),
        Color32::GRAY,
    );
    for (lo, label) in [(y0, format!("{y0:.0}")), (y1, format!("{y1:.0}"))] {
        p.text(
            pos2(plot.left() - 4.0, to(x0, lo).y),
            Align2::RIGHT_CENTER,
            label,
            font.clone(),
            Color32::GRAY,
        );
    }
    p.text(
        pos2(plot.left(), plot.bottom() + 2.0),
        Align2::LEFT_TOP,
        format!("{x0:.0}"),
        font.clone(),
        Color32::GRAY,
    );
    p.text(
        pos2(plot.right(), plot.bottom() + 2.0),
        Align2::RIGHT_TOP,
        format!("{x1:.0}"),
        font,
        Color32::GRAY,
    );
    let mut sorted = valid.clone();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
    p.add(egui::Shape::line(
        sorted.iter().map(|&(x, y)| to(x, y)).collect(),
        Stroke::new(1.0, Color32::from_rgb(110, 200, 255)),
    ));
    for &(x, y) in &valid {
        p.circle_filled(to(x, y), 2.5, Color32::from_rgb(110, 200, 255));
    }
    for q in points.iter().filter(|q| q.spoiled) {
        let x = to(f32::from(q.total()), y0).x;
        p.line_segment(
            [pos2(x, plot.top()), pos2(x, plot.bottom())],
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 90, 80, 120)),
        );
    }
    if let Some(b) = best.and_then(|b| val(&b).map(|v| (f32::from(b.total()), v))) {
        p.circle_stroke(
            to(b.0, b.1),
            5.0,
            Stroke::new(1.5, Color32::from_rgb(110, 220, 130)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A radio whose figure of merit rises with total gain up to 52 dB (the
    /// converter's noise matters less and less), is a little better with the
    /// LNA at 32, and clips from 66.
    fn radio(g: Gains) -> (f32, f32, bool) {
        let total = f32::from(g.lna + g.vga);
        let mer = 24.0 - 0.06 * (total - 52.0).powi(2) - 0.15 * (f32::from(g.lna) - 32.0).abs();
        let rms = -60.0 + 0.8 * total;
        (mer, rms, total >= 66.0)
    }

    fn run(start: Gains) -> (Optimiser, Option<Gains>) {
        let t0 = Instant::now();
        let mut o =
            Optimiser::with_times(start, Duration::from_millis(10), Duration::from_millis(10));
        let mut now = t0;
        let mut applied = start;
        for _ in 0..2000 {
            now += Duration::from_millis(5);
            let (mer, rms, clip) = radio(applied);
            let r = Reading {
                now,
                mer: Some(mer),
                cn: None,
                rms_dbfs: rms,
                clip_percent: if clip { 1.0 } else { 0.0 },
            };
            match o.tick(&r) {
                Action::Wait => {}
                Action::Apply(g) => applied = g,
                Action::Finish(best) => return (o, best),
            }
        }
        panic!("did not finish");
    }

    #[test]
    fn the_best_split_of_the_best_total_is_found() {
        let (o, best) = run(Gains {
            amp: true,
            lna: 16,
            vga: 8,
        });
        let best = best.expect("a best");
        // The peak is at 52 total with the LNA at 32: 32 + 20.
        assert_eq!(
            (best.lna, best.vga),
            (32, 20),
            "{:?}",
            o.points
                .iter()
                .map(|p| (p.gains.lna, p.gains.vga, p.mer))
                .collect::<Vec<_>>()
        );
        assert!(best.amp);
        // It stopped before the clipping and did not try every total.
        assert!(o.points.len() < 24);
    }

    #[test]
    fn a_figure_that_keeps_rising_stops_short_of_the_converter() {
        // Better with every dB of gain, to the end: the headroom stops it.
        let t0 = Instant::now();
        let mut o = Optimiser::with_times(
            Gains {
                amp: true,
                lna: 16,
                vga: 8,
            },
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        let (mut now, mut applied) = (t0, o.current);
        for _ in 0..4000 {
            now += Duration::from_millis(5);
            let total = f32::from(applied.lna + applied.vga);
            let r = Reading {
                now,
                mer: Some(20.0 + 0.05 * total),
                cn: None,
                rms_dbfs: -60.0 + 0.8 * total,
                clip_percent: 0.0,
            };
            match o.tick(&r) {
                Action::Wait => {}
                Action::Apply(g) => applied = g,
                Action::Finish(chosen) => {
                    let point = o.best().expect("a best");
                    assert_eq!(chosen, Some(point.gains));
                    assert!(point.rms_dbfs <= TOO_HOT_DBFS, "{}", point.rms_dbfs);
                    // Not the 8 dB cooler point that is a clear 0.4 dB worse.
                    assert!(point.total() >= 52, "{}", point.total());
                    return;
                }
            }
        }
        panic!("did not finish");
    }

    #[test]
    fn a_radio_with_nothing_to_measure_gives_nothing() {
        let t0 = Instant::now();
        let mut o = Optimiser::with_times(
            Gains::default(),
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        let mut now = t0;
        for _ in 0..4000 {
            now += Duration::from_millis(5);
            let r = Reading {
                now,
                mer: None,
                cn: None,
                rms_dbfs: -40.0,
                clip_percent: 0.0,
            };
            if let Action::Finish(best) = o.tick(&r) {
                assert!(best.is_none());
                return;
            }
        }
        panic!("did not finish");
    }

    #[test]
    fn the_lna_comes_first_in_a_split() {
        assert_eq!(
            split(true, 16),
            Gains {
                amp: true,
                lna: 16,
                vga: 0
            }
        );
        assert_eq!(
            split(true, 52),
            Gains {
                amp: true,
                lna: 40,
                vga: 12
            }
        );
        assert_eq!(
            split(false, 20),
            Gains {
                amp: false,
                lna: 16,
                vga: 4
            }
        );
    }
}
