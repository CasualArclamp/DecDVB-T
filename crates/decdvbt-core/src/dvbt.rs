// made by CasualArclamp
//! DVB-T (EN 300 744) parameters the modulator, the receiver and the signal
//! card share: the transmission mode (FFT size, guard interval,
//! constellation, code rates, hierarchy, channel bandwidth), what it gives
//! (symbol time, useful bitrate) and the C/N it needs.

/// The carrier modulation (EN 300 744 §4.3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modulation {
    Qpsk,
    Qam16,
    Qam64,
}

impl Modulation {
    pub fn name(self) -> &'static str {
        match self {
            Modulation::Qpsk => "QPSK",
            Modulation::Qam16 => "16-QAM",
            Modulation::Qam64 => "64-QAM",
        }
    }

    /// Bits a carrier holds.
    pub fn bits(self) -> usize {
        match self {
            Modulation::Qpsk => 2,
            Modulation::Qam16 => 4,
            Modulation::Qam64 => 6,
        }
    }
}

/// The inner code rate (punctured K = 7 convolutional code, §4.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rate {
    R1_2,
    R2_3,
    R3_4,
    R5_6,
    R7_8,
}

impl Rate {
    pub const ALL: [Rate; 5] = [Rate::R1_2, Rate::R2_3, Rate::R3_4, Rate::R5_6, Rate::R7_8];

    /// (numerator, denominator).
    pub fn fraction(self) -> (usize, usize) {
        match self {
            Rate::R1_2 => (1, 2),
            Rate::R2_3 => (2, 3),
            Rate::R3_4 => (3, 4),
            Rate::R5_6 => (5, 6),
            Rate::R7_8 => (7, 8),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Rate::R1_2 => "1/2",
            Rate::R2_3 => "2/3",
            Rate::R3_4 => "3/4",
            Rate::R5_6 => "5/6",
            Rate::R7_8 => "7/8",
        }
    }
}

/// The OFDM size: 2k and 8k are DVB-T's; 4k is DVB-H's and is not handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FftSize {
    K2,
    K8,
}

impl FftSize {
    pub fn name(self) -> &'static str {
        match self {
            FftSize::K2 => "2k",
            FftSize::K8 => "8k",
        }
    }

    /// Carriers in the transform: the useful symbol is this many samples
    /// (EN 300 744 Table 14).
    pub fn n(self) -> usize {
        match self {
            FftSize::K2 => 2048,
            FftSize::K8 => 8192,
        }
    }

    /// Active carriers, K = Kmax − Kmin + 1: 1705 or 6817 (Table 6).
    pub fn active_carriers(self) -> usize {
        match self {
            FftSize::K2 => 1705,
            FftSize::K8 => 6817,
        }
    }

    /// Carriers that carry data in each symbol, once the scattered, the
    /// continual and the TPS pilots are out: 1512 or 6048 (§4.5).
    pub fn data_carriers(self) -> usize {
        match self {
            FftSize::K2 => 1512,
            FftSize::K8 => 6048,
        }
    }
}

/// The guard interval, a fraction of the useful symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Guard {
    G1_32,
    G1_16,
    G1_8,
    G1_4,
}

impl Guard {
    pub const ALL: [Guard; 4] = [Guard::G1_32, Guard::G1_16, Guard::G1_8, Guard::G1_4];

    pub fn name(self) -> &'static str {
        match self {
            Guard::G1_32 => "1/32",
            Guard::G1_16 => "1/16",
            Guard::G1_8 => "1/8",
            Guard::G1_4 => "1/4",
        }
    }

    /// The guard is Tu / this.
    pub fn divisor(self) -> usize {
        match self {
            Guard::G1_32 => 32,
            Guard::G1_16 => 16,
            Guard::G1_8 => 8,
            Guard::G1_4 => 4,
        }
    }
}

