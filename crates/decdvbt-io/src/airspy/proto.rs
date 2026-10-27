// made by CasualArclamp
//! The Airspy's USB vendor protocol as pure data: every request is built here
//! as a [`Command`] (request, value, index, payload) and nothing in this file
//! touches USB, so the encoding is unit-tested without a radio.
//!
//! Protocol and sequence after libairspy's `airspy.c` and `airspy_commands.h`
//! (airspyone_host, BSD-3-Clause, Airspy project). All requests are
//! Vendor | Device.
//!
//! **Bias tee: never on.** The only command that touches the GPIO the bias tee
//! hangs off is [`bias_tee_off`], and it has no argument: the value is the
//! constant 0. [`start_sequence`] sends it on every open, and a test checks
//! that no command any function here can build ever carries a 1 there.

use decdvbt_core::{Error, Result};

/// `bRequest` codes (libairspy `airspy_vendor_request`).
pub const REQ_RECEIVER_MODE: u8 = 1;
pub const REQ_BOARD_ID_READ: u8 = 9;
pub const REQ_VERSION_STRING_READ: u8 = 10;
pub const REQ_SET_SAMPLERATE: u8 = 12;
pub const REQ_SET_FREQ: u8 = 13;
pub const REQ_SET_LNA_GAIN: u8 = 14;
pub const REQ_SET_MIXER_GAIN: u8 = 15;
pub const REQ_SET_VGA_GAIN: u8 = 16;
pub const REQ_SET_LNA_AGC: u8 = 17;
pub const REQ_SET_MIXER_AGC: u8 = 18;
pub const REQ_GPIO_WRITE: u8 = 21;
pub const REQ_GET_SAMPLERATES: u8 = 25;
pub const REQ_SET_PACKING: u8 = 26;

/// libairspy `airspy_gpio_write(GPIO_PORT1, GPIO_PIN13, ..)`: `wIndex` is
/// `(port << 5) | pin` = 45. Port 1 pin 13 is the antenna bias tee.
pub const GPIO_BIAS_TEE_INDEX: u16 = (1 << 5) | 13;

/// The tuner's range, Hz (libairspy: `AIRSPY_FREQ_MIN/MAX` 24 MHz – 1.75 GHz in
/// the R2's firmware; 1.8 GHz is the upper bound the R820T takes).
pub const FREQ_MIN_HZ: u64 = 24_000_000;
pub const FREQ_MAX_HZ: u64 = 1_800_000_000;

/// Gain stage limits (libairspy: LNA 0–14, mixer 0–15, VGA 0–15).
pub const LNA_MAX: u8 = 14;
pub const MIXER_MAX: u8 = 15;
pub const VGA_MAX: u8 = 15;

/// The one sample rate this front end supports, complex samples per second.
/// The ADC runs at twice this and the bulk stream is a real 20 MS/s signal.
pub const IQ_RATE: u32 = 10_000_000;

/// Bulk IN endpoint carrying the samples.
pub const ENDPOINT_IN: u8 = 0x81;
/// Bytes per bulk transfer, unpacked: 131 072 samples of 16 bits.
pub const TRANSFER_UNPACKED: usize = 262_144;
/// Packed: 8 samples in 12 bytes, 98 304 samples.
pub const TRANSFER_PACKED: usize = 147_456;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Host to device: `data` is the payload.
    Out,
    /// Device to host: `reply_len` bytes are read back.
    In,
}

/// One control transfer: what libairspy hands `libusb_control_transfer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub dir: Dir,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    /// OUT payload (empty for none).
    pub data: Vec<u8>,
    /// IN: bytes to read back (the device's reply must be at least this long).
    pub reply_len: u16,
}

impl Command {
    fn out(request: u8, value: u16, index: u16, data: Vec<u8>) -> Command {
        Command {
            dir: Dir::Out,
            request,
            value,
            index,
            data,
            reply_len: 0,
        }
    }

    fn inp(request: u8, value: u16, index: u16, reply_len: u16) -> Command {
        Command {
            dir: Dir::In,
            request,
            value,
            index,
            data: Vec::new(),
            reply_len,
        }
    }

    /// The "set" requests are IN transfers of one byte whose value is ignored.
    fn set(request: u8, index: u16) -> Command {
        Command::inp(request, 0, index, 1)
    }
}

/// `RECEIVER_MODE`: stream (true) or stop (false).
pub fn receiver_mode(on: bool) -> Command {
    Command::out(REQ_RECEIVER_MODE, on as u16, 0, Vec::new())
}

/// `SET_SAMPLERATE` by index into the device's own rate list.
pub fn set_sample_rate(index: u16) -> Command {
    Command::set(REQ_SET_SAMPLERATE, index)
}

/// `SET_FREQ`: the centre frequency as a little-endian u32 of Hz.
pub fn set_freq(hz: u64) -> Result<Command> {
    if !(FREQ_MIN_HZ..=FREQ_MAX_HZ).contains(&hz) {
        return Err(Error::Other(format!(
            "Airspy: {:.3} MHz is outside the tuner's 24–1800 MHz",
            hz as f64 / 1e6
        )));
    }
    Ok(Command::out(
        REQ_SET_FREQ,
        0,
        0,
        (hz as u32).to_le_bytes().to_vec(),
    ))
}

