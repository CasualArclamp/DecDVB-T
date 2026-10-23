// made by CasualArclamp
//! A PLP's cells of one frame to transport packets: the time and cell
//! interleavers undone, each FEC block decoded (LDPC, BCH) on as many threads
//! as asked, and the BBFRAMEs unpacked.

use decdvbt_core::{CodeRate, FecFrame, Iq};
use decdvbt_fec::t2::bbframe::{TS_LEN, Unpacker};
use decdvbt_fec::t2::plp::{BlockStats, PlpCoder};
use decdvbt_fec::t2::qam::Qam;
use decdvbt_fec::t2::timeint::{CellInterleaver, deinterleave_frame, ti_block_sizes};
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

    /// Cells in one FEC block.
    pub fn cells_per_block(&self) -> usize {
        self.frame.n_ldpc() / self.qam.bits()
    }
}

/// What decoding a frame's blocks did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub blocks: usize,
    pub ok: usize,
    pub ldpc_iterations: u64,
    pub bch_corrected: u64,
    pub packets: usize,
}

pub struct PlpDecoder {
    pub params: PlpParams,
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

    /// Decode `blocks` FEC blocks whose cells (and noise variances) start at
    /// `start` in the frame's cells; transport packets go to `out`.
    pub fn decode_frame(
        &mut self,
        cells: &[Iq],
        var: &[f32],
        start: usize,
        blocks: usize,
        out: &mut Vec<[u8; TS_LEN]>,
    ) -> Option<FrameStats> {
        let cs = self.params.cells_per_block();
        let end = start.checked_add(blocks * cs)?;
        if blocks == 0 || end > cells.len() {
            return None;
        }
        let sizes = ti_block_sizes(blocks, self.params.ti_blocks, false);
        let columns = self.params.ti_blocks != 0;
        let rx = deinterleave_frame(&self.ci, &sizes, columns, &cells[start..end]);
        let rxv = deinterleave_frame(&self.ci, &sizes, columns, &var[start..end]);

        let bb_len = self.workers[0].bbframe_bytes();
        let mut frames: Vec<Option<(Vec<u8>, BlockStats)>> = vec![None; blocks];
        let n_workers = self.workers.len().min(blocks);
        let per = blocks.div_ceil(n_workers);
        std::thread::scope(|scope| {
            for (w, (coder, chunk)) in self
                .workers
                .iter_mut()
                .zip(frames.chunks_mut(per))
                .enumerate()
            {
                let (rx, rxv) = (&rx, &rxv);
                scope.spawn(move || {
                    for (i, slot) in chunk.iter_mut().enumerate() {
                        let b = w * per + i;
                        let mut bb = vec![0u8; bb_len];
                        if let Some(st) = coder.decode(
                            &rx[b * cs..(b + 1) * cs],
                            &rxv[b * cs..(b + 1) * cs],
                            &mut bb,
                        ) && st.ldpc.converged
                        {
                            *slot = Some((bb, st));
                        }
                    }
                });
            }
        });
        let mut stats = FrameStats {
            blocks,
            ..Default::default()
        };
        let before = out.len();
        for f in frames {
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
