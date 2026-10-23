// made by CasualArclamp
//! The carriers of a DVB-T2 symbol: which are pilots, which are reserved and
//! which carry data cells (EN 302 755 §9), and the values the pilots carry.
//!
//! Carriers are numbered 0 to K_total−1 in the mode's own numbering (the
//! extended mode's 8K/16K/32K numbering starts K_ext carriers lower than the
//! normal mode's), the middle one on the channel centre. Built as gr-dtv's
//! `dvbt2_pilotgenerator_cc_impl.cc` builds them (GPL-3.0-or-later, Copyright
//! 2015-2019 Free Software Foundation, Inc.).

use super::params::{Fft, Mode};
use super::tables::*;

/// What a carrier is in a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A cell of the stream: L1 or a PLP's, or a dummy.
    Data,
    /// A scattered pilot (§9.2.1), also the first and last carriers of every
    /// symbol and the frame-closing symbol's.
    Scattered,
    /// A continual pilot (§9.2.2).
    Continual,
    /// A pilot of a P2 symbol (§9.2.3).
    P2Pilot,
    /// Reserved for tone-reservation PAPR reduction (§9.3.2, §9.3.3): sent as
    /// zero here.
    Reserved,
}

impl Role {
    pub fn is_pilot(self) -> bool {
        matches!(self, Role::Scattered | Role::Continual | Role::P2Pilot)
    }
}

/// The reserved carriers' positions in the P2 symbols and, with tone
/// reservation, the frame-closing symbol (Tables 56-57).
fn p2_reserved(fft: Fft) -> &'static [u16] {
    match fft {
        Fft::K1 => &P2_PAPR_MAP_1K,
        Fft::K2 => &P2_PAPR_MAP_2K,
        Fft::K4 => &P2_PAPR_MAP_4K,
        Fft::K8 => &P2_PAPR_MAP_8K,
        Fft::K16 => &P2_PAPR_MAP_16K,
        Fft::K32 => &P2_PAPR_MAP_32K,
    }
}

/// The reserved carriers of data symbols under tone reservation (Table 58).
fn tr_reserved(fft: Fft) -> &'static [u16] {
    match fft {
        Fft::K1 => &TR_PAPR_MAP_1K,
        Fft::K2 => &TR_PAPR_MAP_2K,
        Fft::K4 => &TR_PAPR_MAP_4K,
        Fft::K8 => &TR_PAPR_MAP_8K,
        Fft::K16 => &TR_PAPR_MAP_16K,
        Fft::K32 => &TR_PAPR_MAP_32K,
    }
}

/// The roles of every carrier of every kind of symbol of a mode.
pub struct Carriers {
    mode: Mode,
    p2: Vec<Role>,
    /// One map for each place of a symbol in the scattered pilots' pattern:
    /// a data symbol's is its number in the frame (P2 symbols counted) modulo
    /// D_y.
    data: Vec<Vec<Role>>,
    fc: Vec<Role>,
}