/// Gain requests clamp to the stage's range rather than fail.
pub fn set_lna_gain(v: u8) -> Command {
    Command::set(REQ_SET_LNA_GAIN, v.min(LNA_MAX) as u16)
}
pub fn set_mixer_gain(v: u8) -> Command {
    Command::set(REQ_SET_MIXER_GAIN, v.min(MIXER_MAX) as u16)
}
pub fn set_vga_gain(v: u8) -> Command {
    Command::set(REQ_SET_VGA_GAIN, v.min(VGA_MAX) as u16)
}
pub fn set_lna_agc(on: bool) -> Command {
    Command::set(REQ_SET_LNA_AGC, on as u16)
}
pub fn set_mixer_agc(on: bool) -> Command {
    Command::set(REQ_SET_MIXER_AGC, on as u16)
}

/// `SET_PACKING`: 12-bit samples packed 8 to 12 bytes (true) or one per u16.
pub fn set_packing(on: bool) -> Command {
    Command::set(REQ_SET_PACKING, on as u16)
}

/// Switch the antenna bias tee **off**. Deliberately has no parameter: see the
/// module comment.
pub fn bias_tee_off() -> Command {
    Command::out(REQ_GPIO_WRITE, 0, GPIO_BIAS_TEE_INDEX, Vec::new())
}

pub fn board_id_read() -> Command {
    Command::inp(REQ_BOARD_ID_READ, 0, 0, 1)
}

pub fn version_string_read() -> Command {
    Command::inp(REQ_VERSION_STRING_READ, 0, 0, 127)
}

/// `GET_SAMPLERATES` step 1: the number of rates the device lists.
pub fn sample_rate_count() -> Command {
    Command::inp(REQ_GET_SAMPLERATES, 0, 0, 4)
}

/// Step 2: the rates themselves, 4 bytes each; `wIndex` is the count.
pub fn sample_rate_list(count: u32) -> Command {
    Command::inp(REQ_GET_SAMPLERATES, 0, count as u16, (count * 4) as u16)
}

/// A little-endian u32 from the start of a reply.
pub fn parse_u32(reply: &[u8]) -> Result<u32> {
    reply
        .first_chunk::<4>()
        .map(|b| u32::from_le_bytes(*b))
        .ok_or_else(|| Error::Other("Airspy: short reply".into()))
}

/// The device's rate list: `count` u32s, little-endian.
pub fn parse_sample_rates(reply: &[u8], count: u32) -> Result<Vec<u32>> {
    let n = count as usize;
    if n > 64 || reply.len() < n * 4 {
        return Err(Error::Other(format!(
            "Airspy: bad sample rate list ({count} rates, {} bytes)",
            reply.len()
        )));
    }
    let (chunks, _) = reply[..n * 4].as_chunks::<4>();
    Ok(chunks.iter().map(|c| u32::from_le_bytes(*c)).collect())
}

/// The index of [`IQ_RATE`] in the device's list (the R2 lists 10 MS/s and
/// 2.5 MS/s).
pub fn rate_index(rates: &[u32]) -> Result<u16> {
    rates
        .iter()
        .position(|&r| r == IQ_RATE)
        .map(|i| i as u16)
        .ok_or_else(|| {
            Error::Other(format!(
                "Airspy: this device does not offer 10 MS/s (it lists {rates:?})"
            ))
        })
}

/// One step of starting the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Control(Command),
    /// Clear a halt/stall on the bulk IN endpoint (libusb `clear_halt`).
    ClearHalt,
}

/// What to program. `gains` are already clamped by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartParams {
    pub center_hz: u64,
    pub rate_index: u16,
    pub lna: u8,
    pub mixer: u8,
    pub vga: u8,
    pub packed: bool,
}

