// made by CasualArclamp
//! The meters panel: the fields of `TxStatus` as bars and counters, and the
//! rules for when each turns amber or red (docs/TX_DESIGN.md §7).
//!
//! - amber: the TS ring is under 25 % full, or null packets were stuffed in
//!   the last 5 s (the encoder fell behind: the receiver sees the picture
//!   pause);
//! - red: any radio underrun, any "dts < pcr" warning (the picture is too
//!   complex for the mode), the modulator over 90 % of a core, or clipped
//!   samples still rising.

use std::collections::VecDeque;

use decdvbt_tx::{TxState, TxStatus};
use eframe::egui::{self, Color32, RichText};

use crate::{AMBER, GREEN, RED};

/// How worried to be about a figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Amber,
    Red,
}

impl Level {
    pub fn colour(self, ok: Color32) -> Color32 {
        match self {
            Level::Ok => ok,
            Level::Amber => AMBER,
            Level::Red => RED,
        }
    }
}

/// How far back "rose recently" looks, seconds.
const WINDOW_S: f64 = 5.0;

/// The counters as they were over the last few seconds, to tell "rose in the
/// last 5 s" from "was once". Time is passed in (seconds), so the tests do
/// not wait.
#[derive(Debug, Default)]
pub struct Trend {
    samples: VecDeque<(f64, u64, u64)>,
}

impl Trend {
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Record the counters at time `t` (seconds, increasing).
    pub fn push(&mut self, t: f64, stuffed: u64, clipped: u64) {
        // A counter that went down is a new session.
        if self
            .samples
            .back()
            .is_some_and(|&(_, s, c)| stuffed < s || clipped < c)
        {
            self.samples.clear();
        }
        // Keep one sample per half second.
        if self.samples.back().is_none_or(|&(bt, _, _)| t - bt >= 0.5) {
            self.samples.push_back((t, stuffed, clipped));
        }
        while self
            .samples
            .front()
            .is_some_and(|&(ft, _, _)| t - ft > WINDOW_S + 1.0)
        {
            self.samples.pop_front();
        }
    }

    /// The oldest sample inside the window, as (stuffed, clipped).
    fn base(&self, now: f64) -> Option<(u64, u64)> {
        self.samples
            .iter()
            .find(|&&(t, _, _)| now - t <= WINDOW_S)
            .map(|&(_, s, c)| (s, c))
    }

    /// Null packets were stuffed within the last 5 s.
    pub fn stuffing_recent(&self, now: f64, stuffed_now: u64) -> bool {
        self.base(now).is_some_and(|(s, _)| stuffed_now > s)
    }

    /// Samples were clipped within the last 5 s.
    pub fn clipping_recent(&self, now: f64, clipped_now: u64) -> bool {
        self.base(now).is_some_and(|(_, c)| clipped_now > c)
    }
}

/// The colour classes of every meter for one status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Levels {
    pub ts_fill: Level,
    pub stuffing: Level,
    pub underruns: Level,
    pub modulator: Level,
    pub budget: Level,
    pub clipping: Level,
}

impl Levels {
    /// The worst of them (for the state strip).
    #[cfg(test)]
    pub fn worst(&self) -> Level {
        [
            self.ts_fill,
            self.stuffing,
            self.underruns,
            self.modulator,
            self.budget,
            self.clipping,
        ]
        .into_iter()
        .max()
        .unwrap_or(Level::Ok)
    }
}

pub fn classify(s: &TxStatus, trend: &Trend, now: f64) -> Levels {
    // The ring is filling in `Preparing` and draining at the end of the file:
    // a low fill only means something while the programme is flowing.
    let flowing = matches!(s.state, TxState::Ready | TxState::Transmitting);
    let stuffing_recent = trend.stuffing_recent(now, s.ts_stuffed);
    Levels {
        ts_fill: if flowing && s.ts_fill < 0.25 {
            Level::Amber
        } else {
            Level::Ok
        },
        stuffing: if stuffing_recent {
            Level::Amber
        } else {
            Level::Ok
        },
        underruns: if s.underruns > 0 {
            Level::Red
        } else {
            Level::Ok
        },
        modulator: if s.modulator_load_pct > 90.0 {
            Level::Red
        } else {
            Level::Ok
        },
        budget: if s.dts_pcr_warnings > 0 {
            Level::Red
        } else {
            Level::Ok
        },
        clipping: if trend.clipping_recent(now, s.clipped) {
            Level::Red
        } else {
            Level::Ok
        },
    }
}

/// How far `Preparing` has got, 0..1: half is the TS ring holding its prefill
/// (0.5 s of programme in a ring that holds 1.5 s), half the IQ ring full.
pub fn prepare_progress(s: &TxStatus) -> f32 {
    let ts = (s.ts_fill as f64 * decdvbt_tx::session::TS_RING_SECONDS
        / decdvbt_tx::session::TS_PREFILL_SECONDS)
        .min(1.0);
    (((ts + s.iq_fill as f64) / 2.0) as f32).clamp(0.0, 1.0)
}

