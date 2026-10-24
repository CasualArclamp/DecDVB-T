// made by CasualArclamp
//! MPEG-TS: what the multiplex carries, and where it goes.
//!
//! - [`psi`]: per-PID counts and continuity; PAT, PMT, NIT, SDT and EIT for
//!   the services, their names and now/next.
//! - [`section`]: PSI/SI sections reassembled from packets.
//! - [`sink`]: a `.ts` file, UDP, or a TCP/HTTP server for VLC and PotPlayer.
//! - [`serve`]: the TCP/HTTP server behind the sink.
//! - [`backlog`]: the stream since its last keyframe, for a player that
//!   starts now.
//! - [`udp`]: transport packets carried in UDP, found in an IP stream.

pub mod backlog;
pub mod epg;
pub mod huffman;
mod huffman_tables;
pub mod psi;
pub mod section;
pub mod serve;
pub mod sink;
pub mod udp;

/// A transport packet's length (ISO/IEC 13818-1 §2.4.3.2).
pub const TS_LEN: usize = 188;
/// The sync byte every transport packet starts with.
pub const TS_SYNC: u8 = 0x47;

/// The null packet (PID 0x1FFF).
pub fn null_packet() -> [u8; TS_LEN] {
    let mut p = [0xFFu8; TS_LEN];
    p[0] = TS_SYNC;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
}

pub use epg::{Epg, EpgEvent, LocalTime, content_name, freeview_genre_name, genre_name};
pub use psi::{
    EsInfo, Event, NetworkInfo, OtherService, PidRow, PidStats, Programme, Terrestrial,
    Transponder, TsAnalyser, TsReport, ca_system_name, service_type_name, stream_type_name,
};
pub use sink::{TcpSink, TsFile, TsTap, UdpSink};
pub use udp::{UdpTs, UdpTsFinder};
