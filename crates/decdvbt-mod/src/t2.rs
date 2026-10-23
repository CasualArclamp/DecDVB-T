// made by CasualArclamp
//! The DVB-T2 modulator (EN 302 755): transport packets in, T2 frames out at
//! the sample rate 1/T (64/7 MS/s for 8 MHz), one P1 symbol, the P2 symbols and
//! the data symbols of each frame at a time. One data PLP of type 1 (the
//! layout of the UK's multiplexes and of gr-dtv's modulator), constant coding
//! and modulation, time interleaving within a frame (TIME_IL_TYPE 0).
//!
//! The chain: packets become BBFRAMEs, which the PLP's FEC turns into cells
//! (BCH, LDPC, bit interleaver, mapper with its rotation); the cells of a
//! frame are cell- and time-interleaved; L1-pre and L1-post cells and dummy
//! cells are put in front and behind them; the symbols' cells are
//! frequency-interleaved and set on the carriers among the pilots; each symbol
//! is transformed and given its guard interval; a P1 symbol starts the frame.
//!
//! After gr-dtv's DVB-T2 blocks (GPL-3.0-or-later, Copyright 2015-2019 Free
//! Software Foundation, Inc.; Ron Economos's modulator).

use std::sync::Arc;

use decdvbt_core::{CodeRate, FecFrame, Iq};
use decdvbt_fec::t2::bbframe::NmPacker;
use decdvbt_fec::t2::l1::{L1Coder, L1Mod, PostLayout, post_cells, pre_cells};
use decdvbt_fec::t2::plp::PlpCoder;
use decdvbt_fec::t2::qam::Qam;
use decdvbt_fec::t2::timeint::{CellInterleaver, interleave_frame, ti_block_sizes};
use decdvbt_ofdm::t2::carriers::p2_deal;
use decdvbt_ofdm::t2::l1::{L1Post, L1Pre, PlpConfig, PlpDynamic};
use decdvbt_ofdm::t2::{
    Carriers, Fft, FreqInterleaver, Guard, Mode, PilotPattern, Reference, Role, p1,
};
use rustfft::{Fft as FftTrait, FftPlanner};

use crate::ts::{TS_LEN, TsSource};

/// The sample rate of an 8 MHz channel, Hz (§9.5: 64/7 MHz).
pub const SAMPLE_RATE_HZ: f64 = 64e6 / 7.0;

/// Everything that makes a T2 signal.
#[derive(Debug, Clone)]
pub struct T2Config {
    pub mode: Mode,
    pub frame: FecFrame,
    pub rate: CodeRate,
    pub qam: Qam,
    pub rotated: bool,
    /// FEC blocks of the PLP in each T2 frame, and the TI blocks they are in.
    pub fec_blocks: usize,
    pub ti_blocks: usize,
    /// OFDM symbols of a frame after the P2 ones (L_data), the closing
    /// symbol, if the mode has one, counted.
    pub data_symbols: usize,
    pub frames_per_superframe: u8,
    pub l1_mod: L1Mod,
    pub plp_id: u8,
    pub cell_id: u16,
    pub network_id: u16,
    pub t2_system_id: u16,
    pub frequency_hz: u32,
    /// Say 8K or 32K with the other set of guard intervals (the UK's does).
    pub t2_gi: bool,
}

impl T2Config {
    /// The UK's multiplexes: 32K extended, guard 1/128, PP7, 256-QAM 2/3
    /// rotated, 64K LDPC, 202 blocks in three TI blocks.
    pub fn uk() -> T2Config {
        T2Config {
            mode: Mode::new(Fft::K32, Guard::G1_128, PilotPattern::Pp7, true),
            frame: FecFrame::Normal,
            rate: CodeRate::new(2, 3),
            qam: Qam::Qam256,
            rotated: true,
            fec_blocks: 202,
            ti_blocks: 3,
            data_symbols: 59,
            frames_per_superframe: 2,
            l1_mod: L1Mod::Qam64,
            plp_id: 0,
            cell_id: 3009,
            network_id: 12339,
            t2_system_id: 16435,
            frequency_hz: 545_833_333,
            t2_gi: true,
        }
    }

