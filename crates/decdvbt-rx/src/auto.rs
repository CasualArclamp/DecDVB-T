// made by CasualArclamp
//! A receiver that does not need to be told whether the signal is DVB-T or
//! DVB-T2: while it has no lock both receivers look (each costs little while
//! it finds nothing), and the one that locks keeps the stream; the other is
//! put to sleep until the lock is lost.
//!
//! The DVB-T receiver's extras (scopes, channel measurements, heat rows) are
//! passed through while it holds the lock. The T2 receiver has none of those
//! yet.
//!
//! The channel's width is given (nothing in either signal says it), and only
//! the standards sent in it look: 1 MHz is DVB-T's alone (DecDVB-T's scaled
//! mode), 1.7 MHz DVB-T2's.

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Bandwidth;

use crate::outer::OuterStats;
use crate::receiver::{Receiver, ReceiverConfig, ReceiverStatus};
use crate::scope::{Measurements, Scopes};
use crate::t2::receiver::{T2Config, T2Receiver, T2Status};

/// Which standard the lock is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Standard {
    /// No lock: both receivers are looking.
    #[default]
    Searching,
    Dvbt,
    Dvbt2,
}

impl Standard {
    pub fn name(self) -> &'static str {
        match self {
            Standard::Searching => "searching",
            Standard::Dvbt => "DVB-T",
            Standard::Dvbt2 => "DVB-T2",
        }
    }
}

pub struct AutoReceiver {
    /// The receivers for the standards sent in the channel's width.
    t: Option<Receiver>,
    t2: Option<T2Receiver>,
    standard: Standard,
    bandwidth: Bandwidth,
    /// What a status of a receiver that is not there reads.
    idle_t: ReceiverStatus,
    idle_t2: T2Status,
}

impl AutoReceiver {
    /// An 8 MHz channel.
    pub fn new(fs_in: f64, vfo_offset_hz: f64) -> AutoReceiver {
        AutoReceiver::with_bandwidth(fs_in, vfo_offset_hz, Bandwidth::Mhz8)
    }

    /// A channel `bandwidth` wide.
    pub fn with_bandwidth(fs_in: f64, vfo_offset_hz: f64, bandwidth: Bandwidth) -> AutoReceiver {
        let t = bandwidth.dvbt().then(|| {
            let mut c = ReceiverConfig::new(fs_in, vfo_offset_hz);
            c.bandwidth = bandwidth;
            Receiver::new(c)
        });
        let t2 = bandwidth.t2().then(|| {
            let mut c = T2Config::new(fs_in, vfo_offset_hz);
            c.bandwidth = bandwidth;
            T2Receiver::new(c)
        });
        AutoReceiver {
            t,
            t2,
            standard: Standard::Searching,
            bandwidth,
            idle_t: ReceiverStatus::default(),
            idle_t2: T2Status {
                bandwidth: Some(bandwidth),
                ..T2Status::default()
            },
        }
    }

    pub fn bandwidth(&self) -> Bandwidth {
        self.bandwidth
    }

    /// The samples come from a radio (or are paced like one): the T2 FEC
    /// gives up a frame rather than hold the receive thread when it is behind.
    pub fn set_realtime(&mut self, on: bool) {
        if let Some(t2) = &mut self.t2 {
            t2.set_realtime(on);
        }
    }

    /// The DVB-T2 PLP to decode, by id (`None`: the first data PLP).
    pub fn set_plp(&mut self, plp: Option<u8>) {
        if let Some(t2) = &mut self.t2 {
            t2.set_plp(plp);
        }
    }

    pub fn standard(&self) -> Standard {
        self.standard
    }

    /// The DVB-T receiver's own status (its counters go on counting only
    /// while it is the one looking or locked).
    pub fn dvbt(&self) -> &ReceiverStatus {
        self.t.as_ref().map_or(&self.idle_t, |t| t.status())
    }

    pub fn dvbt2(&self) -> &T2Status {
        self.t2.as_ref().map_or(&self.idle_t2, |t2| t2.status())
    }

    /// The status in the DVB-T receiver's terms, whichever has the lock, for
    /// whatever shows only the common things (the lock, the carrier offset,
    /// the signal-to-noise ratio, the blocks the code lost): with a T2 lock
    /// its TPS stands for L1 read, its Reed–Solomon blocks for LDPC blocks.
    pub fn status(&self) -> ReceiverStatus {
        let mut s = self.dvbt().clone();
        if self.standard == Standard::Dvbt2 {
            let t2 = self.dvbt2();
            s.tracking = t2.tracking;
            s.tps_synced = t2.l1_ok;
            s.decoding = t2.decoding;
            s.hierarchical = false;
            (s.fft, s.guard, s.tps) = (None, None, None);
            s.cfo_hz = t2.cfo_hz;
            s.clock_ppm = t2.clock_ppm;
            s.snr_db = t2.snr_db;
            s.effective_snr_db = t2.effective_snr_db;
            s.locks = t2.locks;
            s.outer = OuterStats {
                blocks: t2.blocks_ok + t2.blocks_bad,
                clean: t2.blocks_ok,
                uncorrectable: t2.blocks_bad,
                losses: t2.resyncs,
                packets: t2.packets,
                ..Default::default()
            };
        }
        s
    }

