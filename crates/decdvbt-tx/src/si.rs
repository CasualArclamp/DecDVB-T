// made by CasualArclamp
//! Service information we add to ffmpeg's stream.
//!
//! ffmpeg's muxer writes PAT, PMT, SDT and (with `+nit`) a bare NIT. It writes
//! no terrestrial_delivery_system_descriptor, no TDT/TOT and no EIT. Receivers
//! scan faster, and some only accept a mux, when the NIT says where it is, so
//! the transmitter swaps packets **in place**, which changes no packet's
//! position and so no PCR or rate:
//!
//! - a PID 0x0010 packet is replaced by our NIT (one packet; the continuity
//!   counter ffmpeg wrote is kept);
//! - a null packet (PID 0x1FFF) is replaced by a TDT or a TOT when one is due
//!   (every 1 s each), with a counter of our own per PID.
//!
//! The section builders are our own, not `decdvbt_ts::psi::section`, which
//! writes `last_section_number = 1` for a single section (the spec wants 0;
//! `decdvbt-mod/src/ts.rs::long_section` has it right and is the model).
//! CRC-32 is `decdvbt_core::crc::crc32_mpeg2`. No EIT in v1.

use std::time::{SystemTime, UNIX_EPOCH};

use decdvbt_core::crc::crc32_mpeg2;
use decdvbt_core::dvbt::{Bandwidth, FftSize, Mode, Modulation};
use decdvbt_mod::ts::{TS_LEN, TS_SYNC};

use crate::config::ServiceInfo;

const PID_NIT: u16 = 0x0010;
const PID_TDT: u16 = 0x0014;
const PID_NULL: u16 = 0x1FFF;

/// What the injected tables say.
#[derive(Debug, Clone)]
pub struct SiConfig {
    pub service: ServiceInfo,
    pub mode: Mode,
    pub centre_hz: f64,
    /// Seconds from UTC to local time, for the TOT's local_time_offset
    /// descriptor (the GUI passes the PC's).
    pub utc_offset_s: i32,
}

/// Tag, length, contents.
fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
    debug_assert!(body.len() <= 255);
    let mut d = vec![tag, body.len() as u8];
    d.extend_from_slice(body);
    d
}

/// EN 300 468 §6.2.13.4: the terrestrial_delivery_system_descriptor (0x5A)
/// for `mode` at `centre_hz`: centre in 10 Hz units, bandwidth, constellation,
/// hierarchy, code rates, guard, transmission mode.
pub fn terrestrial_delivery_descriptor(mode: &Mode, centre_hz: f64) -> Vec<u8> {
    let mut b = Vec::with_capacity(11);
    b.extend_from_slice(&((centre_hz / 10.0).round() as u32).to_be_bytes());
    let bandwidth = match mode.bandwidth {
        Bandwidth::Mhz8 => 0u8,
        Bandwidth::Mhz7 => 1,
        Bandwidth::Mhz6 => 2,
    };
    // bandwidth(3), priority(1: HP), time_slicing(1: not used),
    // MPE-FEC(1: not used), reserved(2).
    b.push(bandwidth << 5 | 0x1F);
    let constellation = match mode.modulation {
        Modulation::Qpsk => 0u8,
        Modulation::Qam16 => 1,
        Modulation::Qam64 => 2,
    };
    // Table 50: 1/2 2/3 3/4 5/6 7/8 are 0 to 4.
    let rate = |r: decdvbt_core::dvbt::Rate| match r.fraction() {
        (1, 2) => 0u8,
        (2, 3) => 1,
        (3, 4) => 2,
        (5, 6) => 3,
        _ => 4,
    };
    // constellation(2), hierarchy_information(3: none, native interleaver),
    // code_rate_HP(3).
    b.push(constellation << 6 | rate(mode.rate_hp));
    let guard = match mode.guard.divisor() {
        32 => 0u8,
        16 => 1,
        8 => 2,
        _ => 3,
    };
    let tm = match mode.fft {
        FftSize::K2 => 0u8,
        FftSize::K8 => 1,
    };
    // code_rate_LP(3: unused), guard_interval(2), transmission_mode(2),
    // other_frequency_flag(1).
    b.push(guard << 3 | tm << 1);
    b.extend_from_slice(&[0xFF; 4]); // reserved_future_use
    descriptor(0x5A, &b)
}

