// made by CasualArclamp
//! Narrowband interference (a "birdie": a CW carrier, a spur) in the channel,
//! found per carrier after the OFDM transform and handed to the demapper as
//! extra noise at those carriers, which erases them.
//!
//! **Why here and not before the FFT.** A tone inside an OFDM channel lands on
//! one carrier (and, if it is between two, a sinc skirt on the next few) with
//! the same amplitude in every symbol. The transform has already concentrated
//! it, so a detector costs one multiply-add per carrier per observed symbol
//! and the cure costs nothing per symbol (the carrier's noise estimate goes
//! up, and the soft decoder gives its cell zero weight). A time-domain notch
//! would be an IIR running at the full sample rate every sample, notch the
//! signal's own energy at that frequency, and needs a tracker for a drifting
//! tone anyway. What it cannot do that a time-domain subtractor could is
//! recover the cells under the tone; those are lost (erased), and the FEC
//! repairs them. A tone 20 dB under the signal's total power is 18 dB above
//! the carrier it hits in 8k mode, so a lost carrier or three is the cheap
//! outcome.
//!
//! **Detection.** [`CarrierWatch::observe`] keeps a slow exponential average of
//! each carrier's raw power (|Y|², before equalising: a tone's power is
//! constant, the data's averages out; the pilots' extra 2.5 dB is far under
//! the threshold). [`CarrierWatch::refresh`], every so often, finds the local
//! level (the median of the averages over a block of 192 carriers, blocks
//! overlapping by half and interpolated: robust to fades, which are broad
//! compared with a tone) and flags the carriers more than [`FLAG_RATIO`] over
//! it. If any is flagged the medians are redone without them, so the skirt of a
//! strong tone cannot raise its own reference. A flagged carrier's
//! interference power is its excess over the level, in the same units as the
//! channel estimate's noise, which is what the demapper weighs it by. A
//! tone that drifts is followed because the averages and flags are
//! recomputed continuously; one that moves faster than the average's time
//! constant (~8 observations) is smeared and flagged over its track.
//!
//! **Pilots.** A tone over a scattered pilot spoils that pilot's channel
//! sample too. The receivers already see that as a large innovation and raise
//! the noise estimate around it; the extra noise here covers the continual
//! pilots' carriers and the data cells the lattice's neighbours would
//! otherwise have trusted.
//!
//! On a clean signal nothing is flagged and the demapper's inputs are not
//! touched. Set `DECDVBT_NOTCH=0` in the environment to switch it off (for
//! comparisons).

use decdvbt_core::Iq;

/// The averaged power at a carrier over the local level that flags it (7 dB):
/// above a fade's peaks (two equal paths peak 3 dB over their median) and the
/// 2.5 dB the boosted pilots add, and under any tone that spoils a cell.
pub const FLAG_RATIO: f32 = 5.0;
/// A skirt carrier's power over the level (2.5 dB) that keeps it flagged.
const GROW_RATIO: f32 = 1.8;
/// A tone's peak over the carriers six either side of it (4.8 dB).
const NARROW_RATIO: f32 = 3.0;
/// The farthest a skirt is followed from its seed, carriers.
const GROW_MAX: usize = 512;
/// The block of carriers a local level is taken over, and the step between
/// blocks.
const BLOCK: usize = 192;
const HOP: usize = 96;
/// Fewest unflagged carriers in a block for the second pass to trust it.
const MIN_LEFT: usize = 48;
/// Observations before the first refresh, and the average's rate.
const WARM_UP: u32 = 8;
const RATE: f32 = 0.125;

/// What the watcher found, for the status lines.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct NotchStat {
    /// Carriers flagged, and the groups of neighbouring carriers they make.
    pub carriers: u32,
    pub tones: u32,
    /// The strongest tone's carrier (index in the symbol) and its power over
    /// the local level, dB.
    pub strongest_carrier: u32,
    pub strongest_db: f32,
}

