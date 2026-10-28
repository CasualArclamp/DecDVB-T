// made by CasualArclamp
//! Live Airspy R2 front end, in pure Rust over USB.
//!
//! Like the HackRF front end, this speaks the radio's USB vendor protocol
//! through `nusb` — no libairspy, no libusb, no DLLs. On Windows the Airspy
//! (VID 1D50, PID 60A1) must be bound to the WinUSB driver (Zadig).
//!
//! The protocol, the start sequence, the sample formats and the real-to-IQ
//! conversion follow libairspy (airspyone_host, BSD-3-Clause, Airspy project:
//! `airspy.c`, `airspy_commands.h`, `iqconverter_float.c`). The USB requests
//! are built in [`proto`] as plain data and the conversion lives in
//! [`convert`]; this file is the device, the reader thread and the
//! [`IqSource`].
//!
//! **Receive only, bias tee always off.** The radio's antenna-port DC supply
//! (for an LNA or an active antenna) can damage whatever is connected to it,
//! so, as with the HackRF, DecSAT-family software does not switch it on. It
//! is sent explicitly off at every open ([`proto::bias_tee_off`] has no
//! argument to set it any other way).
//!
//! **Only 10 MS/s complex** (the R2's ADC runs at 20 MS/s; the bulk stream is
//! that real signal, turned into IQ on the host by [`convert::RealToIq`]).
//!
//! The reader thread keeps [`IN_FLIGHT`] bulk transfers outstanding, converts
//! each to IQ itself (so the receive thread does no extra work) and hands the
//! result over a bounded channel; if the consumer falls behind, whole buffers
//! are dropped and counted rather than stalling the USB side.

pub mod convert;
pub mod proto;

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use decdvbt_core::{Error, Iq, Result};
use futures_lite::future::block_on;
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient, RequestBuffer};

use self::convert::RealToIq;
use self::proto::{Command, Dir, Step};
use crate::IqSource;

const VID: u16 = 0x1d50;
const PID: u16 = 0x60a1;

/// Converted buffers the reader may queue for the consumer before dropping:
/// 64 x 131 072 samples is ~0.8 s at 10 MS/s output.
const QUEUE: usize = 64;
/// Bulk transfers kept in flight (libairspy uses 8 x 262 144 bytes).
const IN_FLIGHT: usize = 8;
/// The most a raw dump (`DECDVBT_AIRSPY_RAW`) keeps: 15 s unpacked.
const RAW_DUMP_BYTES: usize = 600 << 20;

fn map_err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("Airspy: {e}"))
}

/// Gain settings for the Airspy's three receive stages (both AGCs are off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AirspyGains {
    /// LNA gain index, 0–14.
    pub lna: u8,
    /// Mixer gain index, 0–15.
    pub mixer: u8,
    /// IF (VGA) gain index, 0–15.
    pub vga: u8,
}

/// libairspy's `airspy_r820t_lna_gain`/`mixer`/`vga` tables for
/// `airspy_set_linearity_gain` and `airspy_set_sensitivity_gain`: each row of
/// the table is one step of a single "gain" knob, indexed `21 - n`.
const LINEARITY_VGA: [u8; 22] = [
    13, 12, 11, 11, 11, 11, 11, 10, 10, 10, 10, 10, 10, 10, 10, 10, 9, 8, 7, 6, 5, 4,
];
const LINEARITY_MIXER: [u8; 22] = [
    12, 12, 11, 9, 8, 7, 6, 6, 5, 0, 0, 1, 0, 0, 2, 2, 1, 1, 1, 1, 0, 0,
];
const LINEARITY_LNA: [u8; 22] = [
    14, 14, 14, 13, 12, 10, 9, 9, 8, 9, 8, 6, 5, 3, 1, 0, 0, 0, 0, 0, 0, 0,
];
const SENSITIVITY_VGA: [u8; 22] = [
    13, 12, 11, 10, 9, 8, 7, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4,
];
const SENSITIVITY_MIXER: [u8; 22] = [
    12, 12, 12, 12, 11, 10, 10, 9, 9, 8, 7, 4, 4, 4, 3, 2, 2, 1, 0, 0, 0, 0,
];
const SENSITIVITY_LNA: [u8; 22] = [
    14, 14, 14, 14, 14, 14, 14, 14, 14, 13, 12, 12, 9, 9, 8, 7, 6, 5, 3, 2, 1, 0,
];