/// Hierarchical modulation: the constellation's α (TPS bits s40–s42,
/// Table 9). Non-hierarchical is the usual case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Alpha {
    NonHierarchical,
    A1,
    A2,
    A4,
}

impl Alpha {
    pub fn name(self) -> &'static str {
        match self {
            Alpha::NonHierarchical => "off",
            Alpha::A1 => "α = 1",
            Alpha::A2 => "α = 2",
            Alpha::A4 => "α = 4",
        }
    }
}

/// The channel's width: it sets the elementary period T (EN 300 744 §4.7:
/// 7/64, 1/8 or 7/48 µs; EN 302 755 §9.5, Table 66, for DVB-T2), so every
/// duration and frequency of the signal scales with it while its samples at
/// 1/T stay the same. Shared by DVB-T and DVB-T2: not every width is in both
/// standards ([`Bandwidth::dvbt`], [`Bandwidth::t2`]).
///
/// - **1 MHz** is not in either standard: DVB-T with every clock of the 8 MHz
///   mode divided by 8 (T = 7/8 µs, 8/7 MS/s, about 0.95 MHz occupied), the
///   way narrowband amateur DATV runs it. DVB-T only.
/// - **1.7 MHz** is DVB-T2's narrow channel (T = 71/131 µs, 1.845 MS/s; the
///   delivery descriptor calls it 1.712 MHz). DVB-T2 only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bandwidth {
    Mhz1,
    Mhz1_7,
    Mhz6,
    Mhz7,
    Mhz8,
}

impl Bandwidth {
    /// Every width, narrowest first.
    pub const ALL: [Bandwidth; 5] = [
        Bandwidth::Mhz1,
        Bandwidth::Mhz1_7,
        Bandwidth::Mhz6,
        Bandwidth::Mhz7,
        Bandwidth::Mhz8,
    ];

    /// The nominal width, MHz (what C/N is measured against).
    pub fn mhz(self) -> f64 {
        match self {
            Bandwidth::Mhz1 => 1.0,
            Bandwidth::Mhz1_7 => 1.7,
            Bandwidth::Mhz6 => 6.0,
            Bandwidth::Mhz7 => 7.0,
            Bandwidth::Mhz8 => 8.0,
        }
    }

