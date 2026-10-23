// made by CasualArclamp
//! The L1 signalling's fields (EN 302 755 §7.2.3): L1-pre, which fixes the
//! frame's shape and says how L1-post is coded, and L1-post, with the
//! configuration of every physical layer pipe and the dynamic parameters of
//! the frame. Written to and read from bits (one to a byte, MSB first); the
//! CRC-32 and the coding belong to `decdvbt-fec`.
//!
//! The field order and widths follow gr-dtv's frame mapper
//! (`dvbt2_framemapper_cc_impl.cc`, GPL-3.0-or-later, Copyright 2015-2019
//! Free Software Foundation, Inc.) and the standard.

use super::params::{Fft, Guard, Mode, PilotPattern};

struct Writer(Vec<u8>);

impl Writer {
    fn put(&mut self, value: u64, bits: u32) {
        for n in (0..bits).rev() {
            self.0.push(((value >> n) & 1) as u8);
        }
    }
}

struct Reader<'a> {
    bits: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn get(&mut self, n: u32) -> Option<u64> {
        let end = self.at + n as usize;
        let s = self.bits.get(self.at..end)?;
        self.at = end;
        Some(s.iter().fold(0u64, |a, &b| (a << 1) | u64::from(b & 1)))
    }

    fn left(&self) -> usize {
        self.bits.len() - self.at
    }
}

/// L1-pre (§7.2.3.1): 168 bits, and a CRC-32 that makes 200.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct L1Pre {
    /// The input stream type: 0 for transport streams only.
    pub stream_type: u8,
    pub bwt_ext: bool,
    pub s1: u8,
    pub s2: u8,
    pub l1_repetition: bool,
    pub guard_interval: u8,
    pub papr: u8,
    pub l1_mod: u8,
    pub l1_cod: u8,
    pub l1_fec_type: u8,
    /// L1-post's cells and information bits (without the CRC).
    pub l1_post_size: u32,
    pub l1_post_info_size: u32,
    pub pilot_pattern: u8,
    pub tx_id_availability: u8,
    pub cell_id: u16,
    pub network_id: u16,
    pub t2_system_id: u16,
    pub num_t2_frames: u8,
    pub num_data_symbols: u16,
    pub regen_flag: u8,
    pub l1_post_extension: bool,
    pub num_rf: u8,
    pub current_rf_index: u8,
    pub t2_version: u8,
    pub l1_post_scrambled: bool,
    pub t2_base_lite: bool,
    pub reserved: u8,
}

pub const L1_PRE_INFO_BITS: usize = 168;

impl L1Pre {
    pub fn to_bits(&self) -> Vec<u8> {
        let mut w = Writer(Vec::with_capacity(L1_PRE_INFO_BITS));
        w.put(u64::from(self.stream_type), 8);
        w.put(u64::from(self.bwt_ext), 1);
        w.put(u64::from(self.s1), 3);
        w.put(u64::from(self.s2), 4);
        w.put(u64::from(self.l1_repetition), 1);
        w.put(u64::from(self.guard_interval), 3);
        w.put(u64::from(self.papr), 4);
        w.put(u64::from(self.l1_mod), 4);
        w.put(u64::from(self.l1_cod), 2);
        w.put(u64::from(self.l1_fec_type), 2);
        w.put(u64::from(self.l1_post_size), 18);
        w.put(u64::from(self.l1_post_info_size), 18);
        w.put(u64::from(self.pilot_pattern), 4);
        w.put(u64::from(self.tx_id_availability), 8);
        w.put(u64::from(self.cell_id), 16);
        w.put(u64::from(self.network_id), 16);
        w.put(u64::from(self.t2_system_id), 16);
        w.put(u64::from(self.num_t2_frames), 8);
        w.put(u64::from(self.num_data_symbols), 12);
        w.put(u64::from(self.regen_flag), 3);
        w.put(u64::from(self.l1_post_extension), 1);
        w.put(u64::from(self.num_rf), 3);
        w.put(u64::from(self.current_rf_index), 3);
        w.put(u64::from(self.t2_version), 4);
        w.put(u64::from(self.l1_post_scrambled), 1);
        w.put(u64::from(self.t2_base_lite), 1);
        w.put(u64::from(self.reserved), 4);
        debug_assert_eq!(w.0.len(), L1_PRE_INFO_BITS);
        w.0
    }

