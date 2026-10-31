// made by CasualArclamp
//! The gain optimiser, one for both radios: try gain settings on the signal
//! the radio is tuned to, read how good each is, and keep the best. Copied
//! back from DecLAB's `gain_opt.rs`, which grew it out of this app's HackRF
//! optimiser by making the search a plug-in, so the Airspy has one too.
//!
//! What is shared: the settle/measure timing, the figure of merit, the
//! clipping and headroom limits, "two points 3 dB under the best means past
//! the peak", and "of the points within 0.3 dB of the best, the coolest". The
//! search differs by radio ([`Plan`]):
//!
//! - **HackRF**, as before: total gain (LNA + VGA) in 4 dB steps from 20 dB
//!   under the present setting (the LNA first, to 40 dB, the VGA for the
//!   rest), up to 90 dB or 24 points; then the LNA/VGA splits (16, 24, 32, 40)
//!   of the best total.
//! - **Airspy R2**: three stages with the AGCs off (LNA 0-14, mixer 0-15,
//!   IF/VGA 0-15), so one number cannot reach them all. It climbs libairspy's
//!   "sensitivity" ladder (one knob that moves the three together, tuned by
//!   Airspy for noise figure), then up to three rounds of hill climbing step
//!   each stage on its own around the best rung.
//!
//! The figure of merit is the receiver's MER while it is locked, else the
//! spectrum's C/N. A setting is out if it clips or runs hotter than
//! [`TOO_HOT_DBFS`]. If the best C/N of the sweep is under [`MIN_CN_DB`] there
//! is no signal and nothing is changed.

use std::fmt;
use std::time::{Duration, Instant};

use crate::source::{AirGains, Gains as HackGains};

/// Time given to a setting to take hold (the radio's buffers, the equaliser)
/// and then to measure it, per point.
const SETTLE: Duration = Duration::from_millis(800);
const MEASURE: Duration = Duration::from_millis(1300);
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
/// Under this C/N at the best point of the sweep there is no signal to tune to.
const MIN_CN_DB: f32 = 3.0;
/// The most points in all, so a hill climb ends.
const MAX_POINTS: usize = 40;

// HackRF: total gain, its step, the most sweep points, the LNA splits tried.
const HACK_MAX_TOTAL: u16 = 90;
const HACK_STEP: u16 = 4;
const HACK_MAX_SWEEP_POINTS: usize = 24;
const HACK_SPLITS: [u16; 4] = [16, 24, 32, 40];

// Airspy: how far one stage moves in a hill-climbing step, and the rounds.
const AIR_STEP: u8 = 2;
const AIR_ROUNDS: usize = 3;
/// The top of each stage's range (`decdvbt-io`'s `airspy/proto.rs`).
const AIR_TOP: AirGains = AirGains {
    lna: 14,
    mixer: 15,
    vga: 15,
};

/// A setting of whichever radio is being optimised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gains {
    Air(AirGains),
    Hack(HackGains),
}

impl Gains {
    /// The x axis of the sweep plot: dB for the HackRF, the sum of the three
    /// stage indices for the Airspy.
    pub fn total(&self) -> u16 {
        match self {
            Gains::Air(g) => u16::from(g.lna + g.mixer + g.vga),
            Gains::Hack(g) => g.lna + g.vga,
        }
    }
}

impl fmt::Display for Gains {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Gains::Air(g) => write!(f, "LNA {} mixer {} VGA {}", g.lna, g.mixer, g.vga),
            Gains::Hack(g) => write!(f, "LNA {} VGA {}", g.lna, g.vga),
        }
    }
}

/// The settings one step from `g` on a single stage, in range.
fn neighbours(g: AirGains) -> Vec<AirGains> {
    let mut out = Vec::with_capacity(6);
    for stage in 0..3 {
        let (value, top) = match stage {
            0 => (g.lna, AIR_TOP.lna),
            1 => (g.mixer, AIR_TOP.mixer),
            _ => (g.vga, AIR_TOP.vga),
        };
        // `checked_sub` is `None` below zero, so a stage at 0 has no step down.
        for v in [
            value.checked_sub(AIR_STEP),
            Some(value + AIR_STEP).filter(|v| *v <= top),
        ]
        .into_iter()
        .flatten()
        {
            let mut n = g;
            match stage {
                0 => n.lna = v,
                1 => n.mixer = v,
                _ => n.vga = v,
            }
            out.push(n);
        }
    }
    out
}

/// The HackRF's total gain split: the LNA first (to 40 dB), the VGA for the
/// rest.
fn hack_split(amp: bool, total: u16) -> HackGains {
    let lna = (total / 8 * 8).min(40);
    HackGains {
        amp,
        lna,
        vga: (total - lna).min(62) / 2 * 2,
    }
}

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
        self.gains.total()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Up through the gains.
    Sweep,
    /// Around the best of the sweep.
    Refine,
}

