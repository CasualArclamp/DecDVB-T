// made by CasualArclamp
//! The sinks behind one pull contract.
//!
//! The HackRF's transmit thread calls a closure "append cs8 bytes" each time
//! its USB queue has room (`decdvbt-io/src/hackrf_tx.rs`). [`pull`] is that
//! closure's body, and the file and null sinks call the very same function
//! from [`VirtualRadio`], so a test with no radio runs the code the radio
//! runs, including underrun accounting.
//!
//! **This file holds the only call that opens the HackRF** (`open_hackrf`), and
//! it needs an [`ArmedProof`] by value (see `arm.rs`; a test checks no other
//! file contains the call).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use decdvbt_core::{Error, Result};

use crate::arm::ArmedProof;
use crate::config::{HackRfTxConfig, TxConfig, TxSink};
use crate::ring::IqRing;

/// When this environment variable is set (to anything) the radio cannot be
/// opened: every test script, CI job and benchmark sets it, so a mistake in a
/// test can not key the HackRF.
pub const NO_RADIO_ENV: &str = "DECDVBT_TX_NO_RADIO";

/// Bytes the HackRF driver pads with zeros when the source gives nothing: one
/// USB transfer (`TRANSFER` in `hackrf_tx.rs`). The virtual radio does the
/// same, so an underrun costs the same in a test as on the air.
pub const UNDERRUN_ZERO_BYTES: usize = 1 << 18;

/// Transfers the USB driver keeps in flight: the radio's own queue, which the
/// paced virtual radio imitates (it hands over this much ahead of the clock).
const IN_FLIGHT_TRANSFERS: usize = 3;

/// What the radio side counts. All relaxed atomics.
#[derive(Debug, Default)]
pub struct RadioStats {
    /// Bytes handed to the radio (two a sample), zeros included.
    pub bytes_out: AtomicU64,
    /// Times the radio wanted data and the ring was empty while the
    /// modulator was still producing: **the number that must stay zero**.
    pub underruns: AtomicU64,
    /// The sink thread has stopped (and why, in `error`).
    pub stopped: AtomicBool,
    pub error: Mutex<Option<String>>,
}

/// What the radio side shares with the modulator thread.
pub struct RadioShared {
    pub ring: Arc<IqRing>,
    pub stats: RadioStats,
    /// The modulator thread has produced its last block (end of file, or the
    /// sink's `seconds`): an empty ring is then the end, not an underrun.
    pub producer_done: AtomicBool,
}

impl RadioShared {
    pub fn new(ring: Arc<IqRing>) -> RadioShared {
        RadioShared {
            ring,
            stats: RadioStats::default(),
            producer_done: AtomicBool::new(false),
        }
    }
}