    pub fn from_bits(bits: &[u8]) -> Option<L1Pre> {
        if bits.len() != L1_PRE_INFO_BITS {
            return None;
        }
        let mut r = Reader { bits, at: 0 };
        let mut g = |n| r.get(n);
        Some(L1Pre {
            stream_type: g(8)? as u8,
            bwt_ext: g(1)? == 1,
            s1: g(3)? as u8,
            s2: g(4)? as u8,
            l1_repetition: g(1)? == 1,
            guard_interval: g(3)? as u8,
            papr: g(4)? as u8,
            l1_mod: g(4)? as u8,
            l1_cod: g(2)? as u8,
            l1_fec_type: g(2)? as u8,
            l1_post_size: g(18)? as u32,
            l1_post_info_size: g(18)? as u32,
            pilot_pattern: g(4)? as u8,
            tx_id_availability: g(8)? as u8,
            cell_id: g(16)? as u16,
            network_id: g(16)? as u16,
            t2_system_id: g(16)? as u16,
            num_t2_frames: g(8)? as u8,
            num_data_symbols: g(12)? as u16,
            regen_flag: g(3)? as u8,
            l1_post_extension: g(1)? == 1,
            num_rf: g(3)? as u8,
            current_rf_index: g(3)? as u8,
            t2_version: g(4)? as u8,
            l1_post_scrambled: g(1)? == 1,
            t2_base_lite: g(1)? == 1,
            reserved: g(4)? as u8,
        })
    }

    /// The FFT size, from S2's first field.
    pub fn fft(&self) -> Option<Fft> {
        Fft::from_s2_code(self.s2 >> 1)
    }

    pub fn guard(&self) -> Option<Guard> {
        Guard::from_l1_code(self.guard_interval)
    }

    pub fn pilots(&self) -> Option<PilotPattern> {
        PilotPattern::from_number(self.pilot_pattern + 1)
    }

    /// Whether the P1 said MISO (S1 1 or 4).
    pub fn miso(&self) -> bool {
        matches!(self.s1, 1 | 4)
    }

    /// The tone-reservation PAPR bit of PAPR: 2 (TR) or 3 (both).
    pub fn papr_tr(&self) -> bool {
        matches!(self.papr, 2 | 3)
    }

    /// The transmission this L1-pre describes, if it is one we can receive.
    pub fn mode(&self) -> Option<Mode> {
        let (fft, guard, pp) = (self.fft()?, self.guard()?, self.pilots()?);
        let mut m = Mode::new(fft, guard, pp, self.bwt_ext);
        m.miso = self.miso();
        m.papr_tr = self.papr_tr();
        Some(m)
    }
}

/// What L1-post says of one PLP and does not change from frame to frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlpConfig {
    pub plp_id: u8,
    pub plp_type: u8,
    pub plp_payload_type: u8,
    pub ff_flag: bool,
    pub first_rf_idx: u8,
    pub first_frame_idx: u8,
    pub plp_group_id: u8,
    pub plp_cod: u8,
    pub plp_mod: u8,
    pub plp_rotation: bool,
    pub plp_fec_type: u8,
    pub plp_num_blocks_max: u16,
    pub frame_interval: u8,
    pub time_il_length: u8,
    pub time_il_type: bool,
    pub in_band_a: bool,
    pub in_band_b: bool,
    pub reserved_1: u16,
    pub plp_mode: u8,
    pub static_flag: bool,
    pub static_padding_flag: bool,
}

/// A PLP's parameters in the current frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlpDynamic {
    pub plp_id: u8,
    pub plp_start: u32,
    pub plp_num_blocks: u16,
    pub reserved_4: u8,
}

/// L1-post (§7.2.3.2 and .3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct L1Post {
    pub sub_slices_per_frame: u16,
    pub num_plp: u8,
    pub num_aux: u8,
    pub aux_config_rfu: u8,
    /// RF index and frequency (Hz) of each RF channel.
    pub rf: Vec<(u8, u32)>,
    /// FEF type, length and interval when the S2 field says FEFs are there.
    pub fef: Option<(u8, u32, u8)>,
    pub plps: Vec<PlpConfig>,
    pub fef_length_msb: u8,
    pub reserved_2: u32,
    pub frame_idx: u8,
    pub sub_slice_interval: u32,
    pub type_2_start: u32,
    pub l1_change_counter: u8,
    pub start_rf_idx: u8,
    pub reserved_3: u8,
    pub dynamic: Vec<PlpDynamic>,
    pub reserved_5: u8,
    pub aux_dynamic: Vec<u64>,
    /// Whatever follows the dynamic part: the next frame's dynamic part when
    /// L1_REPETITION_FLAG is set, the extension.
    pub rest: Vec<u8>,
}

