// made by CasualArclamp
//! DVB-T2's transmission parameters: FFT size, guard interval, pilot pattern
//! and extended carrier mode, and what follows from them (EN 302 755 §8.3,
//! §9.2, Tables 62-63).

use std::fmt;

/// The OFDM transform size, 1K to 32K.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fft {
    K1,
    K2,
    K4,
    K8,
    K16,
    K32,
}

impl Fft {
    pub const ALL: [Fft; 6] = [Fft::K1, Fft::K2, Fft::K4, Fft::K8, Fft::K16, Fft::K32];

    /// The transform's length, which is the number of samples in a symbol's
    /// useful part.
    pub const fn n(self) -> usize {
        match self {
            Fft::K1 => 1024,
            Fft::K2 => 2048,
            Fft::K4 => 4096,
            Fft::K8 => 8192,
            Fft::K16 => 16384,
            Fft::K32 => 32768,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Fft::K1 => "1K",
            Fft::K2 => "2K",
            Fft::K4 => "4K",
            Fft::K8 => "8K",
            Fft::K16 => "16K",
            Fft::K32 => "32K",
        }
    }

    /// The code of S2 field 1 in the P1 symbol (§7.2.3.2, Table 18).
    pub const fn s2_code(self) -> u8 {
        match self {
            Fft::K2 => 0,
            Fft::K8 => 1,
            Fft::K4 => 2,
            Fft::K1 => 3,
            Fft::K16 => 4,
            Fft::K32 => 5,
        }
    }

    /// The FFT size of an S2 field 1 (6 and 7 are 8K and 32K with the other
    /// set of guard intervals).
    pub const fn from_s2_code(c: u8) -> Option<Fft> {
        Some(match c {
            0 => Fft::K2,
            1 | 6 => Fft::K8,
            2 => Fft::K4,
            3 => Fft::K1,
            4 => Fft::K16,
            5 | 7 => Fft::K32,
            _ => return None,
        })
    }

    /// Carriers in a symbol, K_total (Table 62): the extended mode (only in
    /// 8K, 16K and 32K) adds `ext_carriers` at each end.
    pub const fn active_carriers(self, extended: bool) -> usize {
        match (self, extended) {
            (Fft::K1, _) => 853,
            (Fft::K2, _) => 1705,
            (Fft::K4, _) => 3409,
            (Fft::K8, false) => 6817,
            (Fft::K8, true) => 6913,
            (Fft::K16, false) => 13633,
            (Fft::K16, true) => 13921,
            (Fft::K32, false) => 27265,
            (Fft::K32, true) => 27841,
        }
    }

    /// K_ext: the extra carriers at each end of the extended mode.
    pub const fn ext_carriers(self, extended: bool) -> usize {
        if !extended {
            return 0;
        }
        match self {
            Fft::K8 => 48,
            Fft::K16 => 144,
            Fft::K32 => 288,
            _ => 0,
        }
    }

    /// Where the reference sequence is started for the first carrier: the
    /// normal mode's carriers are the extended mode's less the extra ones.
    pub const fn ref_offset(self, extended: bool) -> usize {
        if extended {
            return 0;
        }
        match self {
            Fft::K8 => 48,
            Fft::K16 => 144,
            Fft::K32 => 288,
            _ => 0,
        }
    }

    /// N_P2: the P2 symbols of a frame (Table 63).
    pub const fn p2_symbols(self) -> usize {
        match self {
            Fft::K1 => 16,
            Fft::K2 => 8,
            Fft::K4 => 4,
            Fft::K8 => 2,
            Fft::K16 | Fft::K32 => 1,
        }
    }

    /// Whether the mode has an extended carrier mode.
    pub const fn has_extended(self) -> bool {
        matches!(self, Fft::K8 | Fft::K16 | Fft::K32)
    }
}

impl fmt::Display for Fft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The guard interval, as a fraction of the useful part (L1-pre
/// GUARD_INTERVAL, Table 20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Guard {
    G1_32,
    G1_16,
    G1_8,
    G1_4,
    G1_128,
    G19_128,
    G19_256,
}

impl Guard {
    pub const ALL: [Guard; 7] = [
        Guard::G1_128,
        Guard::G1_32,
        Guard::G1_16,
        Guard::G19_256,
        Guard::G1_8,
        Guard::G19_128,
        Guard::G1_4,
    ];

    /// The 3-bit code in L1-pre.
    pub const fn l1_code(self) -> u8 {
        match self {
            Guard::G1_32 => 0,
            Guard::G1_16 => 1,
            Guard::G1_8 => 2,
            Guard::G1_4 => 3,
            Guard::G1_128 => 4,
            Guard::G19_128 => 5,
            Guard::G19_256 => 6,
        }
    }

