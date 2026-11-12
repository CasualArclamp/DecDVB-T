// made by CasualArclamp
//! The RTL-SDR's two chips as plain logic: the Realtek RTL2832U's USB register
//! protocol, and the Rafael Micro R820T / R820T2 / R828D tuner behind its I2C
//! repeater. Everything talks to the dongle through the [`RtlBus`] trait (two
//! kinds of vendor control transfer and a sleep), so the whole bring-up, the
//! tuner's PLL, filters and gain are unit-tested here against a mock dongle;
//! `rtlsdr.rs` (feature `rtlsdr`) puts the trait on a real USB device. From
//! DecLAB's `declab-io/src/rtlsdr_proto.rs` (DecDAB's before it).
//!
//! The register sequences, tables and formulas are librtlsdr's (osmocom
//! rtl-sdr: `librtlsdr.c`, Copyright (C) 2012-2014 Steve Markgraf, Dimitri
//! Stolnikov and others; `tuner_r82xx.c`, Copyright (C) 2013 Mauro Carvalho
//! Chehab, 2013 Steve Markgraf; both GPL-2.0-or-later), and for the RTL-SDR
//! Blog V4's input switching the RTL-SDR Blog fork of it, re-implemented here
//! rather than linked. The chips' datasheets are not public; where a value has
//! no explanation it has none upstream either.
//!
//! Unlike the Airspy's protocol (`airspy_proto`), which is fire-and-forget
//! commands, the R82xx needs read-modify-write on a shadow of its registers
//! and must wait for its PLL to report lock, so this is a driver over a bus
//! rather than a list of commands.
//!
//! **Antenna-port power (the bias tee) is off unless asked.** RTL-SDR Blog
//! dongles drive it from GPIO 0; [`Rtl2832::open`] drives that pin low, only
//! [`Rtl2832::set_bias_tee`] (from [`RtlSettings::bias_tee`], false by
//! default) drives it high, and [`Rtl2832::close`] drives it low again. Tests
//! check each of those.

use std::time::Duration;

use decdvbt_core::{Error, Iq, Result};

use crate::raw_stream::RawToIq;

/// USB ids of RTL2832U dongles (librtlsdr's `known_devices`, the common
/// ones). Most dongles sold today, the RTL-SDR Blog V3 and V4 among them, are
/// the generic 0BDA:2838.
pub const DEVICES: &[(u16, u16)] = &[
    (0x0bda, 0x2838), // generic RTL2832U OEM (RTL-SDR Blog V3 / V4, NooElec, …)
    (0x0bda, 0x2832), // generic RTL2832U
    (0x0413, 0x6680), // DigitalNow Quad DVB-T PCI-E
    (0x0413, 0x6f0f), // Leadtek WinFast DTV Dongle mini D
    (0x0458, 0x707f), // Genius TVGo DVB-T03
    (0x0ccd, 0x00a9), // Terratec Cinergy T Stick Black (rev 1)
    (0x0ccd, 0x00b3), // Terratec NOXON DAB/DAB+ (rev 1)
    (0x0ccd, 0x00d3), // Terratec Cinergy T Stick RC (rev 3)
    (0x0ccd, 0x00e0), // Terratec NOXON DAB/DAB+ (rev 2)
    (0x185b, 0x0620), // Compro Videomate U620F
    (0x185b, 0x0650), // Compro Videomate U650F
    (0x1b80, 0xd393), // GIGABYTE GT-U7300
    (0x1b80, 0xd394), // DIKOM USB-DVBT HD
    (0x1b80, 0xd395), // Peak 102569AGPK
    (0x1b80, 0xd39d), // SVEON STV20
    (0x1b80, 0xd3a4), // Twintech UT-40
    (0x1d19, 0x1101), // Dexatek DK DVB-T (Logilink VG0002A)
    (0x1d19, 0x1102), // Dexatek DK DVB-T (MSI DigiVox mini II V3.0)
    (0x1d19, 0x1103), // Dexatek DK 5217 DVB-T
    (0x1f4d, 0xb803), // GTek T803
    (0x1f4d, 0xc803), // Lifeview LV5TDeluxe
];

/// The bulk IN endpoint the samples arrive on.
pub const ENDPOINT_IN: u8 = 0x81;

/// The RTL2832U's crystal, and the R828D's own on most R828D dongles (the
/// R820T, and the RTL-SDR Blog V4's R828D, run from the RTL2832U's 28.8 MHz).
pub const RTL_XTAL_HZ: u32 = 28_800_000;
pub const R828D_XTAL_HZ: u32 = 16_000_000;

/// The one rate DecDVB-T runs the dongle at (DecDAB's: DAB's own; it holds a
/// 1 MHz DVB-T or a 1.7 MHz DVB-T2 channel, not the wider ones). The RTL2832U's
/// resampler makes it exactly from 28.8 MHz (ratio 0x0384_0000), and it is
/// well inside what the USB side sustains (3.2 MS/s at most).
pub const SAMPLE_RATE_HZ: u32 = 2_048_000;

/// The tuner's range: the PLL needs an LO (the tuned frequency plus the IF)
/// of at least 1.77 GHz / 64; 1.766 GHz is librtlsdr's top for the R820T.
pub const FREQ_MIN_HZ: u64 = 28_000_000;
pub const FREQ_MAX_HZ: u64 = 1_766_000_000;

/// Bytes per bulk transfer: 32 768 samples, 16 ms at 2.048 MS/s. A multiple
/// of the 512-byte packet.
pub const TRANSFER_BYTES: usize = 1 << 16;

/// The RTL2832U's register blocks (the `index` high byte of a request).
mod block {
    pub const USB: u16 = 1;
    pub const SYS: u16 = 2;
    pub const IIC: u16 = 6;
}

/// Registers in the USB and SYS blocks.
mod reg {
    pub const USB_SYSCTL: u16 = 0x2000;
    pub const USB_EPA_CTL: u16 = 0x2148;
    pub const USB_EPA_MAXPKT: u16 = 0x2158;
    pub const DEMOD_CTL: u16 = 0x3000;
    pub const GPO: u16 = 0x3001;
    pub const GPOE: u16 = 0x3003;
    pub const GPD: u16 = 0x3004;
    pub const DEMOD_CTL_1: u16 = 0x300b;
}

/// The bias-tee pin on RTL-SDR Blog dongles.
const BIAS_TEE_GPIO: u8 = 0;

/// The R82xx's I2C addresses (8-bit form, as the RTL2832U takes them), and
/// what its register 0 reads as.
const R820T_I2C_ADDR: u8 = 0x34;
const R828D_I2C_ADDR: u8 = 0x74;
const R82XX_CHECK_VAL: u8 = 0x69;

/// The R82xx's registers 0x05..=0x1f as librtlsdr powers it up
/// (`r82xx_init_array`).
const R82XX_INIT: [u8; 27] = [
    0x83, 0x32, 0x75, // 05..07
    0xc0, 0x40, 0xd6, 0x6c, // 08..0b
    0xf5, 0x63, 0x75, 0x68, // 0c..0f
    0x6c, 0x83, 0x80, 0x00, // 10..13
    0x0f, 0x00, 0xc0, 0x30, // 14..17
    0x48, 0xcc, 0x60, 0x00, // 18..1b
    0x54, 0xae, 0x4a, 0xc0, // 1c..1f
];
/// The first register the shadow holds; 0..=4 are read-only status.
const SHADOW_START: u8 = 0x05;
/// The RTL2832U's I2C bridge takes at most 8 bytes, the register address one
/// of them.
const I2C_MAX_DATA: usize = 7;

/// `r82xx_freq_ranges`, by LO frequency: from `mhz` up, the RF input's open
/// drain (reg 0x17 bit 3), the RF mux / polyphase filter (reg 0x1a) and the
/// tracking filter's band (reg 0x1b). The crystal-capacitor columns are
/// left out: librtlsdr runs the crystal at "high cap 0 pF", which is 0
/// throughout.
const FREQ_RANGES: [(u32, u8, u8, u8); 21] = [
    (0, 0x08, 0x02, 0xdf),
    (50, 0x08, 0x02, 0xbe),
    (55, 0x08, 0x02, 0x8b),
    (60, 0x08, 0x02, 0x7b),
    (65, 0x08, 0x02, 0x69),
    (70, 0x08, 0x02, 0x58),
    (75, 0x00, 0x02, 0x44),
    (80, 0x00, 0x02, 0x44),
    (90, 0x00, 0x02, 0x34),
    (100, 0x00, 0x02, 0x34),
    (110, 0x00, 0x02, 0x24),
    (120, 0x00, 0x02, 0x24),
    (140, 0x00, 0x02, 0x14),
    (180, 0x00, 0x02, 0x13),
    (220, 0x00, 0x02, 0x13),
    (250, 0x00, 0x02, 0x11),
    (280, 0x00, 0x02, 0x00),
    (310, 0x00, 0x41, 0x00),
    (450, 0x00, 0x41, 0x00),
    (588, 0x00, 0x40, 0x00),
    (650, 0x00, 0x40, 0x00),
];