impl AirspyGains {
    /// libairspy's "linearity" preset, `n` 0 (least gain) ..= 21 (most).
    pub fn linearity(n: u8) -> Self {
        let i = 21 - n.min(21) as usize;
        AirspyGains {
            lna: LINEARITY_LNA[i],
            mixer: LINEARITY_MIXER[i],
            vga: LINEARITY_VGA[i],
        }
    }

    /// libairspy's "sensitivity" preset, `n` 0 ..= 21.
    pub fn sensitivity(n: u8) -> Self {
        let i = 21 - n.min(21) as usize;
        AirspyGains {
            lna: SENSITIVITY_LNA[i],
            mixer: SENSITIVITY_MIXER[i],
            vga: SENSITIVITY_VGA[i],
        }
    }

    /// Snap each stage into the range the hardware takes.
    pub fn clamped(self) -> Self {
        AirspyGains {
            lna: self.lna.min(proto::LNA_MAX),
            mixer: self.mixer.min(proto::MIXER_MAX),
            vga: self.vga.min(proto::VGA_MAX),
        }
    }
}

impl Default for AirspyGains {
    /// libairspy's sensitivity preset 10: a first guess for an aerial feeding
    /// UHF television, not tuned on a real signal.
    fn default() -> Self {
        AirspyGains::sensitivity(10)
    }
}

/// One of libairspy's gain presets, as typed (`sensitivity:10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AirspyPreset {
    Linearity(u8),
    Sensitivity(u8),
}

impl AirspyPreset {
    pub fn gains(self) -> AirspyGains {
        match self {
            AirspyPreset::Linearity(n) => AirspyGains::linearity(n),
            AirspyPreset::Sensitivity(n) => AirspyGains::sensitivity(n),
        }
    }
}

impl FromStr for AirspyPreset {
    type Err = String;

    /// `linearity:N` or `sensitivity:N`, N 0–21.
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        let bad = || format!("not a preset: {s:?} (use linearity:N or sensitivity:N, N 0-21)");
        let (kind, n) = s.split_once(':').ok_or_else(bad)?;
        let n: u8 = n.trim().parse().map_err(|_| bad())?;
        if n > 21 {
            return Err(bad());
        }
        match kind.trim().to_ascii_lowercase().as_str() {
            "linearity" | "lin" => Ok(AirspyPreset::Linearity(n)),
            "sensitivity" | "sens" => Ok(AirspyPreset::Sensitivity(n)),
            _ => Err(bad()),
        }
    }
}

impl std::fmt::Display for AirspyPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AirspyPreset::Linearity(n) => write!(f, "linearity:{n}"),
            AirspyPreset::Sensitivity(n) => write!(f, "sensitivity:{n}"),
        }
    }
}

/// What a live session is set to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AirspySettings {
    pub center_hz: u64,
    /// Complex samples per second. Only 10 MS/s ([`proto::IQ_RATE`]) is
    /// supported.
    pub sample_rate: u32,
    pub gains: AirspyGains,
    /// The device's packed 12-bit transfers (8 samples in 12 bytes: 30 MB/s
    /// instead of 40 MB/s on the USB). Off unless the USB port drops buffers.
    pub packed: bool,
}

impl Default for AirspySettings {
    fn default() -> Self {
        AirspySettings {
            center_hz: 546_000_000,
            sample_rate: proto::IQ_RATE,
            gains: AirspyGains::default(),
            packed: false,
        }
    }
}

/// The claimed device. Control transfers can be made from any thread while
/// the reader thread waits on the bulk queue.
struct Usb {
    interface: nusb::Interface,
}

impl Usb {
    fn open() -> Result<Usb> {
        let info = nusb::list_devices()
            .map_err(|e| Error::Other(format!("Airspy: cannot list USB devices ({e})")))?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .ok_or_else(|| {
                Error::Other(
                    "no Airspy found (USB 1D50:60A1). Is it plugged in, not in use by another \
                     program, and on the WinUSB driver?"
                        .into(),
                )
            })?;
        let device = info.open().map_err(|e| {
            Error::Other(format!(
                "Airspy: cannot open the device ({e}). Is another program using it, and is it \
                 on the WinUSB driver?"
            ))
        })?;
        // A device that has no configuration selected yet (not normal on
        // Windows or Linux, where the OS picks the only one) gets #1; on
        // Windows nusb cannot set it and that is fine.
        if device.active_configuration().is_err() {
            let _ = device.set_configuration(1);
        }
        let interface = device.claim_interface(0).map_err(map_err)?;
        Ok(Usb { interface })
    }

