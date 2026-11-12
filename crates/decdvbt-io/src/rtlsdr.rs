// made by CasualArclamp
//! Live RTL-SDR front end, in pure Rust over USB (from DecLAB's
//! `declab-io/src/rtlsdr.rs`, DecDAB's before it).
//!
//! Puts `rtlsdr_proto`'s [`RtlBus`] on a real dongle through `nusb`, as the
//! Airspy driver does: no librtlsdr, no libusb, no DLLs. On Windows the
//! dongle must be bound to the WinUSB driver (Zadig); on Linux the kernel's
//! DVB-T driver (`dvb_usb_rtl28xxu`) is detached when it is opened.
//!
//! **Receive only; antenna power off unless asked.** `rtlsdr_proto` drives
//! the bias-tee pin low at open and when the dongle is closed; only
//! [`RtlSettings::bias_tee`] or [`RtlSdrControl::set_bias_tee`] drive it high.
//!
//! The dongle streams unsigned 8-bit IQ at 2.048 MS/s (4 MB/s). A reader
//! thread keeps [`IN_FLIGHT`] bulk transfers queued and hands the bytes over
//! a bounded channel; if the consumer falls behind, whole buffers are dropped
//! and counted. [`IqSource::read`] turns them into samples.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use decdvbt_core::{Error, Iq, Result};
use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};

use crate::IqSource;
use crate::raw_stream::RawStream;
use crate::rtlsdr_proto::{
    self as proto, ENDPOINT_IN, Rtl2832, RtlBus, RtlGains, RtlSettings, TRANSFER_BYTES, Tuner,
    U8ToIq,
};
use crate::usb::{BulkReader, block_on_timeout};

/// Bulk transfers kept in flight: 128 ms of samples at 16 ms each.
const IN_FLIGHT: usize = 8;
/// Buffers the reader may queue for the consumer before dropping: 64 × 16 ms,
/// enough to ride out a GUI hiccup.
const QUEUE: usize = 64;
/// A control transfer that takes longer than this has hung.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);

fn map_err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("RTL-SDR: {e}"))
}

/// The RTL2832U's vendor requests over a claimed interface.
struct UsbBus {
    interface: nusb::Interface,
}

impl RtlBus for UsbBus {
    fn control_out(&mut self, value: u16, index: u16, data: &[u8]) -> Result<()> {
        let done = block_on_timeout(
            self.interface.control_out(ControlOut {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: 0,
                value,
                index,
                data,
            }),
            CONTROL_TIMEOUT,
        )
        .ok_or_else(|| map_err("a control transfer timed out"))?
        .into_result()
        .map_err(map_err)?;
        if done.actual_length() == data.len() {
            Ok(())
        } else {
            Err(map_err("a control transfer was cut short"))
        }
    }