/// The LNA's and mixer's gain for each step of their index, tenths of a dB
/// (`r82xx_lna_gain_steps`, `r82xx_mixer_gain_steps`).
const LNA_STEPS: [i32; 16] = [0, 9, 13, 40, 38, 13, 31, 22, 26, 31, 26, 14, 19, 5, 35, 13];
const MIXER_STEPS: [i32; 16] = [0, 5, 10, 10, 19, 9, 10, 25, 17, 10, 8, 16, 13, 6, 3, -8];

/// The gains librtlsdr offers for the R82xx, tenths of a dB (`r82xx_gains`):
/// each is where the LNA/mixer interleave of [`gain_indices`] lands.
pub const GAINS_DB10: [u16; 29] = [
    0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364, 372,
    386, 402, 421, 434, 439, 445, 480, 496,
];

/// The IF filter's low-pass corners, Hz (`r82xx_if_low_pass_bw_table`), and
/// the two high-pass steps.
const IF_LOW_PASS_HZ: [u32; 10] = [
    1_700_000, 1_600_000, 1_550_000, 1_450_000, 1_200_000, 900_000, 700_000, 550_000, 450_000,
    350_000,
];
const FILT_HP_BW1: u32 = 350_000;
const FILT_HP_BW2: u32 = 380_000;

/// The RTL2832U's decimation filter (`fir_default`): eight 8-bit taps then
/// eight 12-bit ones, the first half of a symmetric 32-tap low-pass.
const FIR_DEFAULT: [i32; 16] = [
    -54, -36, -41, -40, -32, -14, 14, 53, 101, 156, 215, 273, 327, 372, 404, 421,
];

/// The dongle as two kinds of vendor control transfer (request 0, to the
/// device) and a pause. Rust note: a trait with the transport behind it is
/// how the driver below is run against real USB (`rtlsdr.rs`) and against a
/// mock in the tests with the same code.
pub trait RtlBus {
    fn control_out(&mut self, value: u16, index: u16, data: &[u8]) -> Result<()>;
    fn control_in(&mut self, value: u16, index: u16, len: u16) -> Result<Vec<u8>>;
    fn sleep(&mut self, d: Duration);
}

/// Which R82xx it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tuner {
    R820T,
    R828D,
}

impl Tuner {
    pub fn name(self) -> &'static str {
        match self {
            Tuner::R820T => "R820T",
            Tuner::R828D => "R828D",
        }
    }
}

/// The tuner's gain: its own AGC, or a fixed LNA + mixer gain (the VGA then
/// sits at a fixed 16.3 dB, as in librtlsdr).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtlGains {
    pub agc: bool,
    /// Tenths of a dB, 0–496; snapped up to the next step of [`GAINS_DB10`].
    pub db10: u16,
}

impl Default for RtlGains {
    fn default() -> Self {
        // 40.2 dB leaves a little headroom below the top (DecDAB's choice for
        // weak Band III; VHF pagers are usually strong, so try lower first).
        RtlGains {
            agc: false,
            db10: 402,
        }
    }
}

impl RtlGains {
    /// The step of [`GAINS_DB10`] nearest `db10`.
    pub fn nearest_step(db10: u16) -> u16 {
        *GAINS_DB10
            .iter()
            .min_by_key(|&&g| g.abs_diff(db10))
            .expect("the table is not empty")
    }

    /// Parse "40.2" (dB) or "agc", for the command line.
    pub fn parse(s: &str) -> std::result::Result<RtlGains, String> {
        if s.trim().eq_ignore_ascii_case("agc") {
            return Ok(RtlGains {
                agc: true,
                ..RtlGains::default()
            });
        }
        let db: f32 = s
            .trim()
            .trim_end_matches("dB")
            .trim()
            .parse()
            .map_err(|_| format!("not a gain in dB or \"agc\": {s}"))?;
        if !(0.0..=49.6).contains(&db) {
            return Err(format!("{db} dB is outside the tuner's 0–49.6 dB"));
        }
        Ok(RtlGains {
            agc: false,
            db10: Self::nearest_step((db * 10.0).round() as u16),
        })
    }
}

/// What a live session is set to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RtlSettings {
    pub center_hz: u64,
    /// Only [`SAMPLE_RATE_HZ`] is offered.
    pub sample_rate: u32,
    pub gains: RtlGains,
    /// The dongle's clock error, ppm (positive: it runs high).
    pub ppm: f64,
    /// Power the antenna port (RTL-SDR Blog dongles). Off by default.
    pub bias_tee: bool,
}

impl Default for RtlSettings {
    fn default() -> Self {
        RtlSettings {
            center_hz: 153_000_000,
            sample_rate: SAMPLE_RATE_HZ,
            gains: RtlGains::default(),
            ppm: 0.0,
            bias_tee: false,
        }
    }
}

/// Check what can be wrong without a dongle.
pub fn check_settings(s: &RtlSettings) -> Result<()> {
    if s.sample_rate != SAMPLE_RATE_HZ {
        return Err(Error::Other(format!(
            "RTL-SDR: DecDVB-T runs it at 2.048 MS/s only, not {:.3}",
            s.sample_rate as f64 / 1e6
        )));
    }
    check_freq(s.center_hz)?;
    if !(-200.0..=200.0).contains(&s.ppm) {
        return Err(Error::Other(format!(
            "RTL-SDR: {} ppm is outside ±200",
            s.ppm
        )));
    }
    Ok(())
}

fn check_freq(hz: u64) -> Result<()> {
    if (FREQ_MIN_HZ..=FREQ_MAX_HZ).contains(&hz) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "RTL-SDR: {:.3} MHz is outside the tuner's {}–{} MHz",
            hz as f64 / 1e6,
            FREQ_MIN_HZ / 1_000_000,
            FREQ_MAX_HZ / 1_000_000
        )))
    }
}

// ---- the arithmetic, kept apart so it can be tested on its own -----------

/// The RTL2832U's resampler ratio for `rate` (`rtlsdr_set_sample_rate`):
/// xtal · 2²² / rate, its two low bits clear. With the rate it really makes.
pub fn resampler_ratio(rate_hz: u32, xtal_hz: u32) -> (u32, f64) {
    let two22 = (1u64 << 22) as f64;
    let ratio = ((xtal_hz as f64 * two22) / rate_hz as f64) as u32 & 0x0fff_fffc;
    let real = ratio | ((ratio & 0x0800_0000) << 1);
    (ratio, xtal_hz as f64 * two22 / real as f64)
}

/// The demodulator's IF register value for an IF of `if_hz`
/// (`rtlsdr_set_if_freq`): −if · 2²² / xtal, 22 bits.
pub fn if_register(if_hz: u32, xtal_hz: f64) -> i32 {
    -(((if_hz as f64 * (1u64 << 22) as f64) / xtal_hz) as i32)
}

/// The sample-clock correction for `ppm` (`rtlsdr_set_sample_freq_correction`):
/// −ppm · 2²⁴ / 10⁶ as a 14-bit two's complement value.
pub fn sample_correction(ppm: f64) -> i16 {
    (-ppm * (1u64 << 24) as f64 / 1e6) as i16
}

/// A crystal `ppm` off nominal, as librtlsdr counts it (`APPLY_PPM_CORR`).
fn corrected(xtal_hz: u32, ppm: f64) -> f64 {
    xtal_hz as f64 * (1.0 + ppm / 1e6)
}

/// The FIR's 16 taps as the 20 register bytes (`rtlsdr_set_fir`): eight
/// signed bytes, then the 12-bit taps packed two to three bytes.
pub fn fir_bytes() -> [u8; 20] {
    let mut b = [0u8; 20];
    for (o, &t) in b.iter_mut().zip(&FIR_DEFAULT[..8]) {
        *o = t as i8 as u8;
    }
    for i in (0..8).step_by(2) {
        let (v0, v1) = (FIR_DEFAULT[8 + i], FIR_DEFAULT[9 + i]);
        let at = 8 + i * 3 / 2;
        b[at] = (v0 >> 4) as u8;
        b[at + 1] = ((v0 << 4) | ((v1 >> 8) & 0x0f)) as u8;
        b[at + 2] = v1 as u8;
    }
    b
}