    /// A small signal for tests: 2K, QPSK or 16-QAM, short frames.
    pub fn small(qam: Qam, rotated: bool) -> T2Config {
        T2Config {
            mode: Mode::new(Fft::K2, Guard::G1_8, PilotPattern::Pp2, false),
            frame: FecFrame::Short,
            rate: CodeRate::new(1, 2),
            qam,
            rotated,
            fec_blocks: match qam {
                Qam::Qpsk => 2,
                Qam::Qam16 => 4,
                Qam::Qam64 => 6,
                Qam::Qam256 => 8,
            },
            ti_blocks: 2,
            data_symbols: 12,
            frames_per_superframe: 2,
            l1_mod: L1Mod::Qpsk,
            plp_id: 0,
            cell_id: 1,
            network_id: 1,
            t2_system_id: 1,
            frequency_hz: 500_000_000,
            t2_gi: false,
        }
    }
}

impl T2Config {
    /// A transmission from a description: `32k-1/128-pp7-256qam-2/3-ext-rot`
    /// (FFT size, guard interval, pilot pattern, constellation, code rate and
    /// any of `ext` (extended carriers), `rot` (rotated constellation),
    /// `short` (16K LDPC frames), `ti=N` (time interleaving blocks), `blocks=N`
    /// (FEC blocks a frame; by default as many as fit), `syms=N` (data
    /// symbols a frame; by default about 217 ms of them, the UK's).
    /// `uk` is the UK's multiplex.
    pub fn parse(spec: &str) -> Result<T2Config, String> {
        if spec == "uk" {
            return Ok(T2Config::uk());
        }
        let mut fft = Fft::K32;
        let mut guard = Guard::G1_128;
        let mut pp = PilotPattern::Pp7;
        let mut qam = Qam::Qam256;
        let mut rate = CodeRate::new(2, 3);
        let (mut ext, mut rot, mut short) = (false, false, false);
        let (mut ti, mut blocks, mut syms) = (3usize, None, None);
        for tok in spec.split('-') {
            let t = tok.to_ascii_lowercase();
            match t.as_str() {
                "1k" => fft = Fft::K1,
                "2k" => fft = Fft::K2,
                "4k" => fft = Fft::K4,
                "8k" => fft = Fft::K8,
                "16k" => fft = Fft::K16,
                "32k" => fft = Fft::K32,
                "qpsk" => qam = Qam::Qpsk,
                "16qam" => qam = Qam::Qam16,
                "64qam" => qam = Qam::Qam64,
                "256qam" => qam = Qam::Qam256,
                "ext" => ext = true,
                "rot" => rot = true,
                "short" => short = true,
                g if g.starts_with("pp") => {
                    pp = g[2..]
                        .parse()
                        .ok()
                        .and_then(PilotPattern::from_number)
                        .ok_or(format!("not a pilot pattern: {g}"))?;
                }
                g if g.starts_with("ti=") => ti = g[3..].parse().map_err(|_| "bad ti=")?,
                g if g.starts_with("blocks=") => {
                    blocks = Some(g[7..].parse().map_err(|_| "bad blocks=")?);
                }
                g if g.starts_with("syms=") => {
                    syms = Some(g[5..].parse().map_err(|_| "bad syms=")?);
                }
                g => {
                    let (a, b) = g.split_once('/').ok_or(format!("not understood: {g}"))?;
                    let (a, b): (usize, usize) = (
                        a.parse().map_err(|_| format!("not understood: {g}"))?,
                        b.parse().map_err(|_| format!("not understood: {g}"))?,
                    );
                    // 1/2 and 3/4 are only rates, 1/8 and 1/128 only guard
                    // intervals; 1/4 is a guard, 2/3 a rate, and so on.
                    if let Some(gi) = Guard::ALL.into_iter().find(|x| x.fraction() == (a, b)) {
                        guard = gi;
                    } else if plp_cod(CodeRate::new(a as u16, b as u16)).is_some() {
                        rate = CodeRate::new(a as u16, b as u16);
                    } else {
                        return Err(format!("not a guard interval or a code rate: {g}"));
                    }
                }
            }
        }
        let mode = Mode::new(fft, guard, pp, ext);
        let n_p2 = fft.p2_symbols();
        let data_symbols = syms.unwrap_or_else(|| {
            let ts = mode.symbol_samples() as f64 / SAMPLE_RATE_HZ;
            ((0.217 / ts).round() as usize).saturating_sub(n_p2).max(3)
        });
        let mut cfg = T2Config {
            mode,
            frame: if short {
                FecFrame::Short
            } else {
                FecFrame::Normal
            },
            rate,
            qam,
            rotated: rot,
            fec_blocks: 1,
            ti_blocks: ti,
            data_symbols,
            frames_per_superframe: 2,
            l1_mod: L1Mod::Qam64,
            plp_id: 0,
            cell_id: 3009,
            network_id: 12339,
            t2_system_id: 16435,
            frequency_hz: 545_833_333,
            t2_gi: fft == Fft::K32 || fft == Fft::K8,
        };
        cfg.fec_blocks = match blocks {
            Some(b) => b,
            None => cfg.max_blocks()?,
        };
        cfg.ti_blocks = cfg.ti_blocks.min(cfg.fec_blocks);
        Ok(cfg)
    }