    fn control_in(&mut self, value: u16, index: u16, len: u16) -> Result<Vec<u8>> {
        block_on_timeout(
            self.interface.control_in(ControlIn {
                control_type: ControlType::Vendor,
                recipient: Recipient::Device,
                request: 0,
                value,
                index,
                length: len,
            }),
            CONTROL_TIMEOUT,
        )
        .ok_or_else(|| map_err("a control transfer timed out"))?
        .into_result()
        .map_err(map_err)
    }

    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The open dongle: what the control handle and the source share.
struct Inner {
    // Held so the device stays open as long as the interface is in use.
    _device: nusb::Device,
    interface: nusb::Interface,
    /// The chips. Rust note: a `Mutex` because the GUI thread retunes while
    /// the source is read on the engine's; only control transfers go
    /// through it, never the samples.
    rtl: Mutex<Rtl2832<UsbBus>>,
    product: String,
}

impl Inner {
    fn open() -> Result<Inner> {
        let info = nusb::list_devices()
            .map_err(|e| map_err(format!("cannot list the USB devices: {e}")))?
            .find(|d| proto::DEVICES.contains(&(d.vendor_id(), d.product_id())))
            .ok_or_else(|| {
                Error::Other(
                    "no RTL-SDR found. Is it plugged in, not in use by another program, \
                     and on the WinUSB driver (Zadig)?"
                        .into(),
                )
            })?;
        let manufacturer = info.manufacturer_string().unwrap_or("").to_string();
        let product = info.product_string().unwrap_or("").to_string();
        // The RTL-SDR Blog V4 says so in its USB strings (as the Blog fork
        // of librtlsdr checks). On Windows the manufacturer string can be
        // missing (nusb takes it from the driver's INF), so a missing one
        // passes; a V4 taken for a plain R828D tunes nothing (16 MHz crystal
        // assumed instead of 28.8 MHz).
        let v4 = product == "Blog V4" && matches!(manufacturer.as_str(), "RTLSDRBlog" | "");
        let device = info.open().map_err(|e| {
            Error::Other(format!(
                "cannot open the RTL-SDR ({e}). Is it in use by another program, \
                 and on the WinUSB driver?"
            ))
        })?;
        // Detaches the DVB-T kernel driver on Linux; a plain claim elsewhere.
        let interface = device.detach_and_claim_interface(0).map_err(map_err)?;
        let rtl = Rtl2832::open(
            UsbBus {
                interface: interface.clone(),
            },
            v4,
        )?;
        let product = match (manufacturer.as_str(), product.as_str()) {
            (_, "") => "RTL-SDR".to_string(),
            ("", p) => p.to_string(),
            (m, p) => format!("{m} {p}"),
        };
        Ok(Inner {
            _device: device,
            interface,
            rtl: Mutex::new(rtl),
            product,
        })
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last owner: the tuner to standby, the demodulator off.
        if let Ok(mut r) = self.rtl.lock() {
            let _ = r.close();
        }
    }
}

/// What the dongle says about itself.
#[derive(Debug, Clone)]
pub struct RtlInfo {
    /// The USB manufacturer and product strings.
    pub product: String,
    pub tuner: Tuner,
}

/// Open the first RTL-SDR, bring it up far enough to find its tuner, and
/// close it again. It is not started.
pub fn rtlsdr_info() -> Result<RtlInfo> {
    let dev = Inner::open()?;
    let tuner = dev.rtl.lock().unwrap().tuner();
    Ok(RtlInfo {
        product: dev.product.clone(),
        tuner,
    })
}

/// Live controls on a running RTL-SDR, usable from the GUI thread while the
/// engine reads samples.
#[derive(Clone)]
pub struct RtlSdrControl {
    dev: Arc<Inner>,
    overflows: Arc<AtomicU64>,
}

impl RtlSdrControl {
    pub fn settings(&self) -> RtlSettings {
        self.dev.rtl.lock().unwrap().settings()
    }

    pub fn tuner(&self) -> Tuner {
        self.dev.rtl.lock().unwrap().tuner()
    }

    /// Retune. Takes effect within a transfer or two.
    pub fn set_center(&self, hz: u64) -> Result<()> {
        self.dev.rtl.lock().unwrap().set_center(hz)
    }

    /// The tuner's AGC, or a fixed gain (snapped to the tuner's steps).
    pub fn set_gains(&self, gains: RtlGains) -> Result<()> {
        self.dev.rtl.lock().unwrap().set_gains(gains)
    }

    /// Power the antenna port (RTL-SDR Blog dongles), or not.
    pub fn set_bias_tee(&self, on: bool) -> Result<()> {
        self.dev.rtl.lock().unwrap().set_bias_tee(on)
    }

    /// The dongle's clock error, ppm (positive: it runs high).
    pub fn set_ppm(&self, ppm: f64) -> Result<()> {
        self.dev.rtl.lock().unwrap().set_ppm(ppm)
    }

    /// Buffers dropped because the consumer fell behind.
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }
}