pub enum Action {
    Wait,
    /// Set these gains (the optimiser is waiting on them).
    Apply(Gains),
    /// Done: the best gains found, or `None` if nothing was measurable (the
    /// original ones are to be put back).
    Finish(Option<Gains>),
}

/// How a radio's gains are searched (see the module comment).
enum Plan {
    Ladder { ladder: Vec<AirGains>, next: usize },
    Totals { amp: bool, next_total: u16 },
}

impl Plan {
    /// The most points the sweep may make.
    fn max_sweep(&self) -> usize {
        match self {
            Plan::Ladder { .. } => MAX_POINTS,
            Plan::Totals { .. } => HACK_MAX_SWEEP_POINTS,
        }
    }

    /// The next setting up the sweep, or `None` at its end.
    fn sweep_next(&mut self) -> Option<Gains> {
        match self {
            Plan::Ladder { ladder, next } => {
                let g = ladder.get(*next).copied()?;
                *next += 1;
                Some(Gains::Air(g))
            }
            Plan::Totals { amp, next_total } => {
                if *next_total + HACK_STEP > HACK_MAX_TOTAL {
                    return None;
                }
                *next_total += HACK_STEP;
                Some(Gains::Hack(hack_split(*amp, *next_total)))
            }
        }
    }

    /// How many rounds of refinement follow the sweep.
    fn rounds(&self) -> usize {
        match self {
            Plan::Ladder { .. } => AIR_ROUNDS,
            Plan::Totals { .. } => 1,
        }
    }

    /// The settings to try about the best one.
    fn refine(&self, best: Gains) -> Vec<Gains> {
        match best {
            Gains::Air(g) => neighbours(g).into_iter().map(Gains::Air).collect(),
            // Other splits of the best total between the LNA (a better noise
            // figure, but it overloads sooner) and the VGA (adds noise as well
            // as gain).
            Gains::Hack(g) => {
                let total = g.lna + g.vga;
                HACK_SPLITS
                    .into_iter()
                    .filter(|&lna| lna != g.lna && lna <= total && total - lna <= 62)
                    .map(|lna| {
                        Gains::Hack(HackGains {
                            amp: g.amp,
                            lna,
                            vga: (total - lna) / 2 * 2,
                        })
                    })
                    .collect()
            }
        }
    }
}

/// Finds the gains that give the best signal-to-noise ratio on the signal the
/// radio is tuned to. Each point takes 2 s; a run is twenty to forty.
pub struct Optimiser {
    pub original: Gains,
    pub points: Vec<Point>,
    pub current: Gains,
    plan: Plan,
    stage: Stage,
    /// The setting being refined about, and the round.
    centre: Option<Gains>,
    round: usize,
    queue: Vec<Gains>,
    changed_at: Option<Instant>,
    mer: (f32, u32),
    cn: (f32, u32),
    samples: u32,
    settle: Duration,
    measure: Duration,
}

impl Optimiser {
    /// The Airspy's: `ladder` is the settings to sweep, least gain first.
    pub fn airspy(original: AirGains, ladder: Vec<AirGains>) -> Optimiser {
        Optimiser::airspy_with_times(original, ladder, SETTLE, MEASURE)
    }

    fn airspy_with_times(
        original: AirGains,
        ladder: Vec<AirGains>,
        settle: Duration,
        measure: Duration,
    ) -> Optimiser {
        let first = ladder.first().copied().unwrap_or(original);
        Optimiser::with_plan(
            Gains::Air(original),
            Gains::Air(first),
            Plan::Ladder { ladder, next: 1 },
            settle,
            measure,
        )
    }

    /// The HackRF's: from 20 dB under the present total gain.
    pub fn hackrf(original: HackGains) -> Optimiser {
        Optimiser::hackrf_with_times(original, SETTLE, MEASURE)
    }

    fn hackrf_with_times(original: HackGains, settle: Duration, measure: Duration) -> Optimiser {
        let start = (original.lna + original.vga)
            .saturating_sub(20)
            .clamp(8, 70)
            / HACK_STEP
            * HACK_STEP;
        Optimiser::with_plan(
            Gains::Hack(original),
            Gains::Hack(hack_split(original.amp, start)),
            Plan::Totals {
                amp: original.amp,
                next_total: start,
            },
            settle,
            measure,
        )
    }