    /// "1.7 MHz".
    pub fn name(self) -> &'static str {
        match self {
            Bandwidth::Mhz1 => "1 MHz",
            Bandwidth::Mhz1_7 => "1.7 MHz",
            Bandwidth::Mhz6 => "6 MHz",
            Bandwidth::Mhz7 => "7 MHz",
            Bandwidth::Mhz8 => "8 MHz",
        }
    }

    /// As a mode string spells it: "1mhz", "1.7mhz", "8mhz".
    pub fn token(self) -> &'static str {
        match self {
            Bandwidth::Mhz1 => "1mhz",
            Bandwidth::Mhz1_7 => "1.7mhz",
            Bandwidth::Mhz6 => "6mhz",
            Bandwidth::Mhz7 => "7mhz",
            Bandwidth::Mhz8 => "8mhz",
        }
    }

    /// Read a width as typed: "1.7", "1.7mhz", "8 MHz" (1.712 is 1.7).
    pub fn parse(s: &str) -> Option<Bandwidth> {
        let t = s.trim().to_ascii_lowercase().replace(' ', "");
        let t = t.strip_suffix("mhz").unwrap_or(&t);
        Some(match t {
            "1" => Bandwidth::Mhz1,
            "1.7" | "1.712" => Bandwidth::Mhz1_7,
            "6" => Bandwidth::Mhz6,
            "7" => Bandwidth::Mhz7,
            "8" => Bandwidth::Mhz8,
            _ => return None,
        })
    }

    /// Whether DVB-T is sent in this width (1 MHz: the scaled, non-standard
    /// mode above).
    pub fn dvbt(self) -> bool {
        self != Bandwidth::Mhz1_7
    }

    /// Whether DVB-T2 is (EN 302 755 Table 66; 1 MHz is not).
    pub fn t2(self) -> bool {
        self != Bandwidth::Mhz1
    }

    /// The narrow widths, whose native rate is under the HackRF's lowest
    /// sample rate (2 MS/s): they need a resampler to reach the radio.
    pub fn narrow(self) -> bool {
        matches!(self, Bandwidth::Mhz1 | Bandwidth::Mhz1_7)
    }

    /// The elementary period T, seconds.
    pub fn t_s(self) -> f64 {
        match self {
            // 7/64 µs × 8: the 8 MHz mode's clock divided by 8.
            Bandwidth::Mhz1 => 7.0 / 8.0 * 1e-6,
            // EN 302 755 Table 66.
            Bandwidth::Mhz1_7 => 71.0 / 131.0 * 1e-6,
            Bandwidth::Mhz6 => 7.0 / 48.0 * 1e-6,
            Bandwidth::Mhz7 => 1.0 / 8.0 * 1e-6,
            Bandwidth::Mhz8 => 7.0 / 64.0 * 1e-6,
        }
    }

    /// The sample rate at which one sample is one T: 64/7 MS/s for 8 MHz,
    /// 8/7 for 1 MHz, 131/71 for 1.7 MHz.
    pub fn sample_rate_hz(self) -> f64 {
        1.0 / self.t_s()
    }

    /// The bandwidth field (3 bits) of the terrestrial_delivery_system_descriptor
    /// (EN 300 468 §6.2.13.4, Table 46: 8, 7, 6, 5 MHz are 0 to 3; 4 to 7 are
    /// reserved for future use). 1 MHz has no code: DecDVB-T sends **7**, the
    /// last reserved value (the least likely to be given a meaning soon), and
    /// reads 7 back as 1 MHz. A standard receiver ignores or rejects it, which
    /// is right for a signal it could not receive anyway. `None`: not DVB-T.
    pub fn terrestrial_code(self) -> Option<u8> {
        match self {
            Bandwidth::Mhz8 => Some(0),
            Bandwidth::Mhz7 => Some(1),
            Bandwidth::Mhz6 => Some(2),
            Bandwidth::Mhz1 => Some(7),
            Bandwidth::Mhz1_7 => None,
        }
    }

    /// The width a terrestrial descriptor's code says, kHz (5 MHz included;
    /// 7 is DecDVB-T's 1 MHz, see [`Bandwidth::terrestrial_code`]; 0 unknown).
    pub fn terrestrial_khz(code: u8) -> u32 {
        [8000, 7000, 6000, 5000, 0, 0, 0, 1000][(code & 7) as usize]
    }

    /// The bandwidth field (4 bits) of the T2_delivery_system_descriptor
    /// (EN 300 468 §6.4.6.3, Table 149: 8, 7, 6, 5, 10 MHz are 0 to 4 and
    /// 1.712 MHz is 5). `None`: not DVB-T2.
    pub fn t2_code(self) -> Option<u8> {
        match self {
            Bandwidth::Mhz8 => Some(0),
            Bandwidth::Mhz7 => Some(1),
            Bandwidth::Mhz6 => Some(2),
            Bandwidth::Mhz1_7 => Some(5),
            Bandwidth::Mhz1 => None,
        }
    }

    /// The width a T2 descriptor's code says, kHz (0 unknown).
    pub fn t2_khz(code: u8) -> u32 {
        match code {
            0 => 8000,
            1 => 7000,
            2 => 6000,
            3 => 5000,
            4 => 10000,
            5 => 1712,
            _ => 0,
        }
    }

    /// The width against 8 MHz (1/8 for 1 MHz): what an 8 MHz figure in Hz
    /// (a filter, a search range) scales by.
    pub fn scale(self) -> f64 {
        self.sample_rate_hz() / Bandwidth::Mhz8.sample_rate_hz()
    }
}

