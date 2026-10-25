// made by CasualArclamp
//! A PLP's cells of one frame to transport packets: the time and cell
//! interleavers undone, each FEC block decoded (LDPC, BCH) on as many threads
//! as asked, and the BBFRAMEs unpacked.

use std::time::{Duration, Instant};

use decdvbt_core::{CodeRate, FecFrame, Iq};
use decdvbt_fec::t2::bbframe::{TS_LEN, Unpacker};
use decdvbt_fec::t2::plp::{BlockStats, DecodeTimes, PlpCoder};
use decdvbt_fec::t2::qam::Qam;
use decdvbt_fec::t2::timeint::{CellInterleaver, ti_block_sizes};
use decdvbt_ofdm::t2::l1::PlpConfig;

/// The rate of a PLP_COD (EN 302 755 Table 28).
pub fn rate_of_cod(c: u8) -> Option<CodeRate> {
    Some(match c {
        0 => CodeRate::new(1, 2),
        1 => CodeRate::new(3, 5),
        2 => CodeRate::new(2, 3),
        3 => CodeRate::new(3, 4),
        4 => CodeRate::new(4, 5),
        5 => CodeRate::new(5, 6),
        6 => CodeRate::new(1, 3),
        7 => CodeRate::new(2, 5),
        _ => return None,
    })
}

/// What a PLP's coding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlpParams {
    pub frame: FecFrame,
    pub rate: CodeRate,
    pub qam: Qam,
    pub rotated: bool,
    pub ti_blocks: usize,
}

impl PlpParams {
    /// From L1-post's configuration of a PLP; `None` for a coding this
    /// receiver does not do (time interleaving over several frames).
    pub fn from_l1(p: &PlpConfig) -> Option<PlpParams> {
        if p.time_il_type {
            return None;
        }
        Some(PlpParams {
            // PLP_FEC_TYPE: 0 is 16K LDPC, 1 is 64K.
            frame: match p.plp_fec_type {
                0 => FecFrame::Short,
                1 => FecFrame::Normal,
                _ => return None,
            },
            rate: rate_of_cod(p.plp_cod)?,
            qam: Qam::from_plp_mod(p.plp_mod)?,
            rotated: p.plp_rotation,
            ti_blocks: p.time_il_length as usize,
        })
    }

    /// The C/N this receiver needs for the PLP's coding in a Gaussian channel,
    /// dB (see [`decdvbt_fec::t2::threshold`]).
    pub fn qef_cn_db(&self) -> Option<f32> {
        decdvbt_fec::t2::threshold::qef_cn_db(self.frame, self.rate, self.qam, self.rotated)
    }

    /// Cells in one FEC block.
    pub fn cells_per_block(&self) -> usize {
        self.frame.n_ldpc() / self.qam.bits()
    }
}

/// The first FEC block's constellation points from a frame's cells, `blocks`
/// blocks of the PLP from `start`: the interleavers undone, and for a
/// rotated constellation each point's I from its cell and Q from the next
/// (EN 302 755 §6.1.5's cyclic Q delay), turned back by the rotation's angle.
/// For the display: false if the cells do not reach.
pub fn first_block_points(
    params: &PlpParams,
    ci: &CellInterleaver,
    cells: &[Iq],
    start: usize,
    blocks: usize,
    out: &mut Vec<Iq>,
) -> bool {
    let cs = params.cells_per_block();
    let Some(&f) = ti_block_sizes(blocks, params.ti_blocks, false).first() else {
        return false;
    };
    if blocks == 0 || start + f * cs > cells.len() {
        return false;
    }
    let shift = ci.shifts(f)[0];
    let (mut a, mut b) = (vec![Iq::new(0.0, 0.0); cs], vec![Iq::new(0.0, 0.0); cs]);
    let src = &cells[start..start + f * cs];
    ci.deinterleave_block(src, src, f, 0, shift, params.ti_blocks != 0, &mut a, &mut b);
    out.clear();
    if !params.rotated {
        out.extend_from_slice(&a);
        return true;
    }
    let back = Iq::from_polar(1.0, -params.qam.rotation_deg().to_radians() as f32);
    out.extend((0..cs).map(|j| Iq::new(a[j].re, a[(j + 1) % cs].im) * back));
    true
}

/// What decoding a frame's blocks did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub blocks: usize,
    pub ok: usize,
    pub ldpc_iterations: u64,
    pub bch_corrected: u64,
    pub packets: usize,
    /// Wall time (summed over the threads) of the stages.
    pub deinterleave: Duration,
    pub blocks_time: DecodeTimes,
    /// The blocks (by number in the frame) that did not decode.
    pub failed: Vec<u16>,
}

pub struct PlpDecoder {
    pub params: PlpParams,
    /// Decode only this many of a frame's blocks (the first), when set.
    limit: Option<usize>,
    workers: Vec<PlpCoder>,
    ci: CellInterleaver,
    unpacker: Unpacker,
}

