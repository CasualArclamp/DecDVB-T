// made by CasualArclamp
//! A signal-quality estimate from the spectrum alone, for before the
//! receiver can measure MER from its pilots (M3).
//!
//! A DVB-T/T2 block is noise-like and flat, so its power next to the noise
//! beside it is a C/N: the block's mean power per bin is S + N, the empty
//! band just outside it is N, and C/N = (S + N − N) / N. It is an estimate,
//! and a modest one:
//!
//! - The noise is the quietest 100 kHz window in the zones either side of
//!   the block (3.9–4.75 MHz out: short of the span's edge, where the radio's
//!   own filter rolls off). Neighbouring channels fill much of that
//!   (a UK mux often has a mux either side), but their blocks do not touch:
//!   the gap between them is empty down to the noise, and the minimum finds
//!   it. If no gap is clean, N comes out high and the C/N low: the error is
//!   on the safe side.
//! - A faded block reads its average power, which is what the C/N of a
//!   channel is, but is not the MER the FEC sees after equalisation.

/// What is measured, relative to the block's nominal centre: the signal
/// band (the occupied band reaches ±3.805 MHz, ±3.47 MHz at the worst UK
/// offset), and the zones either side searched for the noise.
const SIGNAL_HZ: f64 = 3.4e6;
const GUARD_HZ: (f64, f64) = (3.9e6, 4.75e6);
/// The width the noise is averaged over, Hz: wide enough for a steady
/// reading (a few dozen bins), narrow enough to fit a gap between blocks.
const WINDOW_HZ: f64 = 100e3;

/// Smallest excess of the signal band over the noise that counts as a
/// signal (dB): below it the band is just noise.
const MIN_EXCESS_DB: f32 = 2.0;

/// The C/N of the block centred on `vfo_hz`, dB, from a spectrum row
/// (dBFS per bin, most negative frequency first) spanning `span_hz` around
/// `centre_hz`. `None` if the guards are not in the span or the band holds
/// no more than noise.
pub fn spectral_cn_db(db: &[f32], centre_hz: f64, span_hz: f64, vfo_hz: f64) -> Option<f32> {
    if db.len() < 64 || span_hz <= 0.0 {
        return None;
    }
    let n = db.len();
    let lo_hz = centre_hz - span_hz / 2.0;
    // The bin holding a frequency, clamped into the row.
    let bin = |hz: f64| (((hz - lo_hz) / span_hz) * n as f64).clamp(0.0, n as f64) as usize;
    let mean_power = |a: f64, b: f64| -> Option<f32> {
        // `a`..`b` are absolute frequencies, both inside the span (else None).
        if a < lo_hz || b > lo_hz + span_hz {
            return None;
        }
        let r = bin(a)..bin(b);
        if r.is_empty() {
            return None;
        }
        let sum: f32 = db[r.clone()].iter().map(|d| 10f32.powf(d / 10.0)).sum();
        Some(sum / r.len() as f32)
    };
    let signal = mean_power(vfo_hz - SIGNAL_HZ, vfo_hz + SIGNAL_HZ)?;
    let win = ((WINDOW_HZ / span_hz) * n as f64).round().max(1.0) as usize;
    // The quietest window in either zone (only those inside the span).
    let quietest = |a: f64, b: f64| -> Option<f32> {
        if a < lo_hz || b > lo_hz + span_hz {
            return None;
        }
        let lin: Vec<f32> = db[bin(a)..bin(b)]
            .iter()
            .map(|d| 10f32.powf(d / 10.0))
            .collect();
        if lin.len() < win {
            return None;
        }
        // A sliding sum: add the bin entering, drop the one leaving.
        let mut sum: f32 = lin[..win].iter().sum();
        let mut best = sum;
        for k in win..lin.len() {
            sum += lin[k] - lin[k - win];
            best = best.min(sum);
        }
        Some(best / win as f32)
    };
    let below = quietest(vfo_hz - GUARD_HZ.1, vfo_hz - GUARD_HZ.0);
    let above = quietest(vfo_hz + GUARD_HZ.0, vfo_hz + GUARD_HZ.1);
    let noise = match (below, above) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return None,
    };
    let excess = 10.0 * (signal / noise).log10();
    (excess >= MIN_EXCESS_DB).then(|| 10.0 * ((signal - noise) / noise).log10())
}