    pub const fn from_l1_code(c: u8) -> Option<Guard> {
        Some(match c {
            0 => Guard::G1_32,
            1 => Guard::G1_16,
            2 => Guard::G1_8,
            3 => Guard::G1_4,
            4 => Guard::G1_128,
            5 => Guard::G19_128,
            6 => Guard::G19_256,
            _ => return None,
        })
    }

    /// The fraction as numerator and denominator.
    pub const fn fraction(self) -> (usize, usize) {
        match self {
            Guard::G1_32 => (1, 32),
            Guard::G1_16 => (1, 16),
            Guard::G1_8 => (1, 8),
            Guard::G1_4 => (1, 4),
            Guard::G1_128 => (1, 128),
            Guard::G19_128 => (19, 128),
            Guard::G19_256 => (19, 256),
        }
    }

    /// The guard's length in samples for an FFT size.
    pub const fn samples(self, fft: Fft) -> usize {
        let (a, b) = self.fraction();
        fft.n() * a / b
    }

    pub const fn name(self) -> &'static str {
        match self {
            Guard::G1_32 => "1/32",
            Guard::G1_16 => "1/16",
            Guard::G1_8 => "1/8",
            Guard::G1_4 => "1/4",
            Guard::G1_128 => "1/128",
            Guard::G19_128 => "19/128",
            Guard::G19_256 => "19/256",
        }
    }
}

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The scattered pilot pattern PP1 to PP8 (§9.2.1, Table 51).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PilotPattern {
    Pp1,
    Pp2,
    Pp3,
    Pp4,
    Pp5,
    Pp6,
    Pp7,
    Pp8,
}

impl PilotPattern {
    pub const ALL: [PilotPattern; 8] = [
        PilotPattern::Pp1,
        PilotPattern::Pp2,
        PilotPattern::Pp3,
        PilotPattern::Pp4,
        PilotPattern::Pp5,
        PilotPattern::Pp6,
        PilotPattern::Pp7,
        PilotPattern::Pp8,
    ];

    /// 1 to 8; the code in L1-pre is one less.
    pub const fn number(self) -> u8 {
        self as u8 + 1
    }

    pub const fn from_number(n: u8) -> Option<PilotPattern> {
        Some(match n {
            1 => PilotPattern::Pp1,
            2 => PilotPattern::Pp2,
            3 => PilotPattern::Pp3,
            4 => PilotPattern::Pp4,
            5 => PilotPattern::Pp5,
            6 => PilotPattern::Pp6,
            7 => PilotPattern::Pp7,
            8 => PilotPattern::Pp8,
            _ => return None,
        })
    }

    /// Carriers between scattered pilots in a symbol, D_x, and symbols over
    /// which the pattern repeats, D_y.
    pub const fn dx(self) -> usize {
        match self {
            PilotPattern::Pp1 => 3,
            PilotPattern::Pp2 | PilotPattern::Pp3 | PilotPattern::Pp8 => 6,
            PilotPattern::Pp4 | PilotPattern::Pp5 => 12,
            PilotPattern::Pp6 | PilotPattern::Pp7 => 24,
        }
    }

    pub const fn dy(self) -> usize {
        match self {
            PilotPattern::Pp1 | PilotPattern::Pp3 | PilotPattern::Pp5 | PilotPattern::Pp7 => 4,
            PilotPattern::Pp2 | PilotPattern::Pp4 | PilotPattern::Pp6 => 2,
            PilotPattern::Pp8 => 16,
        }
    }

    /// The scattered pilots' amplitude against the data's rms (Table 51).
    pub fn amplitude(self) -> f32 {
        match self {
            PilotPattern::Pp1 | PilotPattern::Pp2 => 4.0 / 3.0,
            PilotPattern::Pp3 | PilotPattern::Pp4 => 7.0 / 4.0,
            _ => 7.0 / 3.0,
        }
    }
}

impl fmt::Display for PilotPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PP{}", self.number())
    }
}

/// A transmission: everything that decides where the carriers go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mode {
    pub fft: Fft,
    pub guard: Guard,
    pub pp: PilotPattern,
    /// The extended carrier mode (8K, 16K and 32K).
    pub extended: bool,
    /// A MISO transmission (the P1's S1 says): it has fewer P2 data cells and
    /// its own pilots. Only the SISO layout is supported.
    pub miso: bool,
    /// Tone reservation for PAPR reduction reserves carriers in every symbol.
    pub papr_tr: bool,
}

impl Mode {
    pub fn new(fft: Fft, guard: Guard, pp: PilotPattern, extended: bool) -> Mode {
        Mode {
            fft,
            guard,
            pp,
            extended: extended && fft.has_extended(),
            miso: false,
            papr_tr: false,
        }
    }