    /// Cells in a FEC block of the PLP (0 for a code that does not exist).
    pub fn cells_per_block(&self) -> usize {
        PlpCoder::new(self.frame, self.rate, self.qam, self.rotated).map_or(0, |p| p.cells())
    }

    /// The most FEC blocks a frame can hold with its L1.
    pub fn max_blocks(&self) -> Result<usize, String> {
        let layout = FrameLayout::new(&self.mode, self.data_symbols)?;
        let cs = PlpCoder::new(self.frame, self.rate, self.qam, self.rotated)
            .ok_or("no such code")?
            .cells();
        // L1-post's cells for one PLP (318 bits and the CRC).
        let post = PostLayout::new(350, self.l1_mod, layout.n_p2)
            .ok_or("L1-post does not fit")?
            .cells;
        let free = layout
            .total_cells
            .checked_sub(decdvbt_fec::t2::l1::PRE_CELLS + post + (layout.n_fc - layout.c_fc))
            .ok_or("the frame is too short for its L1")?;
        Ok((free / cs).max(1))
    }
}

/// The code of PLP_COD for a rate (Table 28).
pub fn plp_cod(rate: CodeRate) -> Option<u8> {
    Some(match (rate.num, rate.den) {
        (1, 2) => 0,
        (3, 5) => 1,
        (2, 3) => 2,
        (3, 4) => 3,
        (4, 5) => 4,
        (5, 6) => 5,
        (1, 3) => 6,
        (2, 5) => 7,
        _ => return None,
    })
}

/// The rate of a PLP_COD.
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

/// How a frame's cells are laid out.
#[derive(Debug, Clone, Copy)]
pub struct FrameLayout {
    pub n_p2: usize,
    pub c_p2: usize,
    pub c_data: usize,
    pub n_fc: usize,
    pub c_fc: usize,
    /// Symbols of the frame: P2, data and the closing one.
    pub symbols: usize,
    pub has_fc: bool,
    /// All the cells a frame holds that carry L1, a PLP or dummies.
    pub total_cells: usize,
}