/// Widths, Hz, that count as the occupied band of a DVB-T or DVB-T2 block
/// (7.61 MHz, and up to 7.77 with T2's extended carriers).
const BLOCK_WIDTH_HZ: (f64, f64) = (6.8e6, 8.1e6);
/// An edge of the block is where the spectrum has dropped this far under the
/// block's level and stays down for [`EDGE_RUN_HZ`]: a fade inside the block
/// is deep but narrow, the edge is a cliff and then nothing.
const EDGE_DROP_DB: f32 = 10.0;
const EDGE_RUN_HZ: f64 = 150e3;
/// How far from the nominal centre the edges are looked for, Hz.
const EDGE_REACH_HZ: f64 = 4.3e6;

/// Where a block really is, for a signal whose centre is not where the channel
/// plan puts it (the UK's muxes are transmitted up to a third of a megahertz
/// off): its centre and its width, Hz, from a spectrum row (dBFS per bin,
/// most negative frequency first) spanning `span_hz` around `centre_hz`, looking
/// out from the channel's `nominal_hz`. `None` if the span does not hold the
/// block with room either side, nothing stands clear of the noise, or what
/// stands is not as wide as a block.
pub fn signal_block(
    db: &[f32],
    centre_hz: f64,
    span_hz: f64,
    nominal_hz: f64,
) -> Option<(f64, f64)> {
    let n = db.len();
    if n < 64 || span_hz <= 0.0 {
        return None;
    }
    let lo_hz = centre_hz - span_hz / 2.0;
    if nominal_hz - EDGE_REACH_HZ < lo_hz || nominal_hz + EDGE_REACH_HZ > lo_hz + span_hz {
        return None;
    }
    let hz_per_bin = span_hz / n as f64;
    let bin = |hz: f64| ((hz - lo_hz) / hz_per_bin) as usize;
    // The block's level: the middle of its inner part, which any offset the
    // plan can have leaves inside it.
    let mut inner: Vec<f32> = db[bin(nominal_hz - 2.5e6)..bin(nominal_hz + 2.5e6)].to_vec();
    inner.sort_by(f32::total_cmp);
    let level = inner[inner.len() / 2];
    // Something has to stand clear of the quiet part of the band round it.
    let mut all: Vec<f32> =
        db[bin(nominal_hz - EDGE_REACH_HZ)..bin(nominal_hz + EDGE_REACH_HZ)].to_vec();
    all.sort_by(f32::total_cmp);
    if level - all[all.len() / 33] < EDGE_DROP_DB + 2.0 {
        return None;
    }
    let threshold = level - EDGE_DROP_DB;
    let run = (EDGE_RUN_HZ / hz_per_bin).ceil().max(2.0) as usize;
    let start = bin(nominal_hz) as isize;
    let reach = (EDGE_REACH_HZ / hz_per_bin) as isize;
    // Out from the middle until the spectrum is down and stays down: the
    // edge is where that run began.
    let edge = |dir: isize| -> Option<f64> {
        let mut below = 0usize;
        for k in 1..=reach {
            let i = start + dir * k;
            if db[i as usize] < threshold {
                below += 1;
                if below >= run {
                    let first = i - dir * (run as isize - 1);
                    return Some(lo_hz + (first as f64 + 0.5) * hz_per_bin);
                }
            } else {
                below = 0;
            }
        }
        None
    };
    let (low, high) = (edge(-1)?, edge(1)?);
    let width = high - low;
    (BLOCK_WIDTH_HZ.0..=BLOCK_WIDTH_HZ.1)
        .contains(&width)
        .then_some(((low + high) / 2.0, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block of `level` dB between `lo` and `hi` Hz from the row's centre,
    /// noise at `floor`, with fades (100 kHz wide, 25 dB deep) at `notches`.
    fn block(floor: f32, level: f32, lo: f64, hi: f64, notches: &[f64]) -> Vec<f32> {
        (0..4096)
            .map(|k| {
                let hz = (k as f64 / 4096.0 - 0.5) * 10e6;
                if hz < lo || hz > hi {
                    floor
                } else if notches.iter().any(|n| (hz - n).abs() < 50e3) {
                    level - 25.0
                } else {
                    level
                }
            })
            .collect()
    }

    #[test]
    fn an_offset_block_is_found_by_its_edges_through_fades() {
        // A block 167 kHz low of its channel, with three fades in it.
        let r = block(-85.0, -50.0, -3.97e6, 3.64e6, &[-2.0e6, 0.4e6, 2.9e6]);
        let (centre, width) = signal_block(&r, 546e6, 10e6, 546e6).unwrap();
        assert!((centre - (546e6 - 165e3)).abs() < 15e3, "{centre}");
        assert!((width - 7.61e6).abs() < 80e3, "{width}");
    }

    #[test]
    fn neighbours_across_a_gap_do_not_move_the_edges() {
        // Blocks either side 0.4 MHz clear, at the same level.
        let mut r = block(-85.0, -50.0, -3.8e6, 3.8e6, &[]);
        for (k, v) in r.iter_mut().enumerate() {
            let hz = (k as f64 / 4096.0 - 0.5) * 10e6;
            if hz.abs() > 4.2e6 {
                *v = -50.0;
            }
        }
        let (centre, _) = signal_block(&r, 546e6, 10e6, 546e6).unwrap();
        assert!((centre - 546e6).abs() < 15e3, "{centre}");
    }

    #[test]
    fn noise_and_narrow_things_are_not_blocks() {
        assert_eq!(
            signal_block(&block(-85.0, -84.0, -3.8e6, 3.8e6, &[]), 546e6, 10e6, 546e6),
            None
        );
        // A 2 MHz carrier is not a mux.
        assert_eq!(
            signal_block(&block(-85.0, -50.0, -1.0e6, 1.0e6, &[]), 546e6, 10e6, 546e6),
            None
        );
        // The span too narrow to see round it.
        assert_eq!(
            signal_block(&block(-85.0, -50.0, -3.8e6, 3.8e6, &[]), 546e6, 8e6, 546e6),
            None
        );
    }

    /// A row of 4096 bins over 10 MHz: flat noise at `floor` dB, and a block
    /// of `block` dB (S + N) to ±`half` Hz of the centre.
    fn row(floor: f32, block: f32, half: f64) -> Vec<f32> {
        (0..4096)
            .map(|k| {
                let hz = (k as f64 / 4096.0 - 0.5) * 10e6;
                if hz.abs() < half { block } else { floor }
            })
            .collect()
    }

    #[test]
    fn a_block_twenty_db_over_the_floor_reads_about_twenty() {
        let r = row(-80.0, -60.0, 3.8e6);
        let cn = spectral_cn_db(&r, 546e6, 10e6, 546e6).unwrap();
        // S + N = 100 N, so S/N = 99: 19.96 dB.
        assert!((cn - 19.96).abs() < 0.1, "{cn}");
    }

    #[test]
    fn an_empty_channel_reads_nothing() {
        let r = row(-80.0, -79.5, 3.8e6);
        assert_eq!(spectral_cn_db(&r, 546e6, 10e6, 546e6), None);
    }

    #[test]
    fn the_cleaner_guard_is_the_noise() {
        // A neighbour fills the upper guard 15 dB above the floor.
        let mut r = row(-80.0, -60.0, 3.8e6);
        for (k, v) in r.iter_mut().enumerate() {
            if (k as f64 / 4096.0 - 0.5) * 10e6 > 4.2e6 {
                *v = -65.0;
            }
        }
        let cn = spectral_cn_db(&r, 546e6, 10e6, 546e6).unwrap();
        assert!((cn - 19.96).abs() < 0.1, "{cn}");
    }

    #[test]
    fn a_gap_between_neighbouring_blocks_gives_the_noise() {
        // Neighbours either side (-62 dB, 15 dB over the floor... and more),
        // 0.35 MHz clear of this block: only the gap is down at the noise.
        let r: Vec<f32> = (0..4096)
            .map(|k| {
                let hz = (k as f64 / 4096.0 - 0.5) * 10e6;
                match hz.abs() {
                    a if a < 3.8e6 => -60.0,
                    a if a < 4.15e6 => -80.0,
                    _ => -62.0,
                }
            })
            .collect();
        let cn = spectral_cn_db(&r, 546e6, 10e6, 546e6).unwrap();
        assert!((cn - 19.96).abs() < 0.2, "{cn}");
    }

    #[test]
    fn a_guard_outside_the_span_is_not_guessed() {
        // A 6 MHz span: the guards, 4.3 MHz out, are not in it.
        let r = row(-80.0, -60.0, 3.8e6);
        assert_eq!(spectral_cn_db(&r, 546e6, 6e6, 546e6), None);
    }
}
