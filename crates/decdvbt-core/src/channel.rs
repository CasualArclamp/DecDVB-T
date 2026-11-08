// made by CasualArclamp
//! The European television channel plan, for tuning by channel number.
//!
//! Region 1 UHF raster (ITU GE06 plan): 8 MHz channels, channel N centred on
//! 306 + 8·N MHz, so C21 is 474 MHz and C69 is 858 MHz.
//!
//! Region 1 VHF Band III raster (GE06): 7 MHz channels E5–E12, channel N
//! centred on 142.5 + 7·N MHz, so E5 is 177.5 MHz and E12 is 226.5 MHz.
//! Their numbers (5–12) do not meet the UHF ones (21–69), so one `u8` names
//! either; [`channel_label`] writes them "E5" and "C21".
//!
//! The UK has used channels 21–48 since the 700 MHz clearance (2020), less
//! C38 (606–614 MHz), which is kept for radio astronomy and PMSE. The rest of
//! Europe uses the VHF channels and, in places, UHF up to 69.
//!
//! The transmitter keeps to the UHF functions (`uhf_*`).

/// The first and last UHF channel numbers of the Region 1 raster.
pub const UHF_FIRST: u8 = 21;
pub const UHF_LAST: u8 = 69;

/// A channel's bandwidth, Hz.
pub const CHANNEL_BW_HZ: f64 = 8e6;

/// The centre frequency of UHF channel `n`, Hz, or `None` outside 21–69.
pub fn uhf_centre_hz(n: u8) -> Option<f64> {
    // `then_some` turns a bool into an Option: Some(value) when true.
    (UHF_FIRST..=UHF_LAST)
        .contains(&n)
        .then_some((306.0 + 8.0 * n as f64) * 1e6)
}

/// The UHF channel whose 8 MHz band holds `hz`, if any.
pub fn uhf_channel_at(hz: f64) -> Option<u8> {
    let n = ((hz / 1e6 - 306.0) / 8.0).round();
    (UHF_FIRST as f64..=UHF_LAST as f64)
        .contains(&n)
        .then_some(n as u8)
        .filter(|&n| (hz - uhf_centre_hz(n).unwrap()).abs() <= CHANNEL_BW_HZ / 2.0)
}

/// The channels UK transmitters use: 21–48 less 38.
pub fn uk_channels() -> impl Iterator<Item = u8> {
    // `impl Iterator` in return position: the caller gets a lazy iterator
    // without the concrete (and unwieldy) adapter type being named here.
    (UHF_FIRST..=48).filter(|&n| n != 38)
}

/// The first and last VHF Band III channel numbers (E5–E12).
pub const VHF_FIRST: u8 = 5;
pub const VHF_LAST: u8 = 12;

/// The centre of VHF channel `n`, Hz, or `None` outside E5–E12.
pub fn vhf_centre_hz(n: u8) -> Option<f64> {
    (VHF_FIRST..=VHF_LAST)
        .contains(&n)
        .then_some((142.5 + 7.0 * n as f64) * 1e6)
}

// ---- the selectable country plans ------------------------------------------

use std::sync::atomic::{AtomicU8, Ordering};

/// The internal number of Australian channel 9A (205.5 MHz). Its name has a
/// letter, so a `u8` code stands in: 13 is free (no plan uses E13).
pub const AU_CH_9A: u8 = 13;

/// A country's DVB-T channel plan. Channel numbers are a `u8` in every plan,
/// and the same number can sit at a different frequency in another one (UK
/// C28 is 530 MHz, Australian C28 is 529.5 MHz), so the whole program works
/// in the plan chosen with [`set_plan`]; the `Plan` methods below take it
/// explicitly and are what the tests use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Continental Europe, ITU Region 1 / GE06: VHF E5–E12 (7 MHz), UHF
    /// C21–C69 (8 MHz).
    Eu,
    /// United Kingdom: UHF C21–C48 less C38, 8 MHz (post-700 MHz clearance).
    Uk,
    /// Australia (ACMA): VHF 6–12 and 9A (7 MHz), UHF 28–51 (7 MHz, since the
    /// 700 MHz clearance), per the Australian Radiofrequency Spectrum Plan.
    Au,
}

impl Plan {
    pub const ALL: [Plan; 3] = [Plan::Eu, Plan::Uk, Plan::Au];