/// A live RTL-SDR receive stream.
pub struct RtlSdrSource {
    stream: RawStream<U8ToIq>,
    control: RtlSdrControl,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RtlSdrSource {
    /// Open the first RTL-SDR, configure it and start receiving.
    pub fn open(settings: RtlSettings) -> Result<Self> {
        // What can be wrong without a dongle is checked first.
        proto::check_settings(&settings)?;
        let dev = Arc::new(Inner::open()?);
        dev.rtl.lock().unwrap().start(&settings)?;

        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let (recycle_tx, recycle_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let overflows = Arc::new(AtomicU64::new(0));
        let reader = BulkReader {
            interface: dev.interface.clone(),
            endpoint: ENDPOINT_IN,
            in_flight: IN_FLIGHT,
            transfer: TRANSFER_BYTES,
            tx,
            recycled: recycle_rx,
            stop: Arc::clone(&stop),
            overflows: Arc::clone(&overflows),
            what: "RTL-SDR",
        };
        let thread = std::thread::Builder::new()
            .name("rtlsdr-rx".into())
            .spawn(move || reader.run())
            .map_err(|e| map_err(format!("cannot start the reader: {e}")))?;

        Ok(RtlSdrSource {
            stream: RawStream::new(rx, recycle_tx, U8ToIq, "RTL-SDR"),
            control: RtlSdrControl { dev, overflows },
            stop,
            thread: Some(thread),
        })
    }

    /// A handle for retuning and gain changes while the engine reads.
    pub fn control(&self) -> RtlSdrControl {
        self.control.clone()
    }
}

impl IqSource for RtlSdrSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        self.stream.read(out)
    }

    fn sample_rate(&self) -> f64 {
        self.control.settings().sample_rate as f64
    }

    fn center_freq(&self) -> f64 {
        self.control.settings().center_hz as f64
    }

    fn describe(&self) -> String {
        let s = self.control.settings();
        let gain = if s.gains.agc {
            "AGC".to_string()
        } else {
            format!("gain {:.1} dB", f32::from(s.gains.db10) / 10.0)
        };
        format!(
            "RTL-SDR ({}) @ {:.4} MHz, {:.3} MS/s, {gain}",
            self.control.tuner().name(),
            s.center_hz as f64 / 1e6,
            s.sample_rate as f64 / 1e6,
        )
    }

    /// Moves the centre and throws away what was captured before it took
    /// effect: the buffers waiting and the transfers in flight as it
    /// retuned, and one more for the PLL's settling, so the receiver is not
    /// handed the old channel's samples as the new one's.
    fn retune(&mut self, hz: f64) -> Result<bool> {
        self.control.set_center(hz.round() as u64)?;
        self.stream.restart(IN_FLIGHT as u32 + 1);
        Ok(true)
    }

    fn is_live(&self) -> bool {
        true
    }
}

impl Drop for RtlSdrSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The reader polls the flag every 100 ms at most and cancels its
        // transfers on the way out; the dongle is powered down when the last
        // handle on it goes.
        self.stream.drain();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn rejects_what_is_wrong_before_any_usb_access() {
        for s in [
            RtlSettings {
                sample_rate: 2_400_000,
                ..Default::default()
            },
            RtlSettings {
                center_hz: 5_000_000,
                ..Default::default()
            },
        ] {
            // Checked before the device is opened, so this needs no dongle.
            let e = RtlSdrSource::open(s).err().expect("must fail").to_string();
            assert!(!e.contains("no RTL-SDR"), "{e}");
        }
    }

    /// Live: `cargo test -p decdvbt-io --release --features rtlsdr live_rtlsdr -- --ignored --nocapture`.
    /// Receive only, antenna power off. Streams 3 s at 153 MHz and reports the
    /// rate, the level and the buffers dropped; then retunes to 153.2 MHz.
    #[test]
    #[ignore = "needs an RTL-SDR attached"]
    fn live_rtlsdr() {
        let info = rtlsdr_info().expect("info");
        eprintln!("{info:?}");
        let mut src = RtlSdrSource::open(RtlSettings::default()).expect("open");
        let mut buf = Vec::new();
        let mut n = 0usize;
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            n += src.read(&mut buf).expect("read");
        }
        let rate = n as f64 / t0.elapsed().as_secs_f64();
        let rms = (buf.iter().map(|s| s.norm_sqr()).sum::<f32>() / buf.len() as f32).sqrt();
        eprintln!(
            "{} — {:.3} MS/s delivered, rms {rms:.4}, overflows {}",
            src.describe(),
            rate / 1e6,
            src.control().overflows()
        );
        assert!(rate > 1.95e6, "only {:.3} MS/s", rate / 1e6);
        assert!(src.retune(153_200_000.0).expect("retune"));
        src.read(&mut buf).expect("read after retune");
        assert_eq!(src.control().settings().center_hz, 153_200_000);
    }
}