/// Append the next block to `out`, or nothing if there is none. The caller (the
/// driver's loop) pads zeros when nothing was appended.
pub fn pull(shared: &RadioShared, out: &mut Vec<u8>) {
    match shared.ring.try_pop() {
        Some(block) => {
            out.extend_from_slice(&block);
            shared.ring.recycle(block);
        }
        None => {
            if !shared.producer_done.load(Ordering::Acquire) {
                shared.stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// A radio that is not one: drains the ring into a file or nowhere, at the
/// sample clock (`paced`) or flat out.
pub struct VirtualRadio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VirtualRadio {
    /// Start draining. `sink` is `Null` or `IqFile`; `sample_rate_hz` paces
    /// it (by an absolute schedule from the start, not sleeps that add up).
    pub fn start(
        shared: Arc<RadioShared>,
        sink: &TxSink,
        sample_rate_hz: u32,
    ) -> Result<VirtualRadio> {
        let (paced, file, limit) = match sink {
            TxSink::Null { paced } => (*paced, None, None),
            TxSink::IqFile {
                path,
                paced,
                seconds,
            } => {
                let f = File::create(path)
                    .map_err(|e| Error::Other(format!("cannot create {}: {e}", path.display())))?;
                // Two bytes a sample.
                let limit = seconds.map(|s| (s * f64::from(sample_rate_hz) * 2.0) as u64);
                (*paced, Some(BufWriter::with_capacity(1 << 20, f)), limit)
            }
            TxSink::HackRf(_) => {
                return Err(Error::Other(
                    "the virtual radio cannot take a HackRF sink".into(),
                ));
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("tx-virtual".into())
                .spawn(move || {
                    let r = run_virtual(&shared, &stop, paced, file, limit, sample_rate_hz);
                    if let Err(e) = r {
                        *shared.stats.error.lock().unwrap() = Some(e);
                    }
                    shared.stats.stopped.store(true, Ordering::Release);
                })
                .map_err(|e| Error::Other(format!("cannot start the virtual radio: {e}")))?
        };
        Ok(VirtualRadio {
            stop,
            thread: Some(thread),
        })
    }
}

/// The virtual radio's thread: the HackRF thread's loop (fill a USB transfer
/// from [`pull`], zeros if there is nothing, write it) with the write going to
/// a file or nowhere, and, when `paced`, the radio's clock imitated.
fn run_virtual(
    shared: &RadioShared,
    stop: &AtomicBool,
    paced: bool,
    mut file: Option<BufWriter<File>>,
    limit_bytes: Option<u64>,
    sample_rate_hz: u32,
) -> std::result::Result<(), String> {
    let transfer = UNDERRUN_ZERO_BYTES;
    let mut pending: Vec<u8> = Vec::with_capacity(2 * transfer);
    let t0 = Instant::now();
    let mut handed: u64 = 0;
    let bytes_per_s = f64::from(sample_rate_hz) * 2.0;
    'outer: while !stop.load(Ordering::Relaxed) {
        let mut ended = false;
        while pending.len() < transfer {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            // Read before pulling: if the producer was done before this look,
            // an empty pull is the end for certain.
            let done = shared.producer_done.load(Ordering::Acquire);
            if !paced && !done && shared.ring.len_blocks() == 0 {
                // Flat out: nothing to do until the modulator has made some.
                // (No zeros, no underrun: speed is the point of this mode.)
                std::thread::sleep(Duration::from_micros(200));
                continue;
            }
            let before = pending.len();
            pull(shared, &mut pending);
            if pending.len() == before {
                if done {
                    ended = true;
                    break;
                }
                // The source gave nothing: send silence rather than stall the
                // radio, as the driver does.
                pending.resize(before + transfer, 0);
            }
        }
        let mut n = pending.len().min(transfer);
        let mut last = ended;
        if let Some(limit) = limit_bytes
            && handed + n as u64 >= limit
        {
            n = (limit - handed) as usize;
            last = true;
        }
        if let Some(f) = &mut file {
            f.write_all(&pending[..n])
                .map_err(|e| format!("writing the IQ file: {e}"))?;
        }
        pending.drain(..n);
        handed += n as u64;
        shared
            .stats
            .bytes_out
            .fetch_add(n as u64, Ordering::Relaxed);
        if last {
            break;
        }
        if paced {
            // The device plays what it was given at its clock and has a few
            // transfers queued: we may be that far ahead and no further.
            let ahead = (IN_FLIGHT_TRANSFERS - 1) * transfer;
            let play = handed.saturating_sub(ahead as u64) as f64 / bytes_per_s;
            let due = t0 + Duration::from_secs_f64(play);
            // Sleep in short steps so a stop is not waited for.
            while let Some(left) = due.checked_duration_since(Instant::now()) {
                if stop.load(Ordering::Relaxed) {
                    break 'outer;
                }
                std::thread::sleep(left.min(Duration::from_millis(20)));
            }
        }
    }
    if let Some(f) = &mut file {
        f.flush().map_err(|e| format!("writing the IQ file: {e}"))?;
    }
    Ok(())
}

impl Drop for VirtualRadio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Either kind of radio.
pub enum Radio {
    Virtual(VirtualRadio),
    #[cfg(feature = "hackrf")]
    HackRf(decdvbt_io::HackRfTx),
}

impl Radio {
    /// TX VGA while transmitting (only the HackRF has one).
    pub fn set_txvga(&self, db: u16) -> Result<()> {
        let _ = db; // unused without the hackrf feature
        match self {
            Radio::Virtual(_) => Ok(()),
            #[cfg(feature = "hackrf")]
            Radio::HackRf(r) => r.set_txvga(db),
        }
    }

    /// The thread behind it has stopped by itself (error or end).
    pub fn has_stopped(&self, shared: &RadioShared) -> bool {
        match self {
            Radio::Virtual(_) => shared.stats.stopped.load(Ordering::Acquire),
            #[cfg(feature = "hackrf")]
            Radio::HackRf(r) => r.counters.stopped.load(Ordering::Acquire),
        }
    }

    /// Why it stopped, for the GUI.
    pub fn error(&self, shared: &RadioShared) -> Option<String> {
        match self {
            Radio::Virtual(_) => shared.stats.error.lock().unwrap().clone(),
            #[cfg(feature = "hackrf")]
            Radio::HackRf(r) => r.counters.error.lock().unwrap().clone(),
        }
    }

    /// Bytes handed to the radio so far (two a sample), zeros included.
    pub fn bytes_out(&self, shared: &RadioShared) -> u64 {
        match self {
            Radio::Virtual(_) => shared.stats.bytes_out.load(Ordering::Relaxed),
            #[cfg(feature = "hackrf")]
            Radio::HackRf(r) => r.counters.bytes.load(Ordering::Relaxed),
        }
    }
}

/// Open the HackRF and start transmitting from `shared`'s ring. **The only
/// place the radio is opened.** The settings start at TX VGA 0, amp off unless
/// the config says otherwise; antenna power is never on (the driver sets it).
#[cfg(feature = "hackrf")]
pub fn open_hackrf(
    proof: ArmedProof,
    cfg: &TxConfig,
    hackrf: &HackRfTxConfig,
    shared: Arc<RadioShared>,
) -> Result<Radio> {
    // Used up here: the proof cannot be used for a second transmission.
    let _used_up = proof;
    // Test harnesses set this; unit tests are refused whatever the env says.
    if cfg!(test) || std::env::var_os(NO_RADIO_ENV).is_some() {
        return Err(decdvbt_core::Error::Other(format!(
            "the radio is disabled ({NO_RADIO_ENV} is set, or this is a test build)"
        )));
    }
    let settings = decdvbt_io::HackRfTxSettings {
        center_hz: cfg.centre_hz.round() as u64,
        sample_rate: cfg.sample_rate_hz(),
        txvga_db: hackrf.txvga_db,
        amp: hackrf.amp,
        baseband_filter_hz: hackrf.baseband_filter_hz,
    };
    let tx = decdvbt_io::HackRfTx::start(settings, move |out| pull(&shared, out))?;
    Ok(Radio::HackRf(tx))
}

/// Without the `hackrf` feature there is no radio to open.
#[cfg(not(feature = "hackrf"))]
pub fn open_hackrf(
    proof: ArmedProof,
    cfg: &TxConfig,
    hackrf: &HackRfTxConfig,
    shared: Arc<RadioShared>,
) -> Result<Radio> {
    let _ = (proof, cfg, hackrf, shared);
    Err(decdvbt_core::Error::Other(
        "built without the hackrf feature".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(blocks: usize, block_len: usize) -> Arc<RadioShared> {
        let ring = Arc::new(IqRing::new(blocks + 1));
        for i in 0..blocks {
            ring.push_blocking(vec![i as u8 + 1; block_len]);
        }
        Arc::new(RadioShared::new(ring))
    }

    fn wait_stopped(s: &RadioShared) {
        let t = Instant::now();
        while !s.stats.stopped.load(Ordering::Acquire) {
            assert!(t.elapsed() < Duration::from_secs(10), "radio never stopped");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn pull_counts_an_underrun_only_while_the_producer_runs() {
        let s = shared(1, 4);
        let mut out = Vec::new();
        pull(&s, &mut out);
        assert_eq!(out, [1, 1, 1, 1]);
        pull(&s, &mut out);
        assert_eq!(s.stats.underruns.load(Ordering::Relaxed), 1);
        s.producer_done.store(true, Ordering::Release);
        pull(&s, &mut out);
        assert_eq!(s.stats.underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_flat_out_null_sink_drains_everything_and_stops_at_the_end() {
        // 6 blocks of 100 000 bytes: 600 000 bytes, 2 transfers and a bit.
        let s = shared(6, 100_000);
        s.producer_done.store(true, Ordering::Release);
        let _r = VirtualRadio::start(s.clone(), &TxSink::Null { paced: false }, 9_142_857).unwrap();
        wait_stopped(&s);
        assert_eq!(s.stats.bytes_out.load(Ordering::Relaxed), 600_000);
        assert_eq!(s.stats.underruns.load(Ordering::Relaxed), 0);
        assert!(s.stats.error.lock().unwrap().is_none());
    }

    #[test]
    fn the_file_sink_writes_the_bytes_and_obeys_seconds() {
        let dir = std::env::temp_dir().join(format!("decdvbt-tx-radio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.cs8");
        let s = shared(8, 100_000);
        let sink = TxSink::IqFile {
            path: path.clone(),
            paced: false,
            seconds: Some(0.05), // 100 000 bytes a 1 MS/s second... see below
        };
        // 1 000 000 Hz x 0.05 s x 2 bytes = 100 000 bytes.
        let _r = VirtualRadio::start(s.clone(), &sink, 1_000_000).unwrap();
        wait_stopped(&s);
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 100_000);
        assert!(data.iter().all(|&b| b == 1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_paced_radio_pads_zeros_and_counts_underruns_when_starved() {
        let ring = Arc::new(IqRing::new(4));
        let s = Arc::new(RadioShared::new(ring));
        let r = VirtualRadio::start(s.clone(), &TxSink::Null { paced: true }, 2_000_000).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        drop(r);
        assert!(s.stats.underruns.load(Ordering::Relaxed) >= 1);
        assert!(s.stats.bytes_out.load(Ordering::Relaxed) >= UNDERRUN_ZERO_BYTES as u64);
    }

    #[test]
    fn a_paced_radio_keeps_the_sample_clock() {
        // 1 MS/s = 2 MB/s. The radio may be two transfers (0.52 MB) ahead, so
        // after 0.5 s it has handed over about 1 MB + 0.52 MB.
        let ring = Arc::new(IqRing::new(64));
        let s = Arc::new(RadioShared::new(ring.clone()));
        for _ in 0..40 {
            ring.push_blocking(vec![7u8; 131_072]);
        }
        let r = VirtualRadio::start(s.clone(), &TxSink::Null { paced: true }, 1_000_000).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let got = s.stats.bytes_out.load(Ordering::Relaxed) as f64;
        drop(r);
        assert!((1.2e6..=1.9e6).contains(&got), "{got}");
        assert_eq!(s.stats.underruns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_radio_is_refused_in_tests_even_with_a_proof() {
        let mut arm = crate::arm::Arm::new();
        arm.arm(crate::arm::LicenceAck::accept(crate::arm::LICENCE_WARNING).unwrap());
        let proof = arm.take().unwrap();
        let cfg = TxConfig::new("x.mp4".into());
        let s = shared(1, 4);
        let e = open_hackrf(proof, &cfg, &HackRfTxConfig::default(), s)
            .err()
            .expect("must refuse");
        assert!(e.to_string().contains("disabled") || e.to_string().contains("without"));
    }
}
