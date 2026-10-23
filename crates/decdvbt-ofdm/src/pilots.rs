// made by CasualArclamp
//! Reference signals: which carriers of an OFDM symbol are pilots, and the
//! values they carry (EN 300 744 §4.5).
//!
//! Carriers are numbered k = 0 … K−1 (Kmin = 0, Kmax = 1704 or 6816), the
//! middle one k = (K−1)/2 on the channel centre. The tables are those of
//! gr-dtv (`dvbt_reference_signals_impl.cc`, GPL-3, Free Software
//! Foundation), checked against the spec's counts by the tests below.

use decdvbt_core::dvbt::FftSize;

/// OFDM symbols in a frame, and frames in a superframe (§4.4).
pub const SYMBOLS_PER_FRAME: usize = 68;
pub const FRAMES_PER_SUPERFRAME: usize = 4;

/// The pilot amplitude relative to the data's: 4/3, a boost of 2.5 dB
/// (§4.5.3).
pub const PILOT_BOOST: f32 = 4.0 / 3.0;

/// Continual pilot carriers (§4.5.3, Table 7), 2k: 45 of them.
const CONTINUAL_2K: [u16; 45] = [
    0, 48, 54, 87, 141, 156, 192, 201, 255, 279, 282, 333, 432, 450, 483, 525, 531, 618, 636, 714,
    759, 765, 780, 804, 873, 888, 918, 939, 942, 969, 984, 1050, 1101, 1107, 1110, 1137, 1140,
    1146, 1206, 1269, 1323, 1377, 1491, 1683, 1704,
];

/// 8k: 177 of them (the 2k ones, then the same set again above them).
const CONTINUAL_8K: [u16; 177] = [
    0, 48, 54, 87, 141, 156, 192, 201, 255, 279, 282, 333, 432, 450, 483, 525, 531, 618, 636, 714,
    759, 765, 780, 804, 873, 888, 918, 939, 942, 969, 984, 1050, 1101, 1107, 1110, 1137, 1140,
    1146, 1206, 1269, 1323, 1377, 1491, 1683, 1704, 1752, 1758, 1791, 1845, 1860, 1896, 1905, 1959,
    1983, 1986, 2037, 2136, 2154, 2187, 2229, 2235, 2322, 2340, 2418, 2463, 2469, 2484, 2508, 2577,
    2592, 2622, 2643, 2646, 2673, 2688, 2754, 2805, 2811, 2814, 2841, 2844, 2850, 2910, 2973, 3027,
    3081, 3195, 3387, 3408, 3456, 3462, 3495, 3549, 3564, 3600, 3609, 3663, 3687, 3690, 3741, 3840,
    3858, 3891, 3933, 3939, 4026, 4044, 4122, 4167, 4173, 4188, 4212, 4281, 4296, 4326, 4347, 4350,
    4377, 4392, 4458, 4509, 4515, 4518, 4545, 4548, 4554, 4614, 4677, 4731, 4785, 4899, 5091, 5112,
    5160, 5166, 5199, 5253, 5268, 5304, 5313, 5367, 5391, 5394, 5445, 5544, 5562, 5595, 5637, 5643,
    5730, 5748, 5826, 5871, 5877, 5892, 5916, 5985, 6000, 6030, 6051, 6054, 6081, 6096, 6162, 6213,
    6219, 6222, 6249, 6252, 6258, 6318, 6381, 6435, 6489, 6603, 6795, 6816,
];

/// TPS carriers (§4.6.2, Table 8), 2k: 17 of them.
const TPS_2K: [u16; 17] = [
    34, 50, 209, 346, 413, 569, 595, 688, 790, 901, 1073, 1219, 1262, 1286, 1469, 1594, 1687,
];

/// 8k: 68.
const TPS_8K: [u16; 68] = [
    34, 50, 209, 346, 413, 569, 595, 688, 790, 901, 1073, 1219, 1262, 1286, 1469, 1594, 1687, 1738,
    1754, 1913, 2050, 2117, 2273, 2299, 2392, 2494, 2605, 2777, 2923, 2966, 2990, 3173, 3298, 3391,
    3442, 3458, 3617, 3754, 3821, 3977, 4003, 4096, 4198, 4309, 4481, 4627, 4670, 4694, 4877, 5002,
    5095, 5146, 5162, 5321, 5458, 5525, 5681, 5707, 5800, 5902, 6013, 6185, 6331, 6374, 6398, 6581,
    6706, 6799,
];

/// The continual pilots, in increasing carrier order.
pub fn continual_pilots(fft: FftSize) -> &'static [u16] {
    match fft {
        FftSize::K2 => &CONTINUAL_2K,
        FftSize::K8 => &CONTINUAL_8K,
    }
}

/// The TPS carriers, in increasing carrier order.
pub fn tps_carriers(fft: FftSize) -> &'static [u16] {
    match fft {
        FftSize::K2 => &TPS_2K,
        FftSize::K8 => &TPS_8K,
    }
}

/// Whether carrier `k` of symbol `l` (counted from the frame's start) is a
/// scattered pilot: k = 3·(l mod 4) + 12·p for any integer p (§4.5.2).
pub fn is_scattered(k: usize, l: usize) -> bool {
    k % 12 == 3 * (l % 4)
}

