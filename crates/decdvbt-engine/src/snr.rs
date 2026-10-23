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
//!   the block (3.9–4.9 MHz out). Neighbouring channels fill much of that
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
const GUARD_HZ: (f64, f64) = (3.9e6, 4.9e6);
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

#[cfg(test)]
mod tests {
    use super::*;

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
