// made by CasualArclamp
//! A physical layer pipe's FEC chain (EN 302 755 §5, §6): a BBFRAME is
//! scrambled, BCH and LDPC coded, bit-interleaved and mapped to the cells of
//! one FEC block (or, the way back, the block's cells to the BBFRAME).

use std::time::{Duration, Instant};

use decdvbt_core::{CodeRate, FecFrame, Iq};

use super::bitmap::BitMap;
use super::qam::{Constellation, Qam};
use crate::bch::Bch;
use crate::ldpc::{DecodeOutcome, LdpcCode, LdpcDecoder};
use crate::params::FecParams;

/// LDPC iterations at most, and the LLR scale for the decoder.
const ITERATIONS: usize = 50;
/// Soft bits are quantised to a byte at this many steps per unit of LLR
/// (d₁−d₀ over the cell's noise variance). Measured on the UK multiplex off
/// the air: 4 took 12.3 LDPC iterations a block and failed 1 in 100; 8 takes
/// 8.5 and fails 1 in 500; 16 saturates the strong bits and fails more again.
const SCALE: f32 = 8.0;
/// The min-sum's normalization 1 − 2^−4: 7/8 (the DVB-S2 default) failed
/// twice as many blocks of the same signal.
const NORM_SHIFT: u32 = 4;

/// What decoding a block did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockStats {
    pub ldpc: DecodeOutcome,
    /// Bit errors the BCH code corrected.
    pub bch_corrected: usize,
}

/// Wall time spent in each stage of decoding blocks, since the last
/// [`PlpCoder::take_times`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeTimes {
    pub demap: Duration,
    /// The bit deinterleaver and the quantiser.
    pub gather: Duration,
    pub ldpc: Duration,
    pub bch: Duration,
}

impl std::ops::AddAssign for DecodeTimes {
    fn add_assign(&mut self, o: DecodeTimes) {
        self.demap += o.demap;
        self.gather += o.gather;
        self.ldpc += o.ldpc;
        self.bch += o.bch;
    }
}

pub struct PlpCoder {
    pub params: FecParams,
    pub qam: Qam,
    pub rotated: bool,
    bch: Bch,
    decoder: LdpcDecoder,
    map: BitMap,
    cst: Constellation,
    /// The BB scrambler's bytes for a BBFRAME.
    scrambler: Vec<u8>,
    // Working memory: the soft bits quantised, by cell and by codeword.
    cell_q: Vec<i8>,
    q: Vec<i8>,
    info: Vec<u8>,
    times: DecodeTimes,
    /// The soft bits' scale, the LDPC iterations at most.
    scale: f32,
    iterations: usize,
}

impl PlpCoder {
    pub fn new(frame: FecFrame, rate: CodeRate, qam: Qam, rotated: bool) -> Option<PlpCoder> {
        let params = FecParams::new_t2(frame, rate)?;
        let map = BitMap::new(frame, rate, qam)?;
        let n = params.n_ldpc;
        // The scrambler: 1 + x^14 + x^15, started with 100101010000000,
        // restarted for each BBFRAME.
        let mut sr: u32 = 0x4a80;
        let bits: Vec<u8> = (0..params.k_bch)
            .map(|_| {
                let b = (sr ^ (sr >> 1)) & 1;
                sr >>= 1;
                if b == 1 {
                    sr |= 0x4000;
                }
                b as u8
            })
            .collect();
        let mut scrambler = vec![0u8; params.k_bch / 8];
        for (i, &b) in bits.iter().enumerate() {
            scrambler[i / 8] |= b << (7 - i % 8);
        }
        Some(PlpCoder {
            bch: Bch::new(frame, params.t, params.n_bch),
            decoder: {
                let mut d = LdpcDecoder::new(LdpcCode::new(params.ldpc_table()));
                d.set_norm_shift(NORM_SHIFT);
                d
            },
            cst: Constellation::new(qam, rotated),
            cell_q: vec![0; n],
            q: vec![0; n],
            info: vec![0; params.n_bch / 8],
            times: DecodeTimes::default(),
            scale: SCALE,
            iterations: ITERATIONS,
            params,
            qam,
            rotated,
            map,
            scrambler,
        })
    }

    /// Cells in one FEC block.
    pub fn cells(&self) -> usize {
        self.map.cells
    }

    /// Change the decoder's tuning (for the tools that look for the best):
    /// soft bits a unit of LLR, the min-sum's normalization 1 − 2^−shift and
    /// the most LDPC iterations.
    pub fn set_tuning(&mut self, scale: f32, norm_shift: u32, iterations: usize) {
        self.scale = scale;
        self.decoder.set_norm_shift(norm_shift);
        self.iterations = iterations;
    }

    /// The time the stages took since the last call.
    pub fn take_times(&mut self) -> DecodeTimes {
        std::mem::take(&mut self.times)
    }

    /// A BBFRAME's length in bytes.
    pub fn bbframe_bytes(&self) -> usize {
        self.params.k_bch / 8
    }

    /// The cells of a BBFRAME (`bbframe_bytes` of them).
    pub fn encode(&self, bbframe: &[u8], out: &mut Vec<Iq>) {
        let p = &self.params;
        assert_eq!(bbframe.len(), p.k_bch / 8);
        let mut cw = vec![0u8; p.n_bch / 8];
        for ((d, s), m) in cw.iter_mut().zip(bbframe).zip(&self.scrambler) {
            *d = s ^ m;
        }
        self.bch.encode(&mut cw);
        let code = self.decoder.code();
        let mut parity = vec![0u8; (p.n_ldpc - p.n_bch) / 8];
        code.encode(&cw, &mut parity);
        cw.extend_from_slice(&parity);
        let bits: Vec<u8> = (0..p.n_ldpc)
            .map(|i| (cw[i / 8] >> (7 - i % 8)) & 1)
            .collect();
        let mut labels = Vec::new();
        self.map.labels(&bits, &mut labels);
        self.cst.map_block(&labels, out);
    }

