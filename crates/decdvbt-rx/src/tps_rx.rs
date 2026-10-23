// made by CasualArclamp
//! The TPS carriers of the equalised symbols: the bit each symbol carries (a
//! DBPSK vote over all the TPS carriers, weighted by their channel gain), the
//! frame's start from the sync word and the BCH parity, and the transmission
//! parameters the 68 bits say.
//!
//! Once a frame has been found the receiver counts symbols; every frame's end
//! it reads the block again, with the parity correcting up to two wrong bits,
//! and gives the position up if several in a row are beyond that.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::FftSize;
use decdvbt_ofdm::pilots::{SYMBOLS_PER_FRAME, tps_carriers};
use decdvbt_ofdm::tps::{TPS_BITS, TpsFrame, parse_correcting};

/// Wrong bits the parity may mend when a frame is first found (a 2-bit
/// mend passes a random block once in 4·10⁻⁶: too often for a search that
/// runs a thousand symbols a second) and once it is being followed.
const FIX_SEARCH: usize = 1;
const FIX_TRACK: usize = 2;
/// Frames in a row that may fail before the position is given up.
const LOST_AFTER: u32 = 4;

/// Where a symbol is in the transmission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Its place in the frame, 0–67.
    pub l: u8,
    /// The frame's place in the superframe, 0–3.
    pub frame: u8,
}

pub struct TpsReceiver {
    carriers: Vec<usize>,
    prev: Vec<Iq>,
    have_prev: bool,
    /// The last 68 symbols' votes: positive for "same as before".
    votes: [f32; TPS_BITS],
    /// Symbols seen since the search began (to know the history is full).
    seen: usize,
    /// While following: the next symbol's place.
    next: Option<Position>,
    /// The latest block that read correctly.
    info: Option<TpsFrame>,
    bad_in_a_row: u32,
    pub frames_ok: u64,
    pub frames_bad: u64,
    /// Wrong bits mended over the frames that read.
    pub bits_fixed: u64,
}

impl TpsReceiver {
    pub fn new(fft: FftSize) -> TpsReceiver {
        let carriers: Vec<usize> = tps_carriers(fft).iter().map(|&k| k as usize).collect();
        TpsReceiver {
            prev: vec![Iq::new(0.0, 0.0); carriers.len()],
            carriers,
            have_prev: false,
            votes: [0.0; TPS_BITS],
            seen: 0,
            next: None,
            info: None,
            bad_in_a_row: 0,
            frames_ok: 0,
            frames_bad: 0,
            bits_fixed: 0,
        }
    }

    pub fn reset(&mut self) {
        self.have_prev = false;
        self.seen = 0;
        self.next = None;
        self.info = None;
        self.bad_in_a_row = 0;
    }

    /// Whether the position in the frame is known.
    pub fn synced(&self) -> bool {
        self.next.is_some()
    }

    /// The latest transmission parameters read correctly.
    pub fn info(&self) -> Option<&TpsFrame> {
        self.info.as_ref()
    }

    /// The vote on a symbol's bit: +1 for the carriers keeping their sign
    /// since the last symbol, −1 for flipping, weighted by |H|².
    fn vote(&mut self, z: &[Iq], w: &[f32]) -> f32 {
        let (mut num, mut den) = (0.0f32, 0.0f32);
        for (i, &k) in self.carriers.iter().enumerate() {
            let c = z[k];
            if self.have_prev {
                num += w[k] * (c * self.prev[i].conj()).re;
            }
            den += w[k] * c.norm_sqr().max(1e-6);
            self.prev[i] = c;
        }
        let v = if self.have_prev && den > 0.0 {
            num / den
        } else {
            0.0
        };
        self.have_prev = true;
        v
    }

    /// The 68-bit block ending at the latest vote (s67 the newest).
    fn block(&self, newest: usize) -> [u8; TPS_BITS] {
        let mut b = [0u8; TPS_BITS];
        for (i, v) in b.iter_mut().enumerate().skip(1) {
            // s_i is `67 − i` votes before the newest.
            let at = (newest + TPS_BITS - (TPS_BITS - 1 - i)) % TPS_BITS;
            *v = u8::from(self.votes[at] < 0.0);
        }
        b
    }