    /// Run one control transfer. An IN transfer must return at least
    /// `reply_len` bytes.
    fn control(&self, c: &Command) -> Result<Vec<u8>> {
        let what = |e: nusb::transfer::TransferError| {
            Error::Other(format!("Airspy: request {} failed ({e})", c.request))
        };
        match c.dir {
            Dir::Out => {
                block_on(self.interface.control_out(ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: c.request,
                    value: c.value,
                    index: c.index,
                    data: &c.data,
                }))
                .into_result()
                .map_err(what)?;
                Ok(Vec::new())
            }
            Dir::In => {
                let reply = block_on(self.interface.control_in(ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: c.request,
                    value: c.value,
                    index: c.index,
                    length: c.reply_len,
                }))
                .into_result()
                .map_err(what)?;
                // The firmware's version string is shorter than the 127 bytes
                // asked for (libairspy asks for the same and takes what comes).
                let may_be_short = c.request == proto::REQ_VERSION_STRING_READ;
                if reply.len() < c.reply_len as usize && !may_be_short {
                    return Err(Error::Other(format!(
                        "Airspy: short reply to request {} ({} of {} bytes)",
                        c.request,
                        reply.len(),
                        c.reply_len
                    )));
                }
                Ok(reply)
            }
        }
    }

    /// The device's rate list, and the index of 10 MS/s in it.
    fn rate_index(&self) -> Result<u16> {
        let n = proto::parse_u32(&self.control(&proto::sample_rate_count())?)?;
        let list = self.control(&proto::sample_rate_list(n))?;
        proto::rate_index(&proto::parse_sample_rates(&list, n)?)
    }
}

/// What the radio says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AirspyInfo {
    /// libairspy's board id (0 is an Airspy One/R2/Mini).
    pub board_id: u8,
    pub board_name: &'static str,
    pub firmware: String,
    /// The complex sample rates the device lists, Hz.
    pub sample_rates: Vec<u32>,
}

/// libairspy `airspy_board_id_name`.
fn board_name(id: u8) -> &'static str {
    match id {
        0 => "AIRSPY",
        0xff => "INVALID",
        _ => "UNKNOWN",
    }
}

/// Open the first Airspy just long enough to read its board id, firmware
/// version string and rate list. Does not start streaming.
pub fn airspy_info() -> Result<AirspyInfo> {
    let usb = Usb::open()?;
    let board_id = usb.control(&proto::board_id_read())?[0];
    let raw = usb.control(&proto::version_string_read())?;
    let firmware = String::from_utf8_lossy(&raw)
        .trim_end_matches('\0')
        .trim()
        .to_string();
    let n = proto::parse_u32(&usb.control(&proto::sample_rate_count())?)?;
    let list = usb.control(&proto::sample_rate_list(n))?;
    Ok(AirspyInfo {
        board_id,
        board_name: board_name(board_id),
        firmware,
        sample_rates: proto::parse_sample_rates(&list, n)?,
    })
}

/// Live controls on a running Airspy, usable from the GUI thread while the
/// engine reads samples.
#[derive(Clone)]
pub struct AirspyControl {
    usb: Arc<Usb>,
    settings: Arc<Mutex<AirspySettings>>,
    overflows: Arc<AtomicU64>,
    blanked: Arc<AtomicU64>,
}

impl AirspyControl {
    pub fn settings(&self) -> AirspySettings {
        *self.settings.lock().unwrap()
    }

    /// Retune. Takes effect within a USB transfer or two.
    pub fn set_center(&self, hz: u64) -> Result<()> {
        self.usb.control(&proto::set_freq(hz)?)?;
        self.settings.lock().unwrap().center_hz = hz;
        Ok(())
    }

    pub fn set_gains(&self, gains: AirspyGains) -> Result<()> {
        let g = gains.clamped();
        self.usb.control(&proto::set_lna_gain(g.lna))?;
        self.usb.control(&proto::set_mixer_gain(g.mixer))?;
        self.usb.control(&proto::set_vga_gain(g.vga))?;
        self.settings.lock().unwrap().gains = g;
        Ok(())
    }