/// A line for the stats: what was found and done about it (None: nothing).
pub fn describe(n: &NotchStat, t: &crate::tone::ToneStat) -> Option<String> {
    let mut bits = Vec::new();
    if t.tones > 0 {
        bits.push(format!(
            "{} CW tone(s) cancelled in the samples (strongest {:+.0} kHz from the radio's centre, {:.0} dB over the floor)",
            t.tones,
            t.freq_hz / 1e3,
            t.over_db
        ));
    }
    if n.carriers > 0 {
        bits.push(format!(
            "{} carrier(s) in {} narrowband tone(s) erased (strongest at carrier {}, {:.0} dB over)",
            n.carriers, n.tones, n.strongest_carrier, n.strongest_db
        ));
    }
    (!bits.is_empty()).then(|| bits.join("; "))
}

/// The per-carrier watcher: one per receiver, used for one symbol length.
pub struct CarrierWatch {
    enabled: bool,
    /// Averaged |Y|² of each carrier.
    avg: Vec<f32>,
    /// The interference power at each carrier (zero but at the flagged).
    j: Vec<f32>,
    flagged: Vec<u32>,
    level: Vec<f32>,
    /// The power each carrier is expected to show over the data cells' (the
    /// pilots' boost, averaged over the pilot pattern); empty: all 1.
    expect: Vec<f32>,
    meds: Vec<f32>,
    scratch: Vec<f32>,
    seen: u32,
    stat: NotchStat,
}

impl Default for CarrierWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl CarrierWatch {
    pub fn new() -> CarrierWatch {
        CarrierWatch {
            enabled: !std::env::var("DECDVBT_NOTCH").is_ok_and(|v| v == "0"),
            avg: Vec::new(),
            j: Vec::new(),
            flagged: Vec::new(),
            level: Vec::new(),
            expect: Vec::new(),
            meds: Vec::new(),
            scratch: Vec::new(),
            seen: 0,
            stat: NotchStat::default(),
        }
    }

    /// Say how much more power than a data cell each carrier has in the
    /// signal itself (pilots are boosted, and a continual pilot is there in
    /// every symbol), so they are not taken for tones.
    pub fn set_expected(&mut self, expect: Vec<f32>) {
        self.expect = expect;
    }