    /// The short code used on command lines and in the prefs file.
    pub fn code(self) -> &'static str {
        match self {
            Plan::Eu => "eu",
            Plan::Uk => "uk",
            Plan::Au => "au",
        }
    }

    /// The name shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Plan::Eu => "Europe (EU)",
            Plan::Uk => "United Kingdom",
            Plan::Au => "Australia",
        }
    }

    /// Read a country as typed ("au", "UK", "australia", "europe", ...).
    pub fn parse(s: &str) -> Option<Plan> {
        match s.trim().to_ascii_lowercase().as_str() {
            "eu" | "europe" | "e" => Some(Plan::Eu),
            "uk" | "gb" | "united kingdom" | "britain" => Some(Plan::Uk),
            "au" | "aus" | "australia" => Some(Plan::Au),
            _ => None,
        }
    }

    /// Every channel of the plan, lowest frequency first.
    pub fn channels(self) -> Vec<u8> {
        match self {
            Plan::Eu => (VHF_FIRST..=VHF_LAST).chain(UHF_FIRST..=UHF_LAST).collect(),
            Plan::Uk => uk_channels().collect(),
            // 9A sits between 9 and 10, so the list is in frequency order,
            // not number order.
            Plan::Au => [6, 7, 8, 9, AU_CH_9A, 10, 11, 12]
                .into_iter()
                .chain(28..=51)
                .collect(),
        }
    }

    /// The centre of channel `n`, Hz, or `None` if the plan has no such channel.
    pub fn centre_hz(self, n: u8) -> Option<f64> {
        match self {
            Plan::Eu => vhf_centre_hz(n).or_else(|| uhf_centre_hz(n)),
            Plan::Uk => uk_channels()
                .any(|c| c == n)
                .then(|| uhf_centre_hz(n))
                .flatten(),
            Plan::Au => match n {
                // Channel 6 is 174–181 MHz; each next one is 7 MHz up.
                6..=9 => Some((135.5 + 7.0 * n as f64) * 1e6),
                // 9A (202–209) is slotted in, so 10–12 are 7 MHz higher still.
                10..=12 => Some((142.5 + 7.0 * n as f64) * 1e6),
                AU_CH_9A => Some(205.5e6),
                // Channel 28 is 526–533 MHz.
                28..=51 => Some((333.5 + 7.0 * n as f64) * 1e6),
                _ => None,
            },
        }
    }

    /// Channel `n`'s width, Hz.
    pub fn bandwidth_hz(self, n: u8) -> Option<f64> {
        self.centre_hz(n)?;
        Some(match self {
            Plan::Eu if vhf_centre_hz(n).is_some() => 7e6,
            Plan::Eu | Plan::Uk => CHANNEL_BW_HZ,
            Plan::Au => 7e6,
        })
    }

    /// The channel whose band holds `hz`, if any.
    pub fn channel_at(self, hz: f64) -> Option<u8> {
        self.channels().into_iter().find(|&n| {
            let c = self.centre_hz(n).unwrap();
            (hz - c).abs() <= self.bandwidth_hz(n).unwrap() / 2.0
        })
    }

    /// How a channel is written: "E5" / "C21" in Europe, "V6" / "V9A" / "C28"
    /// in Australia.
    pub fn label(self, n: u8) -> String {
        match self {
            Plan::Au if n == AU_CH_9A => "V9A".into(),
            Plan::Au if n <= 12 => format!("V{n}"),
            Plan::Eu if vhf_centre_hz(n).is_some() => format!("E{n}"),
            _ => format!("C{n}"),
        }
    }

    /// Read a channel as typed: "C34", "c34", "ch34", "34", or a VHF one
    /// ("E5", "V6", "9A") where the plan has them.
    pub fn parse_channel(self, s: &str) -> Option<u8> {
        let up = s.trim().to_ascii_uppercase();
        if self == Plan::Au && matches!(up.as_str(), "9A" | "V9A" | "CH9A") {
            return Some(AU_CH_9A);
        }
        let vhf_letter = if self == Plan::Au { 'V' } else { 'E' };
        if let Some(d) = up.strip_prefix(vhf_letter) {
            let n: u8 = d.parse().ok()?;
            return (n <= 12).then(|| self.centre_hz(n)).flatten().map(|_| n);
        }
        let digits = up
            .strip_prefix("CH")
            .or_else(|| up.strip_prefix('C'))
            .unwrap_or(&up);
        let n: u8 = digits.parse().ok()?;
        self.centre_hz(n).map(|_| n)
    }
}

/// The plan in force (an index into `Plan::ALL`). An atomic, so any thread
/// may read it without a lock.
static PLAN: AtomicU8 = AtomicU8::new(0);

/// The plan the program is using (Europe until [`set_plan`] says otherwise).
pub fn plan() -> Plan {
    Plan::ALL[PLAN.load(Ordering::Relaxed) as usize % Plan::ALL.len()]
}

/// Choose the country's plan for the whole program.
pub fn set_plan(p: Plan) {
    let i = Plan::ALL.iter().position(|&x| x == p).unwrap_or(0);
    PLAN.store(i as u8, Ordering::Relaxed);
}

/// The centre of channel `n` in the plan in force, Hz.
pub fn centre_hz(n: u8) -> Option<f64> {
    plan().centre_hz(n)
}

/// Channel `n`'s width, Hz (7 MHz in VHF and in Australia, 8 elsewhere).
pub fn bandwidth_hz(n: u8) -> Option<f64> {
    plan().bandwidth_hz(n)
}

/// The channel (of the plan in force) whose band holds `hz`, if any.
pub fn channel_at(hz: f64) -> Option<u8> {
    plan().channel_at(hz)
}

/// Every channel of the plan in force, lowest frequency first.
pub fn all_channels() -> std::vec::IntoIter<u8> {
    plan().channels().into_iter()
}

/// How a channel is written in the plan in force.
pub fn channel_label(n: u8) -> String {
    plan().label(n)
}