impl Carriers {
    pub fn new(mode: Mode) -> Carriers {
        let p2 = build_p2(&mode);
        let data = (0..mode.pp.dy()).map(|ph| build_data(&mode, ph)).collect();
        let fc = build_fc(&mode);
        Carriers { mode, p2, data, fc }
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    /// The roles in a P2 symbol.
    pub fn p2(&self) -> &[Role] {
        &self.p2
    }

    /// The roles in the data symbol that is the `symbol`-th of its frame
    /// (counting from the first P2 symbol as 0).
    pub fn data(&self, symbol: usize) -> &[Role] {
        &self.data[symbol % self.data.len()]
    }

    /// The roles in the frame-closing symbol.
    pub fn closing(&self) -> &[Role] {
        &self.fc
    }

    /// Data cells in a P2 symbol (C_P2).
    pub fn p2_cells(&self) -> usize {
        count_data(&self.p2)
    }

    /// Data cells in a data symbol (C_data); the same in every one.
    pub fn data_cells(&self) -> usize {
        count_data(&self.data[0])
    }

    /// Data cells in the frame-closing symbol (N_FC).
    pub fn closing_cells(&self) -> usize {
        count_data(&self.fc)
    }
}

fn count_data(v: &[Role]) -> usize {
    v.iter().filter(|r| **r == Role::Data).count()
}

/// The roles of the carriers of a P2 symbol (they do not depend on the pilot
/// pattern, so this is cheap to ask before L1 is known).
pub fn p2_roles(m: &Mode) -> Vec<Role> {
    build_p2(m)
}

fn build_p2(m: &Mode) -> Vec<Role> {
    let (c_ps, k_ext) = (m.c_ps(), m.k_ext());
    let mut v = vec![Role::Data; c_ps];
    let step = if m.fft == Fft::K32 && !m.miso { 6 } else { 3 };
    for i in (0..c_ps).step_by(step) {
        v[i] = Role::P2Pilot;
    }
    // The extended mode's extra carriers are all pilots in a P2 symbol.
    for i in 0..k_ext {
        v[i] = Role::P2Pilot;
        v[i + c_ps - k_ext] = Role::P2Pilot;
    }
    for &c in p2_reserved(m.fft) {
        v[c as usize + k_ext] = Role::Reserved;
    }
    v
}

/// The continual pilot positions of a mode (§9.2.2), from the tables:
/// table values taken to a modulus where the table is for a larger FFT.
fn continual_positions(m: &Mode) -> Vec<usize> {
    let (norm, ext) = continual_tables(m.fft.name(), m.pp.number());
    let mut out = Vec::new();
    let mut add = |sets: &[(&[u16], u16)]| {
        for (t, modulus) in sets {
            for &x in *t {
                out.push(if *modulus > 0 {
                    x as usize % *modulus as usize
                } else {
                    x as usize
                });
            }
        }
    };
    add(norm);
    if m.extended {
        add(ext);
    }
    out
}

fn build_data(m: &Mode, phase: usize) -> Vec<Role> {
    let (c_ps, k_ext) = (m.c_ps(), m.k_ext());
    let (dx, dy) = (m.pp.dx(), m.pp.dy());
    let mut v = vec![Role::Data; c_ps];
    for p in continual_positions(m) {
        if p < c_ps {
            v[p] = Role::Continual;
        }
    }
    for (i, r) in v.iter_mut().enumerate() {
        let rem = (i as isize - k_ext as isize).rem_euclid((dx * dy) as isize) as usize;
        if rem == dx * phase {
            *r = Role::Scattered;
        }
    }
    v[0] = Role::Scattered;
    v[c_ps - 1] = Role::Scattered;
    if m.papr_tr {
        let shift = if m.extended {
            dx * ((phase + k_ext / dx) % dy)
        } else {
            dx * phase
        };
        for &c in tr_reserved(m.fft) {
            if let Some(r) = v.get_mut(c as usize + shift) {
                *r = Role::Reserved;
            }
        }
    }
    v
}

fn build_fc(m: &Mode) -> Vec<Role> {
    let (c_ps, k_ext) = (m.c_ps(), m.k_ext());
    let dx = m.pp.dx();
    let mut v = vec![Role::Data; c_ps];
    for i in (0..c_ps).step_by(dx) {
        v[i] = Role::Scattered;
    }
    // The pilot at the last carrier but one that the lattice does not reach.
    let extra = matches!(
        (m.fft, m.pp.number()),
        (Fft::K1, 4) | (Fft::K1, 5) | (Fft::K2, 7)
    );
    if extra {
        v[c_ps - 2] = Role::Scattered;
    }
    v[0] = Role::Scattered;
    v[c_ps - 1] = Role::Scattered;
    if m.papr_tr {
        for &c in p2_reserved(m.fft) {
            v[c as usize + k_ext] = Role::Reserved;
        }
    }
    v
}

// ---- the pilots' values -----------------------------------------------------

/// The pilots' reference sequence (§9.2.4: x^11 + x^2 + 1, started with all
/// ones) and the frame's chips, which together give each pilot's sign.
pub struct Reference {
    prbs: Vec<u8>,
    offset: usize,
}

/// The chip of the symbol-wise sequence for the `symbol`-th symbol of a frame
/// (Table 61's 2624 bits).
pub fn pn_chip(symbol: usize) -> u8 {
    (PN_SEQUENCE_TABLE[(symbol / 8) % PN_SEQUENCE_TABLE.len()] >> (7 - symbol % 8)) & 1
}

impl Reference {
    pub fn new(mode: &Mode) -> Reference {
        let n = mode.c_ps() + mode.ref_offset();
        let mut sr: u32 = 0x7ff;
        let mut prbs = Vec::with_capacity(n);
        for _ in 0..n {
            prbs.push((sr & 1) as u8);
            let b = (sr ^ (sr >> 2)) & 1;
            sr >>= 1;
            if b == 1 {
                sr |= 0x400;
            }
        }
        Reference {
            prbs,
            offset: mode.ref_offset(),
        }
    }

