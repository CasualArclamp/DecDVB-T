// made by CasualArclamp
//! The band scan: the radio steps over the UK UHF channels, a short average
//! of its spectrum is kept for each, and the pieces are stitched into one wide,
//! short trace along the top of the window with each channel's number on its
//! carrier. The channel being received is bracketed; a click tunes one.
//!
//! The scan borrows the live radio: it retunes it channel by channel, reads
//! the rows the engine already makes (nothing new is computed per sample), and
//! puts the radio back on the channel it was on. It does not touch the gains,
//! so a strong signal that does not overload the receiver does not overload
//! the scan.

use std::time::{Duration, Instant};

use decdvbt_core::channel::{uhf_centre_hz, uk_channels};
use decdvbt_engine::Row;
use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Shape, Stroke, pos2, vec2,
};

/// Columns kept across each channel's 8 MHz.
const COLS: usize = 32;
/// Half the width kept for a channel, Hz (the raster's 8 MHz).
const HALF_HZ: f64 = 4.0e6;
/// Rows thrown away after a retune (the radio and the engine's filters
/// settle), and rows averaged.
const SKIP_ROWS: u32 = 4;
const KEEP_ROWS: u32 = 8;
/// A channel that gives no rows in this long is left empty.
const GIVE_UP: Duration = Duration::from_millis(2500);
/// How close the row's centre must be to the channel's for the row to count.
const CENTRE_TOL_HZ: f64 = 100e3;

const AMBER: Color32 = Color32::from_rgb(255, 176, 40);
const TRACE: Color32 = Color32::from_rgb(150, 205, 255);

/// What the app is to do after feeding the scan some rows.
pub enum Step {
    Idle,
    /// Retune the radio to this centre, Hz.
    Retune(f64),
    /// The scan is over: put the radio back on this channel.
    Done(u8),
}

struct Run {
    chans: Vec<u8>,
    at: usize,
    /// Linear power per column, summed, and the rows in it.
    acc: [f32; COLS],
    kept: u32,
    skipped: u32,
    since: Instant,
    restore: u8,
    new: Vec<(u8, Vec<f32>)>,
}

#[derive(Default)]
pub struct BandScan {
    /// The last scan: channel, dBFS per column (`None`-like empty: no data).
    strip: Vec<(u8, Vec<f32>)>,
    run: Option<Run>,
}

pub enum Action {
    Tune(u8),
}

impl BandScan {
    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// Begin a scan; the radio is on channel `restore`. Returns the first
    /// centre to tune.
    pub fn start(&mut self, restore: u8) -> f64 {
        let chans: Vec<u8> = uk_channels().collect();
        let first = uhf_centre_hz(chans[0]).unwrap_or(0.0);
        self.run = Some(Run {
            chans,
            at: 0,
            acc: [0.0; COLS],
            kept: 0,
            skipped: 0,
            since: Instant::now(),
            restore,
            new: Vec::new(),
        });
        first
    }

    pub fn cancel(&mut self) -> Option<u8> {
        self.run.take().map(|r| r.restore)
    }

    /// The channel being scanned and how many there are.
    pub fn progress(&self) -> Option<(usize, usize)> {
        self.run.as_ref().map(|r| (r.at, r.chans.len()))
    }

    /// Give the scan this frame's rows (it keeps those from the channel it
    /// waits for).
    pub fn feed(&mut self, rows: &[Row]) -> Step {
        let Some(run) = &mut self.run else {
            return Step::Idle;
        };
        let target = uhf_centre_hz(run.chans[run.at]).unwrap_or(0.0);
        for row in rows {
            if (row.center_hz - target).abs() > CENTRE_TOL_HZ || row.db.is_empty() {
                continue;
            }
            if run.skipped < SKIP_ROWS {
                run.skipped += 1;
                continue;
            }
            add_row(&mut run.acc, row, target);
            run.kept += 1;
        }
        let done = run.kept >= KEEP_ROWS;
        if !done && run.since.elapsed() < GIVE_UP {
            return Step::Idle;
        }
        let db = if run.kept == 0 {
            Vec::new()
        } else {
            run.acc
                .iter()
                .map(|p| 10.0 * (p / run.kept as f32).max(1e-20).log10())
                .collect()
        };
        run.new.push((run.chans[run.at], db));
        run.at += 1;
        run.acc = [0.0; COLS];
        run.kept = 0;
        run.skipped = 0;
        run.since = Instant::now();
        if run.at < run.chans.len() {
            return Step::Retune(uhf_centre_hz(run.chans[run.at]).unwrap_or(0.0));
        }
        let run = self.run.take().unwrap();
        self.strip = run.new;
        Step::Done(run.restore)
    }