impl std::fmt::Display for Bandwidth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A DVB-T transmission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mode {
    pub fft: FftSize,
    pub guard: Guard,
    pub modulation: Modulation,
    /// The high-priority (or only) stream's code rate.
    pub rate_hp: Rate,
    /// The low-priority stream's, when hierarchical.
    pub rate_lp: Rate,
    pub alpha: Alpha,
    pub bandwidth: Bandwidth,
}

impl Mode {
    /// The UK's usual: 8k, 1/32, 8 MHz, non-hierarchical.
    pub fn uk(modulation: Modulation, rate: Rate) -> Mode {
        Mode {
            fft: FftSize::K8,
            guard: Guard::G1_32,
            modulation,
            rate_hp: rate,
            rate_lp: rate,
            alpha: Alpha::NonHierarchical,
            bandwidth: Bandwidth::Mhz8,
        }
    }

    /// Read a mode as typed: "64qam-2/3", "16QAM-3/4-2k", "qpsk-1/2-8k-1/4".
    /// After the constellation and the rate, in any order: the FFT size
    /// ("2k", "8k"), the guard interval ("1/32", "1/16", "1/8", "1/4") and
    /// the bandwidth ("1mhz", "6mhz", "7mhz", "8mhz"; 1.7 MHz is DVB-T2's
    /// only and is refused). The rest is the UK's ([`Mode::uk`]); hierarchy is
    /// not spelled.
    pub fn parse(s: &str) -> Result<Mode, String> {
        let s = s.to_ascii_lowercase();
        let mut parts = s.split('-');
        let modulation = match parts.next() {
            Some("qpsk") => Modulation::Qpsk,
            Some("16qam") => Modulation::Qam16,
            Some("64qam") => Modulation::Qam64,
            other => return Err(format!("not a constellation: {}", other.unwrap_or(""))),
        };
        let rate_text = parts
            .next()
            .ok_or("no code rate (1/2, 2/3, 3/4, 5/6, 7/8)")?;
        // `find` takes the first rate whose printed name ("2/3") matches.
        let rate = Rate::ALL
            .into_iter()
            .find(|r| r.name() == rate_text)
            .ok_or(format!("not a code rate: {rate_text}"))?;
        let mut mode = Mode::uk(modulation, rate);
        for p in parts {
            match p {
                "2k" => mode.fft = FftSize::K2,
                "8k" => mode.fft = FftSize::K8,
                b if b.ends_with("mhz") => {
                    mode.bandwidth = Bandwidth::parse(b)
                        .filter(|b| b.dvbt())
                        .ok_or(format!("not a DVB-T bandwidth: {b}"))?
                }
                g => {
                    mode.guard = Guard::ALL
                        .into_iter()
                        .find(|x| x.name() == g)
                        .ok_or(format!("not an FFT size, guard or bandwidth: {g}"))?
                }
            }
        }
        Ok(mode)
    }

    pub fn hierarchical(&self) -> bool {
        self.alpha != Alpha::NonHierarchical
    }

    /// The useful symbol Tu = N·T, seconds (224 or 896 µs at 8 MHz).
    pub fn useful_s(&self) -> f64 {
        self.fft.n() as f64 * self.bandwidth.t_s()
    }

    /// The whole symbol Ts = Tu + Δ, seconds.
    pub fn symbol_s(&self) -> f64 {
        self.useful_s() * (1.0 + 1.0 / self.guard.divisor() as f64)
    }

    /// The guard interval's length, seconds.
    pub fn guard_s(&self) -> f64 {
        self.useful_s() / self.guard.divisor() as f64
    }

    /// The HP (or only) stream's useful bitrate, bit/s: the data carriers'
    /// bits at the code rate, less the RS(204,188) overhead, a symbol at a
    /// time (EN 300 744 Table 17: 24.13 Mbit/s for 8k 64-QAM 2/3 1/32).
    /// Hierarchical: the HP stream rides the two bits of the QPSK.
    pub fn bitrate_hp(&self) -> f64 {
        let bits = if self.hierarchical() {
            2
        } else {
            self.modulation.bits()
        };
        self.stream_bitrate(bits, self.rate_hp)
    }