    /// The BBFRAME of a FEC block's cells: `y` equalised cells (in the order
    /// the transmitter's mapper made them, after the interleavers are undone)
    /// and each cell's noise variance. `None` if the BCH code cannot make a
    /// codeword of what the LDPC code leaves.
    pub fn decode(&mut self, y: &[Iq], var: &[f32], bbframe: &mut [u8]) -> Option<BlockStats> {
        let p = self.params;
        assert_eq!(y.len(), self.map.cells);
        let t0 = Instant::now();
        self.cst.demap_block_q(y, var, self.scale, &mut self.cell_q);
        let t1 = Instant::now();
        self.map.gather_q(&self.cell_q, &mut self.q);
        let t2 = Instant::now();
        let ldpc = self
            .decoder
            .decode(&self.q, &mut self.info, self.iterations);
        let t3 = Instant::now();
        let bch = self.bch.decode(&mut self.info);
        let t4 = Instant::now();
        self.times.demap += t1 - t0;
        self.times.gather += t2 - t1;
        self.times.ldpc += t3 - t2;
        self.times.bch += t4 - t3;
        let bch_corrected = bch.ok()?;
        for ((o, s), m) in bbframe
            .iter_mut()
            .zip(&self.info[..p.k_bch / 8])
            .zip(&self.scrambler)
        {
            *o = s ^ m;
        }
        Some(BlockStats {
            ldpc,
            bch_corrected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 24) as u8
            })
            .collect()
    }

    fn noise(seed: &mut u32) -> f32 {
        // Roughly Gaussian: a sum of four uniforms.
        let mut a = 0.0;
        for _ in 0..4 {
            *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            a += (*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
        }
        a * 1.7320508
    }

    #[test]
    fn a_block_goes_through_clean_for_every_code_and_constellation() {
        let mut n = 0;
        for frame in [FecFrame::Short, FecFrame::Normal] {
            for rate in [(1, 2), (3, 5), (2, 3), (3, 4), (4, 5), (5, 6)] {
                for qam in Qam::ALL {
                    for rotated in [false, true] {
                        // The long ones only at one constellation each, to
                        // keep the test fast.
                        if frame == FecFrame::Normal
                            && (qam == Qam::Qpsk || (rotated && qam != Qam::Qam256))
                        {
                            continue;
                        }
                        let rate = CodeRate::new(rate.0, rate.1);
                        let mut c = PlpCoder::new(frame, rate, qam, rotated).unwrap();
                        let bb = frame_bytes(c.bbframe_bytes(), 5);
                        let mut cells = Vec::new();
                        c.encode(&bb, &mut cells);
                        assert_eq!(cells.len(), c.cells());
                        let var = vec![0.01f32; cells.len()];
                        let mut out = vec![0u8; bb.len()];
                        let st = c.decode(&cells, &var, &mut out).unwrap_or_else(|| {
                            panic!("{frame:?} {rate} {qam:?} rotated {rotated}: no codeword")
                        });
                        assert_eq!(out, bb, "{frame:?} {rate} {qam:?} rotated {rotated}");
                        assert!(st.ldpc.converged);
                        n += 1;
                    }
                }
            }
        }
        assert!(n > 40, "{n}");
    }

    #[test]
    fn the_uk_mode_survives_noise_at_its_threshold() {
        // 256-QAM 2/3 rotated, normal FECFRAME: needs about 17 dB C/N.
        let mut c =
            PlpCoder::new(FecFrame::Normal, CodeRate::new(2, 3), Qam::Qam256, true).unwrap();
        let bb = frame_bytes(c.bbframe_bytes(), 9);
        let mut cells = Vec::new();
        c.encode(&bb, &mut cells);
        let sigma2 = 10f32.powf(-21.0 / 10.0);
        let mut seed = 77;
        let y: Vec<Iq> = cells
            .iter()
            .map(|&x| x + Iq::new(noise(&mut seed), noise(&mut seed)) * (sigma2 / 2.0).sqrt())
            .collect();
        let var = vec![sigma2; y.len()];
        let mut out = vec![0u8; bb.len()];
        let st = c.decode(&y, &var, &mut out).expect("decodes at 21 dB");
        assert_eq!(out, bb);
        assert!(st.ldpc.iterations < 30, "{}", st.ldpc.iterations);
    }

    #[test]
    fn t2_uses_its_own_tables_for_two_codes() {
        let a = FecParams::new_t2(FecFrame::Normal, CodeRate::new(2, 3)).unwrap();
        let b = FecParams::new(FecFrame::Normal, CodeRate::new(2, 3)).unwrap();
        assert_ne!(a.ldpc_table().addr, b.ldpc_table().addr);
        let c = FecParams::new_t2(FecFrame::Short, CodeRate::new(3, 5)).unwrap();
        let d = FecParams::new(FecFrame::Short, CodeRate::new(3, 5)).unwrap();
        assert_ne!(c.ldpc_table().addr, d.ldpc_table().addr);
        // Others are DVB-S2's.
        let e = FecParams::new_t2(FecFrame::Normal, CodeRate::new(3, 4)).unwrap();
        let f = FecParams::new(FecFrame::Normal, CodeRate::new(3, 4)).unwrap();
        assert_eq!(e.ldpc_table().addr, f.ldpc_table().addr);
        // Not a T2 rate.
        assert!(FecParams::new_t2(FecFrame::Normal, CodeRate::new(9, 10)).is_none());
    }
}