/// The start sequence of libairspy's `airspy_open` + `airspy_start_rx`: rate,
/// frequency, gains with both AGCs off, bias tee off, receiver off, clear
/// halt, receiver on. (Packing is set before the rate, as libairspy's callers
/// do between open and start.)
pub fn start_sequence(p: &StartParams) -> Result<Vec<Step>> {
    use Step::{ClearHalt, Control};
    Ok(vec![
        Control(set_packing(p.packed)),
        ClearHalt,
        Control(set_sample_rate(p.rate_index)),
        Control(set_freq(p.center_hz)?),
        Control(set_lna_agc(false)),
        Control(set_mixer_agc(false)),
        Control(set_lna_gain(p.lna)),
        Control(set_mixer_gain(p.mixer)),
        Control(set_vga_gain(p.vga)),
        Control(bias_tee_off()),
        Control(receiver_mode(false)),
        ClearHalt,
        Control(receiver_mode(true)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_is_a_little_endian_u32() {
        let c = set_freq(546_000_000).unwrap();
        assert_eq!(c.dir, Dir::Out);
        assert_eq!((c.request, c.value, c.index), (13, 0, 0));
        assert_eq!(c.data, 546_000_000u32.to_le_bytes());
        assert!(set_freq(23_999_999).is_err());
        assert!(set_freq(1_800_000_001).is_err());
        assert!(set_freq(24_000_000).is_ok());
    }

    #[test]
    fn set_requests_are_one_byte_in_transfers() {
        for (c, req, idx) in [
            (set_sample_rate(1), 12, 1),
            (set_lna_gain(7), 14, 7),
            (set_mixer_gain(7), 15, 7),
            (set_vga_gain(7), 16, 7),
            (set_lna_agc(true), 17, 1),
            (set_mixer_agc(false), 18, 0),
            (set_packing(true), 26, 1),
        ] {
            assert_eq!(c.dir, Dir::In);
            assert_eq!((c.request, c.value, c.index, c.reply_len), (req, 0, idx, 1));
            assert!(c.data.is_empty());
        }
    }

    #[test]
    fn gain_requests_clamp() {
        assert_eq!(set_lna_gain(200).index, 14);
        assert_eq!(set_mixer_gain(200).index, 15);
        assert_eq!(set_vga_gain(16).index, 15);
        assert_eq!(set_lna_gain(0).index, 0);
    }

    #[test]
    fn receiver_mode_encoding() {
        let on = receiver_mode(true);
        let off = receiver_mode(false);
        assert_eq!(
            (on.dir, on.request, on.value, on.index),
            (Dir::Out, 1, 1, 0)
        );
        assert_eq!((off.request, off.value, off.index), (1, 0, 0));
        assert!(on.data.is_empty() && off.data.is_empty());
    }

    #[test]
    fn rate_list_steps() {
        let c = sample_rate_count();
        assert_eq!(
            (c.dir, c.request, c.value, c.index, c.reply_len),
            (Dir::In, 25, 0, 0, 4)
        );
        let l = sample_rate_list(2);
        assert_eq!((l.request, l.index, l.reply_len), (25, 2, 8));
    }

    #[test]
    fn parses_the_r2_rate_list() {
        let mut reply = Vec::new();
        reply.extend(10_000_000u32.to_le_bytes());
        reply.extend(2_500_000u32.to_le_bytes());
        assert_eq!(parse_u32(&2u32.to_le_bytes()).unwrap(), 2);
        let rates = parse_sample_rates(&reply, 2).unwrap();
        assert_eq!(rates, [10_000_000, 2_500_000]);
        assert_eq!(rate_index(&rates).unwrap(), 0);
        // The order is the device's: find 10 MS/s wherever it is.
        assert_eq!(rate_index(&[2_500_000, 10_000_000]).unwrap(), 1);
        assert!(rate_index(&[2_500_000, 6_000_000]).is_err());
        assert!(parse_sample_rates(&reply, 3).is_err());
        assert!(parse_u32(&[1, 2]).is_err());
    }

    #[test]
    fn bias_tee_is_off_on_every_path() {
        let off = bias_tee_off();
        assert_eq!(off.dir, Dir::Out);
        assert_eq!(off.request, REQ_GPIO_WRITE);
        assert_eq!((off.value, off.index), (0, 45));
        assert!(off.data.is_empty());
        // Every command any start sequence can produce: GPIO writes carry 0.
        for packed in [false, true] {
            for hz in [24_000_000u64, 546_000_000, 1_800_000_000] {
                for g in [0u8, 7, 15, 255] {
                    let seq = start_sequence(&StartParams {
                        center_hz: hz,
                        rate_index: 0,
                        lna: g,
                        mixer: g,
                        vga: g,
                        packed,
                    })
                    .unwrap();
                    let mut gpio = 0;
                    for s in &seq {
                        if let Step::Control(c) = s
                            && c.request == REQ_GPIO_WRITE
                        {
                            gpio += 1;
                            assert_eq!(c.value, 0, "bias tee must stay off");
                            assert_eq!(c.index, 45);
                        }
                    }
                    assert_eq!(gpio, 1);
                }
            }
        }
    }

    #[test]
    fn start_sequence_order() {
        let seq = start_sequence(&StartParams {
            center_hz: 514_000_000,
            rate_index: 0,
            lna: 14,
            mixer: 12,
            vga: 13,
            packed: false,
        })
        .unwrap();
        let reqs: Vec<Option<u8>> = seq
            .iter()
            .map(|s| match s {
                Step::Control(c) => Some(c.request),
                Step::ClearHalt => None,
            })
            .collect();
        // The rate before the frequency; receiver off, clear halt, receiver on last.
        let pos = |r: u8| reqs.iter().position(|x| *x == Some(r)).unwrap();
        assert!(pos(REQ_SET_SAMPLERATE) < pos(REQ_SET_FREQ));
        assert!(pos(REQ_SET_FREQ) < pos(REQ_SET_LNA_GAIN));
        let n = seq.len();
        assert_eq!(seq[n - 1], Step::Control(receiver_mode(true)));
        assert_eq!(seq[n - 2], Step::ClearHalt);
        assert_eq!(seq[n - 3], Step::Control(receiver_mode(false)));
        // Both AGCs explicitly off.
        assert!(seq.contains(&Step::Control(set_lna_agc(false))));
        assert!(seq.contains(&Step::Control(set_mixer_agc(false))));
    }
}