    /// The LP stream's, when hierarchical (the bits the QPSK leaves).
    pub fn bitrate_lp(&self) -> Option<f64> {
        self.hierarchical()
            .then(|| self.stream_bitrate(self.modulation.bits() - 2, self.rate_lp))
    }

    fn stream_bitrate(&self, bits: usize, rate: Rate) -> f64 {
        let (num, den) = rate.fraction();
        let per_symbol = self.fft.data_carriers() as f64 * bits as f64 * num as f64 / den as f64;
        per_symbol * 188.0 / 204.0 / self.symbol_s()
    }

    /// The C/N the HP stream needs for QEF, dB. `None` when hierarchical
    /// (the thresholds differ with α; not tabulated here).
    pub fn qef_cn_db(&self) -> Option<f32> {
        (!self.hierarchical()).then(|| qef_cn_db(self.modulation, self.rate_hp))
    }
}

/// The C/N, dB, a Gaussian channel needs for quasi error free reception: a
/// BER of 2×10⁻⁴ after the Viterbi decoder, which the Reed–Solomon code
/// takes to under one uncorrected error an hour. EN 300 744 Annex A,
/// Table A.1 (non-hierarchical, ideal receiver).
///
/// Typed from the table as remembered: check against the spec text when the
/// receiver lands (M3).
pub fn qef_cn_db(m: Modulation, r: Rate) -> f32 {
    use Modulation::*;
    use Rate::*;
    match (m, r) {
        (Qpsk, R1_2) => 3.1,
        (Qpsk, R2_3) => 4.9,
        (Qpsk, R3_4) => 5.9,
        (Qpsk, R5_6) => 6.9,
        (Qpsk, R7_8) => 7.7,
        (Qam16, R1_2) => 8.8,
        (Qam16, R2_3) => 11.1,
        (Qam16, R3_4) => 12.5,
        (Qam16, R5_6) => 13.5,
        (Qam16, R7_8) => 13.9,
        (Qam64, R1_2) => 14.4,
        (Qam64, R2_3) => 16.5,
        (Qam64, R3_4) => 18.0,
        (Qam64, R5_6) => 19.3,
        (Qam64, R7_8) => 20.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uk_mode_gives_the_published_bitrate() {
        // EN 300 744 Table 17, 8 MHz, non-hierarchical. At 1/32: 24.13
        // Mbit/s for 64-QAM 2/3, 31.67 for 64-QAM 7/8, 6.03 for QPSK 1/2;
        // at 1/4, QPSK 1/2 is 4.98.
        let br = |m, r| Mode::uk(m, r).bitrate_hp() / 1e6;
        assert!((br(Modulation::Qam64, Rate::R2_3) - 24.13).abs() < 0.01);
        assert!((br(Modulation::Qam64, Rate::R7_8) - 31.67).abs() < 0.01);
        assert!((br(Modulation::Qpsk, Rate::R1_2) - 6.03).abs() < 0.01);
        let mut m = Mode::uk(Modulation::Qpsk, Rate::R1_2);
        m.guard = Guard::G1_4;
        assert!((m.bitrate_hp() / 1e6 - 4.98).abs() < 0.01);
    }

    #[test]
    fn modes_parse_as_typed() {
        let m = Mode::parse("64QAM-2/3").unwrap();
        assert_eq!(m, Mode::uk(Modulation::Qam64, Rate::R2_3));
        let m = Mode::parse("qpsk-1/2-2k-1/4-7mhz").unwrap();
        assert_eq!(
            (m.fft, m.guard, m.bandwidth),
            (FftSize::K2, Guard::G1_4, Bandwidth::Mhz7)
        );
        // The guard 1/8 and the rate 1/2 do not confuse: a rate comes second.
        assert_eq!(Mode::parse("16qam-1/2-1/8").unwrap().guard, Guard::G1_8);
        assert!(Mode::parse("256qam-2/3").is_err());
        assert!(Mode::parse("64qam-9/10").is_err());
        assert!(Mode::parse("64qam").is_err());
        assert!(Mode::parse("64qam-2/3-4k").is_err());
        assert_eq!(
            Mode::parse("qpsk-1/2-1mhz").unwrap().bandwidth,
            Bandwidth::Mhz1
        );
        // 1.7 MHz is DVB-T2's.
        assert!(Mode::parse("qpsk-1/2-1.7mhz").is_err());
    }

    #[test]
    fn the_narrow_widths_have_their_clocks() {
        // 1 MHz: the 8 MHz clock over 8; the bitrate likewise.
        assert!((Bandwidth::Mhz1.sample_rate_hz() - 8e6 / 7.0).abs() < 1e-6);
        assert!((Bandwidth::Mhz1.scale() - 0.125).abs() < 1e-12);
        let mut m = Mode::uk(Modulation::Qam64, Rate::R2_3);
        let r8 = m.bitrate_hp();
        m.bandwidth = Bandwidth::Mhz1;
        assert!((m.bitrate_hp() * 8.0 - r8).abs() < 1e-6);
        assert!((m.useful_s() * 1e6 - 7168.0).abs() < 1e-6);
        // 1.7 MHz: T = 71/131 µs (EN 302 755 Table 66).
        assert!((Bandwidth::Mhz1_7.sample_rate_hz() - 131e6 / 71.0).abs() < 1e-6);
        for b in Bandwidth::ALL {
            assert_eq!(Bandwidth::parse(b.token()), Some(b));
            assert_eq!(Bandwidth::parse(b.name()), Some(b));
        }
        assert_eq!(Bandwidth::parse("1.712"), Some(Bandwidth::Mhz1_7));
        assert!(Bandwidth::Mhz1.dvbt() && !Bandwidth::Mhz1.t2());
        assert!(!Bandwidth::Mhz1_7.dvbt() && Bandwidth::Mhz1_7.t2());
    }

    #[test]
    fn symbol_times_follow_the_bandwidth() {
        let m = Mode::uk(Modulation::Qam64, Rate::R2_3);
        assert!((m.useful_s() * 1e6 - 896.0).abs() < 1e-9);
        assert!((m.guard_s() * 1e6 - 28.0).abs() < 1e-9);
        assert!((m.symbol_s() * 1e6 - 924.0).abs() < 1e-9);
        assert!((Bandwidth::Mhz8.sample_rate_hz() - 64e6 / 7.0).abs() < 1.0);
    }

    #[test]
    fn hierarchy_splits_the_bits() {
        let mut m = Mode::uk(Modulation::Qam64, Rate::R1_2);
        m.alpha = Alpha::A2;
        m.rate_lp = Rate::R3_4;
        // HP: 2 bits of 6 at 1/2; LP: 4 bits of 6 at 3/4.
        let hp = m.bitrate_hp();
        let lp = m.bitrate_lp().unwrap();
        assert!((hp / lp - (2.0 * 0.5) / (4.0 * 0.75)).abs() < 1e-9);
        assert_eq!(m.qef_cn_db(), None);
    }

    #[test]
    fn thresholds_rise_with_rate_and_with_constellation() {
        for m in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
            let v: Vec<f32> = Rate::ALL.iter().map(|&r| qef_cn_db(m, r)).collect();
            assert!(v.windows(2).all(|w| w[0] < w[1]), "{m:?} {v:?}");
        }
        for r in Rate::ALL {
            assert!(qef_cn_db(Modulation::Qpsk, r) < qef_cn_db(Modulation::Qam16, r));
            assert!(qef_cn_db(Modulation::Qam16, r) < qef_cn_db(Modulation::Qam64, r));
        }
    }
}