impl PlpDecoder {
    pub fn new(params: PlpParams, threads: usize) -> Option<PlpDecoder> {
        let mk = || PlpCoder::new(params.frame, params.rate, params.qam, params.rotated);
        let workers: Option<Vec<PlpCoder>> = (0..threads.max(1)).map(|_| mk()).collect();
        Some(PlpDecoder {
            params,
            limit: None,
            workers: workers?,
            ci: CellInterleaver::new(params.frame, params.qam),
            unpacker: Unpacker::default(),
        })
    }

    /// Resyncs the unpacker has counted.
    pub fn resyncs(&self) -> u64 {
        self.unpacker.resyncs
    }

    /// Forget a packet cut by the last frame (the stream has jumped).
    pub fn reset(&mut self) {
        self.unpacker.reset();
    }

    /// Decode only the first `n` blocks of each frame (`None`: all of them).
    /// For a signal under the code's threshold: a few blocks a frame tell
    /// when it comes back, at a small part of the cost of all of them.
    pub fn set_limit(&mut self, n: Option<usize>) {
        self.limit = n;
    }

    /// Give hopeless blocks up early (see `PlpCoder::set_hurried`).
    pub fn set_hurried(&mut self, on: bool) {
        for w in &mut self.workers {
            w.set_hurried(on);
        }
    }

    /// Threads (and so coders) this decoder has: the most it can use.
    pub fn max_threads(&self) -> usize {
        self.workers.len()
    }

    /// Decode `blocks` FEC blocks whose cells (and noise variances) start at
    /// `start` in the frame's cells, on `threads` threads (at most
    /// [`max_threads`](Self::max_threads)); transport packets go to `out`.
    pub fn decode_frame(
        &mut self,
        cells: &[Iq],
        var: &[f32],
        start: usize,
        blocks: usize,
        threads: usize,
        out: &mut Vec<[u8; TS_LEN]>,
    ) -> Option<FrameStats> {
        let cs = self.params.cells_per_block();
        let end = start.checked_add(blocks * cs)?;
        if blocks == 0 || end > cells.len() {
            return None;
        }
        // Where each block lies in its TI block's cells, and its cell
        // interleaver's shift: the workers undo both interleavers for the
        // blocks they decode.
        let sizes = ti_block_sizes(blocks, self.params.ti_blocks, false);
        let columns = self.params.ti_blocks != 0;
        let mut plan: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(blocks);
        let mut at = start;
        for &f in &sizes {
            for (r, shift) in self.ci.shifts(f).into_iter().enumerate() {
                plan.push((at, f, r, shift));
            }
            at += f * cs;
        }

        // The plan is the whole frame's (the blocks' places depend on how
        // many there are); a limit decodes its first blocks only.
        let blocks = self.limit.map_or(blocks, |l| l.min(blocks));
        let bb_len = self.workers[0].bbframe_bytes();
        let mut frames: Vec<Option<(Vec<u8>, BlockStats)>> = vec![None; blocks];
        let n_workers = threads.clamp(1, self.workers.len()).min(blocks);
        let per = blocks.div_ceil(n_workers);
        let (ci, plan) = (&self.ci, &plan);
        let mut deinterleave = Duration::ZERO;
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (w, (coder, chunk)) in self
                .workers
                .iter_mut()
                .zip(frames.chunks_mut(per))
                .enumerate()
            {
                handles.push(scope.spawn(move || {
                    let mut time = Duration::ZERO;
                    let (mut rx, mut rxv) = (vec![Iq::new(0.0, 0.0); cs], vec![0f32; cs]);
                    for (i, slot) in chunk.iter_mut().enumerate() {
                        let b = w * per + i;
                        let (ti_at, f, r, shift) = plan[b];
                        let t0 = Instant::now();
                        ci.deinterleave_block(
                            &cells[ti_at..ti_at + f * cs],
                            &var[ti_at..ti_at + f * cs],
                            f,
                            r,
                            shift,
                            columns,
                            &mut rx,
                            &mut rxv,
                        );
                        time += t0.elapsed();
                        let mut bb = vec![0u8; bb_len];
                        if let Some(st) = coder.decode(&rx, &rxv, &mut bb) {
                            *slot = Some((bb, st));
                        }
                    }
                    time
                }));
            }
            for h in handles {
                deinterleave += h.join().unwrap_or_default();
            }
        });
        let mut stats = FrameStats {
            blocks,
            deinterleave,
            ..Default::default()
        };
        for w in &mut self.workers {
            stats.blocks_time += w.take_times();
        }
        let before = out.len();
        for (b, f) in frames.into_iter().enumerate() {
            if f.is_none() {
                stats.failed.push(b as u16);
            }
            match f {
                Some((bb, st)) => {
                    stats.ok += 1;
                    stats.ldpc_iterations += st.ldpc.iterations as u64;
                    stats.bch_corrected += st.bch_corrected as u64;
                    self.unpacker.push(&bb, out);
                }
                None => self.unpacker.reset(),
            }
        }
        stats.packets = out.len() - before;
        Some(stats)
    }
}