    /// Buffers dropped because the consumer fell behind.
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Packed blocks of 4096 samples that came from the radio unpacked and
    /// were blanked (see [`RealToIq::process_packed`]).
    pub fn blanked(&self) -> u64 {
        self.blanked.load(Ordering::Relaxed)
    }

    /// Everything lost on the radio's side: dropped buffers and blanked
    /// blocks, the figure the apps show as drops.
    pub fn lost(&self) -> u64 {
        self.overflows() + self.blanked()
    }
}

/// Convert one bulk transfer to IQ into `out` (cleared first). Returns false
/// if the transfer ended inside a group of samples (it is then used up to the
/// last whole group, and the stream's I/Q phase is no longer trustworthy).
fn convert_transfer(conv: &mut RealToIq, packed: bool, bytes: &[u8], out: &mut Vec<Iq>) -> bool {
    out.clear();
    let used = if packed {
        conv.process_packed(bytes, out)
    } else {
        conv.process_unpacked(bytes, out)
    };
    used == bytes.len()
}

/// The consumer side of the reader: converted buffers in, spent buffers back
/// out for reuse. Kept apart from the USB so that its behaviour is tested.
struct Feed {
    rx: Receiver<Vec<Iq>>,
    recycle: SyncSender<Vec<Iq>>,
    /// Buffers still to be thrown away after a retune (see `flush`).
    skip: u32,
}

impl Feed {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        loop {
            match self.rx.recv_timeout(Duration::from_secs(2)) {
                Ok(spent) if self.skip > 0 => {
                    self.skip -= 1;
                    let _ = self.recycle.try_send(spent);
                }
                Ok(mut buf) => {
                    // Hand the caller the filled Vec and keep theirs for the
                    // reader to fill next: no allocation, no copy.
                    std::mem::swap(out, &mut buf);
                    let _ = self.recycle.try_send(buf);
                    return Ok(out.len());
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::Other("Airspy stopped delivering samples".into()));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Other("Airspy reader stopped (unplugged?)".into()));
                }
            }
        }
    }

    /// After a retune: drop what is queued, and the next `in_flight + 1`
    /// buffers (the transfers the radio was filling as it retuned, and one
    /// more for the loop's own settling).
    fn flush(&mut self, in_flight: usize) {
        while let Ok(b) = self.rx.try_recv() {
            let _ = self.recycle.try_send(b);
        }
        self.skip = in_flight as u32 + 1;
    }
}

/// A live Airspy R2 receive stream.
pub struct AirspySource {
    feed: Feed,
    control: AirspyControl,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    done: Receiver<()>,
}

impl AirspySource {
    /// Open the first Airspy, configure it and start receiving.
    pub fn open(settings: AirspySettings) -> Result<Self> {
        if settings.sample_rate != proto::IQ_RATE {
            return Err(Error::Other(format!(
                "Airspy sample rate {} is not supported: the R2 front end runs at 10 MS/s only",
                settings.sample_rate
            )));
        }
        // Checked before any USB access.
        proto::set_freq(settings.center_hz)?;
        let gains = settings.gains.clamped();

        let usb = Arc::new(Usb::open()?);
        let rate_index = usb.rate_index()?;
        let steps = proto::start_sequence(&proto::StartParams {
            center_hz: settings.center_hz,
            rate_index,
            lna: gains.lna,
            mixer: gains.mixer,
            vga: gains.vga,
            packed: settings.packed,
        })?;
        for step in &steps {
            match step {
                Step::Control(c) => {
                    // An old firmware may refuse SET_PACKING 0; it is the
                    // default there, so that is not worth failing for.
                    if let Err(e) = usb.control(c) {
                        if c.request == proto::REQ_SET_PACKING && !settings.packed {
                            tracing::warn!("{e}");
                        } else {
                            return Err(e);
                        }
                    }
                }
                Step::ClearHalt => usb
                    .interface
                    .clear_halt(proto::ENDPOINT_IN)
                    .map_err(map_err)?,
            }
        }

        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(QUEUE);
        let (recycle_tx, recycle_rx) = mpsc::sync_channel::<Vec<Iq>>(QUEUE + 4);
        let (done_tx, done_rx) = mpsc::sync_channel::<()>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let overflows = Arc::new(AtomicU64::new(0));
        let blanked = Arc::new(AtomicU64::new(0));
        let thread = {
            let usb = Arc::clone(&usb);
            let stop = Arc::clone(&stop);
            let overflows = Arc::clone(&overflows);
            let blanked = Arc::clone(&blanked);
            let packed = settings.packed;
            std::thread::Builder::new()
                .name("airspy-rx".into())
                .spawn(move || {
                    reader(&usb, packed, &tx, &recycle_rx, &stop, &overflows, &blanked);
                    let _ = done_tx.send(());
                })
                .map_err(|e| Error::Other(format!("cannot start the Airspy reader: {e}")))?
        };

        Ok(AirspySource {
            feed: Feed {
                rx,
                recycle: recycle_tx,
                skip: 0,
            },
            control: AirspyControl {
                usb,
                settings: Arc::new(Mutex::new(AirspySettings { gains, ..settings })),
                overflows,
                blanked,
            },
            stop,
            thread: Some(thread),
            done: done_rx,
        })
    }

