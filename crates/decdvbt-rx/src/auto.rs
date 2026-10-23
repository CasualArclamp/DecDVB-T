// made by CasualArclamp
//! A receiver that does not need to be told whether the signal is DVB-T or
//! DVB-T2: while it has no lock both receivers look (each costs little while
//! it finds nothing), and the one that locks keeps the stream; the other is
//! put to sleep until the lock is lost.
//!
//! The DVB-T receiver's extras (scopes, channel measurements, heat rows) are
//! passed through while it holds the lock. The T2 receiver has none of those
//! yet.

use decdvbt_core::Iq;

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
    t: Receiver,
    t2: T2Receiver,
    standard: Standard,
}

impl AutoReceiver {
    pub fn new(fs_in: f64, vfo_offset_hz: f64) -> AutoReceiver {
        AutoReceiver {
            t: Receiver::new(ReceiverConfig::new(fs_in, vfo_offset_hz)),
            t2: T2Receiver::new(T2Config::new(fs_in, vfo_offset_hz)),
            standard: Standard::Searching,
        }
    }

    pub fn standard(&self) -> Standard {
        self.standard
    }

    /// The DVB-T receiver's status (its counters go on counting only while it
    /// is the one looking or locked).
    pub fn dvbt(&self) -> &ReceiverStatus {
        self.t.status()
    }

    pub fn dvbt2(&self) -> &T2Status {
        self.t2.status()
    }

    /// Whether the locked receiver is decoding.
    pub fn decoding(&self) -> bool {
        match self.standard {
            Standard::Searching => false,
            Standard::Dvbt => self.t.status().decoding,
            Standard::Dvbt2 => self.t2.status().decoding,
        }
    }

    /// Forget the lock and the stretch wanted: the stream has jumped.
    pub fn reset(&mut self) {
        self.t.reset();
        self.t2.reset();
        self.standard = Standard::Searching;
    }

    pub fn take_ts(&mut self, out: &mut Vec<u8>) {
        self.t.take_ts(out);
        self.t2.take_ts(out);
    }

    pub fn request_scopes(&mut self) {
        if self.standard != Standard::Dvbt2 {
            self.t.request_scopes();
        }
    }

    pub fn take_scopes(&mut self) -> Option<Scopes> {
        self.t.take_scopes()
    }

    pub fn set_heat(&mut self, rows_per_s: f64) {
        self.t.set_heat(rows_per_s);
    }

    pub fn take_heat(&mut self, out: &mut Vec<Measurements>) {
        self.t.take_heat(out);
    }

    pub fn measure(&mut self) -> Option<Measurements> {
        if self.standard == Standard::Dvbt2 {
            return None;
        }
        self.t.measure()
    }

    /// Hand over a block of the radio's samples, whose first is the stream's
    /// `abs_start`-th.
    pub fn push(&mut self, block: &[Iq], abs_start: u64) {
        match self.standard {
            Standard::Searching => {
                self.t.push(block, abs_start);
                self.t2.push(block, abs_start);
                // A DVB-T lock is one whose TPS is found; a T2 lock is one
                // whose L1 is read.
                if self.t.status().tracking {
                    self.standard = Standard::Dvbt;
                    self.t2.reset();
                } else if self.t2.status().tracking {
                    self.standard = Standard::Dvbt2;
                    self.t.reset();
                }
            }
            Standard::Dvbt => {
                self.t.push(block, abs_start);
                if !self.t.status().tracking {
                    self.standard = Standard::Searching;
                }
            }
            Standard::Dvbt2 => {
                self.t2.push(block, abs_start);
                if !self.t2.status().tracking {
                    self.standard = Standard::Searching;
                }
            }
        }
    }
}
