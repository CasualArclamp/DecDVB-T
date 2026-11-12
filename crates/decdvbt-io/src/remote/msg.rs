// made by CasualArclamp
//! The control messages of DecSAT's remote protocol, encoded with postcard
//! (from DecSAT's `decsat-remote/src/msg.rs` and `radios.rs`).
//!
//! postcard writes an enum as its variant's number and a struct as its
//! fields in order, with no names: every type here must keep DecSAT's
//! variants and fields, in DecSAT's order, including the ones DecDVB-T never
//! sends. [`super::wire::PROTO`] is DecSAT's count of such changes, and the
//! handshake refuses a server on another.
//!
//! The status structures of smart mode (decoding on the server) are not
//! here: they travel in frames of their own kinds, which this client skips.

use serde::{Deserialize, Serialize};

/// One gain control. A switch is a stage with `max` 1 and `step` 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gain {
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub value: f64,
}

/// A radio on offer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioInfo {
    pub key: String,
    pub name: String,
    pub min_hz: f64,
    pub max_hz: f64,
    /// Sample rates offered, Hz.
    pub rates: Vec<f64>,
    pub gains: Vec<Gain>,
}

/// What a client opens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioRequest {
    pub key: String,
    pub center_hz: f64,
    pub sample_rate: f64,
    /// Stages left out keep the radio's defaults.
    pub gains: Vec<(String, f64)>,
}

/// A client's first message, after [`super::wire::MAGIC`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u16,
    pub version: String,
    /// Shown to other users in shared mode.
    pub name: String,
    /// Replace a client that already holds the server, if the server allows.
    pub takeover: bool,
}

/// How a server hands out its radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMode {
    /// One client at a time, in full control.
    Exclusive,
    /// A radio fixed by the server, decoded there for several users: no IQ
    /// to be had.
    Shared,
}

/// The IQ stream's options.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IqParams {
    /// 4, 6, 8, 10, 12 or 16.
    pub bits: u8,
    /// zstd level, 0 for off.
    pub zstd: i32,
    /// Send only a slice of the band: its centre relative to the radio's
    /// and its bandwidth, Hz. `None` for the whole band.
    pub slice: Option<(f64, f64)>,
}

impl Default for IqParams {
    fn default() -> Self {
        IqParams {
            bits: 8,
            zstd: 0,
            slice: None,
        }
    }
}

/// Smart mode's options: never sent from here, kept for [`Mode`]'s layout.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SmartParams {
    pub fft_size: u32,
    pub rows_per_sec: f32,
    pub display_zstd: i32,
    pub ts_zstd: i32,
    pub scatter_points: u32,
    pub dc_removal: bool,
}

/// What the client wants the server to do. DecDVB-T only asks for `Dumb`:
/// the IQ, decoded here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Mode {
    Dumb(IqParams),
    Smart(SmartParams),
}

/// A decoder on the server (smart mode): kept for the layout of the
/// messages that carry one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VfoRequest {
    pub name: String,
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
    pub decoder: String,
    pub enabled: bool,
    pub symbol_rate: Option<f64>,
    pub gold_code: u32,
    pub follow_carrier: bool,
    pub ts_isi: Option<u8>,
    pub biss_key: String,
}

/// Client to server. DecDVB-T sends `Hello`, `Auth`, `Start`, `Tune`,
/// `Gain` and `Ping`; the rest hold their places.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum C2S {
    Hello(Hello),
    Auth {
        client_nonce: [u8; 32],
        proof: Vec<u8>,
    },
    /// Open a radio and start.
    Start {
        radio: Option<RadioRequest>,
        mode: Mode,
    },
    Stop,
    Tune {
        hz: f64,
    },
    Gain {
        name: String,
        value: f64,
    },
    SetPaused(bool),
    SetDcRemoval(bool),
    SetFftSize(u32),
    AddVfo {
        id: u32,
        vfo: VfoRequest,
    },
    UpdateVfo {
        id: u32,
        vfo: VfoRequest,
        carried_by: Option<f64>,
    },
    RemoveVfo {
        id: u32,
    },
    Camp {
        id: u32,
        server_id: u32,
    },
    TsPids {
        id: u32,
        pids: Vec<u16>,
    },
    Ping(u64),
}

/// One decoder on a shared server (smart mode; kept for the layout).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedVfo {
    pub server_id: u32,
    pub name: String,
    pub owner: Option<String>,
    pub offset_hz: f64,
    pub bandwidth_hz: f64,
    pub decoder: String,
    pub campers: u32,
}

/// Server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum S2C {
    ServerHello {
        proto: u16,
        version: String,
        name: String,
        mode: ServerMode,
        /// Present when a password is needed.
        nonce: Option<[u8; 32]>,
    },
    /// The client may proceed.
    Ready {
        radios: Vec<RadioInfo>,
    },
    /// The connection is over, and why.
    Refused(String),
    Started {
        sample_rate: f64,
        center_hz: f64,
        description: String,
        gains: Vec<Gain>,
    },
    Stopped(String),
    /// Something the user should see; the session carries on.
    Note(String),
    VfoGone {
        id: u32,
        why: String,
    },
    Camped {
        id: u32,
        vfo: VfoRequest,
    },
    Shared(Vec<SharedVfo>),
    /// A radio retune took effect.
    Tuned {
        center_hz: f64,
    },
    Stats(LinkStats),
    Pong(u64),
}

/// What the server knows about the link.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkStats {
    /// Bytes per second written to the socket, last second.
    pub bytes_per_sec: f64,
    /// IQ samples thrown away because the link fell behind, since start.
    pub iq_dropped: u64,
    pub ts_dropped: u64,
    pub rows_thinned: u64,
    pub row_divider: u32,
}

/// Encode a message for the wire.
pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    // Rust note: serialising these types to a Vec cannot fail: only a sink
    // that runs out of room would.
    postcard::to_stdvec(m).expect("encoding to a Vec")
}

/// Decode a message from the wire.
pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Option<T> {
    postcard::from_bytes(b).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers postcard writes for the variants this client uses, as
    /// DecSAT's server reads them: a variant moved here would still round-trip
    /// with itself and fail only against the real thing.
    #[test]
    fn variant_numbers_are_decsats() {
        assert_eq!(encode(&C2S::Stop), [3]);
        assert_eq!(encode(&C2S::Tune { hz: 0.0 })[0], 4);
        assert_eq!(
            encode(&C2S::Gain {
                name: String::new(),
                value: 0.0
            })[0],
            5
        );
        assert_eq!(encode(&C2S::Ping(1)), [14, 1]);
        assert_eq!(encode(&S2C::Refused(String::new())), [2, 0]);
        assert_eq!(encode(&S2C::Tuned { center_hz: 0.0 })[0], 9);
        assert_eq!(encode(&S2C::Stats(LinkStats::default()))[0], 10);
        assert_eq!(encode(&S2C::Pong(1)), [11, 1]);
        // Start: variant 2, a radio (Some), then Mode::Dumb as 0.
        let start = encode(&C2S::Start {
            radio: None,
            mode: Mode::Dumb(IqParams::default()),
        });
        assert_eq!(start, [2, 0, 0, 8, 0, 0]);
    }
}