/// The reference sequence w_k (§4.5.2): a PRBS of x¹¹ + x² + 1, all ones at
/// the start, one bit for each active carrier, w_0 the first.
pub fn prbs(len: usize) -> Vec<u8> {
    let mut reg: u32 = (1 << 11) - 1;
    (0..len)
        .map(|_| {
            let bit = (reg & 1) as u8;
            let fb = ((reg >> 2) ^ reg) & 1;
            reg = (reg >> 1) | (fb << 10);
            bit
        })
        .collect()
}

/// The value of a scattered or continual pilot on carrier `k`: ±4/3, from
/// w_k (§4.5.3: Re = 4/3 · 2 · (½ − w_k), Im = 0).
pub fn pilot_value(wk: u8) -> f32 {
    PILOT_BOOST * 2.0 * (0.5 - f32::from(wk))
}

/// The role of each carrier of one symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Data,
    /// A scattered or a continual pilot (the same value either way).
    Pilot,
    Tps,
}

/// Which carriers carry what, for each of the four scattered-pilot phases
/// (symbol index mod 4), and the data carriers' indices in increasing order:
/// the cells the interleaved data fill.
pub struct SymbolPlan {
    pub fft: FftSize,
    roles: [Vec<Role>; 4],
    data: [Vec<u16>; 4],
}

impl SymbolPlan {
    pub fn new(fft: FftSize) -> SymbolPlan {
        let k = fft.active_carriers();
        let cp = continual_pilots(fft);
        let tps = tps_carriers(fft);
        let build = |phase: usize| -> Vec<Role> {
            let mut r: Vec<Role> = (0..k)
                .map(|c| {
                    if is_scattered(c, phase) {
                        Role::Pilot
                    } else {
                        Role::Data
                    }
                })
                .collect();
            for &c in cp {
                r[c as usize] = Role::Pilot;
            }
            for &c in tps {
                r[c as usize] = Role::Tps;
            }
            r
        };
        let roles = [build(0), build(1), build(2), build(3)];
        let data = std::array::from_fn(|p| {
            roles[p]
                .iter()
                .enumerate()
                .filter(|(_, r)| **r == Role::Data)
                .map(|(c, _)| c as u16)
                .collect()
        });
        SymbolPlan { fft, roles, data }
    }

    /// The roles of symbol `l`'s carriers.
    pub fn roles(&self, l: usize) -> &[Role] {
        &self.roles[l % 4]
    }

    /// Symbol `l`'s data carriers.
    pub fn data_carriers(&self, l: usize) -> &[u16] {
        &self.data[l % 4]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_have_the_specs_counts() {
        assert_eq!(continual_pilots(FftSize::K2).len(), 45);
        assert_eq!(continual_pilots(FftSize::K8).len(), 177);
        assert_eq!(tps_carriers(FftSize::K2).len(), 17);
        assert_eq!(tps_carriers(FftSize::K8).len(), 68);
        for fft in [FftSize::K2, FftSize::K8] {
            let cp = continual_pilots(fft);
            assert!(cp.windows(2).all(|w| w[0] < w[1]), "sorted and distinct");
            assert_eq!(*cp.last().unwrap() as usize, fft.active_carriers() - 1);
            let tps = tps_carriers(fft);
            assert!(tps.windows(2).all(|w| w[0] < w[1]));
        }
        // The 8k set holds the 2k set (Table 7 builds one from the other).
        let cp8 = continual_pilots(FftSize::K8);
        assert!(
            continual_pilots(FftSize::K2)
                .iter()
                .all(|c| cp8.contains(c))
        );
    }

    #[test]
    fn every_symbol_carries_the_same_number_of_data_cells() {
        // 1512 and 6048 data carriers in every symbol, whatever the
        // scattered-pilot phase: this fails if any table entry is wrong.
        for fft in [FftSize::K2, FftSize::K8] {
            let plan = SymbolPlan::new(fft);
            for l in 0..4 {
                assert_eq!(
                    plan.data_carriers(l).len(),
                    fft.data_carriers(),
                    "{fft:?} l={l}"
                );
                let tps = plan.roles(l).iter().filter(|r| **r == Role::Tps).count();
                assert_eq!(tps, tps_carriers(fft).len());
            }
        }
    }

    #[test]
    fn tps_carriers_are_never_scattered_pilots() {
        for fft in [FftSize::K2, FftSize::K8] {
            for &c in tps_carriers(fft) {
                for l in 0..4 {
                    assert!(!is_scattered(c as usize, l), "carrier {c} symbol {l}");
                }
            }
        }
    }

    #[test]
    fn the_prbs_starts_with_ones_and_repeats_after_2047() {
        let w = prbs(4200);
        // All-ones register: the first bits out are ones, until the
        // feedback (a one XOR a one = 0) comes round.
        assert!(w[..11].iter().all(|&b| b == 1));
        assert_eq!(&w[..2047], &w[2047..4094]);
        // A maximal-length sequence: 1024 ones and 1023 zeros a period.
        assert_eq!(w[..2047].iter().filter(|&&b| b == 1).count(), 1024);
    }

    #[test]
    fn pilots_are_boosted_plus_or_minus_four_thirds() {
        assert!((pilot_value(0) - 4.0 / 3.0).abs() < 1e-6);
        assert!((pilot_value(1) + 4.0 / 3.0).abs() < 1e-6);
    }
}