/// Read a channel as typed, in the plan in force.
pub fn parse_channel(s: &str) -> Option<u8> {
    plan().parse_channel(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_raster_matches_known_channels() {
        assert_eq!(uhf_centre_hz(21), Some(474e6));
        assert_eq!(uhf_centre_hz(22), Some(482e6));
        assert_eq!(uhf_centre_hz(48), Some(690e6));
        assert_eq!(uhf_centre_hz(69), Some(858e6));
        assert_eq!(uhf_centre_hz(20), None);
        assert_eq!(uhf_centre_hz(70), None);
    }

    #[test]
    fn a_frequency_maps_back_to_its_channel() {
        assert_eq!(uhf_channel_at(482e6), Some(22));
        assert_eq!(uhf_channel_at(485.9e6), Some(22));
        assert_eq!(uhf_channel_at(478.1e6), Some(22));
        assert_eq!(uhf_channel_at(400e6), None);
    }

    #[test]
    fn channels_parse_as_typed() {
        assert_eq!(parse_channel("C34"), Some(34));
        assert_eq!(parse_channel("c34"), Some(34));
        assert_eq!(parse_channel("ch34"), Some(34));
        assert_eq!(parse_channel(" 34 "), Some(34));
        assert_eq!(parse_channel("C99"), None);
        assert_eq!(parse_channel("abc"), None);
    }

    #[test]
    fn the_vhf_raster() {
        assert_eq!(centre_hz(5), Some(177.5e6));
        assert_eq!(centre_hz(12), Some(226.5e6));
        assert_eq!(centre_hz(13), None);
        assert_eq!(bandwidth_hz(7), Some(7e6));
        assert_eq!(bandwidth_hz(21), Some(8e6));
        assert_eq!(channel_at(191.5e6), Some(7));
        assert_eq!(channel_at(194.9e6), Some(7));
        assert_eq!(channel_at(474e6), Some(21));
        assert_eq!(channel_at(300e6), None);
        assert_eq!(uhf_channel_at(191.5e6), None);
        assert_eq!(parse_channel("E5"), Some(5));
        assert_eq!(parse_channel("e12"), Some(12));
        assert_eq!(parse_channel("E21"), None);
        assert_eq!(parse_channel("8"), Some(8));
        assert_eq!(channel_label(5), "E5");
        assert_eq!(channel_label(69), "C69");
        let all: Vec<u8> = all_channels().collect();
        assert_eq!(all.len(), 8 + 49);
        assert_eq!((all[0], all[7], all[8], all[56]), (5, 12, 21, 69));
    }

    #[test]
    fn the_uk_list_skips_38() {
        let v: Vec<u8> = uk_channels().collect();
        assert_eq!(v.len(), 27);
        assert!(!v.contains(&38));
        assert_eq!((v[0], v[v.len() - 1]), (21, 48));
    }

    #[test]
    fn the_au_plan() {
        let au = Plan::Au;
        assert_eq!(au.centre_hz(6), Some(177.5e6));
        assert_eq!(au.centre_hz(9), Some(198.5e6));
        assert_eq!(au.centre_hz(AU_CH_9A), Some(205.5e6));
        assert_eq!(au.centre_hz(10), Some(212.5e6));
        assert_eq!(au.centre_hz(12), Some(226.5e6));
        assert_eq!(au.centre_hz(28), Some(529.5e6));
        assert_eq!(au.centre_hz(51), Some(690.5e6));
        assert_eq!(au.centre_hz(52), None);
        assert_eq!(au.centre_hz(5), None);
        assert_eq!(au.bandwidth_hz(35), Some(7e6));
        assert_eq!(au.channel_at(205.5e6), Some(AU_CH_9A));
        assert_eq!(au.channel_at(212.4e6), Some(10));
        assert_eq!(au.parse_channel("9a"), Some(AU_CH_9A));
        assert_eq!(au.parse_channel("V6"), Some(6));
        assert_eq!(au.parse_channel("C28"), Some(28));
        assert_eq!(au.parse_channel("E6"), None);
        assert_eq!(au.label(AU_CH_9A), "V9A");
        let all = au.channels();
        assert_eq!(all.len(), 8 + 24);
        let hz: Vec<f64> = all.iter().map(|&n| au.centre_hz(n).unwrap()).collect();
        assert!(hz.windows(2).all(|w| w[0] < w[1]), "frequency order");
    }

    #[test]
    fn the_uk_and_eu_plans() {
        assert_eq!(Plan::Uk.centre_hz(38), None);
        assert_eq!(Plan::Uk.centre_hz(49), None);
        assert_eq!(Plan::Uk.centre_hz(30), Some(546e6));
        assert_eq!(Plan::Uk.channels().len(), 27);
        assert_eq!(Plan::Uk.channel_at(191.5e6), None);
        assert_eq!(Plan::Eu.centre_hz(38), Some(610e6));
        assert_eq!(Plan::parse("Australia"), Some(Plan::Au));
        assert_eq!(Plan::parse("gb"), Some(Plan::Uk));
        assert_eq!(Plan::parse("xx"), None);
    }
}