/// `m:ss` or `h:mm:ss`.
pub fn hms(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, t % 3600 / 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

/// One meter row: a label, a bar (or nothing) and the texts beside it.
fn row(ui: &mut egui::Ui, label: &str, bar: Option<(f32, Color32)>, texts: Vec<RichText>) {
    ui.label(RichText::new(label).weak());
    ui.horizontal(|ui| {
        if let Some((f, colour)) = bar {
            ui.add(
                egui::ProgressBar::new(f.clamp(0.0, 1.0))
                    .fill(colour)
                    .desired_width(150.0)
                    .desired_height(12.0),
            );
        }
        ui.spacing_mut().item_spacing.x = 0.0;
        for t in texts {
            ui.label(t);
        }
    });
    ui.end_row();
}

/// What the meters panel needs besides the status.
pub struct MetersView<'a> {
    pub status: &'a TxStatus,
    pub levels: Levels,
    /// The sink is paced (the radio's clock, or a fake one) and so has
    /// "real time"; an unpaced one reports its speed instead.
    pub paced: bool,
    /// The session is up (the figures mean something).
    pub live: bool,
}

pub fn show(ui: &mut egui::Ui, v: &MetersView) {
    let s = v.status;
    let dim = |t: String| RichText::new(t).weak();
    egui::Grid::new("meters")
        .num_columns(2)
        .spacing([14.0, 5.0])
        .show(ui, |ui| {
            let txt = ui.visuals().text_color();
            let ts_line = if v.live {
                format!(
                    "{:.2} Mbit/s   (ffmpeg {:.2} in)",
                    s.ts_rate_bps / 1e6,
                    s.ts_in_bps / 1e6
                )
            } else {
                format!("{:.2} Mbit/s", s.ts_rate_bps / 1e6)
            };
            row(ui, "TS bitrate", None, vec![RichText::new(ts_line)]);

            row(
                ui,
                "TS buffer",
                Some((s.ts_fill, v.levels.ts_fill.colour(GREEN))),
                vec![dim(format!(
                    " {:.0} %  (ffmpeg backs off when full)",
                    s.ts_fill * 100.0
                ))],
            );
            row(
                ui,
                "IQ buffer",
                Some((s.iq_fill, GREEN)),
                vec![dim(format!(" {:.0} %", s.iq_fill * 100.0))],
            );

            ui.label(RichText::new("Stuffing").weak());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} packets", s.ts_stuffed))
                        .color(v.levels.stuffing.colour(txt)),
                );
                ui.add_space(14.0);
                ui.label(RichText::new("Underruns").weak());
                ui.label(
                    RichText::new(s.underruns.to_string()).color(v.levels.underruns.colour(txt)),
                );
            });
            ui.end_row();

            row(
                ui,
                "Modulator",
                Some((
                    s.modulator_load_pct / 100.0,
                    v.levels.modulator.colour(GREEN),
                )),
                vec![
                    RichText::new(format!(" {:.0} %", s.modulator_load_pct))
                        .color(v.levels.modulator.colour(txt)),
                    dim(if v.paced || !v.live {
                        "  (of one core)".to_string()
                    } else {
                        format!("  (of one core)  speed {:.1}x", s.modulator_speed_x)
                    }),
                ],
            );
            row(
                ui,
                "ffmpeg",
                Some((s.ffmpeg_cpu_pct / 100.0, GREEN)),
                vec![
                    RichText::new(format!(" {:.0} %", s.ffmpeg_cpu_pct)),
                    dim(format!("   speed {:.1}x", s.ffmpeg_speed_x)),
                ],
            );
            row(
                ui,
                "This app",
                Some((s.process_cpu_pct / 100.0, GREEN)),
                vec![
                    RichText::new(format!(" {:.0} %", s.process_cpu_pct)),
                    dim(format!(
                        "   sent {}   programme {}",
                        hms(s.sent_s),
                        hms(s.programme_s)
                    )),
                ],
            );

            ui.label(RichText::new("Over budget").weak());
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(format!("dts < pcr: {}", s.dts_pcr_warnings))
                        .color(v.levels.budget.colour(txt)),
                );
                if s.dts_pcr_warnings > 0 {
                    ui.label(
                        RichText::new(
                            "the picture is too complex for this mode: lower the bitrate \
                             or pick a faster mode",
                        )
                        .color(RED),
                    );
                }
            });
            ui.end_row();

            if !s.chain.is_empty() {
                row(ui, "Encoder", None, vec![RichText::new(&s.chain)]);
            }
            if s.sample_rate_hz > 0 {
                row(
                    ui,
                    "Radio rate",
                    None,
                    vec![RichText::new(format!(
                        "{:.6} MS/s",
                        s.sample_rate_hz as f64 / 1e6
                    ))],
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_tx::{Arm, TxConfig};

    fn status(state: TxState) -> TxStatus {
        let mut s = TxStatus::idle(&TxConfig::new("x.mp4".into()), &Arm::new());
        s.state = state;
        s.ts_fill = 1.0;
        s
    }

    #[test]
    fn a_healthy_stream_is_all_ok() {
        let s = status(TxState::Transmitting);
        let l = classify(&s, &Trend::default(), 10.0);
        assert_eq!(l.worst(), Level::Ok);
    }

    #[test]
    fn a_low_ts_buffer_is_amber_only_while_flowing() {
        let mut s = status(TxState::Transmitting);
        s.ts_fill = 0.2;
        assert_eq!(classify(&s, &Trend::default(), 1.0).ts_fill, Level::Amber);
        s.ts_fill = 0.3;
        assert_eq!(classify(&s, &Trend::default(), 1.0).ts_fill, Level::Ok);
        s.ts_fill = 0.0;
        s.state = TxState::Preparing;
        assert_eq!(classify(&s, &Trend::default(), 1.0).ts_fill, Level::Ok);
        s.state = TxState::Finishing;
        assert_eq!(classify(&s, &Trend::default(), 1.0).ts_fill, Level::Ok);
    }

    #[test]
    fn the_reds() {
        let mut s = status(TxState::Transmitting);
        s.underruns = 1;
        assert_eq!(classify(&s, &Trend::default(), 1.0).underruns, Level::Red);
        s.underruns = 0;
        s.dts_pcr_warnings = 3;
        assert_eq!(classify(&s, &Trend::default(), 1.0).budget, Level::Red);
        s.dts_pcr_warnings = 0;
        s.modulator_load_pct = 91.0;
        assert_eq!(classify(&s, &Trend::default(), 1.0).modulator, Level::Red);
        s.modulator_load_pct = 89.0;
        assert_eq!(classify(&s, &Trend::default(), 1.0).modulator, Level::Ok);
    }

    #[test]
    fn stuffing_is_amber_for_five_seconds_after_it_rose() {
        let mut t = Trend::default();
        let mut s = status(TxState::Transmitting);
        // Quiet, then 10 packets stuffed at t = 3.
        for i in 0..6 {
            t.push(i as f64 * 0.5, 0, 0);
        }
        s.ts_stuffed = 10;
        t.push(3.0, 10, 0);
        assert_eq!(classify(&s, &t, 3.0).stuffing, Level::Amber);
        assert_eq!(classify(&s, &t, 6.0).stuffing, Level::Amber);
        // Seven seconds on with no more: back to normal (the old count stays
        // on the counter but it has not risen).
        for i in 0..14 {
            t.push(3.5 + i as f64 * 0.5, 10, 0);
        }
        assert_eq!(classify(&s, &t, 10.5).stuffing, Level::Ok);
    }

    #[test]
    fn clipping_is_red_only_while_rising() {
        let mut t = Trend::default();
        let mut s = status(TxState::Transmitting);
        t.push(0.0, 0, 2);
        t.push(1.0, 0, 2);
        s.clipped = 2;
        assert_eq!(classify(&s, &t, 1.0).clipping, Level::Ok);
        t.push(2.0, 0, 40);
        s.clipped = 40;
        assert_eq!(classify(&s, &t, 2.0).clipping, Level::Red);
    }

    #[test]
    fn a_new_session_starts_the_trend_afresh() {
        let mut t = Trend::default();
        t.push(0.0, 50, 9);
        t.push(1.0, 60, 9);
        // Counters back to 0: a new session, not "negative stuffing".
        t.push(2.0, 0, 0);
        assert!(!t.stuffing_recent(2.0, 0));
    }

    #[test]
    fn preparing_progress_fills_to_one() {
        let mut s = status(TxState::Preparing);
        s.ts_fill = 0.0;
        s.iq_fill = 0.0;
        assert_eq!(prepare_progress(&s), 0.0);
        // The TS ring at a third (0.5 s of 1.5 s) and the IQ ring full.
        s.ts_fill = 1.0 / 3.0;
        s.iq_fill = 1.0;
        assert!((prepare_progress(&s) - 1.0).abs() < 1e-6);
        s.ts_fill = 1.0;
        assert_eq!(prepare_progress(&s), 1.0);
    }

    #[test]
    fn times_read_well() {
        assert_eq!(hms(0.0), "0:00");
        assert_eq!(hms(161.4), "2:41");
        assert_eq!(hms(3725.0), "1:02:05");
        assert_eq!(hms(-3.0), "0:00");
    }
}
