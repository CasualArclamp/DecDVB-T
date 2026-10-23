// made by CasualArclamp
//! The DVB-T modulator: transport packets in, OFDM symbols out, one symbol at
//! a time at the sample rate 1/T (64/7 MS/s for 8 MHz), one sample a T.
//!
//! Each symbol is 68-per-frame numbered; its carriers are the data cells the
//! inner chain makes, the scattered and continual pilots (boosted 4/3, BPSK by
//! the reference PRBS) and the TPS carriers (DBPSK), placed on the carriers
//! of [`SymbolPlan`], inverse-transformed, and preceded by a copy of their
//! own last 1/4, 1/8, 1/16 or 1/32 (EN 300 744 §4.4–4.7).

use std::sync::Arc;

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Mode;
use decdvbt_ofdm::pilots::{
    FRAMES_PER_SUPERFRAME, Role, SYMBOLS_PER_FRAME, SymbolPlan, pilot_value, prbs, tps_carriers,
};
use decdvbt_ofdm::tps::{TPS_BITS, Tps};
use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::inner::InnerEncoder;
use crate::outer::OuterEncoder;
use crate::ts::{TS_LEN, TsSource};

pub struct Modulator {
    mode: Mode,
    tps: Tps,
    plan: SymbolPlan,
    wk: Vec<u8>,
    ts: Box<dyn TsSource>,
    outer: OuterEncoder,
    inner: InnerEncoder,
    /// Symbol in the frame (0–67) and frame in the superframe (0–3) of the
    /// symbol [`Modulator::next_symbol`] makes next.
    l: usize,
    frame: usize,
    tps_word: [u8; TPS_BITS],
    /// The sign each TPS carrier carries now (±1).
    tps_sign: Vec<f32>,
    ifft: Arc<dyn Fft<f32>>,
    fft_scratch: Vec<Iq>,
    buf: Vec<Iq>,
    packet: [u8; TS_LEN],
    block: Vec<u8>,
}

impl Modulator {
    pub fn new(
        mode: Mode,
        cell_id: Option<u16>,
        ts: Box<dyn TsSource>,
    ) -> Result<Modulator, String> {
        let inner = InnerEncoder::new(mode)?;
        let n = mode.fft.n();
        let ifft = FftPlanner::new().plan_fft_inverse(n);
        let tps = Tps { mode, cell_id };
        let mut m = Modulator {
            mode,
            tps,
            plan: SymbolPlan::new(mode.fft),
            wk: prbs(mode.fft.active_carriers()),
            ts,
            outer: OuterEncoder::default(),
            inner,
            l: 0,
            frame: 0,
            tps_word: tps.word(0),
            tps_sign: vec![1.0; tps_carriers(mode.fft).len()],
            fft_scratch: vec![Iq::new(0.0, 0.0); ifft.get_inplace_scratch_len()],
            ifft,
            buf: vec![Iq::new(0.0, 0.0); n],
            packet: [0; TS_LEN],
            block: Vec::new(),
        };
        m.tps_word = m.tps.word(0);
        Ok(m)
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// Samples a symbol, guard included.
    pub fn symbol_len(&self) -> usize {
        let n = self.mode.fft.n();
        n + n / self.mode.guard.divisor()
    }

    /// What each carrier is multiplied by on its way into the transform; the
    /// factor a test divides out after transforming back is `n` times this.
    pub fn carrier_scale(&self) -> f32 {
        1.0 / (self.mode.fft.active_carriers() as f32).sqrt()
    }

    /// Symbols to discard from the start of a stream for it to be ordinary:
    /// the outer byte interleaver takes (I − 1)·M·I = 2244 bytes to fill, and
    /// until it has, data bypass the randomiser as zeros and every data
    /// carrier carries the same point (one huge pulse a symbol).
    pub fn warmup_symbols(&self) -> usize {
        let (num, den) = self.mode.rate_hp.fraction();
        let bytes_a_symbol =
            self.mode.fft.data_carriers() * self.mode.modulation.bits() * num / den / 8;
        2244usize.div_ceil(bytes_a_symbol.max(1)) + 1
    }

    /// The mean power of a sample, from the carriers' powers: unit data
    /// cells, pilots at (4/3)², TPS at 1, all scaled by `carrier_scale`. The
    /// channel's C/N is set against it.
    pub fn mean_power(&self) -> f64 {
        let k = self.mode.fft.active_carriers() as f64;
        let data = self.mode.fft.data_carriers() as f64;
        let tps = tps_carriers(self.mode.fft).len() as f64;
        let pilots = k - data - tps;
        let s = f64::from(self.carrier_scale());
        s * s * (data + pilots * 16.0 / 9.0 + tps)
    }

    /// The (frame, symbol) the next symbol is.
    pub fn position(&self) -> (usize, usize) {
        (self.frame, self.l)
    }

    /// Make the next symbol into `out` (cleared first).
    pub fn next_symbol(&mut self, out: &mut Vec<Iq>) {
        let n = self.mode.fft.n();
        let k_total = self.mode.fft.active_carriers();
        let kmid = (k_total - 1) / 2;
        let l = self.l;
        let scale = self.carrier_scale();

        // Packets in, until the inner chain has what a symbol takes.
        while self.inner.bytes_needed() > 0 {
            self.ts.next_packet(&mut self.packet);
            self.block.clear();
            self.outer.push_packet(&self.packet, &mut self.block);
            self.inner.feed(&self.block);
        }
        let cells = self.inner.symbol_cells(l);

        // TPS: the first symbol of a frame sets each carrier from the PRBS,
        // the rest flip it for a 1 (§4.6.3).
        if l == 0 {
            self.tps_word = self.tps.word(self.frame);
            for (s, &k) in self.tps_sign.iter_mut().zip(tps_carriers(self.mode.fft)) {
                *s = 2.0 * (0.5 - f32::from(self.wk[k as usize]));
            }
        } else if self.tps_word[l] == 1 {
            for s in &mut self.tps_sign {
                *s = -*s;
            }
        }

        // The carriers, on the transform's bins: carrier k is at k − kmid
        // from the centre, negative frequencies wrapping to the top.
        self.buf.fill(Iq::new(0.0, 0.0));
        let (mut next_cell, mut next_tps) = (0, 0);
        for (k, role) in self.plan.roles(l).iter().enumerate() {
            let c: Complex32 = match role {
                Role::Data => {
                    let c = cells[next_cell];
                    next_cell += 1;
                    c
                }
                Role::Pilot => Complex32::new(pilot_value(self.wk[k]), 0.0),
                Role::Tps => {
                    let c = Complex32::new(self.tps_sign[next_tps], 0.0);
                    next_tps += 1;
                    c
                }
            };
            let bin = (k as isize - kmid as isize).rem_euclid(n as isize) as usize;
            self.buf[bin] = c * scale;
        }
        debug_assert_eq!(next_cell, cells.len());

        self.ifft
            .process_with_scratch(&mut self.buf, &mut self.fft_scratch);

        // The guard interval: the end of the symbol, then the symbol.
        let guard = n / self.mode.guard.divisor();
        out.clear();
        out.reserve(n + guard);
        out.extend_from_slice(&self.buf[n - guard..]);
        out.extend_from_slice(&self.buf);

        self.l += 1;
        if self.l == SYMBOLS_PER_FRAME {
            self.l = 0;
            self.frame = (self.frame + 1) % FRAMES_PER_SUPERFRAME;
        }
    }
}