/// The IF filter for a bandwidth (`r82xx_set_bandwidth`): registers 0x0a and
/// 0x0b, and the IF the tuner then puts the channel at.
pub fn if_filter(bw_hz: u32) -> (u8, u8, u32) {
    if bw_hz > 7_000_000 {
        (0x10, 0x0b, 4_570_000) // 8 MHz
    } else if bw_hz > 6_000_000 {
        (0x10, 0x2a, 4_570_000) // 7 MHz
    } else if bw_hz > IF_LOW_PASS_HZ[0] + FILT_HP_BW1 + FILT_HP_BW2 {
        (0x10, 0x6b, 3_570_000) // 6 MHz
    } else {
        let (mut bw, mut reg_0b, mut int_freq, mut real_bw) = (bw_hz, 0x80u8, 2_300_000u32, 0u32);
        if bw > IF_LOW_PASS_HZ[0] + FILT_HP_BW1 {
            bw -= FILT_HP_BW2;
            int_freq += FILT_HP_BW2;
            real_bw += FILT_HP_BW2;
        } else {
            reg_0b |= 0x20;
        }
        if bw > IF_LOW_PASS_HZ[0] {
            bw -= FILT_HP_BW1;
            int_freq += FILT_HP_BW1;
            real_bw += FILT_HP_BW1;
        } else {
            reg_0b |= 0x40;
        }
        // The narrowest low-pass that is still at least as wide as what is
        // left.
        let i = IF_LOW_PASS_HZ
            .iter()
            .position(|&c| bw > c)
            .unwrap_or(IF_LOW_PASS_HZ.len())
            .saturating_sub(1);
        reg_0b |= 15 - i as u8;
        real_bw += IF_LOW_PASS_HZ[i];
        (0x00, reg_0b, int_freq - real_bw / 2)
    }
}

/// The R82xx's PLL for an LO of `lo_hz` from a reference of `ref_hz`
/// (`r82xx_set_pll`): the VCO runs at LO · `mix_div`, within 1.77–3.54 GHz,
/// and is 2 · ref · (`nint` + `sdm` / 65536).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pll {
    pub mix_div: u32,
    /// log2(`mix_div`) − 1, before the VCO fine-tune correction.
    pub div_num: u8,
    pub nint: u32,
    pub sdm: u32,
}

impl Pll {
    pub fn new(lo_hz: u32, ref_hz: u32, tuner: Tuner) -> Result<Pll> {
        const VCO_MIN_KHZ: u64 = 1_770_000;
        let freq_khz = (u64::from(lo_hz) + 500) / 1000;
        let (mix_div, div_num) = (1..=6u8)
            .map(|k| (1u32 << k, k - 1))
            .find(|&(m, _)| (VCO_MIN_KHZ..2 * VCO_MIN_KHZ).contains(&(freq_khz * u64::from(m))))
            .ok_or_else(|| {
                Error::Other(format!(
                    "R82xx: no PLL divider for an LO of {:.3} MHz",
                    lo_hz as f64 / 1e6
                ))
            })?;
        // Rounded to the nearest 1/65536: (ref + 65536 · vco) / (2 · ref).
        let vco = u64::from(lo_hz) * u64::from(mix_div);
        let r = u64::from(ref_hz);
        let vco_div = (r + 65536 * vco) / (2 * r);
        let (nint, sdm) = ((vco_div / 65536) as u32, (vco_div % 65536) as u32);
        let max = 128 / vco_power_ref(tuner) - 1;
        if !(13..=max).contains(&nint) {
            return Err(Error::Other(format!(
                "R82xx: no PLL values for {:.3} MHz",
                lo_hz as f64 / 1e6
            )));
        }
        Ok(Pll {
            mix_div,
            div_num,
            nint,
            sdm,
        })
    }

    /// Register 0x14: `nint` as 13 + 4·ni + si.
    pub fn reg_14(&self) -> u8 {
        let ni = (self.nint - 13) / 4;
        let si = self.nint - 4 * ni - 13;
        (ni + (si << 6)) as u8
    }

    /// The LO this makes, Hz.
    pub fn lo_hz(&self, ref_hz: f64) -> f64 {
        2.0 * ref_hz * (self.nint as f64 + self.sdm as f64 / 65536.0) / self.mix_div as f64
    }
}

fn vco_power_ref(tuner: Tuner) -> u32 {
    match tuner {
        Tuner::R828D => 1,
        Tuner::R820T => 2,
    }
}

/// The LNA and mixer indices for a gain of `db10` tenths of a dB, and the
/// gain they make (`r82xx_set_gain`): the two are raised in turn until the
/// sum reaches it.
pub fn gain_indices(db10: u16) -> (u8, u8, i32) {
    let want = i32::from(db10);
    let (mut total, mut lna, mut mix) = (0i32, 0usize, 0usize);
    for _ in 0..15 {
        if total >= want {
            break;
        }
        lna += 1;
        total += LNA_STEPS[lna];
        if total >= want {
            break;
        }
        mix += 1;
        total += MIXER_STEPS[mix];
    }
    (lna as u8, mix as u8, total)
}

/// The RTL-SDR Blog V4's R828D has three inputs, each with its own filter:
/// HF (through the upconverter), VHF and UHF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum V4Band {
    Hf,
    Vhf,
    Uhf,
}

/// Unsigned 8-bit IQ, as the RTL2832U sends it, to samples (centred on
/// 127.5, scaled as `decdvbt_core::bytes_to_iq` scales cu8, so a capture
/// written back as cu8 is bit-exact).
pub struct U8ToIq;

impl RawToIq for U8ToIq {
    fn process(&mut self, bytes: &[u8], out: &mut Vec<Iq>) {
        let (pairs, _) = bytes.as_chunks::<2>();
        out.extend(
            pairs
                .iter()
                .map(|p| Iq::new((p[0] as f32 - 127.5) / 128.0, (p[1] as f32 - 127.5) / 128.0)),
        );
    }

    fn reset(&mut self) {}

    fn output_rate(&self) -> f64 {
        SAMPLE_RATE_HZ as f64
    }
}

// ---- the driver ------------------------------------------------------------

/// An RTL2832U with an R82xx tuner, brought up and tuned over an [`RtlBus`].
pub struct Rtl2832<B: RtlBus> {
    bus: B,
    tuner: Tuner,
    i2c_addr: u8,
    /// The tuner's nominal crystal.
    tuner_xtal_hz: u32,
    /// An RTL-SDR Blog V4 (its R828D's inputs are switched by band).
    v4: bool,
    v4_band: Option<V4Band>,
    /// Which of a plain R828D's inputs is in use (reg 0x05 bits 5–6).
    r828d_input: u8,
    /// Shadow of the tuner's registers 0x05..=0x1f (the chip's writes cannot
    /// be read back, so read-modify-write works on this).
    regs: [u8; 27],
    /// The IF the tuner puts the channel at.
    int_freq: u32,
    has_lock: bool,
    tuner_up: bool,
    settings: RtlSettings,
}