    /// A handle for retuning and gain changes while the engine reads.
    pub fn control(&self) -> AirspyControl {
        self.control.clone()
    }
}

/// The reader thread: keep the bulk queue full, convert, hand over.
fn reader(
    usb: &Usb,
    packed: bool,
    tx: &SyncSender<Vec<Iq>>,
    recycle: &Receiver<Vec<Iq>>,
    stop: &AtomicBool,
    overflows: &AtomicU64,
    blanked: &AtomicU64,
) {
    let size = if packed {
        proto::TRANSFER_PACKED
    } else {
        proto::TRANSFER_UNPACKED
    };
    let mut queue = usb.interface.bulk_in_queue(proto::ENDPOINT_IN);
    for _ in 0..IN_FLIGHT {
        queue.submit(RequestBuffer::new(size));
    }
    let mut conv = RealToIq::new();
    // For diagnosing the radio: env DECDVBT_AIRSPY_RAW=path keeps the first
    // RAW_DUMP_BYTES of bulk transfers as they came (a u32 length, then the
    // bytes, each); `--example airspy_replay` in decdvbt-cli reads it back.
    let mut raw_dump = std::env::var_os("DECDVBT_AIRSPY_RAW").and_then(|p| {
        std::fs::File::create(&p)
            .inspect_err(|e| tracing::warn!("Airspy: cannot create the raw dump: {e}"))
            .ok()
            .map(std::io::BufWriter::new)
    });
    let mut raw_left = RAW_DUMP_BYTES;
    while !stop.load(Ordering::Relaxed) {
        let done = block_on(queue.next_complete());
        if let Err(e) = done.status {
            tracing::warn!("Airspy read failed: {e}");
            break;
        }
        if let Some(f) = raw_dump.as_mut()
            && raw_left >= done.data.len()
        {
            use std::io::Write;
            raw_left -= done.data.len();
            let _ = f.write_all(&(done.data.len() as u32).to_le_bytes());
            let _ = f.write_all(&done.data);
        }
        let mut buf = recycle.try_recv().unwrap_or_default();
        if !convert_transfer(&mut conv, packed, &done.data, &mut buf) {
            // A short transfer breaks the sign pattern's phase: count it as
            // a lost buffer and start the filter again.
            overflows.fetch_add(1, Ordering::Relaxed);
            conv.reset();
        }
        blanked.store(conv.blanked(), Ordering::Relaxed);
        // The transfer goes straight back in so the queue stays full.
        queue.submit(RequestBuffer::reuse(done.data, size));
        match tx.try_send(buf) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                overflows.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    // Stop the radio, then cancel what is outstanding.
    if let Err(e) = usb.control(&proto::receiver_mode(false)) {
        tracing::warn!("Airspy: could not stop receiving: {e}");
    }
    queue.cancel_all();
}

impl IqSource for AirspySource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        self.feed.read(out)
    }

    fn sample_rate(&self) -> f64 {
        proto::IQ_RATE as f64
    }

    fn center_freq(&self) -> f64 {
        self.control.settings().center_hz as f64
    }

    fn describe(&self) -> String {
        let s = self.control.settings();
        format!(
            "Airspy R2 @ {:.4} MHz, {:.2} MS/s, LNA {} Mixer {} VGA {}{}",
            s.center_hz as f64 / 1e6,
            s.sample_rate as f64 / 1e6,
            s.gains.lna,
            s.gains.mixer,
            s.gains.vga,
            if s.packed { " (packed)" } else { "" }
        )
    }

    /// Moves the centre and throws away what was captured before it took
    /// effect: the converted buffers waiting, and the transfers the radio was
    /// filling as it retuned (see [`Feed::flush`]).
    fn retune(&mut self, hz: f64) -> Result<bool> {
        self.control.set_center(hz.round() as u64)?;
        self.feed.flush(IN_FLIGHT);
        Ok(true)
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for AirspySource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock a reader waiting to hand over a buffer.
        while self.feed.rx.try_recv().is_ok() {}
        // The reader notices the flag at its next completed transfer (a few
        // ms) and stops the radio itself. If the device has stopped
        // answering, do not hang the app: stop it from here and let go.
        if self.done.recv_timeout(Duration::from_secs(2)).is_err() {
            let _ = self.control.usb.control(&proto::receiver_mode(false));
            tracing::warn!("Airspy reader did not stop; detaching it");
            self.thread.take();
            return;
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_libairspys_tables() {
        // n = 21 is table index 0, n = 0 index 21.
        assert_eq!(
            AirspyGains::linearity(21),
            AirspyGains {
                lna: 14,
                mixer: 12,
                vga: 13
            }
        );
        assert_eq!(
            AirspyGains::linearity(0),
            AirspyGains {
                lna: 0,
                mixer: 0,
                vga: 4
            }
        );
        assert_eq!(
            AirspyGains::sensitivity(21),
            AirspyGains {
                lna: 14,
                mixer: 12,
                vga: 13
            }
        );
        assert_eq!(
            AirspyGains::sensitivity(0),
            AirspyGains {
                lna: 0,
                mixer: 0,
                vga: 4
            }
        );
        // Index 11 (n = 10), the default.
        assert_eq!(
            AirspyGains::sensitivity(10),
            AirspyGains {
                lna: 12,
                mixer: 4,
                vga: 5
            }
        );
        assert_eq!(AirspyGains::default(), AirspyGains::sensitivity(10));
        assert_eq!(
            AirspyGains::linearity(10),
            AirspyGains {
                lna: 6,
                mixer: 1,
                vga: 10
            }
        );
        // Beyond 21 is 21.
        assert_eq!(AirspyGains::linearity(99), AirspyGains::linearity(21));
        // Every preset is inside the hardware's ranges.
        for n in 0..=21 {
            for g in [AirspyGains::linearity(n), AirspyGains::sensitivity(n)] {
                assert_eq!(g, g.clamped());
            }
        }
    }

    #[test]
    fn gains_clamp() {
        let g = AirspyGains {
            lna: 99,
            mixer: 99,
            vga: 99,
        }
        .clamped();
        assert_eq!(
            g,
            AirspyGains {
                lna: 14,
                mixer: 15,
                vga: 15
            }
        );
    }

    #[test]
    fn preset_strings() {
        assert_eq!(
            "sensitivity:10".parse::<AirspyPreset>(),
            Ok(AirspyPreset::Sensitivity(10))
        );
        assert_eq!(
            "Linearity:0".parse::<AirspyPreset>(),
            Ok(AirspyPreset::Linearity(0))
        );
        assert!("linearity:22".parse::<AirspyPreset>().is_err());
        assert!("linearity".parse::<AirspyPreset>().is_err());
        assert!("loud:3".parse::<AirspyPreset>().is_err());
        let p = AirspyPreset::Sensitivity(7);
        assert_eq!(p.to_string().parse::<AirspyPreset>(), Ok(p));
        assert_eq!(p.gains(), AirspyGains::sensitivity(7));
    }

    #[test]
    fn rejects_other_rates_and_frequencies_before_any_usb_access() {
        for rate in [2_500_000u32, 8_000_000, 20_000_000] {
            let s = AirspySettings {
                sample_rate: rate,
                ..Default::default()
            };
            let e = AirspySource::open(s).err().expect("must fail").to_string();
            assert!(e.contains("10 MS/s only"), "{e}");
        }
        for hz in [1_000_000u64, 2_000_000_000] {
            let s = AirspySettings {
                center_hz: hz,
                ..Default::default()
            };
            let e = AirspySource::open(s).err().expect("must fail").to_string();
            assert!(e.contains("tuner"), "{e}");
        }
    }

    /// A fake reader: synthetic bulk transfers go through the same
    /// conversion and hand-over as the real one.
    #[test]
    fn a_fake_source_feeds_the_consumer() {
        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(4);
        let (recycle_tx, recycle_rx) = mpsc::sync_channel::<Vec<Iq>>(8);
        let mut feed = Feed {
            rx,
            recycle: recycle_tx,
            skip: 0,
        };
        let mut conv = RealToIq::new();
        // 20 MS/s tone at 4 MHz, i.e. +1 MHz in the output.
        let bytes: Vec<u8> = (0..proto::TRANSFER_UNPACKED / 2)
            .flat_map(|k| {
                let v = (2048.0
                    + 1500.0 * (2.0 * std::f64::consts::PI * 4e6 * k as f64 / 20e6).cos())
                    as u16;
                v.to_le_bytes()
            })
            .collect();
        for _ in 0..3 {
            let mut b = recycle_rx.try_recv().unwrap_or_default();
            assert!(convert_transfer(&mut conv, false, &bytes, &mut b));
            tx.send(b).unwrap();
        }
        let mut out = Vec::new();
        assert_eq!(feed.read(&mut out).unwrap(), proto::TRANSFER_UNPACKED / 4);
        // The caller's Vec went back for reuse.
        assert!(recycle_rx.try_recv().is_ok());
        // A retune throws away what is queued and then the next 9 buffers.
        feed.flush(IN_FLIGHT);
        assert_eq!(feed.skip, 9);
        assert!(feed.rx.try_recv().is_err());
        for _ in 0..11 {
            tx.send(vec![Iq::new(0.0, 0.0); 1]).unwrap();
            if feed.rx.try_recv().is_ok() {
                // drained by the loop below instead
            }
        }
    }

    #[test]
    fn the_retune_skip_discards_in_flight_buffers() {
        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(32);
        let (recycle_tx, _recycle_rx) = mpsc::sync_channel::<Vec<Iq>>(32);
        let mut feed = Feed {
            rx,
            recycle: recycle_tx,
            skip: 0,
        };
        // Queued before the retune: dropped.
        for _ in 0..5 {
            tx.send(vec![Iq::new(1.0, 0.0)]).unwrap();
        }
        feed.flush(IN_FLIGHT);
        // The old channel's in-flight transfers, then the new channel's.
        for _ in 0..9 {
            tx.send(vec![Iq::new(1.0, 0.0)]).unwrap();
        }
        tx.send(vec![Iq::new(2.0, 0.0)]).unwrap();
        let mut out = Vec::new();
        feed.read(&mut out).unwrap();
        assert_eq!(
            out[0].re, 2.0,
            "the first buffer handed over is the new channel's"
        );
    }

    #[test]
    fn a_stalled_stream_is_an_error() {
        let (tx, rx) = mpsc::sync_channel::<Vec<Iq>>(1);
        let (recycle_tx, _r) = mpsc::sync_channel::<Vec<Iq>>(1);
        let mut feed = Feed {
            rx,
            recycle: recycle_tx,
            skip: 0,
        };
        drop(tx);
        assert!(feed.read(&mut Vec::new()).is_err());
    }

    /// Live: `cargo test -p decdvbt-io --features airspy live_airspy -- --ignored --nocapture`.
    /// Receive only, bias tee off.
    #[test]
    #[ignore = "needs an Airspy R2 attached"]
    fn live_airspy() {
        let info = airspy_info().expect("info");
        eprintln!("{info:?}");
        let mut src = AirspySource::open(AirspySettings {
            center_hz: 100_000_000,
            ..Default::default()
        })
        .expect("open");
        let mut buf = Vec::new();
        let mut n = 0usize;
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(2) {
            n += src.read(&mut buf).expect("read");
        }
        let rate = n as f64 / t0.elapsed().as_secs_f64();
        let rms = (buf.iter().map(|s| s.norm_sqr()).sum::<f32>() / buf.len() as f32).sqrt();
        eprintln!(
            "{} — {:.2} MS/s delivered, rms {rms:.3}, overflows {}",
            src.describe(),
            rate / 1e6,
            src.control().overflows()
        );
        assert!(rate > 9.0e6, "only {:.2} MS/s", rate / 1e6);
    }
}