    fn with_plan(
        original: Gains,
        first: Gains,
        plan: Plan,
        settle: Duration,
        measure: Duration,
    ) -> Optimiser {
        Optimiser {
            original,
            points: Vec::new(),
            current: first,
            plan,
            stage: Stage::Sweep,
            centre: None,
            round: 0,
            queue: Vec::new(),
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
        if p.spoiled {
            None
        } else if self.uses_mer() {
            p.mer
        } else {
            p.cn
        }
    }

    fn uses_mer(&self) -> bool {
        self.points.iter().any(|p| !p.spoiled && p.mer.is_some())
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
                let past_peak = {
                    let v: Vec<f32> = self.points.iter().filter_map(|p| self.figure(p)).collect();
                    let top = self.best().and_then(|b| self.figure(&b));
                    match (top, v.len()) {
                        (Some(top), n) if n >= 3 => {
                            v[n - 2..].iter().all(|x| *x < top - PAST_PEAK_DB)
                        }
                        _ => false,
                    }
                };
                let stop = last.spoiled || past_peak || self.points.len() >= self.plan.max_sweep();
                // `if … && let` (a let-chain): go on up the sweep only if not
                // stopping and the plan has a next setting.
                if !stop && let Some(g) = self.plan.sweep_next() {
                    self.current = g;
                    return Action::Wait;
                }
                let Some(best) = self.best() else {
                    return Action::Finish(None);
                };
                // A C/N this small is noise reading noise: the channel has no
                // signal, and the "best" would be chance and the tie-break.
                if !self.uses_mer() && self.figure(&best).is_some_and(|c| c < MIN_CN_DB) {
                    return Action::Finish(None);
                }
                self.stage = Stage::Refine;
                self.round = 1;
                self.refine_about(best.gains);
                self.next_refine()
            }
            Stage::Refine => self.next_refine(),
        }
    }

    /// Queue the untried settings about `g`.
    fn refine_about(&mut self, g: Gains) {
        self.centre = Some(g);
        self.queue = self
            .plan
            .refine(g)
            .into_iter()
            .filter(|n| !self.points.iter().any(|p| p.gains == *n))
            .collect();
    }

    fn next_refine(&mut self) -> Action {
        if let Some(g) = self.queue.pop() {
            self.current = g;
            return Action::Wait;
        }
        // A round is over: go round again if the best has moved.
        let Some(best) = self.best() else {
            return Action::Finish(None);
        };
        if self.round < self.plan.rounds()
            && Some(best.gains) != self.centre
            && self.points.len() < MAX_POINTS
        {
            self.round += 1;
            self.refine_about(best.gains);
            if !self.queue.is_empty() {
                return self.next_refine();
            }
        }
        Action::Finish(Some(best.gains))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn air(lna: u8, mixer: u8, vga: u8) -> AirGains {
        AirGains { lna, mixer, vga }
    }

    /// libairspy's sensitivity ladder for the tests: (lna, mixer, vga) at
    /// rungs 0, 2 .. 20, 21.
    fn ladder() -> Vec<AirGains> {
        vec![
            air(0, 0, 4),
            air(1, 0, 4),
            air(3, 0, 4),
            air(6, 2, 4),
            air(8, 3, 4),
            air(9, 4, 5),
            air(12, 7, 5),
            air(13, 9, 5),
            air(14, 10, 6),
            air(14, 12, 9),
            air(14, 12, 11),
            air(14, 12, 13),
        ]
    }

    /// A made-up front end, as (C/N, rms dBFS) from the total gain: the C/N
    /// rises with it until `knee`, where the intermodulation takes over, and
    /// the level follows it.
    fn curve(total: f32, floor: f32, knee: f32, offset: f32) -> (f32, f32) {
        let cn = 30.0
            - 10.0 * (1.0 + 10f32.powf((floor - total) / 10.0)).log10()
            - (total - knee).max(0.0) * 2.0;
        (cn, offset + total)
    }

    fn radio(g: Gains) -> (f32, f32) {
        match g {
            Gains::Air(g) => curve((g.lna + g.mixer + g.vga) as f32, 15.0, 30.0, -60.0),
            Gains::Hack(g) => curve((g.lna + g.vga) as f32, 30.0, 52.0, -90.0),
        }
    }

    const FAST: Duration = Duration::from_millis(10);

    /// Run `o` against `measure`, 5 ms a tick; `clip_over` is the level over
    /// which the converter clips.
    fn run(
        mut o: Optimiser,
        mut measure: impl FnMut(Gains) -> (Option<f32>, Option<f32>, f32),
        clip_over: f32,
    ) -> (Option<Gains>, Optimiser) {
        let t0 = Instant::now();
        let mut applied = o.original;
        for i in 0..100_000u64 {
            let (mer, cn, rms_dbfs) = measure(applied);
            let r = Reading {
                now: t0 + Duration::from_millis(5 * i),
                mer,
                cn,
                rms_dbfs,
                clip_percent: if rms_dbfs > clip_over { 1.0 } else { 0.0 },
            };
            match o.tick(&r) {
                Action::Wait => {}
                Action::Apply(g) => applied = g,
                Action::Finish(g) => return (g, o),
            }
        }
        panic!("never finished");
    }

    fn by_cn(g: Gains) -> (Option<f32>, Option<f32>, f32) {
        let (cn, rms) = radio(g);
        (None, Some(cn), rms)
    }

    fn airspy(original: AirGains) -> Optimiser {
        Optimiser::airspy_with_times(original, ladder(), FAST, FAST)
    }

    fn hackrf(original: HackGains) -> Optimiser {
        Optimiser::hackrf_with_times(original, FAST, FAST)
    }

    #[test]
    fn the_airspy_finds_the_top_of_the_cn_curve() {
        let (best, o) = run(airspy(air(8, 8, 8)), by_cn, 0.0);
        let g = best.expect("a result");
        let cn = radio(g).0;
        let peak = (0..=14u8)
            .flat_map(|l| (0..=15u8).flat_map(move |m| (0..=15u8).map(move |v| air(l, m, v))))
            .map(|g| radio(Gains::Air(g)).0)
            .fold(f32::MIN, f32::max);
        assert!(cn > peak - 0.5, "{g} gives {cn} dB against the best {peak}");
        assert!(radio(g).1 <= TOO_HOT_DBFS);
        assert!(o.done() < MAX_POINTS);
    }

    /// The HackRF test this app had before the Airspy: a figure of merit that
    /// rises with total gain up to 52 dB, is a little better with the LNA at
    /// 32, and clips from 66.
    #[test]
    fn the_best_split_of_the_best_total_is_found() {
        let (best, o) = run(
            hackrf(HackGains {
                amp: true,
                lna: 16,
                vga: 8,
            }),
            |g| {
                let Gains::Hack(g) = g else { unreachable!() };
                let total = f32::from(g.lna + g.vga);
                let mer =
                    24.0 - 0.06 * (total - 52.0).powi(2) - 0.15 * (f32::from(g.lna) - 32.0).abs();
                (Some(mer), None, -60.0 + 0.8 * total)
            },
            -60.0 + 0.8 * 65.0,
        );
        let Some(Gains::Hack(best)) = best else {
            panic!("no result")
        };
        assert_eq!((best.lna, best.vga), (32, 20), "{} points", o.done());
        assert!(best.amp);
        assert!(o.points.len() < 24);
    }

    #[test]
    fn a_setting_that_clips_is_never_chosen() {
        let (best, _) = run(airspy(air(8, 8, 8)), by_cn, -40.0);
        let g = best.expect("a result");
        assert!(radio(g).1 <= -40.0, "{g} at {} dBFS", radio(g).1);
        let h = HackGains {
            amp: false,
            lna: 24,
            vga: 20,
        };
        let (best, _) = run(hackrf(h), by_cn, -50.0);
        let g = best.expect("a result");
        assert!(radio(g).1 <= -50.0, "{g} at {} dBFS", radio(g).1);
    }

    #[test]
    fn nothing_measurable_gives_back_none() {
        let hack = HackGains {
            amp: false,
            lna: 8,
            vga: 8,
        };
        for o in [airspy(air(8, 8, 8)), hackrf(hack)] {
            let (g, _) = run(o, |_| (None, None, -40.0), 0.0);
            assert_eq!(g, None);
        }
    }

    #[test]
    fn a_channel_with_no_signal_keeps_the_old_gains() {
        // C/N wanders around 1 dB at every setting: noise reading noise.
        let mut i = 0;
        let (g, _) = run(
            airspy(air(8, 8, 8)),
            move |_| {
                i += 1;
                (None, Some(1.0 + (i % 7) as f32 * 0.05), -40.0)
            },
            0.0,
        );
        assert_eq!(g, None);
    }

    #[test]
    fn neighbours_stay_in_range() {
        let n = neighbours(air(0, 15, 1));
        assert!(
            n.iter()
                .all(|g| g.lna <= 14 && g.mixer <= 15 && g.vga <= 15)
        );
        // lna can only go up, mixer only down, vga only up (1 - 2 underflows).
        assert_eq!(n.len(), 3);
    }

    #[test]
    fn the_lna_comes_first_in_a_split() {
        assert_eq!(
            (hack_split(true, 16).lna, hack_split(true, 16).vga),
            (16, 0)
        );
        assert_eq!(
            (hack_split(true, 52).lna, hack_split(true, 52).vga),
            (40, 12)
        );
        assert_eq!(
            (hack_split(false, 20).lna, hack_split(false, 20).vga),
            (16, 4)
        );
    }
}