impl<B: RtlBus> Rtl2832<B> {
    /// Power up the RTL2832U, find the tuner and initialise it
    /// (`rtlsdr_open`), and drive the bias tee off. `v4`: the USB strings say
    /// RTL-SDR Blog V4. Not yet tuned: [`Rtl2832::start`] does that.
    pub fn open(bus: B, v4: bool) -> Result<Self> {
        let mut d = Rtl2832 {
            bus,
            tuner: Tuner::R820T,
            i2c_addr: R820T_I2C_ADDR,
            tuner_xtal_hz: RTL_XTAL_HZ,
            v4,
            v4_band: None,
            r828d_input: 0,
            regs: R82XX_INIT,
            int_freq: 0,
            has_lock: false,
            tuner_up: false,
            settings: RtlSettings {
                center_hz: 0,
                ..RtlSettings::default()
            },
        };
        // A dummy write: librtlsdr resets a dongle that fails it; here it is
        // just reported.
        d.write_reg(block::USB, reg::USB_SYSCTL, 0x09, 1)
            .map_err(|e| Error::Other(format!("RTL-SDR does not answer ({e}); replug it")))?;
        d.init_baseband()?;

        d.i2c_repeater(true)?;
        let found = if d.i2c_read_reg(R820T_I2C_ADDR, 0) == Some(R82XX_CHECK_VAL) {
            Some((Tuner::R820T, R820T_I2C_ADDR, RTL_XTAL_HZ))
        } else if d.i2c_read_reg(R828D_I2C_ADDR, 0) == Some(R82XX_CHECK_VAL) {
            // The Blog V4's R828D runs from the RTL2832U's crystal; other
            // R828D dongles have their own 16 MHz one.
            let xtal = if v4 { RTL_XTAL_HZ } else { R828D_XTAL_HZ };
            Some((Tuner::R828D, R828D_I2C_ADDR, xtal))
        } else {
            None
        };
        let Some((tuner, addr, xtal)) = found else {
            let _ = d.i2c_repeater(false);
            return Err(Error::Other(
                "RTL-SDR: the tuner is not an R820T/R820T2/R828D, the only ones DecDVB-T drives \
                 (RTL-SDR Blog V3/V4 and most dongles of the last decade have one)"
                    .into(),
            ));
        };
        (d.tuner, d.i2c_addr, d.tuner_xtal_hz) = (tuner, addr, xtal);
        // Low IF: zero-IF off, the I ADC only, the IF, spectrum inverted.
        d.demod_write(1, 0xb1, 0x1a, 1)?;
        d.demod_write(0, 0x08, 0x4d, 1)?;
        d.set_if_freq(3_570_000)?;
        d.demod_write(1, 0x15, 0x01, 1)?;
        d.r82xx_init()?;
        d.i2c_repeater(false)?;

        d.set_bias_tee(false)?;
        Ok(d)
    }

    pub fn tuner(&self) -> Tuner {
        self.tuner
    }

    pub fn settings(&self) -> RtlSettings {
        self.settings
    }

    /// The tuner's PLL locked at the last tuning.
    pub fn locked(&self) -> bool {
        self.has_lock
    }

    #[cfg(test)]
    fn bus(&self) -> &B {
        &self.bus
    }

    /// Set the rate, the correction, the gain and the frequency, and clear
    /// the sample FIFO: ready to stream.
    pub fn start(&mut self, s: &RtlSettings) -> Result<()> {
        check_settings(s)?;
        self.settings.ppm = s.ppm;
        self.settings.center_hz = s.center_hz;
        self.set_sample_rate(s.sample_rate)?;
        self.set_gains(s.gains)?;
        self.set_bias_tee(s.bias_tee)?;
        self.reset_buffer()
    }

    /// `rtlsdr_set_sample_rate`, with the tuner's IF filter set to the rate.
    fn set_sample_rate(&mut self, rate: u32) -> Result<()> {
        let (ratio, real) = resampler_ratio(rate, RTL_XTAL_HZ);
        self.i2c_repeater(true)?;
        let (r0a, r0b, int_freq) = if_filter(rate);
        self.tuner_write_mask(0x0a, r0a, 0x10)?;
        self.tuner_write_mask(0x0b, r0b, 0xef)?;
        self.int_freq = int_freq;
        self.set_if_freq(int_freq)?;
        self.tuner_set_freq(self.settings.center_hz)?;
        self.i2c_repeater(false)?;
        self.demod_write(1, 0x9f, (ratio >> 16) as u16, 2)?;
        self.demod_write(1, 0xa1, (ratio & 0xffff) as u16, 2)?;
        self.write_sample_correction()?;
        // Soft reset of the demodulator.
        self.demod_write(1, 0x01, 0x14, 1)?;
        self.demod_write(1, 0x01, 0x10, 1)?;
        self.settings.sample_rate = real.round() as u32;
        Ok(())
    }

    /// Retune (`rtlsdr_set_center_freq`).
    pub fn set_center(&mut self, hz: u64) -> Result<()> {
        check_freq(hz)?;
        self.i2c_repeater(true)?;
        let r = self.tuner_set_freq(hz);
        self.i2c_repeater(false)?;
        r?;
        self.settings.center_hz = hz;
        Ok(())
    }

