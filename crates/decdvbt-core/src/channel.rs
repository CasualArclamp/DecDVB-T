// made by CasualArclamp
//! The UHF television channel plan, for tuning by channel number.
//!
//! Region 1 UHF raster (ITU GE06 plan): 8 MHz channels, channel N centred on
//! 306 + 8·N MHz, so C21 is 474 MHz and C69 is 858 MHz.
//!
//! The UK has used channels 21–48 since the 700 MHz clearance (2020), less
//! C38 (606–614 MHz), which is kept for radio astronomy and PMSE.

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

/// Read a channel as typed: "C34", "c34", "ch34" or "34".
pub fn parse_channel(s: &str) -> Option<u8> {
    let s = s.trim();
    let digits = s
        .strip_prefix("ch")
        .or_else(|| s.strip_prefix("CH"))
        .or_else(|| s.strip_prefix("Ch"))
        .or_else(|| s.strip_prefix('C'))
        .or_else(|| s.strip_prefix('c'))
        .unwrap_or(s);
    let n: u8 = digits.parse().ok()?;
    uhf_centre_hz(n).map(|_| n)
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
    fn the_uk_list_skips_38() {
        let v: Vec<u8> = uk_channels().collect();
        assert_eq!(v.len(), 27);
        assert!(!v.contains(&38));
        assert_eq!((v[0], v[v.len() - 1]), (21, 48));
    }
}