    /// The strip, `height` points tall across the window's width; `current`
    /// is the channel being received.
    pub fn show(&self, ui: &mut egui::Ui, height: f32, current: Option<u8>) -> Option<Action> {
        let (rect, resp) =
            ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, Color32::from_gray(14));
        let chans: Vec<u8> = uk_channels().collect();
        let (Some(&first), Some(&last)) = (chans.first(), chans.last()) else {
            return None;
        };
        let lo = uhf_centre_hz(first).unwrap_or(0.0) - HALF_HZ;
        let hi = uhf_centre_hz(last).unwrap_or(0.0) + HALF_HZ;
        let plot = Rect::from_min_max(
            pos2(rect.left() + 4.0, rect.top() + 14.0),
            pos2(rect.right() - 4.0, rect.bottom() - 2.0),
        );
        let x_of = |hz: f64| plot.left() + ((hz - lo) / (hi - lo)) as f32 * plot.width();

        if self.strip.is_empty() {
            let msg = if self.running() {
                "scanning…"
            } else {
                "Band scan: press Scan to sweep the UHF band"
            };
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                msg,
                FontId::proportional(12.0),
                Color32::from_gray(150),
            );
            return None;
        }

        // Scale: the quietest column to 6 dB over the loudest.
        let (mut dmin, mut dmax) = (f32::MAX, f32::MIN);
        for (_, d) in &self.strip {
            for &v in d {
                dmin = dmin.min(v);
                dmax = dmax.max(v);
            }
        }
        if dmin > dmax {
            return None;
        }
        let dmax = (dmax + 3.0).max(dmin + 20.0);
        let y_of =
            |db: f32| plot.bottom() - ((db - dmin) / (dmax - dmin)).clamp(0.0, 1.0) * plot.height();

        for (n, d) in &self.strip {
            let cur = current == Some(*n);
            let centre = uhf_centre_hz(*n).unwrap_or(0.0);
            let (x0, x1) = (x_of(centre - HALF_HZ), x_of(centre + HALF_HZ));
            if cur {
                p.rect_filled(
                    Rect::from_min_max(pos2(x0, rect.top()), pos2(x1, rect.bottom())),
                    0.0,
                    Color32::from_rgba_unmultiplied(255, 176, 40, 28),
                );
            }
            if d.is_empty() {
                continue;
            }
            let pts: Vec<Pos2> = d
                .iter()
                .enumerate()
                .map(|(k, &v)| {
                    let f = centre - HALF_HZ + (k as f64 + 0.5) / COLS as f64 * 2.0 * HALF_HZ;
                    pos2(x_of(f), y_of(v))
                })
                .collect();
            let mut fill = pts.clone();
            fill.push(pos2(pts[pts.len() - 1].x, plot.bottom()));
            fill.insert(0, pos2(pts[0].x, plot.bottom()));
            p.add(Shape::convex_polygon(
                fill,
                Color32::from_rgba_unmultiplied(150, 205, 255, 40),
                Stroke::NONE,
            ));
            p.add(Shape::line(
                pts,
                Stroke::new(1.0, if cur { AMBER } else { TRACE }),
            ));
            // The number sits on the carrier: above the channel's level.
            let mean = d.iter().sum::<f32>() / d.len() as f32;
            p.text(
                pos2((x0 + x1) / 2.0, rect.top() + 1.0),
                Align2::CENTER_TOP,
                format!("{n}"),
                FontId::monospace(if mean > dmin + 8.0 { 11.0 } else { 9.0 }),
                if cur {
                    AMBER
                } else if mean > dmin + 8.0 {
                    Color32::WHITE
                } else {
                    Color32::from_gray(110)
                },
            );
        }

        let mut action = None;
        if let Some(pos) = resp.hover_pos().filter(|q| rect.contains(*q)) {
            let hz = lo + ((pos.x - plot.left()) / plot.width()) as f64 * (hi - lo);
            if let Some(&n) = chans
                .iter()
                .min_by_key(|&&n| (uhf_centre_hz(n).unwrap_or(0.0) - hz).abs() as i64)
            {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                resp.clone().on_hover_text(format!(
                    "C{n}, {:.0} MHz: click to receive",
                    uhf_centre_hz(n).unwrap_or(0.0) / 1e6
                ));
                if resp.clicked() {
                    action = Some(Action::Tune(n));
                }
            }
        }
        action
    }
}

/// Add `row`'s power into the columns across the channel centred on `target`.
fn add_row(acc: &mut [f32; COLS], row: &Row, target: f64) {
    let n = row.db.len();
    let step = row.sample_rate / n as f64;
    let start = row.center_hz - row.sample_rate / 2.0 - target;
    let mut sum = [0f32; COLS];
    let mut cnt = [0u32; COLS];
    for (i, &v) in row.db.iter().enumerate() {
        let f = start + (i as f64 + 0.5) * step;
        if !(-HALF_HZ..HALF_HZ).contains(&f) {
            continue;
        }
        let c = (((f + HALF_HZ) / (2.0 * HALF_HZ)) * COLS as f64) as usize;
        let c = c.min(COLS - 1);
        sum[c] += 10f32.powf(v / 10.0);
        cnt[c] += 1;
    }
    for c in 0..COLS {
        if cnt[c] > 0 {
            acc[c] += sum[c] / cnt[c] as f32;
        }
    }
}