    /// The clock error, ppm (`rtlsdr_set_freq_correction`): the sample clock
    /// is corrected in the demodulator and the tuner's reference in the PLL
    /// sums, then the tuner is retuned.
    pub fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        let ppm = ppm.clamp(-200.0, 200.0);
        if ppm == self.settings.ppm {
            return Ok(());
        }
        self.settings.ppm = ppm;
        self.write_sample_correction()?;
        let hz = self.settings.center_hz;
        if hz != 0 {
            self.set_center(hz)?;
        }
        Ok(())
    }

    /// `r82xx_set_gain`: the tuner's AGC, or a fixed LNA and mixer gain.
    pub fn set_gains(&mut self, g: RtlGains) -> Result<()> {
        let g = RtlGains {
            db10: RtlGains::nearest_step(g.db10),
            ..g
        };
        self.i2c_repeater(true)?;
        if g.agc {
            self.tuner_write_mask(0x05, 0x00, 0x10)?; // LNA auto
            self.tuner_write_mask(0x07, 0x10, 0x10)?; // mixer auto
            self.tuner_write_mask(0x0c, 0x0b, 0x9f)?; // VGA 26.5 dB
        } else {
            self.tuner_write_mask(0x05, 0x10, 0x10)?; // LNA manual
            self.tuner_write_mask(0x07, 0x00, 0x10)?; // mixer manual
            self.tuner_read(4)?;
            self.tuner_write_mask(0x0c, 0x08, 0x9f)?; // VGA 16.3 dB
            let (lna, mix, _) = gain_indices(g.db10);
            self.tuner_write_mask(0x05, lna, 0x0f)?;
            self.tuner_write_mask(0x07, mix, 0x0f)?;
        }
        self.i2c_repeater(false)?;
        self.settings.gains = g;
        Ok(())
    }

    /// Clear the endpoint's FIFO (`rtlsdr_reset_buffer`), before streaming.
    pub fn reset_buffer(&mut self) -> Result<()> {
        self.write_reg(block::USB, reg::USB_EPA_CTL, 0x1002, 2)?;
        self.write_reg(block::USB, reg::USB_EPA_CTL, 0x0000, 2)
    }

    /// Put the tuner in standby and power the demodulator and ADCs down
    /// (`rtlsdr_deinit_baseband`).
    pub fn close(&mut self) -> Result<()> {
        // Antenna power off first: it must not outlive the program.
        self.set_bias_tee(false)?;
        if self.tuner_up {
            self.i2c_repeater(true)?;
            for (r, v) in [
                (0x06, 0xb1),
                (0x05, 0x03),
                (0x07, 0x3a),
                (0x08, 0x40),
                (0x09, 0xc0),
                (0x0a, 0x36),
                (0x0c, 0x35),
                (0x0f, 0x68),
                (0x11, 0x03),
                (0x17, 0xf4),
                (0x19, 0x0c),
            ] {
                self.tuner_write(r, &[v])?;
            }
            self.i2c_repeater(false)?;
            self.tuner_up = false;
        }
        self.write_reg(block::SYS, reg::DEMOD_CTL, 0x20, 1)
    }

    // ---- the RTL2832U ------------------------------------------------------

    /// Registers are written big-endian (`rtlsdr_write_reg`).
    fn be(val: u16, len: usize) -> Vec<u8> {
        if len == 1 {
            vec![val as u8]
        } else {
            val.to_be_bytes().to_vec()
        }
    }

    /// A vendor OUT request. After a pause (the 250 ms in `sysfreq_sel`) an
    /// RTL-SDR Blog V4 was seen to stall every OUT request until an IN one
    /// went through, so a failed one is tried again after the dummy read
    /// `demod_write` uses. An I2C address nothing answers at fails again.
    fn out(&mut self, value: u16, index: u16, data: &[u8]) -> Result<()> {
        if self.bus.control_out(value, index, data).is_ok() {
            return Ok(());
        }
        let _ = self.bus.control_in((0x01 << 8) | 0x20, 0x0a, 1);
        self.bus.control_out(value, index, data)
    }

    fn write_reg(&mut self, blk: u16, addr: u16, val: u16, len: usize) -> Result<()> {
        self.out(addr, (blk << 8) | 0x10, &Self::be(val, len))
    }

    fn read_reg(&mut self, blk: u16, addr: u16) -> Result<u8> {
        let d = self.bus.control_in(addr, blk << 8, 1)?;
        d.first()
            .copied()
            .ok_or_else(|| Error::Other("RTL-SDR: empty register read".into()))
    }

    /// `rtlsdr_demod_write_reg`, with its dummy read that lets the write
    /// complete.
    fn demod_write(&mut self, page: u16, addr: u16, val: u16, len: usize) -> Result<()> {
        self.out((addr << 8) | 0x20, 0x10 | page, &Self::be(val, len))?;
        self.bus.control_in((0x01 << 8) | 0x20, 0x0a, 1)?;
        Ok(())
    }

    fn i2c_repeater(&mut self, on: bool) -> Result<()> {
        self.demod_write(1, 0x01, if on { 0x18 } else { 0x10 }, 1)
    }

    fn i2c_write(&mut self, addr: u8, data: &[u8]) -> Result<()> {
        self.out(u16::from(addr), (block::IIC << 8) | 0x10, data)
    }

    fn i2c_read(&mut self, addr: u8, len: u16) -> Result<Vec<u8>> {
        let d = self.bus.control_in(u16::from(addr), block::IIC << 8, len)?;
        if d.len() == len as usize {
            Ok(d)
        } else {
            Err(Error::Other("RTL-SDR: short I2C read".into()))
        }
    }

    /// One raw register of a chip on the I2C bus, for probing: `None` if
    /// nothing answers there.
    fn i2c_read_reg(&mut self, addr: u8, r: u8) -> Option<u8> {
        self.i2c_write(addr, &[r]).ok()?;
        self.i2c_read(addr, 1).ok()?.first().copied()
    }

    /// `rtlsdr_init_baseband`.
    fn init_baseband(&mut self) -> Result<()> {
        self.write_reg(block::USB, reg::USB_SYSCTL, 0x09, 1)?;
        self.write_reg(block::USB, reg::USB_EPA_MAXPKT, 0x0002, 2)?;
        self.write_reg(block::USB, reg::USB_EPA_CTL, 0x1002, 2)?;
        // Power the demodulator on, and soft-reset it.
        self.write_reg(block::SYS, reg::DEMOD_CTL_1, 0x22, 1)?;
        self.write_reg(block::SYS, reg::DEMOD_CTL, 0xe8, 1)?;
        self.demod_write(1, 0x01, 0x14, 1)?;
        self.demod_write(1, 0x01, 0x10, 1)?;
        // No spectrum inversion or adjacent-channel rejection yet; the DDC
        // shift and IF registers cleared.
        self.demod_write(1, 0x15, 0x00, 1)?;
        self.demod_write(1, 0x16, 0x0000, 2)?;
        for i in 0..6 {
            self.demod_write(1, 0x16 + i, 0x00, 1)?;
        }
        for (i, b) in fir_bytes().into_iter().enumerate() {
            self.demod_write(1, 0x1c + i as u16, u16::from(b), 1)?;
        }
        // SDR mode, digital AGC off; the FSM state-holding registers; the
        // AGC loops and the PID filter off; the default ADC datapath; zero
        // IF with DC and IQ estimation (the tuner turns zero-IF off again);
        // no 4.096 MHz clock out.
        self.demod_write(0, 0x19, 0x05, 1)?;
        self.demod_write(1, 0x93, 0xf0, 1)?;
        self.demod_write(1, 0x94, 0x0f, 1)?;
        self.demod_write(1, 0x11, 0x00, 1)?;
        self.demod_write(1, 0x04, 0x00, 1)?;
        self.demod_write(0, 0x61, 0x60, 1)?;
        self.demod_write(0, 0x06, 0x80, 1)?;
        self.demod_write(1, 0xb1, 0x1b, 1)?;
        self.demod_write(0, 0x0d, 0x83, 1)
    }

    fn set_if_freq(&mut self, if_hz: u32) -> Result<()> {
        let v = if_register(if_hz, corrected(RTL_XTAL_HZ, self.settings.ppm));
        self.demod_write(1, 0x19, ((v >> 16) & 0x3f) as u16, 1)?;
        self.demod_write(1, 0x1a, ((v >> 8) & 0xff) as u16, 1)?;
        self.demod_write(1, 0x1b, (v & 0xff) as u16, 1)
    }

    fn write_sample_correction(&mut self) -> Result<()> {
        let offs = sample_correction(self.settings.ppm);
        self.demod_write(1, 0x3f, (offs & 0xff) as u16, 1)?;
        self.demod_write(1, 0x3e, ((offs >> 8) & 0x3f) as u16, 1)
    }

    /// GPIO 0 an output, driven high (the bias tee on) or low (off), as
    /// librtlsdr's `rtlsdr_set_gpio_output` and `rtlsdr_set_gpio_bit` do.
    pub fn set_bias_tee(&mut self, on: bool) -> Result<()> {
        let bit = 1u8 << BIAS_TEE_GPIO;
        let d = self.read_reg(block::SYS, reg::GPD)?;
        self.write_reg(block::SYS, reg::GPD, u16::from(d & !bit), 1)?;
        let oe = self.read_reg(block::SYS, reg::GPOE)?;
        self.write_reg(block::SYS, reg::GPOE, u16::from(oe | bit), 1)?;
        let o = self.read_reg(block::SYS, reg::GPO)?;
        let o = if on { o | bit } else { o & !bit };
        self.write_reg(block::SYS, reg::GPO, u16::from(o), 1)?;
        self.settings.bias_tee = on;
        Ok(())
    }

    // ---- the R82xx (the I2C repeater must be on) ---------------------------

    /// Write `vals` from register `r` on, in chunks the bridge takes, and
    /// keep the shadow.
    fn tuner_write(&mut self, r: u8, vals: &[u8]) -> Result<()> {
        for (k, &v) in vals.iter().enumerate() {
            let at = usize::from(r) + k;
            if let Some(s) = at
                .checked_sub(usize::from(SHADOW_START))
                .and_then(|i| self.regs.get_mut(i))
            {
                *s = v;
            }
        }
        for (n, chunk) in vals.chunks(I2C_MAX_DATA).enumerate() {
            let mut msg = Vec::with_capacity(chunk.len() + 1);
            msg.push(r + (n * I2C_MAX_DATA) as u8);
            msg.extend_from_slice(chunk);
            self.i2c_write(self.i2c_addr, &msg)?;
        }
        Ok(())
    }

    fn tuner_write_mask(&mut self, r: u8, val: u8, mask: u8) -> Result<()> {
        let old = self.regs[usize::from(r - SHADOW_START)];
        self.tuner_write(r, &[(old & !mask) | (val & mask)])
    }

    /// The status registers from 0 on. The chip sends each byte LSB first.
    fn tuner_read(&mut self, len: u16) -> Result<Vec<u8>> {
        self.i2c_write(self.i2c_addr, &[0x00])?;
        let mut d = self.i2c_read(self.i2c_addr, len)?;
        for b in &mut d {
            *b = b.reverse_bits();
        }
        Ok(d)
    }

    /// `r82xx_init`: the registers, the filter calibration, the DVB-T
    /// system settings.
    fn r82xx_init(&mut self) -> Result<()> {
        self.tuner_write(SHADOW_START, &R82XX_INIT)?;
        self.set_tv_standard()?;
        self.sysfreq_sel()?;
        self.tuner_up = true;
        Ok(())
    }

    /// `r82xx_set_tv_standard` for digital TV under 6 MHz, which calibrates
    /// the IF filter against the PLL at 56 MHz.
    fn set_tv_standard(&mut self) -> Result<()> {
        const FILT_CAL_LO: u32 = 56_000_000;
        const FILT_GAIN: u8 = 0x10; // +3 dB, 6 MHz on
        const IMG_R: u8 = 0x00; // image negative
        const FILT_Q: u8 = 0x10; // low Q
        const HP_COR: u8 = 0x6b; // 1.7 MHz disabled, +2 cap, 1.0 MHz
        const EXT_ENABLE: u8 = 0x60; // ext enable, ext at LNA max−1
        const LOOP_THROUGH: u8 = 0x01; // loop-through off
        const LT_ATT: u8 = 0x00;
        const FLT_EXT_WIDEST: u8 = 0x00;
        const POLYFIL_CUR: u8 = 0x60; // min
        const VER_NUM: u8 = 49;

        self.regs = R82XX_INIT;
        self.tuner_write_mask(0x0c, 0x00, 0x0f)?;
        self.tuner_write_mask(0x13, VER_NUM, 0x3f)?;
        self.tuner_write_mask(0x1d, 0x00, 0x38)?;
        self.bus.sleep(Duration::from_millis(1));
        self.int_freq = 3_570_000;

        let mut cal = 0;
        for _ in 0..2 {
            self.tuner_write_mask(0x0b, HP_COR, 0x60)?;
            self.tuner_write_mask(0x0f, 0x04, 0x04)?; // calibration clock on
            self.tuner_write_mask(0x10, 0x00, 0x03)?; // crystal cap 0 pF
            self.set_pll(FILT_CAL_LO)?;
            if !self.has_lock {
                return Ok(()); // librtlsdr carries on likewise
            }
            self.tuner_write_mask(0x0b, 0x10, 0x10)?; // start
            self.bus.sleep(Duration::from_millis(1));
            self.tuner_write_mask(0x0b, 0x00, 0x10)?; // stop
            self.tuner_write_mask(0x0f, 0x00, 0x04)?; // calibration clock off
            cal = self.tuner_read(5)?[4] & 0x0f;
            if cal != 0 && cal != 0x0f {
                break;
            }
        }
        if cal == 0x0f {
            cal = 0; // narrowest
        }
        self.tuner_write_mask(0x0a, FILT_Q | cal, 0x1f)?;
        self.tuner_write_mask(0x0b, HP_COR, 0xef)?;
        self.tuner_write_mask(0x07, IMG_R, 0x80)?;
        self.tuner_write_mask(0x06, FILT_GAIN, 0x30)?;
        self.tuner_write_mask(0x1e, EXT_ENABLE, 0x60)?;
        self.tuner_write_mask(0x05, LOOP_THROUGH, 0x80)?;
        self.tuner_write_mask(0x1f, LT_ATT, 0x80)?;
        self.tuner_write_mask(0x0f, FLT_EXT_WIDEST, 0x80)?;
        self.tuner_write_mask(0x19, POLYFIL_CUR, 0x60)
    }

    /// `r82xx_sysfreq_sel` for DVB-T: the LNA and mixer AGC thresholds and
    /// currents, inputs, then the LNA's discharge settling.
    fn sysfreq_sel(&mut self) -> Result<()> {
        const MIXER_TOP: u8 = 0x24; // top 13, top−1, low discharge
        const LNA_TOP: u8 = 0xe5; // detect bw 3, LNA top 4, predetect top 2
        const CP_CUR: u8 = 0x38; // auto
        const DIV_BUF_CUR: u8 = 0x30; // 150 µA
        const LNA_VTH_L: u8 = 0x53; // vth 0.84, vtl 0.64
        const MIXER_VTH_L: u8 = 0x75; // vth 1.04, vtl 0.84
        const AIR_CABLE1_IN: u8 = 0x00;
        const CABLE2_IN: u8 = 0x00;
        const LNA_DISCHARGE: u8 = 14;
        const FILTER_CUR: u8 = 0x40; // low

        self.tuner_write_mask(0x1d, LNA_TOP, 0xc7)?;
        self.tuner_write_mask(0x1c, MIXER_TOP, 0xf8)?;
        self.tuner_write(0x0d, &[LNA_VTH_L])?;
        self.tuner_write(0x0e, &[MIXER_VTH_L])?;
        self.r828d_input = AIR_CABLE1_IN;
        self.tuner_write_mask(0x05, AIR_CABLE1_IN, 0x60)?;
        self.tuner_write_mask(0x06, CABLE2_IN, 0x08)?;
        self.tuner_write_mask(0x11, CP_CUR, 0x38)?;
        self.tuner_write_mask(0x17, DIV_BUF_CUR, 0x30)?;
        self.tuner_write_mask(0x0a, FILTER_CUR, 0x60)?;
        // LNA top lowest, normal mode, predetect off, AGC clock 250 Hz; let
        // it settle; then LNA top 3, the discharge mode and current, AGC
        // clock 60 Hz.
        self.tuner_write_mask(0x1d, 0x00, 0x38)?;
        self.tuner_write_mask(0x1c, 0x00, 0x04)?;
        self.tuner_write_mask(0x06, 0x00, 0x40)?;
        self.tuner_write_mask(0x1a, 0x30, 0x30)?;
        self.bus.sleep(Duration::from_millis(250));
        self.tuner_write_mask(0x1d, 0x18, 0x38)?;
        // (The mask is upstream's; it looks wrong there too.)
        self.tuner_write_mask(0x1c, MIXER_TOP, 0x04)?;
        self.tuner_write_mask(0x1e, LNA_DISCHARGE, 0x1f)?;
        self.tuner_write_mask(0x1a, 0x20, 0x30)
    }

    /// `r82xx_set_mux`: the RF filters for an LO of `lo_hz`.
    fn set_mux(&mut self, lo_hz: u32) -> Result<()> {
        let mhz = lo_hz / 1_000_000;
        let i = FREQ_RANGES.iter().rposition(|r| mhz >= r.0).unwrap_or(0);
        let (_, open_d, rf_mux_ploy, tf_c) = FREQ_RANGES[i];
        self.tuner_write_mask(0x17, open_d, 0x08)?;
        self.tuner_write_mask(0x1a, rf_mux_ploy, 0xc3)?;
        self.tuner_write(0x1b, &[tf_c])?;
        self.tuner_write_mask(0x10, 0x00, 0x0b)?; // crystal: high cap, 0 pF
        self.tuner_write_mask(0x08, 0x00, 0x3f)?;
        self.tuner_write_mask(0x09, 0x00, 0x3f)
    }

    /// `r82xx_set_pll`. Leaves `has_lock` saying whether it locked.
    fn set_pll(&mut self, lo_hz: u32) -> Result<()> {
        let ref_hz = corrected(self.tuner_xtal_hz, self.settings.ppm) as u32;
        self.tuner_write_mask(0x10, 0x00, 0x10)?; // refdiv2 off
        self.tuner_write_mask(0x1a, 0x00, 0x0c)?; // autotune 128 kHz
        self.tuner_write_mask(0x12, 0x80, 0xe0)?; // VCO current 100
        let pll = Pll::new(lo_hz, ref_hz, self.tuner)?;

        // The VCO's fine-tune reading moves the divider a step either way.
        let fine = (self.tuner_read(5)?[4] & 0x30) >> 4;
        let power_ref = vco_power_ref(self.tuner) as u8;
        let mut div_num = pll.div_num;
        if fine > power_ref {
            div_num = div_num.wrapping_sub(1);
        } else if fine < power_ref {
            div_num += 1;
        }
        self.tuner_write_mask(0x10, div_num << 5, 0xe0)?;
        self.tuner_write(0x14, &[pll.reg_14()])?;
        // pw_sdm: the sigma-delta modulator off when there is no fraction.
        self.tuner_write_mask(0x12, if pll.sdm == 0 { 0x08 } else { 0x00 }, 0x08)?;
        self.tuner_write(0x16, &[(pll.sdm >> 8) as u8])?;
        self.tuner_write(0x15, &[pll.sdm as u8])?;

        self.has_lock = false;
        for i in 0..2 {
            self.bus.sleep(Duration::from_millis(10));
            if self.tuner_read(3)?[2] & 0x40 != 0 {
                self.has_lock = true;
                break;
            }
            if i == 0 {
                self.tuner_write_mask(0x12, 0x60, 0xe0)?; // more VCO current
            }
        }
        if !self.has_lock {
            tracing::warn!("R82xx PLL did not lock at {:.3} MHz", lo_hz as f64 / 1e6);
            return Ok(());
        }
        self.tuner_write_mask(0x1a, 0x08, 0x08) // autotune 8 kHz
    }

    /// `r82xx_set_freq`: the LO above the channel by the IF, and the input
    /// for the band.
    fn tuner_set_freq(&mut self, hz: u64) -> Result<()> {
        if hz == 0 {
            return Ok(());
        }
        let lo = (hz + u64::from(self.int_freq)) as u32;
        self.set_mux(lo)?;
        self.set_pll(lo)?;
        if !self.has_lock {
            return Ok(());
        }
        if self.tuner == Tuner::R828D {
            if self.v4 {
                // The V4's notch filters are opened when tuned inside them
                // (the FM band and Band III among them), closed elsewhere.
                let f = hz as f64;
                let inside =
                    f <= 2.2e6 || (85e6..=112e6).contains(&f) || (172e6..=242e6).contains(&f);
                self.tuner_write_mask(0x17, if inside { 0x00 } else { 0x08 }, 0x08)?;
                let band = if f <= 28.8e6 {
                    V4Band::Hf
                } else if f < 250e6 {
                    V4Band::Vhf
                } else {
                    V4Band::Uhf
                };
                if self.v4_band != Some(band) {
                    self.v4_band = Some(band);
                    self.tuner_write_mask(
                        0x06,
                        if band == V4Band::Hf { 0x08 } else { 0x00 },
                        0x08,
                    )?;
                    self.tuner_write_mask(
                        0x05,
                        if band == V4Band::Vhf { 0x40 } else { 0x00 },
                        0x40,
                    )?;
                    self.tuner_write_mask(
                        0x05,
                        if band == V4Band::Uhf { 0x00 } else { 0x20 },
                        0x20,
                    )?;
                }
            } else {
                // Cable 1 below 345 MHz, where its noise floor is the lower;
                // air above.
                let input = if hz > 345_000_000 { 0x00 } else { 0x60 };
                if input != self.r828d_input {
                    self.r828d_input = input;
                    self.tuner_write_mask(0x05, input, 0x60)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dongle in memory: answers the probe at one tuner address, keeps the
    /// tuner's registers as written, reports the PLL locked, and logs every
    /// transfer.
    struct Mock {
        tuner_addr: u8,
        tuner: [u8; 32],
        /// What a read of status register 4 returns (the filter
        /// calibration code and the VCO fine-tune), unreversed.
        status4: u8,
        lock: bool,
        sys: std::collections::HashMap<u16, u8>,
        log: Vec<(u16, u16, Vec<u8>)>,
        slept: Duration,
        /// Like the RTL-SDR Blog V4: after a pause of 100 ms or more, every
        /// OUT request stalls until an IN one goes through.
        stall_after_pause: bool,
        stalled: bool,
    }

    impl Mock {
        fn new(tuner_addr: u8) -> Mock {
            Mock {
                tuner_addr,
                tuner: [0; 32],
                status4: 0x25, // fine tune 2, calibration code 5
                lock: true,
                sys: Default::default(),
                log: Vec::new(),
                slept: Duration::ZERO,
                stall_after_pause: false,
                stalled: false,
            }
        }

        /// The demodulator writes, as (page, addr, value).
        fn demod_writes(&self) -> Vec<(u16, u16, u16)> {
            self.log
                .iter()
                .filter(|(v, i, _)| v & 0xff == 0x20 && i & 0x10 != 0 && i >> 8 == 0)
                .map(|(v, i, d)| {
                    let val = d.iter().fold(0u16, |a, &b| (a << 8) | u16::from(b));
                    (i & 0x0f, v >> 8, val)
                })
                .collect()
        }
    }

    impl RtlBus for Mock {
        fn control_out(&mut self, value: u16, index: u16, data: &[u8]) -> Result<()> {
            if self.stalled {
                return Err(Error::Other("endpoint STALL condition".into()));
            }
            self.log.push((value, index, data.to_vec()));
            if index == (block::IIC << 8) | 0x10 && value == u16::from(self.tuner_addr) {
                let r = usize::from(data[0]);
                for (k, &b) in data[1..].iter().enumerate() {
                    self.tuner[r + k] = b;
                }
            }
            if index == (block::SYS << 8) | 0x10 {
                self.sys.insert(value, data[0]);
            }
            Ok(())
        }

        fn control_in(&mut self, value: u16, index: u16, len: u16) -> Result<Vec<u8>> {
            self.stalled = false;
            if index == block::IIC << 8 {
                if value != u16::from(self.tuner_addr) {
                    return Err(Error::Other("no ack".into()));
                }
                let status = [
                    R82XX_CHECK_VAL,
                    0,
                    if self.lock { 0x40u8.reverse_bits() } else { 0 },
                    0,
                    self.status4.reverse_bits(),
                ];
                return Ok(status[..len as usize].to_vec());
            }
            if index == block::SYS << 8 {
                return Ok(vec![*self.sys.get(&value).unwrap_or(&0xff)]);
            }
            Ok(vec![0; len as usize])
        }

        fn sleep(&mut self, d: Duration) {
            self.slept += d;
            self.stalled |= self.stall_after_pause && d >= Duration::from_millis(100);
        }
    }

    fn opened(tuner_addr: u8, v4: bool) -> Rtl2832<Mock> {
        let mut d = Rtl2832::open(Mock::new(tuner_addr), v4).expect("open");
        d.start(&RtlSettings {
            center_hz: 222_064_000, // 11D
            ..RtlSettings::default()
        })
        .expect("start");
        d
    }

    /// The pause in `sysfreq_sel` leaves a V4 stalling OUT requests; open
    /// gets past it.
    #[test]
    fn an_out_stall_after_a_pause_is_got_past() {
        let mut m = Mock::new(R828D_I2C_ADDR);
        m.stall_after_pause = true;
        let mut d = Rtl2832::open(m, true).expect("open");
        assert_eq!(d.tuner(), Tuner::R828D);
        d.start(&RtlSettings::default()).expect("start");
    }

    #[test]
    fn the_resampler_makes_2048_exactly() {
        let (ratio, real) = resampler_ratio(SAMPLE_RATE_HZ, RTL_XTAL_HZ);
        assert_eq!(ratio, 0x0384_0000);
        assert_eq!(real, 2_048_000.0);
    }

    #[test]
    fn fir_bytes_pack_the_12_bit_taps() {
        let b = fir_bytes();
        assert_eq!(b[0], (-54i8) as u8);
        assert_eq!(b[7], 53);
        // 101 = 0x065, 156 = 0x09c → 06 50 9c.
        assert_eq!(&b[8..11], &[0x06, 0x50, 0x9c]);
        // 404 = 0x194, 421 = 0x1a5 → 19 41 a5.
        assert_eq!(&b[17..20], &[0x19, 0x41, 0xa5]);
    }

    #[test]
    fn the_if_filter_for_dab_rate() {
        // 2.048 MHz: below the 6 MHz setting, so both high-pass steps are
        // weighed, one taken, and the widest low-pass: 2.05 MHz in all,
        // centred at 1.625 MHz.
        assert_eq!(if_filter(2_048_000), (0x00, 0xaf, 1_625_000));
        assert_eq!(if_filter(8_000_000), (0x10, 0x0b, 4_570_000));
        assert_eq!(if_filter(6_000_000).2, 3_570_000);
        // Narrower: no high-pass steps, the 1.2 MHz low-pass.
        let (_, r, f) = if_filter(1_000_000);
        assert_eq!(r, 0x80 | 0x60 | 11);
        assert_eq!(f, 1_700_000);
    }

    #[test]
    fn the_pll_lands_within_a_fraction_of_a_khz_over_band_iii() {
        let r = RTL_XTAL_HZ;
        let mut f = 174_000_000u32;
        while f <= 240_000_000 {
            let lo = f + 1_625_000;
            let p = Pll::new(lo, r, Tuner::R820T).unwrap();
            // The VCO is 1.77–3.54 GHz: ×16 low in the band, ×8 above
            // 221.25 MHz.
            assert!(matches!(p.mix_div, 8 | 16), "{f}");
            assert_eq!(1u32 << (p.div_num + 1), p.mix_div);
            let err = p.lo_hz(r as f64) - lo as f64;
            // Resolution: 2 · 28.8 MHz / 65536 / 8 = 110 Hz (55 at ×16).
            assert!(err.abs() <= 56.0, "{f}: {err} Hz");
            // Register 0x14 holds nint as 13 + 4·ni + si.
            let v = p.reg_14();
            assert_eq!(13 + 4 * u32::from(v & 0x3f) + u32::from(v >> 6), p.nint);
            f += 16_000;
        }
    }

    #[test]
    fn the_pll_dividers_cover_the_range() {
        for (lo, div) in [
            (30e6, 64),
            (56e6, 32),
            (100e6, 32),
            (500e6, 4),
            (1.0e9, 2),
            (1.7e9, 2),
        ] {
            let p = Pll::new(lo as u32, RTL_XTAL_HZ, Tuner::R820T).unwrap();
            assert_eq!(p.mix_div, div, "{lo}");
        }
        assert!(Pll::new(20_000_000, RTL_XTAL_HZ, Tuner::R820T).is_err());
        // The R828D's own 16 MHz crystal needs larger integer parts.
        let p = Pll::new(223_689_000, R828D_XTAL_HZ, Tuner::R828D).unwrap();
        assert!(p.nint > 50);
        assert!((p.lo_hz(16e6) - 223_689_000.0).abs() < 60.0);
    }

    #[test]
    fn every_offered_gain_is_reached_exactly() {
        for g in GAINS_DB10 {
            let (lna, mix, total) = gain_indices(g);
            assert_eq!(total, i32::from(g), "{g}");
            assert!(lna <= 15 && mix <= 15);
        }
        assert_eq!(gain_indices(0), (0, 0, 0));
        assert_eq!(gain_indices(496), (15, 14, 496));
        assert_eq!(RtlGains::nearest_step(400), 402);
        assert_eq!(RtlGains::parse("40").unwrap().db10, 402);
        assert!(RtlGains::parse("agc").unwrap().agc);
        assert!(RtlGains::parse("60").is_err());
    }

    #[test]
    fn if_register_and_correction() {
        // 1.625 MHz on 28.8 MHz: −1.625/28.8 · 2²² = −236 657.
        assert_eq!(if_register(1_625_000, 28.8e6), -236_657);
        assert_eq!(sample_correction(0.0), 0);
        // 10 ppm high: −10 · 2²⁴ / 10⁶ = −167.
        assert_eq!(sample_correction(10.0), -167);
    }

    #[test]
    fn u8_samples_are_centred_and_scaled_as_cu8() {
        let mut out = Vec::new();
        U8ToIq.process(&[0, 255, 127, 128, 9], &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], Iq::new(-127.5 / 128.0, 127.5 / 128.0));
        assert_eq!(out[1], Iq::new(-0.5 / 128.0, 0.5 / 128.0));
    }

    #[test]
    fn opens_an_r820t_and_tunes_11d() {
        let d = opened(R820T_I2C_ADDR, false);
        assert_eq!(d.tuner(), Tuner::R820T);
        assert!(d.locked());
        let m = d.bus();
        // The tuner's PLL registers hold what the sums say for 11D + IF.
        let p = Pll::new(222_064_000 + 1_625_000, RTL_XTAL_HZ, Tuner::R820T).unwrap();
        assert_eq!(m.tuner[0x14], p.reg_14());
        assert_eq!(m.tuner[0x15], p.sdm as u8);
        assert_eq!(m.tuner[0x16], (p.sdm >> 8) as u8);
        // Fine tune 2 = the R820T's power reference: the divider as computed.
        assert_eq!(m.tuner[0x10] >> 5, p.div_num);
        // Band III's RF filters (LO 223.7 MHz: the 220 MHz row).
        assert_eq!(m.tuner[0x1b], 0x13);
        // The IF filter for 2.048 MS/s; the calibration code in reg 0x0a.
        assert_eq!(m.tuner[0x0b] & 0xef, 0xaf);
        // (Setting the rate's filter clears bit 4, as librtlsdr does.)
        assert_eq!(m.tuner[0x0a] & 0x1f, 5);
        // Manual gain 40.2 dB: LNA and mixer indices, the VGA fixed.
        let (lna, mix, _) = gain_indices(402);
        assert_eq!(m.tuner[0x05] & 0x1f, 0x10 | lna);
        assert_eq!(m.tuner[0x07] & 0x1f, mix);
        assert_eq!(m.tuner[0x0c] & 0x9f, 0x08);
        // The demodulator: inversion on, the resampler ratio, the IF.
        let w = m.demod_writes();
        assert!(w.contains(&(1, 0x15, 0x01)));
        assert!(w.contains(&(1, 0x9f, 0x0384)));
        assert!(w.contains(&(1, 0xa1, 0x0000)));
        let v = if_register(1_625_000, 28.8e6);
        assert!(w.contains(&(1, 0x1b, (v & 0xff) as u16)));
        // The filter calibration and the LNA settling were waited for.
        assert!(m.slept >= Duration::from_millis(250));
    }

    #[test]
    fn retune_and_ppm_move_the_pll() {
        let mut d = opened(R820T_I2C_ADDR, false);
        d.set_center(225_648_000).unwrap();
        let p = Pll::new(225_648_000 + 1_625_000, RTL_XTAL_HZ, Tuner::R820T).unwrap();
        assert_eq!(d.bus().tuner[0x15], p.sdm as u8);
        d.set_ppm(25.0).unwrap();
        let ref_hz = corrected(RTL_XTAL_HZ, 25.0) as u32;
        let p = Pll::new(225_648_000 + 1_625_000, ref_hz, Tuner::R820T).unwrap();
        assert_eq!(d.bus().tuner[0x15], p.sdm as u8);
        let offs = sample_correction(25.0);
        assert!(
            d.bus()
                .demod_writes()
                .contains(&(1, 0x3f, (offs & 0xff) as u16))
        );
        assert!(d.set_center(10_000_000).is_err());
    }

    #[test]
    fn agc_puts_the_stages_on_auto() {
        let mut d = opened(R820T_I2C_ADDR, false);
        d.set_gains(RtlGains { agc: true, db10: 0 }).unwrap();
        assert_eq!(d.bus().tuner[0x05] & 0x10, 0);
        assert_eq!(d.bus().tuner[0x07] & 0x10, 0x10);
        assert_eq!(d.bus().tuner[0x0c] & 0x9f, 0x0b);
    }

    #[test]
    fn r828d_inputs_by_band() {
        // A plain R828D: cable 1 below 345 MHz.
        let mut d = opened(R828D_I2C_ADDR, false);
        assert_eq!(d.tuner(), Tuner::R828D);
        assert_eq!(d.bus().tuner[0x05] & 0x60, 0x60);
        d.set_center(500_000_000).unwrap();
        assert_eq!(d.bus().tuner[0x05] & 0x60, 0x00);
        // A Blog V4: the VHF input and its notches open in Band III.
        let d = opened(R828D_I2C_ADDR, true);
        let t = d.bus().tuner;
        assert_eq!(t[0x05] & 0x60, 0x40 | 0x20);
        assert_eq!(t[0x06] & 0x08, 0);
        assert_eq!(t[0x17] & 0x08, 0);
    }

    #[test]
    fn an_unknown_tuner_is_refused() {
        let e = Rtl2832::open(Mock::new(0x00), false).err().unwrap();
        assert!(e.to_string().contains("R820T"), "{e}");
    }

    #[test]
    fn close_puts_the_tuner_to_sleep_and_powers_down() {
        let mut d = opened(R820T_I2C_ADDR, false);
        d.close().unwrap();
        assert_eq!(d.bus().tuner[0x06], 0xb1);
        assert_eq!(d.bus().sys.get(&reg::DEMOD_CTL), Some(&0x20));
    }

    /// The levels written to GPO (the bias-tee pin is bit 0), in order.
    fn gpo_writes(d: &Rtl2832<Mock>) -> Vec<u8> {
        d.bus()
            .log
            .iter()
            .filter(|(v, i, _)| *v == reg::GPO && *i == (block::SYS << 8) | 0x10)
            .map(|(_, _, data)| data[0])
            .collect()
    }

    #[test]
    fn the_bias_tee_is_off_unless_asked() {
        let mut d = opened(R820T_I2C_ADDR, true);
        d.set_center(100_000_000).unwrap();
        d.set_ppm(-3.0).unwrap();
        d.close().unwrap();
        let gpo = gpo_writes(&d);
        assert!(!gpo.is_empty(), "the pin is driven low at open");
        assert!(gpo.iter().all(|b| b & 1 == 0), "{gpo:?}");
        // GPIO 0 is an output (so the low level holds).
        assert_eq!(d.bus().sys[&reg::GPOE] & 1, 1);
        assert_eq!(d.bus().sys[&reg::GPD] & 1, 0);
    }

    #[test]
    fn the_bias_tee_goes_on_when_asked_and_off_at_close() {
        let mut d = Rtl2832::open(Mock::new(R820T_I2C_ADDR), true).expect("open");
        d.start(&RtlSettings {
            center_hz: 222_064_000,
            bias_tee: true,
            ..RtlSettings::default()
        })
        .expect("start");
        assert_eq!(
            gpo_writes(&d).last().map(|b| b & 1),
            Some(1),
            "on after start"
        );
        assert!(d.settings().bias_tee);
        d.set_bias_tee(false).unwrap();
        assert_eq!(gpo_writes(&d).last().map(|b| b & 1), Some(0));
        d.set_bias_tee(true).unwrap();
        d.close().unwrap();
        assert_eq!(
            gpo_writes(&d).last().map(|b| b & 1),
            Some(0),
            "off after close"
        );
        assert!(!d.settings().bias_tee);
    }
}