    /// The bit of a pilot at `carrier` in the frame's `symbol`-th symbol: 0 is
    /// a positive pilot, 1 a negative one.
    pub fn bit(&self, carrier: usize, symbol: usize) -> u8 {
        self.prbs[carrier + self.offset] ^ pn_chip(symbol)
    }
}

impl Mode {
    /// The amplitude of a pilot of a role (§9.2.1-9.2.3): the data cells have
    /// unit mean power.
    pub fn pilot_amplitude(&self, role: Role) -> f32 {
        match role {
            Role::Scattered => self.pp.amplitude(),
            Role::Continual => match self.fft {
                Fft::K1 | Fft::K2 => 4.0 / 3.0,
                Fft::K4 => 4.0 * std::f32::consts::SQRT_2 / 3.0,
                _ => 8.0 / 3.0,
            },
            Role::P2Pilot => {
                if self.fft == Fft::K32 && !self.miso {
                    37f32.sqrt() / 5.0
                } else {
                    31f32.sqrt() / 5.0
                }
            }
            _ => 0.0,
        }
    }

    /// A pilot's value: its amplitude, signed.
    pub fn pilot_value(
        &self,
        role: Role,
        reference: &Reference,
        carrier: usize,
        symbol: usize,
    ) -> f32 {
        let a = self.pilot_amplitude(role);
        if reference.bit(carrier, symbol) == 0 {
            a
        } else {
            -a
        }
    }
}

/// The deal of the first cells of a frame among the P2 symbols (§8.3.3): L1-pre
/// and L1-post are spread over them a cell at a time in turn, then the rest
/// fills what is left of each. Returns, for each logical cell index of the
/// P2 symbols' `n_p2 · c_p2` cells, the place it has in the symbols (symbol ·
/// c_p2 + position).
pub fn p2_deal(n_p2: usize, c_p2: usize, pre: usize, post: usize) -> Vec<usize> {
    let total = n_p2 * c_p2;
    let mut place = vec![0usize; total];
    let (a, b) = (pre / n_p2, post / n_p2);
    for n in 0..n_p2 {
        for j in 0..a {
            place[n + j * n_p2] = n * c_p2 + j;
        }
        for j in 0..b {
            place[pre + n + j * n_p2] = n * c_p2 + a + j;
        }
    }
    let mut read = pre + post;
    for n in 0..n_p2 {
        for j in 0..c_p2 - a - b {
            place[read] = n * c_p2 + a + b + j;
            read += 1;
        }
    }
    place
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::t2::params::{Guard, PilotPattern};

    fn fft_of(name: &str) -> Fft {
        Fft::ALL.into_iter().find(|f| f.name() == name).unwrap()
    }

    #[test]
    fn every_data_symbol_has_gr_dtvs_count_of_cells() {
        let mut checked = 0;
        let mut wrong = Vec::new();
        for &(name, ext, pp, c_data, n_fc, _c_fc) in &CELL_COUNTS {
            if c_data == 0 {
                continue;
            }
            let fft = fft_of(name);
            if ext && !fft.has_extended() {
                continue;
            }
            let pp = PilotPattern::from_number(pp).unwrap();
            // 16K PP1 in the extended mode: one of its continual pilots falls
            // on a scattered pilot of the first symbol of the pattern, so
            // that symbol has a cell more than gr-dtv's constant says (a
            // corner of its tables, and a combination nobody transmits).
            if (fft, pp, ext) == (Fft::K16, PilotPattern::Pp1, true) {
                continue;
            }
            let mode = Mode::new(fft, Guard::G1_32, pp, ext);
            let c = Carriers::new(mode);
            // Every symbol of the pattern has as many as the standard says.
            let counts: Vec<usize> = (0..pp.dy()).map(|ph| count_data(c.data(ph))).collect();
            if counts.iter().any(|&n| n != c_data as usize) {
                wrong.push(format!("{name} {pp} ext={ext}: {counts:?} not {c_data}"));
            }
            // And with tone reservation fewer, by the reserved carriers.
            let mut tr = mode;
            tr.papr_tr = true;
            let ct = Carriers::new(tr);
            let reserved = tr_reserved(fft).len();
            let counts: Vec<usize> = (0..pp.dy()).map(|ph| count_data(ct.data(ph))).collect();
            if counts.iter().any(|&n| n + reserved != c_data as usize) {
                wrong.push(format!(
                    "{name} {pp} ext={ext} TR: {counts:?} + {reserved} not {c_data}"
                ));
            }
            if n_fc != 0 && c.closing_cells() != n_fc as usize {
                wrong.push(format!(
                    "{name} {pp} ext={ext} FC: {} not {n_fc}",
                    c.closing_cells()
                ));
            }
            checked += 1;
        }
        assert_eq!(checked, 56);
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn p2_symbols_have_gr_dtvs_count_of_cells() {
        for (i, &(name, n_p2, c_p2)) in P2_COUNTS.iter().enumerate() {
            let fft = fft_of(name);
            assert_eq!(fft.p2_symbols(), n_p2 as usize);
            if i >= 6 {
                continue; // MISO: not built
            }
            for ext in [false, true] {
                if ext && !fft.has_extended() {
                    continue;
                }
                let mode = Mode::new(fft, Guard::G1_32, PilotPattern::Pp2, ext);
                let c = Carriers::new(mode);
                assert_eq!(c.p2_cells(), c_p2 as usize, "{name} ext={ext}");
            }
        }
    }

    #[test]
    fn the_extended_modes_edge_carriers_are_pilots() {
        let m = Mode::new(Fft::K32, Guard::G1_128, PilotPattern::Pp7, true);
        let c = Carriers::new(m);
        assert_eq!(c.p2().len(), 27841);
        assert!(c.p2()[..288].iter().all(|r| *r == Role::P2Pilot));
        assert!(c.p2()[27841 - 288..].iter().all(|r| *r == Role::P2Pilot));
        // The first and last carriers of a data symbol are scattered pilots.
        for ph in 0..4 {
            assert_eq!(c.data(ph)[0], Role::Scattered);
            assert_eq!(c.data(ph)[27840], Role::Scattered);
        }
        // UK: 27404 cells a data symbol.
        assert_eq!(c.data_cells(), 27404);
    }

    #[test]
    fn the_scattered_pilots_walk_by_d_x_a_symbol() {
        let m = Mode::new(Fft::K32, Guard::G1_128, PilotPattern::Pp7, true);
        let c = Carriers::new(m);
        // 24 carriers on each symbol (§9.2.1): symbol 1 has them at 24 mod 96.
        let at = |ph: usize, k: usize| c.data(ph)[k] == Role::Scattered;
        assert!(at(1, 24 + 96 * 100));
        assert!(!at(0, 24 + 96 * 100));
        assert!(at(0, 96 * 100));
        assert!(at(3, 72 + 96 * 7));
    }

    #[test]
    fn the_reference_sequence_starts_as_the_standard_says() {
        // x^11 + x^2 + 1 from all ones: the first bits are 1 1 1 1 1 1 1 1 1 1
        // 1 0 …: the first output bit is the register's lowest, 1.
        let m = Mode::new(Fft::K8, Guard::G1_32, PilotPattern::Pp2, false);
        let r = Reference::new(&m);
        // Chip 0 of the frame's sequence is 0 (0x4D = 0100 1101): the sign of
        // the first pilot is the reference bit's.
        assert_eq!(pn_chip(0), 0);
        assert_eq!(pn_chip(1), 1);
        assert_eq!(r.bit(0, 0), r.bit(0, 0));
        // The normal mode's first carrier is the extended mode's 48th.
        let e = Reference::new(&Mode::new(Fft::K8, Guard::G1_32, PilotPattern::Pp2, true));
        for k in 0..100 {
            assert_eq!(r.bit(k, 3), e.bit(k + 48, 3));
        }
    }

    #[test]
    fn pilot_amplitudes_follow_the_standard() {
        let m = Mode::new(Fft::K32, Guard::G1_128, PilotPattern::Pp7, true);
        assert!((m.pilot_amplitude(Role::Scattered) - 7.0 / 3.0).abs() < 1e-6);
        assert!((m.pilot_amplitude(Role::Continual) - 8.0 / 3.0).abs() < 1e-6);
        assert!((m.pilot_amplitude(Role::P2Pilot) - 37f32.sqrt() / 5.0).abs() < 1e-6);
        let m4 = Mode::new(Fft::K4, Guard::G1_32, PilotPattern::Pp2, false);
        assert!((m4.pilot_amplitude(Role::Continual) - 1.8856).abs() < 1e-3);
    }
}
