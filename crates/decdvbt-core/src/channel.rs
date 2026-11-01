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

/// The centre of channel `n` (VHF E5–E12 or UHF 21–69), Hz.
pub fn centre_hz(n: u8) -> Option<f64> {
    vhf_centre_hz(n).or_else(|| uhf_centre_hz(n))
}

/// Channel `n`'s width, Hz: 7 MHz in VHF, 8 in UHF.
pub fn bandwidth_hz(n: u8) -> Option<f64> {
    if vhf_centre_hz(n).is_some() {
        Some(7e6)
    } else {
        uhf_centre_hz(n).map(|_| CHANNEL_BW_HZ)
    }
}

/// The channel (VHF or UHF) whose band holds `hz`, if any.
pub fn channel_at(hz: f64) -> Option<u8> {
    uhf_channel_at(hz).or_else(|| {
        let n = ((hz / 1e6 - 142.5) / 7.0).round();
        (VHF_FIRST as f64..=VHF_LAST as f64)
            .contains(&n)
            .then_some(n as u8)
            .filter(|&n| (hz - vhf_centre_hz(n).unwrap()).abs() <= 3.5e6)
    })
}

/// Every channel of the plan, lowest first: E5–E12, then 21–69.
pub fn all_channels() -> impl Iterator<Item = u8> {
    (VHF_FIRST..=VHF_LAST).chain(UHF_FIRST..=UHF_LAST)
}

/// How a channel is written: "E5" in VHF, "C21" in UHF.
pub fn channel_label(n: u8) -> String {
    if vhf_centre_hz(n).is_some() {
        format!("E{n}")
    } else {
        format!("C{n}")
    }
}

/// Read a channel as typed: "C34", "c34", "ch34", "34", or a VHF one "E5",
/// "e5", "5".
pub fn parse_channel(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Some(d) = s.strip_prefix('E').or_else(|| s.strip_prefix('e')) {
        let n: u8 = d.parse().ok()?;
        return vhf_centre_hz(n).map(|_| n);
    }
    let digits = s
        .strip_prefix("ch")
        .or_else(|| s.strip_prefix("CH"))
        .or_else(|| s.strip_prefix("Ch"))
        .or_else(|| s.strip_prefix('C'))
        .or_else(|| s.strip_prefix('c'))
        .unwrap_or(s);
    let n: u8 = digits.parse().ok()?;
    centre_hz(n).map(|_| n)
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
}