impl FrameLayout {
    pub fn new(mode: &Mode, data_symbols: usize) -> Result<FrameLayout, String> {
        if !mode.allowed() {
            return Err(format!("{} is not allowed at {}", mode.pp, mode.fft));
        }
        let carriers = Carriers::new(*mode);
        let n_p2 = mode.fft.p2_symbols();
        let c_p2 = carriers.p2_cells();
        let c_data = carriers.data_cells();
        let has_fc = mode.has_closing_symbol();
        let (n_fc, c_fc) = if has_fc {
            let n = carriers.closing_cells();
            let table = decdvbt_ofdm::t2::tables::CELL_COUNTS
                .iter()
                .find(|r| r.0 == mode.fft.name() && r.1 == mode.extended && r.2 == mode.pp.number())
                .map_or(n, |r| r.5 as usize);
            (n, table)
        } else {
            (0, 0)
        };
        if has_fc && data_symbols < 1 {
            return Err("a frame with a closing symbol needs a data symbol".into());
        }
        let normal = data_symbols - usize::from(has_fc);
        let total_cells = n_p2 * c_p2 + normal * c_data + n_fc;
        Ok(FrameLayout {
            n_p2,
            c_p2,
            c_data,
            n_fc,
            c_fc,
            symbols: n_p2 + data_symbols,
            has_fc,
            total_cells,
        })
    }
}

pub struct T2Modulator {
    cfg: T2Config,
    layout: FrameLayout,
    carriers: Carriers,
    reference: Reference,
    plp: PlpCoder,
    ci: CellInterleaver,
    ti_sizes: Vec<usize>,
    l1coder: L1Coder,
    l1pre: L1Pre,
    l1post: L1Post,
    post_layout: PostLayout,
    pre_cells: Vec<Iq>,
    packer: NmPacker,
    ts: Box<dyn TsSource>,
    fi_p2: FreqInterleaver,
    fi_data: FreqInterleaver,
    fi_fc: Option<FreqInterleaver>,
    ifft: Arc<dyn FftTrait<f32>>,
    scratch: Vec<Iq>,
    p1: Vec<Iq>,
    dummy: Vec<Iq>,
    frame: u64,
    /// The last frame's cells in logical order, for tests.
    pub last_cells: Vec<Iq>,
}