    /// Take a symbol's equalised carriers `z` (all K) and their gains `w`
    /// (|H|²). Returns the symbol's place in the transmission, once known;
    /// the symbol that completes the search is the last of its frame.
    pub fn push(&mut self, z: &[Iq], w: &[f32]) -> Option<Position> {
        let v = self.vote(z, w);
        match self.next {
            None => {
                // The search: the block ending here, valid or not.
                let slot = self.seen % TPS_BITS;
                self.votes[slot] = v;
                self.seen += 1;
                if self.seen >= TPS_BITS {
                    let block = self.block(slot);
                    if let Ok((f, fixed)) = parse_correcting(&block, FIX_SEARCH) {
                        self.info = Some(f);
                        self.bad_in_a_row = 0;
                        self.frames_ok += 1;
                        self.bits_fixed += fixed as u64;
                        let frame = f.frame;
                        self.next = Some(Position {
                            l: 0,
                            frame: (frame + 1) % 4,
                        });
                        return Some(Position {
                            l: (SYMBOLS_PER_FRAME - 1) as u8,
                            frame,
                        });
                    }
                }
                None
            }
            Some(pos) => {
                let slot = pos.l as usize;
                self.votes[slot] = v;
                let here = pos;
                if slot == SYMBOLS_PER_FRAME - 1 {
                    // The frame's last symbol: read the block.
                    let block = self.block(slot);
                    match parse_correcting(&block, FIX_TRACK) {
                        Ok((f, fixed)) => {
                            self.frames_ok += 1;
                            self.bits_fixed += fixed as u64;
                            self.bad_in_a_row = 0;
                            self.info = Some(f);
                            // A block that reads is better than our count.
                            self.next = Some(Position {
                                l: 0,
                                frame: (f.frame + 1) % 4,
                            });
                        }
                        Err(_) => {
                            self.frames_bad += 1;
                            self.bad_in_a_row += 1;
                            if self.bad_in_a_row >= LOST_AFTER {
                                self.next = None;
                                self.seen = 0;
                                self.have_prev = false;
                                return Some(here);
                            }
                            self.next = Some(Position {
                                l: 0,
                                frame: (pos.frame + 1) % 4,
                            });
                        }
                    }
                } else {
                    self.next = Some(Position {
                        l: pos.l + 1,
                        frame: pos.frame,
                    });
                }
                Some(here)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chest::Equalizer;
    use crate::chest::tests::make;
    use crate::interp::{Interpolator, Planar};
    use crate::lock::lock_on;
    use crate::symbol::SymbolReader;
    use decdvbt_core::dvbt::Mode;
    use decdvbt_mod::channel::{Channel, Path};

    /// Run a simulated signal through the reader, the equaliser and the TPS
    /// receiver, collecting the position at every symbol.
    fn positions(
        mode_text: &str,
        ch: Channel,
        seconds: f64,
    ) -> (Vec<Option<Position>>, TpsReceiver) {
        let mode = Mode::parse(mode_text).unwrap();
        let x = make(mode_text, ch, seconds);
        let mut planar = Planar::default();
        planar.set(&x);
        let mut head = Planar::default();
        head.set(&x[..70_000]);
        let interp = Interpolator::new();
        let rho = 10e6 / mode.bandwidth.sample_rate_hz();
        let fs = mode.bandwidth.sample_rate_hz();
        let l = lock_on(&interp, &head, 0, rho, fs, 0.0, 200.0).expect("lock");
        let mut reader = SymbolReader::new(l.fft, l.guard, l.apex, l.f_total, rho, fs, 1);
        reader.alpha_cp = 0.05;
        reader.beta_cp = 0.0005;
        let mut eq = Equalizer::new(l.fft);
        let mut tps = TpsReceiver::new(l.fft);
        let mut carriers = Vec::new();
        let mut phase = l.phase;
        let mut out = Vec::new();
        while reader.read(&interp, &planar, 0, &mut carriers).is_ok() {
            eq.process(&mut carriers, phase);
            reader.correct(f64::from(eq.timing_error()));
            let pos = tps.push(&eq.z, &eq.w);
            if let Some(p) = pos {
                // The scattered pilots' phase and the frame position agree.
                assert_eq!(p.l as usize % 4, phase, "symbol phase vs l");
            }
            out.push(pos);
            phase = (phase + 1) % 4;
        }
        (out, tps)
    }

    #[test]
    fn the_frame_is_found_and_followed() {
        let ch = Channel {
            cn_db: Some(25.0),
            ..Default::default()
        };
        let (pos, tps) = positions("64qam-2/3-8k-1/32", ch, 0.4);
        assert!(tps.synced(), "never found the frame");
        let first = pos.iter().position(|p| p.is_some()).unwrap();
        // The first position given is the last symbol of a frame, found at
        // most two frames in (68 symbols to fill the block, one more).
        assert!(first <= 3 * 68, "found at {first}");
        assert_eq!(pos[first].unwrap().l, 67);
        // Every symbol after has the next position.
        for pair in pos[first..].windows(2) {
            let (a, b) = (pair[0].unwrap(), pair[1].unwrap());
            if a.l == 67 {
                assert_eq!((b.l, b.frame), (0, (a.frame + 1) % 4));
            } else {
                assert_eq!((b.l, b.frame), (a.l + 1, a.frame));
            }
        }
        let info = tps.info().unwrap();
        assert_eq!(info.fft, FftSize::K8);
        assert_eq!(info.length, 23);
        assert!(tps.frames_ok >= 2 && tps.frames_bad == 0);
    }

    #[test]
    fn it_follows_through_echoes_and_noise() {
        let ch = Channel {
            paths: vec![
                Path::echo(0.0, 0.0),
                Path::echo(2.5, -4.0),
                Path::echo(7.0, -9.0),
            ],
            cfo_hz: 166_667.0,
            sco_ppm: 15.0,
            cn_db: Some(18.0),
            ..Default::default()
        };
        let (pos, tps) = positions("16qam-1/2-8k-1/32", ch, 0.5);
        assert!(tps.synced());
        assert!(pos.iter().filter(|p| p.is_some()).count() > 300);
        assert!(tps.frames_bad <= 1, "{} bad frames", tps.frames_bad);
    }

    #[test]
    fn a_2k_signal_works_too() {
        let ch = Channel {
            cn_db: Some(20.0),
            ..Default::default()
        };
        let (_, tps) = positions("qpsk-1/2-2k-1/8", ch, 0.4);
        assert!(tps.synced());
        assert_eq!(tps.info().unwrap().fft, FftSize::K2);
    }
}