    /// K_total.
    pub fn c_ps(&self) -> usize {
        self.fft.active_carriers(self.extended)
    }

    pub fn k_ext(&self) -> usize {
        self.fft.ext_carriers(self.extended)
    }

    pub fn ref_offset(&self) -> usize {
        self.fft.ref_offset(self.extended)
    }

    /// The samples in a symbol, guard interval included.
    pub fn symbol_samples(&self) -> usize {
        self.fft.n() + self.guard.samples(self.fft)
    }

    /// Whether the frame ends with a frame-closing symbol (§8.3.2): not with
    /// the combinations (SISO) whose pilots already reach the last carrier,
    /// and not with a pattern that is not allowed at the FFT size.
    pub fn has_closing_symbol(&self) -> bool {
        use Guard::*;
        use PilotPattern::*;
        if !self.miso
            && matches!(
                (self.guard, self.pp),
                (G1_128, Pp7) | (G1_32, Pp4) | (G1_16, Pp2) | (G19_256, Pp2)
            )
        {
            return false;
        }
        // The patterns that are not allowed at an FFT size have no closing
        // symbol, and no data: see `Mode::allowed`.
        self.allowed()
    }

    /// Whether the pilot pattern is one the FFT size has (1K has PP1 to PP5,
    /// 2K and 4K all but PP6 and PP8, 8K all but PP6, 32K only PP2, PP4 and
    /// PP6 to PP8): the others have no cell counts in the standard.
    pub fn allowed(&self) -> bool {
        use Fft::*;
        use PilotPattern::*;
        match self.fft {
            K1 => matches!(self.pp, Pp1 | Pp2 | Pp3 | Pp4 | Pp5),
            K2 | K4 => !matches!(self.pp, Pp6 | Pp8),
            K8 => self.pp != Pp6,
            K16 => true,
            K32 => matches!(self.pp, Pp2 | Pp4 | Pp6 | Pp7 | Pp8),
        }
    }

    /// The scale that takes the carriers (cells of unit mean power, pilots at
    /// their boosts) to a signal of unit mean power (§9.5): the transform
    /// without its 1/N, times this.
    pub fn carrier_scale(&self) -> f32 {
        (5.0 / (27.0 * self.c_ps() as f64).sqrt()) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_counts_follow_table_62() {
        assert_eq!(Fft::K32.active_carriers(true), 27841);
        assert_eq!(Fft::K32.active_carriers(false), 27265);
        assert_eq!(Fft::K8.active_carriers(true), 6913);
        assert_eq!(Fft::K8.active_carriers(false), 6817);
        assert_eq!(Fft::K16.active_carriers(true), 13921);
        assert_eq!(Fft::K1.active_carriers(false), 853);
        // The extended mode adds K_ext at each end.
        for f in [Fft::K8, Fft::K16, Fft::K32] {
            assert_eq!(
                f.active_carriers(true),
                f.active_carriers(false) + 2 * f.ext_carriers(true)
            );
            assert_eq!(f.ref_offset(false), f.ext_carriers(true));
            assert_eq!(f.ref_offset(true), 0);
        }
    }

    #[test]
    fn guard_and_s2_codes_round_trip() {
        for g in Guard::ALL {
            assert_eq!(Guard::from_l1_code(g.l1_code()), Some(g));
        }
        for f in Fft::ALL {
            assert_eq!(Fft::from_s2_code(f.s2_code()), Some(f));
        }
        assert_eq!(Guard::G1_128.samples(Fft::K32), 256);
        assert_eq!(Guard::G19_256.samples(Fft::K32), 2432);
        assert_eq!(Guard::G19_128.samples(Fft::K8), 1216);
    }

    #[test]
    fn the_uk_mode_is_allowed_and_has_no_closing_symbol() {
        let m = Mode::new(Fft::K32, Guard::G1_128, PilotPattern::Pp7, true);
        assert!(m.allowed());
        assert!(!m.has_closing_symbol());
        assert_eq!(m.symbol_samples(), 33024);
        // PP5 is not allowed at 32K.
        assert!(!Mode::new(Fft::K32, Guard::G1_16, PilotPattern::Pp5, false).allowed());
    }

    #[test]
    fn pilot_patterns_match_table_51() {
        let (dx, dy): (Vec<_>, Vec<_>) = PilotPattern::ALL.iter().map(|p| (p.dx(), p.dy())).unzip();
        assert_eq!(dx, vec![3, 6, 6, 12, 12, 24, 24, 6]);
        assert_eq!(dy, vec![4, 2, 4, 2, 4, 2, 4, 16]);
    }
}