impl L1Post {
    /// The bits of a post-signalling with `rest` as it is (a transmitter
    /// writes none).
    pub fn to_bits(&self, pre: &L1Pre) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.put(u64::from(self.sub_slices_per_frame), 15);
        w.put(u64::from(self.num_plp), 8);
        w.put(u64::from(self.num_aux), 4);
        w.put(u64::from(self.aux_config_rfu), 8);
        for &(idx, f) in &self.rf {
            w.put(u64::from(idx), 3);
            w.put(u64::from(f), 32);
        }
        if pre.s2 & 1 == 1 {
            let (t, l, i) = self.fef.unwrap_or_default();
            w.put(u64::from(t), 4);
            w.put(u64::from(l), 22);
            w.put(u64::from(i), 8);
        }
        for p in &self.plps {
            w.put(u64::from(p.plp_id), 8);
            w.put(u64::from(p.plp_type), 3);
            w.put(u64::from(p.plp_payload_type), 5);
            w.put(u64::from(p.ff_flag), 1);
            w.put(u64::from(p.first_rf_idx), 3);
            w.put(u64::from(p.first_frame_idx), 8);
            w.put(u64::from(p.plp_group_id), 8);
            w.put(u64::from(p.plp_cod), 3);
            w.put(u64::from(p.plp_mod), 3);
            w.put(u64::from(p.plp_rotation), 1);
            w.put(u64::from(p.plp_fec_type), 2);
            w.put(u64::from(p.plp_num_blocks_max), 10);
            w.put(u64::from(p.frame_interval), 8);
            w.put(u64::from(p.time_il_length), 8);
            w.put(u64::from(p.time_il_type), 1);
            w.put(u64::from(p.in_band_a), 1);
            w.put(u64::from(p.in_band_b), 1);
            w.put(u64::from(p.reserved_1), 11);
            w.put(u64::from(p.plp_mode), 2);
            w.put(u64::from(p.static_flag), 1);
            w.put(u64::from(p.static_padding_flag), 1);
        }
        w.put(u64::from(self.fef_length_msb), 2);
        w.put(u64::from(self.reserved_2), 30);
        w.put(u64::from(self.frame_idx), 8);
        w.put(u64::from(self.sub_slice_interval), 22);
        w.put(u64::from(self.type_2_start), 22);
        w.put(u64::from(self.l1_change_counter), 8);
        w.put(u64::from(self.start_rf_idx), 3);
        w.put(u64::from(self.reserved_3), 8);
        for d in &self.dynamic {
            w.put(u64::from(d.plp_id), 8);
            w.put(u64::from(d.plp_start), 22);
            w.put(u64::from(d.plp_num_blocks), 10);
            w.put(u64::from(d.reserved_4), 8);
        }
        w.put(u64::from(self.reserved_5), 8);
        for a in &self.aux_dynamic {
            w.put(*a, 48);
        }
        w.0.extend_from_slice(&self.rest);
        w.0
    }

    /// Read L1-post's information bits (without the CRC) given L1-pre.
    pub fn from_bits(bits: &[u8], pre: &L1Pre) -> Option<L1Post> {
        let mut r = Reader { bits, at: 0 };
        let mut p = L1Post {
            sub_slices_per_frame: r.get(15)? as u16,
            num_plp: r.get(8)? as u8,
            num_aux: r.get(4)? as u8,
            aux_config_rfu: r.get(8)? as u8,
            ..Default::default()
        };
        for _ in 0..pre.num_rf {
            p.rf.push((r.get(3)? as u8, r.get(32)? as u32));
        }
        if pre.s2 & 1 == 1 {
            p.fef = Some((r.get(4)? as u8, r.get(22)? as u32, r.get(8)? as u8));
        }
        for _ in 0..p.num_plp {
            p.plps.push(PlpConfig {
                plp_id: r.get(8)? as u8,
                plp_type: r.get(3)? as u8,
                plp_payload_type: r.get(5)? as u8,
                ff_flag: r.get(1)? == 1,
                first_rf_idx: r.get(3)? as u8,
                first_frame_idx: r.get(8)? as u8,
                plp_group_id: r.get(8)? as u8,
                plp_cod: r.get(3)? as u8,
                plp_mod: r.get(3)? as u8,
                plp_rotation: r.get(1)? == 1,
                plp_fec_type: r.get(2)? as u8,
                plp_num_blocks_max: r.get(10)? as u16,
                frame_interval: r.get(8)? as u8,
                time_il_length: r.get(8)? as u8,
                time_il_type: r.get(1)? == 1,
                in_band_a: r.get(1)? == 1,
                in_band_b: r.get(1)? == 1,
                reserved_1: r.get(11)? as u16,
                plp_mode: r.get(2)? as u8,
                static_flag: r.get(1)? == 1,
                static_padding_flag: r.get(1)? == 1,
            });
        }
        p.fef_length_msb = r.get(2)? as u8;
        p.reserved_2 = r.get(30)? as u32;
        p.frame_idx = r.get(8)? as u8;
        p.sub_slice_interval = r.get(22)? as u32;
        p.type_2_start = r.get(22)? as u32;
        p.l1_change_counter = r.get(8)? as u8;
        p.start_rf_idx = r.get(3)? as u8;
        p.reserved_3 = r.get(8)? as u8;
        for _ in 0..p.num_plp {
            p.dynamic.push(PlpDynamic {
                plp_id: r.get(8)? as u8,
                plp_start: r.get(22)? as u32,
                plp_num_blocks: r.get(10)? as u16,
                reserved_4: r.get(8)? as u8,
            });
        }
        p.reserved_5 = r.get(8)? as u8;
        for _ in 0..p.num_aux {
            p.aux_dynamic.push(r.get(48)?);
        }
        let n = r.left();
        p.rest = r.bits[r.at..r.at + n].to_vec();
        Some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What gr-dtv's frame mapper writes for a single-PLP 256-QAM 2/3 stream.
    fn single_plp() -> (L1Pre, L1Post) {
        let pre = L1Pre {
            stream_type: 0,
            bwt_ext: true,
            s1: 0,
            s2: 10,
            guard_interval: 4,
            papr: 0,
            l1_mod: 1,
            l1_post_size: 750,
            l1_post_info_size: 318,
            pilot_pattern: 6,
            network_id: 0x3085,
            t2_system_id: 0x8001,
            num_t2_frames: 2,
            num_data_symbols: 59,
            num_rf: 1,
            t2_version: 1,
            ..Default::default()
        };
        let post = L1Post {
            sub_slices_per_frame: 1,
            num_plp: 1,
            rf: vec![(0, 729_833_333)],
            plps: vec![PlpConfig {
                plp_id: 0,
                plp_type: 1,
                plp_payload_type: 3,
                plp_group_id: 1,
                plp_cod: 2,
                plp_mod: 3,
                plp_rotation: true,
                plp_fec_type: 0,
                plp_num_blocks_max: 8,
                frame_interval: 1,
                time_il_length: 1,
                plp_mode: 2,
                ..Default::default()
            }],
            dynamic: vec![PlpDynamic {
                plp_id: 0,
                plp_start: 0,
                plp_num_blocks: 8,
                reserved_4: 0,
            }],
            ..Default::default()
        };
        (pre, post)
    }

    #[test]
    fn l1_pre_is_168_bits_and_round_trips() {
        let (pre, _) = single_plp();
        let bits = pre.to_bits();
        assert_eq!(bits.len(), L1_PRE_INFO_BITS);
        assert_eq!(L1Pre::from_bits(&bits), Some(pre.clone()));
        let m = pre.mode().unwrap();
        assert_eq!(m.fft, Fft::K32);
        assert_eq!(m.guard, Guard::G1_128);
        assert_eq!(m.pp, PilotPattern::Pp7);
        assert!(m.extended);
    }

    #[test]
    fn l1_post_of_one_plp_is_318_bits_before_the_crc() {
        // gr-dtv: KSIG_POST = 350 with the CRC: 318 of information.
        let (pre, post) = single_plp();
        let bits = post.to_bits(&pre);
        assert_eq!(bits.len(), 318);
        let back = L1Post::from_bits(&bits, &pre).unwrap();
        assert_eq!(back, post);
    }

    #[test]
    fn two_plps_and_an_rf_list_parse() {
        let (mut pre, mut post) = single_plp();
        pre.num_rf = 2;
        post.rf.push((1, 506_000_000));
        post.num_plp = 2;
        post.plps.push(PlpConfig {
            plp_id: 1,
            plp_type: 2,
            ..post.plps[0].clone()
        });
        post.dynamic.push(PlpDynamic {
            plp_id: 1,
            plp_start: 1000,
            plp_num_blocks: 3,
            reserved_4: 0,
        });
        let bits = post.to_bits(&pre);
        assert_eq!(L1Post::from_bits(&bits, &pre).unwrap(), post);
    }
}