    /// Whether the locked receiver is decoding.
    pub fn decoding(&self) -> bool {
        match self.standard {
            Standard::Searching => false,
            Standard::Dvbt => self.dvbt().decoding,
            Standard::Dvbt2 => self.dvbt2().decoding,
        }
    }

    /// Forget the lock and the stretch wanted: the stream has jumped.
    pub fn reset(&mut self) {
        if let Some(t) = &mut self.t {
            t.reset();
        }
        if let Some(t2) = &mut self.t2 {
            t2.reset();
        }
        self.standard = Standard::Searching;
    }

    pub fn take_ts(&mut self, out: &mut Vec<u8>) {
        if let Some(t) = &mut self.t {
            t.take_ts(out);
        }
        if let Some(t2) = &mut self.t2 {
            t2.take_ts(out);
        }
    }

    /// Wait for the T2 receiver's FEC stage to finish what it has (for the
    /// end of a file); the packets are then there for `take_ts`.
    pub fn flush(&mut self) {
        if let Some(t2) = &mut self.t2 {
            t2.flush();
        }
    }

    /// The DVB-T2 receiver while T2 holds the lock or DVB-T is not sent in
    /// this width; the DVB-T receiver otherwise (the extras follow it).
    fn t2_has_extras(&self) -> bool {
        self.standard == Standard::Dvbt2 || self.t.is_none()
    }

    pub fn request_scopes(&mut self) {
        if self.t2_has_extras() {
            if let Some(t2) = &mut self.t2 {
                t2.request_scopes();
            }
        } else if let Some(t) = &mut self.t {
            t.request_scopes();
        }
    }

    pub fn take_scopes(&mut self) -> Option<Scopes> {
        if self.t2_has_extras() {
            self.t2.as_mut()?.take_scopes()
        } else {
            self.t.as_mut()?.take_scopes()
        }
    }

    pub fn set_heat(&mut self, rows_per_s: f64) {
        if let Some(t) = &mut self.t {
            t.set_heat(rows_per_s);
        }
        if let Some(t2) = &mut self.t2 {
            t2.set_heat(rows_per_s);
        }
    }

    /// Make the channel sounder's rows from a DVB-T2 lock's frames while on
    /// (DVB-T has a sounder of its own, [`crate::sounder`]).
    pub fn set_sounding(&mut self, on: bool) {
        if let Some(t2) = &mut self.t2 {
            t2.set_sounding(on);
        }
    }

    /// The T2 lock's channel sounder, while it is on and T2 has the lock.
    pub fn t2_sounder(&mut self) -> Option<&mut crate::t2::sounding::T2Sounder> {
        match self.standard {
            Standard::Dvbt2 => self.t2.as_mut()?.sounder(),
            _ => None,
        }
    }

    pub fn take_heat(&mut self, out: &mut Vec<Measurements>) {
        if let Some(t) = &mut self.t {
            t.take_heat(out);
        }
        if let Some(t2) = &mut self.t2 {
            t2.take_heat(out);
        }
    }

    pub fn measure(&mut self) -> Option<Measurements> {
        if self.t2_has_extras() {
            self.t2.as_mut()?.measure()
        } else {
            self.t.as_mut()?.measure()
        }
    }

    /// Hand over a block of the radio's samples, whose first is the stream's
    /// `abs_start`-th.
    pub fn push(&mut self, block: &[Iq], abs_start: u64) {
        match self.standard {
            Standard::Searching => {
                if let Some(t) = &mut self.t {
                    t.push(block, abs_start);
                }
                if let Some(t2) = &mut self.t2 {
                    t2.push(block, abs_start);
                }
                // A DVB-T lock is one whose TPS is found; a T2 lock is one
                // whose L1 is read.
                if self.dvbt().tracking {
                    self.standard = Standard::Dvbt;
                    if let Some(t2) = &mut self.t2 {
                        t2.reset();
                    }
                } else if self.dvbt2().tracking {
                    self.standard = Standard::Dvbt2;
                    if let Some(t) = &mut self.t {
                        t.reset();
                    }
                }
            }
            Standard::Dvbt => {
                if let Some(t) = &mut self.t {
                    t.push(block, abs_start);
                }
                if !self.dvbt().tracking {
                    self.standard = Standard::Searching;
                }
            }
            Standard::Dvbt2 => {
                if let Some(t2) = &mut self.t2 {
                    t2.push(block, abs_start);
                }
                if !self.dvbt2().tracking {
                    self.standard = Standard::Searching;
                }
            }
        }
    }
}