impl T2Modulator {
    pub fn new(cfg: T2Config, ts: Box<dyn TsSource>) -> Result<T2Modulator, String> {
        let layout = FrameLayout::new(&cfg.mode, cfg.data_symbols)?;
        let plp = PlpCoder::new(cfg.frame, cfg.rate, cfg.qam, cfg.rotated).ok_or("no such code")?;
        let cs = plp.cells();
        let cod = plp_cod(cfg.rate).ok_or("no such rate")?;
        let (fft, n) = (cfg.mode.fft, cfg.mode.fft.n());
        let s2 = (if cfg.t2_gi && matches!(fft, Fft::K8 | Fft::K32) {
            if fft == Fft::K8 { 6u8 } else { 7u8 }
        } else {
            fft.s2_code()
        }) << 1;
        // L1, with L1-post's size worked out from its content.
        let mut l1pre = L1Pre {
            stream_type: 0,
            bwt_ext: cfg.mode.extended,
            s1: p1::s1::T2_SISO,
            s2,
            guard_interval: cfg.mode.guard.l1_code(),
            l1_mod: cfg.l1_mod.code(),
            pilot_pattern: cfg.mode.pp.number() - 1,
            cell_id: cfg.cell_id,
            network_id: cfg.network_id,
            t2_system_id: cfg.t2_system_id,
            num_t2_frames: cfg.frames_per_superframe,
            num_data_symbols: cfg.data_symbols as u16,
            num_rf: 1,
            ..Default::default()
        };
        let l1post = L1Post {
            sub_slices_per_frame: 1,
            num_plp: 1,
            rf: vec![(0, cfg.frequency_hz)],
            plps: vec![PlpConfig {
                plp_id: cfg.plp_id,
                plp_type: 1,
                plp_payload_type: 3,
                plp_group_id: 1,
                plp_cod: cod,
                plp_mod: cfg.qam.plp_mod(),
                plp_rotation: cfg.rotated,
                plp_fec_type: u8::from(cfg.frame == FecFrame::Normal),
                plp_num_blocks_max: cfg.fec_blocks as u16,
                frame_interval: 1,
                time_il_length: cfg.ti_blocks as u8,
                ..Default::default()
            }],
            dynamic: vec![PlpDynamic {
                plp_id: cfg.plp_id,
                plp_start: 0,
                plp_num_blocks: cfg.fec_blocks as u16,
                reserved_4: 0,
            }],
            ..Default::default()
        };
        let info_bits = l1post.to_bits(&l1pre).len();
        let post_layout = PostLayout::new(info_bits + 32, cfg.l1_mod, layout.n_p2)
            .ok_or("L1-post does not fit")?;
        l1pre.l1_post_info_size = info_bits as u32;
        l1pre.l1_post_size = post_layout.cells as u32;
        // The frame must hold what is to go in it.
        let need = decdvbt_fec::t2::l1::PRE_CELLS
            + post_layout.cells
            + cfg.fec_blocks * cs
            + (layout.n_fc - layout.c_fc);
        if need > layout.total_cells {
            return Err(format!(
                "{} FEC blocks and the L1 need {need} cells, the frame has {}",
                cfg.fec_blocks, layout.total_cells
            ));
        }
        let l1coder = L1Coder::new();
        let pre = pre_cells(&l1coder.encode_pre(&l1pre.to_bits()));
        let mut sr: u32 = 0x4a80;
        let dummy: Vec<Iq> = (0..layout.total_cells)
            .map(|_| {
                let b = (sr ^ (sr >> 1)) & 1;
                sr >>= 1;
                if b == 1 {
                    sr |= 0x4000;
                }
                Iq::new(if b == 1 { -1.0 } else { 1.0 }, 0.0)
            })
            .collect();
        let ifft = FftPlanner::new().plan_fft_inverse(n);
        // P1: the S2 field's second bit says every preamble is the same.
        let s2_field = s2 >> 1;
        let p1s = p1_symbol(p1::s1::T2_SISO, p1::s2_value(s2_field, 0));
        Ok(T2Modulator {
            carriers: Carriers::new(cfg.mode),
            reference: Reference::new(&cfg.mode),
            ci: CellInterleaver::new(cfg.frame, cfg.qam),
            ti_sizes: ti_block_sizes(cfg.fec_blocks, cfg.ti_blocks, false),
            fi_p2: FreqInterleaver::new(fft, layout.c_p2),
            fi_data: FreqInterleaver::new(fft, layout.c_data),
            fi_fc: layout
                .has_fc
                .then(|| FreqInterleaver::new(fft, layout.n_fc)),
            scratch: vec![Iq::new(0.0, 0.0); ifft.get_inplace_scratch_len()],
            ifft,
            p1: p1s,
            dummy,
            frame: 0,
            last_cells: Vec::new(),
            plp,
            l1coder,
            l1pre,
            l1post,
            post_layout,
            pre_cells: pre,
            packer: NmPacker::default(),
            ts,
            layout,
            cfg,
        })
    }

    pub fn config(&self) -> &T2Config {
        &self.cfg
    }

    pub fn layout(&self) -> &FrameLayout {
        &self.layout
    }

    pub fn l1(&self) -> (&L1Pre, &L1Post) {
        (&self.l1pre, &self.l1post)
    }

    /// Samples in a frame: the P1 symbol and the OFDM symbols.
    pub fn frame_samples(&self) -> usize {
        p1::SAMPLES + self.layout.symbols * self.cfg.mode.symbol_samples()
    }

    /// The mean power of a sample (the signal is made to unit power).
    pub fn mean_power(&self) -> f64 {
        1.0
    }

    /// The frames made so far.
    pub fn frames(&self) -> u64 {
        self.frame
    }