/// A long-form section: header, `body`, CRC-32 (ISO/IEC 13818-1 §2.4.4.11).
/// `last_section_number` is 0 for the one section we send.
fn long_section(table_id: u8, id_ext: u16, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4; // after section_length: fixed part, body, CRC
    let mut s = vec![
        table_id,
        // section_syntax_indicator, reserved_future_use, reserved, length
        0xF0 | ((len >> 8) as u8 & 0x0F),
        len as u8,
        (id_ext >> 8) as u8,
        id_ext as u8,
        0xC1, // version 0, current
        0,    // section_number
        0,    // last_section_number
    ];
    s.extend_from_slice(body);
    let crc = crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

/// The NIT (table_id 0x40, PID 0x10) as one section: network name descriptor,
/// one transport stream loop with the service list (0x41, type from the
/// video codec) and the terrestrial delivery descriptor.
pub fn nit_section(cfg: &SiConfig, service_type: u8) -> Vec<u8> {
    let s = &cfg.service;
    let name = crate::ffmpeg::sanitise_name(&s.network_name);
    let net_desc = descriptor(0x40, name.as_bytes());

    let mut ts_desc = descriptor(
        0x41,
        &[(s.service_id >> 8) as u8, s.service_id as u8, service_type],
    );
    ts_desc.extend(terrestrial_delivery_descriptor(&cfg.mode, cfg.centre_hz));

    let mut b = Vec::new();
    b.extend_from_slice(&(0xF000 | net_desc.len() as u16).to_be_bytes());
    b.extend_from_slice(&net_desc);
    let entry_len = 6 + ts_desc.len();
    b.extend_from_slice(&(0xF000 | entry_len as u16).to_be_bytes());
    b.extend_from_slice(&s.transport_stream_id.to_be_bytes());
    b.extend_from_slice(&s.original_network_id.to_be_bytes());
    b.extend_from_slice(&(0xF000 | ts_desc.len() as u16).to_be_bytes());
    b.extend_from_slice(&ts_desc);
    long_section(0x40, s.network_id, &b)
}

/// UTC as a DVB time: 16 bits of Modified Julian Date, then hours, minutes and
/// seconds in BCD (EN 300 468 Annex C).
fn dvb_time(secs_since_1970: u64) -> [u8; 5] {
    let mjd = 40587 + secs_since_1970 / 86400;
    let day = secs_since_1970 % 86400;
    let bcd = |v: u64| (((v / 10) << 4) | (v % 10)) as u8;
    [
        (mjd >> 8) as u8,
        mjd as u8,
        bcd(day / 3600),
        bcd(day / 60 % 60),
        bcd(day % 60),
    ]
}

fn unix_secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A TDT (table_id 0x70, short section, no CRC): UTC as MJD + BCD.
pub fn tdt_section(now: SystemTime) -> Vec<u8> {
    let mut s = vec![0x70, 0x70, 5]; // short section: indicator 0, length 5
    s.extend_from_slice(&dvb_time(unix_secs(now)));
    s
}

/// A TOT (table_id 0x73): the TDT's time plus a local_time_offset
/// descriptor (country "GBR", region 0, the PC's offset), CRC-32.
pub fn tot_section(now: SystemTime, utc_offset_s: i32) -> Vec<u8> {
    let secs = unix_secs(now);
    let off = utc_offset_s.unsigned_abs() as u64;
    let bcd = |v: u64| (((v / 10) << 4) | (v % 10)) as u8;
    let offset = [bcd(off / 3600), bcd(off / 60 % 60)];
    // country_code(24), country_region_id(6) reserved(1) polarity(1),
    // local_time_offset(16), time_of_change(40), next_time_offset(16).
    let mut d = Vec::with_capacity(13);
    d.extend_from_slice(b"GBR");
    d.push((utc_offset_s < 0) as u8);
    d.extend_from_slice(&offset);
    d.extend_from_slice(&dvb_time(secs));
    d.extend_from_slice(&offset);
    let desc = descriptor(0x58, &d);

    let mut body = dvb_time(secs).to_vec();
    body.extend_from_slice(&(0xF000 | desc.len() as u16).to_be_bytes());
    body.extend_from_slice(&desc);
    let len = body.len() + 4;
    let mut s = vec![0x73, 0x70 | (len >> 8) as u8, len as u8];
    s.extend_from_slice(&body);
    let crc = crc32_mpeg2(&s);
    s.extend_from_slice(&crc.to_be_bytes());
    s
}

/// One section as a packet on `pid`: pointer field 0, the section, 0xFF fill.
fn section_packet(pid: u16, cc: u8, section: &[u8]) -> [u8; TS_LEN] {
    assert!(section.len() <= TS_LEN - 5, "section too long for a packet");
    let mut p = [0xFFu8; TS_LEN];
    p[0] = TS_SYNC;
    p[1] = 0x40 | (pid >> 8) as u8 & 0x1F; // payload_unit_start_indicator
    p[2] = pid as u8;
    p[3] = 0x10 | (cc & 0x0F); // payload only
    p[4] = 0;
    p[5..5 + section.len()].copy_from_slice(section);
    p
}

/// Swaps packets in place. Cheap: a PID test on every packet, a section
/// build once a second.
pub struct SiInjector {
    cfg: SiConfig,
    nit_packet: [u8; TS_LEN],
    /// The TDT and TOT share PID 0x14, so one continuity counter.
    cc_tdt: u8,
    /// Packets since the last TDT and TOT went out. Start "due", so the
    /// first nulls carry the time.
    packets_since_tdt: u64,
    packets_since_tot: u64,
}

impl SiInjector {
    pub fn new(cfg: SiConfig, service_type: u8) -> SiInjector {
        let nit_packet = section_packet(PID_NIT, 0, &nit_section(&cfg, service_type));
        SiInjector {
            cfg,
            nit_packet,
            cc_tdt: 0,
            packets_since_tdt: u64::MAX / 2,
            packets_since_tot: u64::MAX / 2,
        }
    }

    /// Look at one packet from ffmpeg and rewrite it if it is the NIT's, or
    /// a null that a table is due in. `packets_per_second` is the mux rate
    /// over 1504 bits, for the timers.
    pub fn process(&mut self, packet: &mut [u8; TS_LEN], packets_per_second: f64) {
        self.packets_since_tdt = self.packets_since_tdt.saturating_add(1);
        self.packets_since_tot = self.packets_since_tot.saturating_add(1);
        let pid = u16::from(packet[1] & 0x1F) << 8 | u16::from(packet[2]);
        match pid {
            PID_NIT => {
                // ffmpeg's counter runs on; ours takes its value.
                let cc = packet[3] & 0x0F;
                *packet = self.nit_packet;
                packet[3] = 0x10 | cc;
            }
            PID_NULL => {
                let due = packets_per_second.max(1.0) as u64;
                if self.packets_since_tdt >= due {
                    let now = SystemTime::now();
                    *packet = section_packet(PID_TDT, self.cc_tdt, &tdt_section(now));
                    self.cc_tdt = (self.cc_tdt + 1) & 0x0F;
                    self.packets_since_tdt = 0;
                } else if self.packets_since_tot >= due {
                    let now = SystemTime::now();
                    let tot = tot_section(now, self.cfg.utc_offset_s);
                    *packet = section_packet(PID_TDT, self.cc_tdt, &tot);
                    // The TDT and TOT share PID 0x14: one counter for both.
                    self.cc_tdt = (self.cc_tdt + 1) & 0x0F;
                    self.packets_since_tot = 0;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvbt_core::dvbt::{Guard, Rate};
    use decdvbt_ts::TsAnalyser;
    use std::time::Duration;

    fn cfg() -> SiConfig {
        SiConfig {
            service: ServiceInfo {
                name: "Test clip".into(),
                ..Default::default()
            },
            mode: Mode::uk(Modulation::Qam64, Rate::R2_3),
            centre_hz: 546_000_000.0,
            utc_offset_s: 3600,
        }
    }

    #[test]
    fn the_delivery_descriptor_is_eleven_bytes_of_the_right_fields() {
        let d = terrestrial_delivery_descriptor(&cfg().mode, 546_000_000.0);
        assert_eq!(d[0], 0x5A);
        assert_eq!(d[1], 11);
        assert_eq!(u32::from_be_bytes([d[2], d[3], d[4], d[5]]), 54_600_000);
        assert_eq!(d[6] >> 5, 0); // 8 MHz
        assert_eq!(d[7] >> 6, 2); // 64-QAM
        assert_eq!(d[7] & 7, 1); // 2/3
        assert_eq!((d[8] >> 3) & 3, 0); // 1/32
        assert_eq!((d[8] >> 1) & 3, 1); // 8k
        let mut m = cfg().mode;
        m.guard = Guard::G1_4;
        m.fft = FftSize::K2;
        m.bandwidth = Bandwidth::Mhz7;
        let d = terrestrial_delivery_descriptor(&m, 600e6);
        assert_eq!(d[6] >> 5, 1);
        assert_eq!((d[8] >> 3) & 3, 3);
        assert_eq!((d[8] >> 1) & 3, 0);
    }

    #[test]
    fn sections_have_a_valid_crc_and_the_right_numbers() {
        let c = cfg();
        for s in [nit_section(&c, 0x19), tot_section(SystemTime::now(), 3600)] {
            // The CRC over a whole section, its own included, is zero.
            assert_eq!(crc32_mpeg2(&s), 0);
        }
        let nit = nit_section(&c, 0x19);
        assert_eq!(nit[0], 0x40);
        assert_eq!(nit[6], 0); // section_number
        assert_eq!(nit[7], 0); // last_section_number
        let len = (usize::from(nit[1] & 0x0F) << 8) | usize::from(nit[2]);
        assert_eq!(nit.len(), len + 3);
        let tdt = tdt_section(SystemTime::now());
        assert_eq!((tdt[0], tdt.len()), (0x70, 8));
    }

    #[test]
    fn the_time_is_mjd_and_bcd() {
        // 2026-10-25 13:45:09 UTC is MJD 61338; 1_792_935_909 s after 1970.
        let t = UNIX_EPOCH + Duration::from_secs(1_792_935_909);
        let d = dvb_time(unix_secs(t));
        assert_eq!(u16::from_be_bytes([d[0], d[1]]), 61338);
        assert_eq!(&d[2..], &[0x13, 0x45, 0x09]);
    }

    #[test]
    fn the_analyser_reads_back_what_a_receiver_would() {
        let c = cfg();
        let mut inj = SiInjector::new(c.clone(), 0x19);
        let mut an = TsAnalyser::new();
        // An ffmpeg-like stream: a bare NIT packet and nulls.
        let mut nit_in = [0xFFu8; TS_LEN];
        nit_in[..5].copy_from_slice(&[0x47, 0x40, 0x10, 0x15, 0]);
        let pps = 100.0;
        for i in 0..1000 {
            let mut p = if i % 50 == 0 {
                nit_in
            } else {
                let mut n = [0xFFu8; TS_LEN];
                n[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
                n
            };
            inj.process(&mut p, pps);
            an.packet(&p);
        }
        let r = an.report();
        assert_eq!(r.network.name.as_deref(), Some("DecDVB-T"));
        assert_eq!(r.network.network_id, Some(0xFF01));
        let t = &r.network.transponders[0];
        assert_eq!((t.ts_id, t.onid), (1, 0xFF01));
        assert_eq!(t.services, vec![(1, 0x19)]);
        let d = t.terrestrial.unwrap();
        assert_eq!(d.centre_hz, 546_000_000);
        assert_eq!(d.describe(), "64-QAM 2/3 8k 1/32");
        assert_eq!(d.bandwidth_mhz, 8);
        assert!(r.utc_unix.is_some());
        assert!(r.local_time.is_some());
        assert_eq!(r.bad_sections, 0);
        // The NIT's continuity counter ran on from ffmpeg's: no jump.
        assert_eq!(an.pids[&0x10].cc_errors, 0);
        assert_eq!(an.pids[&0x14].cc_errors, 0);
    }

    #[test]
    fn the_stream_keeps_its_length_and_only_nulls_and_the_nit_change() {
        let mut inj = SiInjector::new(cfg(), 0x19);
        let mut data = [0xFFu8; TS_LEN];
        data[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10]);
        let before = data;
        for _ in 0..500 {
            inj.process(&mut data, 100.0);
            assert_eq!(data, before);
        }
    }

    #[test]
    fn tables_come_about_once_a_second_each() {
        let mut inj = SiInjector::new(cfg(), 0x19);
        let (mut tdt, mut tot) = (0, 0);
        for _ in 0..1000 {
            let mut n = [0xFFu8; TS_LEN];
            n[..4].copy_from_slice(&[0x47, 0x1F, 0xFF, 0x10]);
            inj.process(&mut n, 100.0);
            if n[2] == 0x14 {
                match n[5] {
                    0x70 => tdt += 1,
                    0x73 => tot += 1,
                    _ => panic!(),
                }
            }
        }
        // 10 s of stream at 100 packets a second.
        assert!((10..=11).contains(&tdt), "{tdt}");
        assert!((10..=11).contains(&tot), "{tot}");
    }
}