    /// Whether carrier `k` is the peak of a tone: over the local level by
    /// [`FLAG_RATIO`] (the level and the carrier's expected power both taken
    /// out), and narrow, [`NARROW_RATIO`] over the carriers six either side
    /// (a fade's peak in the channel is wider than that: an echo of 50 us
    /// repeats every 18 carriers of 8k mode; a tone is a carrier or three).
    #[inline]
    fn seed(&self, k: usize, level: f32) -> bool {
        let e = self.exp(k);
        let a = self.avg[k] / e;
        if a <= FLAG_RATIO * level.max(1e-12) {
            return false;
        }
        let n = self.avg.len();
        let near = |i: usize| self.avg[i] / self.exp(i);
        let side = [k.checked_sub(6), (k + 6 < n).then_some(k + 6)];
        let worst = side.iter().flatten().map(|&i| near(i)).fold(0f32, f32::max);
        a > NARROW_RATIO * worst
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.clear();
        }
    }

    /// Forget everything (a new lock, a new symbol length).
    pub fn clear(&mut self) {
        self.avg.clear();
        self.j.clear();
        self.flagged.clear();
        self.seen = 0;
        self.stat = NotchStat::default();
    }

    /// What is flagged now.
    pub fn stat(&self) -> NotchStat {
        self.stat
    }

    /// The flagged carriers.
    pub fn flagged(&self) -> &[u32] {
        &self.flagged
    }

    /// The interference power at carrier `k` (zero if it is not flagged).
    #[inline]
    pub fn power(&self, k: usize) -> f32 {
        self.j.get(k).copied().unwrap_or(0.0)
    }

    /// One symbol's carriers as the transform gave them. A plain loop over
    /// arrays so the compiler vectorises it.
    pub fn observe(&mut self, row: &[Iq]) {
        if !self.enabled {
            return;
        }
        if self.avg.len() != row.len() {
            self.avg.clear();
            self.avg.extend(row.iter().map(|c| c.norm_sqr()));
            self.j.clear();
            self.j.resize(row.len(), 0.0);
            self.flagged.clear();
            self.seen = 0;
            self.stat = NotchStat::default();
        } else {
            for (a, c) in self.avg.iter_mut().zip(row) {
                *a += RATE * (c.re * c.re + c.im * c.im - *a);
            }
        }
        self.seen += 1;
    }

    /// Observations since the last clear.
    pub fn seen(&self) -> u32 {
        self.seen
    }

    /// Redo the flags from the averages.
    pub fn refresh(&mut self) {
        let n = self.avg.len();
        if !self.enabled || self.seen < WARM_UP || n < 2 * BLOCK {
            return;
        }
        // Pass 1 over every carrier; pass 2 without the flagged ones.
        self.levels(false);
        if self.mark() {
            self.levels(true);
        }
        for &k in &self.flagged {
            self.j[k as usize] = 0.0;
        }
        self.flagged.clear();
        self.stat = NotchStat::default();
        if !self.mark() {
            return;
        }
        // The seeds (over the threshold) and the skirts that run out from
        // them while the power stays [`GROW_RATIO`] over the level: the
        // rectangular window's sinc skirt on a strong tone is far above the
        // noise for tens of carriers after it falls under the threshold.
        let mut keep = vec![false; n];
        let (mut best, mut best_ratio) = (0usize, 0f32);
        for k in 0..n {
            if self.seed(k, self.level[k]) {
                keep[k] = true;
                let r = self.avg[k] / (self.level[k].max(1e-12) * self.exp(k));
                if r > best_ratio {
                    (best, best_ratio) = (k, r);
                }
                for side in [-1isize, 1] {
                    for d in 1..=GROW_MAX as isize {
                        let i = k as isize + side * d;
                        if i < 0 || i >= n as isize {
                            break;
                        }
                        let i = i as usize;
                        if keep[i] {
                            continue;
                        }
                        if self.avg[i] > GROW_RATIO * self.exp(i) * self.level[i].max(1e-12) {
                            keep[i] = true;
                        } else {
                            break;
                        }
                    }
                }
            }
        }
        let mut prev: Option<usize> = None;
        for k in (0..n).filter(|&k| keep[k]) {
            let (l, e) = (self.level[k].max(1e-12), self.exp(k));
            self.flagged.push(k as u32);
            // What is over the signal's own power is interference. Capped at
            // 40 dB over the level (a cell that far under is erased all the
            // same, and the SNR figure is not swamped by one tone).
            self.j[k] = (self.avg[k] - l * e).clamp(0.0, 1e4 * l);
            self.stat.carriers += 1;
            if prev.is_none_or(|p| p + 1 != k) {
                self.stat.tones += 1;
            }
            prev = Some(k);
        }
        self.stat.strongest_carrier = best as u32;
        self.stat.strongest_db = 10.0 * best_ratio.max(1e-9).log10();
    }

    #[inline]
    fn exp(&self, k: usize) -> f32 {
        self.expect.get(k).copied().unwrap_or(1.0)
    }

    /// Whether any carrier is over the threshold now.
    fn mark(&self) -> bool {
        (0..self.avg.len()).any(|k| self.seed(k, self.level[k]))
    }

    /// The local level at every carrier: block medians, interpolated. With
    /// `skip_flagged` the carriers over the threshold of the last levels are
    /// left out of the medians.
    fn levels(&mut self, skip_flagged: bool) {
        let n = self.avg.len();
        let blocks = n.saturating_sub(BLOCK).div_ceil(HOP) + 1;
        let old: Vec<bool> = if skip_flagged {
            (0..n).map(|k| self.seed(k, self.level[k])).collect()
        } else {
            Vec::new()
        };
        self.meds.clear();
        for b in 0..blocks {
            let a = (b * HOP).min(n - BLOCK);
            self.scratch.clear();
            if skip_flagged {
                self.scratch.extend(
                    self.avg[a..a + BLOCK]
                        .iter()
                        .zip(&old[a..a + BLOCK])
                        .filter(|(_, f)| !**f)
                        .map(|(&v, _)| v),
                );
                if self.scratch.len() < MIN_LEFT {
                    self.scratch.clear();
                    self.scratch.extend_from_slice(&self.avg[a..a + BLOCK]);
                }
            } else {
                self.scratch.extend_from_slice(&self.avg[a..a + BLOCK]);
            }
            let mid = self.scratch.len() / 2;
            self.meds
                .push(*self.scratch.select_nth_unstable_by(mid, f32::total_cmp).1);
        }
        // Between the block centres, linearly.
        self.level.resize(n, 0.0);
        let half = BLOCK as f32 / 2.0;
        for k in 0..n {
            let pos = ((k as f32 - half) / HOP as f32).clamp(0.0, (blocks - 1) as f32);
            let i = pos as usize;
            let f = pos - i as f32;
            let m1 = self.meds[(i + 1).min(blocks - 1)];
            self.level[k] = self.meds[i] + f * (m1 - self.meds[i]);
        }
    }

    /// Add the interference power to the noise at the flagged carriers
    /// (DVB-T's equaliser's per-carrier noise).
    pub fn add_to_noise(&self, noise: &mut [f32]) {
        for &k in &self.flagged {
            if let Some(v) = noise.get_mut(k as usize) {
                *v += self.j[k as usize];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic noise source.
    fn rng(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((*state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
    }

    fn rows(n: usize, count: usize, tone: Option<(usize, f32)>) -> Vec<Vec<Iq>> {
        let mut s = 7u64;
        (0..count)
            .map(|l| {
                (0..n)
                    .map(|k| {
                        let mut c = Iq::new(rng(&mut s) * 3.0, rng(&mut s) * 3.0);
                        if let Some((kt, a)) = tone
                            && k == kt
                        {
                            c += Iq::from_polar(a, 0.7 * l as f32);
                        }
                        c
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_clean_channel_flags_nothing() {
        let mut w = CarrierWatch::new();
        for r in rows(1705, 64, None) {
            w.observe(&r);
        }
        w.refresh();
        assert_eq!(w.stat(), NotchStat::default());
        let mut noise = vec![1.0f32; 1705];
        w.add_to_noise(&mut noise);
        assert!(noise.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn a_tone_is_flagged_and_its_power_is_found() {
        let mut w = CarrierWatch::new();
        for r in rows(1705, 64, Some((900, 10.0))) {
            w.observe(&r);
        }
        w.refresh();
        let s = w.stat();
        assert_eq!(s.tones, 1);
        assert_eq!(s.strongest_carrier, 900);
        // 100 over a level of 1.5 (the noise's power): 18 dB.
        assert!(s.strongest_db > 15.0 && s.strongest_db < 21.0, "{s:?}");
        assert!((w.power(900) - 100.0).abs() < 15.0, "{}", w.power(900));
        assert_eq!(w.power(100), 0.0);
    }

    #[test]
    fn a_tone_that_goes_is_forgotten() {
        let mut w = CarrierWatch::new();
        for r in rows(1705, 32, Some((300, 10.0))) {
            w.observe(&r);
        }
        w.refresh();
        assert_eq!(w.stat().tones, 1);
        for r in rows(1705, 64, None) {
            w.observe(&r);
        }
        w.refresh();
        assert_eq!(w.stat().tones, 0);
        assert_eq!(w.power(300), 0.0);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// A benchmark: cargo test --release -p decdvbt-rx bench_watch -- --ignored --nocapture
    #[test]
    #[ignore = "a benchmark"]
    fn bench_watch() {
        for n in [6817usize, 27265] {
            let row: Vec<Iq> = (0..n)
                .map(|k| Iq::new((k % 7) as f32 - 3.0, (k % 5) as f32 - 2.0))
                .collect();
            let mut w = CarrierWatch::new();
            let mut best_obs = f64::MAX;
            let mut best_ref = f64::MAX;
            for _ in 0..20 {
                let t = std::time::Instant::now();
                for _ in 0..100 {
                    w.observe(&row);
                }
                best_obs = best_obs.min(t.elapsed().as_secs_f64() / 100.0);
                let t = std::time::Instant::now();
                for _ in 0..10 {
                    w.refresh();
                }
                best_ref = best_ref.min(t.elapsed().as_secs_f64() / 10.0);
            }
            println!(
                "{n} carriers: observe {:.2} us, refresh (clean) {:.1} us",
                best_obs * 1e6,
                best_ref * 1e6
            );
        }
    }
}