    /// The cells of the next frame, in logical order (L1, the PLP, dummies).
    fn frame_cells(&mut self) -> Vec<Iq> {
        let frame_idx = (self.frame % u64::from(self.cfg.frames_per_superframe)) as u8;
        self.l1post.frame_idx = frame_idx;
        let bits = self.l1post.to_bits(&self.l1pre);
        let tx = self
            .l1coder
            .encode_post(&bits, &self.post_layout, self.l1pre.l1_post_scrambled);
        let mut post = Vec::new();
        post_cells(&tx, self.cfg.l1_mod, &mut post);

        let cs = self.plp.cells();
        let mut plp_cells: Vec<Iq> = Vec::with_capacity(self.cfg.fec_blocks * cs);
        let mut one = Vec::new();
        for _ in 0..self.cfg.fec_blocks {
            let bb = self.packer.frame(
                self.cfg.plp_id,
                self.plp.bbframe_bytes(),
                |p: &mut [u8; TS_LEN]| self.ts.next_packet(p),
            );
            self.plp.encode(&bb, &mut one);
            plp_cells.extend_from_slice(&one);
        }
        let plp_cells = interleave_frame(
            &self.ci,
            &self.ti_sizes,
            self.cfg.ti_blocks != 0,
            &plp_cells,
        );

        let mut cells = Vec::with_capacity(self.layout.total_cells);
        cells.extend_from_slice(&self.pre_cells);
        cells.extend_from_slice(&post);
        cells.extend_from_slice(&plp_cells);
        let zeros = self.layout.n_fc - self.layout.c_fc;
        let fill = self.layout.total_cells - cells.len() - zeros;
        cells.extend_from_slice(&self.dummy[..fill]);
        cells.extend(std::iter::repeat_n(Iq::new(0.0, 0.0), zeros));
        debug_assert_eq!(cells.len(), self.layout.total_cells);
        cells
    }

    /// Make the next frame's samples into `out` (cleared first).
    pub fn next_frame(&mut self, out: &mut Vec<Iq>) {
        let cells = self.frame_cells();
        self.last_cells.clone_from(&cells);
        let l = self.layout;
        let m = self.cfg.mode;
        let n = m.fft.n();
        let (c_ps, mid) = (m.c_ps(), (m.c_ps() - 1) / 2);
        let g = m.guard.samples(m.fft);
        let scale = m.carrier_scale();

        // The P2 symbols' cells, dealt, then each symbol's in turn.
        let post_cells = self.post_layout.cells;
        let deal = p2_deal(l.n_p2, l.c_p2, self.pre_cells.len(), post_cells);
        let mut p2_cells = vec![Iq::new(0.0, 0.0); l.n_p2 * l.c_p2];
        for (i, &c) in cells[..l.n_p2 * l.c_p2].iter().enumerate() {
            p2_cells[deal[i]] = c;
        }
        let mut rest = &cells[l.n_p2 * l.c_p2..];

        out.clear();
        out.reserve(self.frame_samples());
        out.extend_from_slice(&self.p1);
        let mut buf = vec![Iq::new(0.0, 0.0); n];
        let mut inter = Vec::new();
        for j in 0..l.symbols {
            let (roles, symbol_cells): (&[Role], &[Iq]) = if j < l.n_p2 {
                inter.clear();
                inter.resize(l.c_p2, Iq::new(0.0, 0.0));
                self.fi_p2
                    .interleave(j, &p2_cells[j * l.c_p2..(j + 1) * l.c_p2], &mut inter);
                (self.carriers.p2(), &inter)
            } else if l.has_fc && j == l.symbols - 1 {
                inter.clear();
                inter.resize(l.n_fc, Iq::new(0.0, 0.0));
                let fi = self.fi_fc.as_ref().expect("closing symbol");
                fi.interleave(j, &rest[..l.n_fc], &mut inter);
                rest = &rest[l.n_fc..];
                (self.carriers.closing(), &inter)
            } else {
                inter.clear();
                inter.resize(l.c_data, Iq::new(0.0, 0.0));
                self.fi_data.interleave(j, &rest[..l.c_data], &mut inter);
                rest = &rest[l.c_data..];
                (self.carriers.data(j), &inter)
            };
            buf.fill(Iq::new(0.0, 0.0));
            let mut next = 0;
            for (k, role) in roles.iter().enumerate().take(c_ps) {
                let v = match role {
                    Role::Data => {
                        let c = symbol_cells[next];
                        next += 1;
                        c
                    }
                    Role::Reserved => Iq::new(0.0, 0.0),
                    pilot => Iq::new(m.pilot_value(*pilot, &self.reference, k, j), 0.0),
                };
                buf[(k as isize - mid as isize).rem_euclid(n as isize) as usize] = v * scale;
            }
            debug_assert_eq!(next, symbol_cells.len());
            self.ifft.process_with_scratch(&mut buf, &mut self.scratch);
            out.extend_from_slice(&buf[n - g..]);
            out.extend_from_slice(&buf);
        }
        debug_assert!(rest.is_empty());
        self.frame += 1;
    }
}

/// The 2048 samples of the P1 symbol for S1 and S2: C (542 samples), A
/// (1024) and B (482), C and B being A shifted up by one carrier spacing
/// (§7.2.1 and Figure 24).
pub fn p1_symbol(s1: u8, s2: u8) -> Vec<Iq> {
    let values = p1::carrier_values(s1, s2);
    let mut freq = vec![Iq::new(0.0, 0.0); p1::FFT];
    for (i, &v) in values.iter().enumerate() {
        freq[p1::bin(i)] = Iq::new(f32::from(v), 0.0);
    }
    let mut shifted = vec![Iq::new(0.0, 0.0); p1::FFT];
    shifted[1..].copy_from_slice(&freq[..p1::FFT - 1]);
    shifted[0] = freq[p1::FFT - 1];
    let ifft = FftPlanner::<f32>::new().plan_fft_inverse(p1::FFT);
    let to_time = |f: &[Iq]| {
        // The centre is bin 512: swap the halves.
        let mut v = vec![Iq::new(0.0, 0.0); p1::FFT];
        v[p1::FFT / 2..].copy_from_slice(&f[..p1::FFT / 2]);
        v[..p1::FFT / 2].copy_from_slice(&f[p1::FFT / 2..]);
        ifft.process(&mut v);
        let norm = (p1::CARRIERS as f32).sqrt();
        for x in &mut v {
            *x /= norm;
        }
        v
    };
    let a = to_time(&freq);
    let b = to_time(&shifted);
    let mut out = Vec::with_capacity(p1::SAMPLES);
    out.extend_from_slice(&b[..p1::C_LEN]);
    out.extend_from_slice(&a);
    out.extend_from_slice(&b[p1::C_LEN..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts::TestStream;

    fn modulator(cfg: T2Config) -> T2Modulator {
        T2Modulator::new(cfg, Box::<TestStream>::default()).unwrap()
    }

    #[test]
    fn the_uk_frame_has_the_cells_the_real_signal_has() {
        let l = FrameLayout::new(&T2Config::uk().mode, 59).unwrap();
        assert_eq!((l.n_p2, l.c_p2, l.c_data), (1, 22432, 27404));
        assert!(!l.has_fc);
        assert_eq!(l.symbols, 60);
        // 202 blocks of 8100 cells and 2090 of L1 fit in 1 639 268.
        assert_eq!(l.total_cells, 22432 + 59 * 27404);
        assert!(202 * 8100 + 1840 + 250 < l.total_cells);
    }

    #[test]
    fn l1_is_what_the_real_signal_says() {
        let m = modulator(T2Config {
            fec_blocks: 4,
            ..T2Config::uk()
        });
        let (pre, post) = m.l1();
        assert_eq!(pre.l1_post_info_size, 318);
        assert_eq!(pre.l1_post_size, 250);
        assert_eq!(pre.s2, 14);
        assert_eq!(pre.guard_interval, 4);
        assert_eq!(pre.pilot_pattern, 6);
        assert_eq!(post.plps[0].plp_mod, 3);
        assert_eq!(post.plps[0].plp_fec_type, 1);
    }

    #[test]
    fn a_frame_has_the_samples_the_real_one_has() {
        // 2048 + 60 × 33024.
        let mut m = modulator(T2Config {
            fec_blocks: 4,
            ..T2Config::uk()
        });
        assert_eq!(m.frame_samples(), 2048 + 60 * 33024);
        let mut out = Vec::new();
        m.next_frame(&mut out);
        assert_eq!(out.len(), m.frame_samples());
        // Unit power overall, give or take the P1's own.
        let p: f64 = out.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / out.len() as f64;
        assert!((p - 1.0).abs() < 0.05, "{p}");
        // The guard interval is a copy of the symbol's end.
        let sym = &out[2048..2048 + 33024];
        assert_eq!(&sym[..256], &sym[32768..]);
    }

    #[test]
    fn the_small_modes_make_frames_too() {
        for (q, rot) in [(Qam::Qpsk, false), (Qam::Qam16, true)] {
            let mut m = modulator(T2Config::small(q, rot));
            let mut out = Vec::new();
            m.next_frame(&mut out);
            assert_eq!(out.len(), 2048 + (8 + 12) * (2048 + 256));
            m.next_frame(&mut out);
            assert_eq!(m.frames(), 2);
        }
    }

    #[test]
    fn p1_is_c_a_b_with_a_shifted_copies() {
        let p = p1_symbol(0, p1::s2_value(5, 0));
        assert_eq!(p.len(), 2048);
        // C is A's first 542 samples times e^{+j2πn/1024}, B the rest of A's
        // (from sample 542) likewise.
        let a = &p[542..542 + 1024];
        for n in 0..542 {
            let ph = std::f32::consts::TAU * n as f32 / 1024.0;
            let want = a[n] * Iq::new(ph.cos(), ph.sin());
            assert!((p[n] - want).norm() < 1e-3, "C sample {n}");
        }
        for n in 542..1024 {
            let ph = std::f32::consts::TAU * n as f32 / 1024.0;
            let want = a[n] * Iq::new(ph.cos(), ph.sin());
            assert!(
                (p[542 + 1024 + (n - 542)] - want).norm() < 1e-3,
                "B sample {n}"
            );
        }
    }

    #[test]
    fn a_description_makes_the_uk_signal_and_others() {
        let uk = T2Config::parse("32k-1/128-pp7-256qam-2/3-ext-rot").unwrap();
        assert_eq!(uk.fec_blocks, 202);
        assert_eq!(uk.data_symbols, 59);
        assert_eq!(uk.ti_blocks, 3);
        assert!(uk.rotated && uk.mode.extended);
        let small = T2Config::parse("8k-1/16-pp4-64qam-3/4-short").unwrap();
        assert_eq!(small.frame, FecFrame::Short);
        assert_eq!(small.rate, CodeRate::new(3, 4));
        assert_eq!(small.mode.guard, Guard::G1_16);
        assert!(small.fec_blocks > 10);
        // It builds.
        T2Modulator::new(small, Box::<TestStream>::default()).unwrap();
        assert!(T2Config::parse("32k-9/9").is_err());
    }

    #[test]
    fn the_p2_deal_is_a_permutation() {
        for (n_p2, c_p2) in [(2usize, 4472usize), (8, 1118), (16, 558)] {
            let d = p2_deal(n_p2, c_p2, 1840, 250 * 2 / n_p2 * n_p2);
            let mut seen = vec![false; n_p2 * c_p2];
            for &p in &d {
                assert!(!seen[p]);
                seen[p] = true;
            }
        }
    }
}
