// made by CasualArclamp
//! Transport streams to modulate: a test stream (PSI/SI and a payload the
//! receiver can check, packet by packet), or a `.ts` file.
//!
//! The test stream is every packet useful (the modulator pulls packets at
//! the channel's rate, so there is no null padding): a PAT, PMT, SDT and
//! NIT repeated now and then, and between them packets of PID 0x101 whose
//! payload is a counter and a pseudo-random fill seeded by it, so a receiver
//! can tell a good packet from a bad one without having seen the sequence
//! ([`check_test_payload`]).

use std::io::Read;
use std::path::Path;

use decdvbt_core::crc::crc32_mpeg2;

pub use decdvbt_fec::outer::{TS_LEN, TS_SYNC};

pub const PID_PAT: u16 = 0x0000;
pub const PID_NIT: u16 = 0x0010;
pub const PID_SDT: u16 = 0x0011;
pub const PID_PMT: u16 = 0x0100;
pub const PID_DATA: u16 = 0x0101;

/// Where transport packets come from.
pub trait TsSource: Send {
    /// The next 188-byte packet, sync byte 0x47 first.
    fn next_packet(&mut self, out: &mut [u8; TS_LEN]);
}

/// One section as a TS packet on `pid`: pointer field 0, the section, padding.
fn section_packet(pid: u16, cc: u8, section: &[u8]) -> [u8; TS_LEN] {
    assert!(
        section.len() <= TS_LEN - 5,
        "section too long for one packet"
    );
    let mut p = [0xFFu8; TS_LEN];
    p[0] = TS_SYNC;
    p[1] = 0x40 | (pid >> 8) as u8; // payload_unit_start_indicator
    p[2] = pid as u8;
    p[3] = 0x10 | (cc & 0x0F); // payload only
    p[4] = 0; // pointer_field
    p[5..5 + section.len()].copy_from_slice(section);
    p
}

/// A long-form PSI section: header, `body`, CRC-32 (ISO/IEC 13818-1
/// §2.4.4.11; the SI tables of EN 300 468 share the layout).
fn long_section(table_id: u8, id_ext: u16, private_bits: u8, body: &[u8]) -> Vec<u8> {
    let len = 5 + body.len() + 4; // after section_length: fixed part, body, CRC
    let mut s = vec![
        table_id,
        0x80 | private_bits | 0x30 | ((len >> 8) as u8 & 0x0F), // syntax indicator, reserved
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

fn pat() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&1u16.to_be_bytes()); // program 1
    b.extend_from_slice(&(0xE000 | PID_PMT).to_be_bytes());
    long_section(0x00, 1, 0, &b)
}

fn pmt() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(0xE000 | PID_DATA).to_be_bytes()); // PCR PID
    b.extend_from_slice(&0xF000u16.to_be_bytes()); // no program info
    b.push(0x06); // PES private data
    b.extend_from_slice(&(0xE000 | PID_DATA).to_be_bytes());
    b.extend_from_slice(&0xF000u16.to_be_bytes());
    long_section(0x02, 1, 0, &b)
}

/// The UK's original network id (ETSI TS 101 162).
const ONID: u16 = 0x233A;

fn sdt() -> Vec<u8> {
    let (provider, name) = (b"DecDVB-T", b"Test stream");
    let mut d = vec![0x48, (3 + provider.len() + name.len()) as u8, 0x01]; // digital TV
    d.push(provider.len() as u8);
    d.extend_from_slice(provider);
    d.push(name.len() as u8);
    d.extend_from_slice(name);
    let mut b = Vec::new();
    b.extend_from_slice(&ONID.to_be_bytes());
    b.push(0xFF);
    b.extend_from_slice(&1u16.to_be_bytes()); // service 1
    b.push(0xFC); // no EIT
    // running (4), not scrambled, descriptors' length
    b.extend_from_slice(&(0x8000 | d.len() as u16).to_be_bytes());
    b.extend_from_slice(&d);
    long_section(0x42, 1, 0x40, &b)
}

fn nit() -> Vec<u8> {
    let name = b"DecDVB-T network";
    let mut nd = vec![0x40, name.len() as u8];
    nd.extend_from_slice(name);
    let mut b = Vec::new();
    b.extend_from_slice(&(0xF000 | nd.len() as u16).to_be_bytes());
    b.extend_from_slice(&nd);
    b.extend_from_slice(&0xF005u16.to_be_bytes()); // transport stream loop: 6 bytes
    b.extend_from_slice(&1u16.to_be_bytes()); // transport_stream_id
    b.extend_from_slice(&ONID.to_be_bytes());
    b.extend_from_slice(&0xF000u16.to_be_bytes()); // no descriptors
    // The loop length counts the entry (6 bytes): fix it up.
    let at = 2 + nd.len();
    b[at..at + 2].copy_from_slice(&0xF006u16.to_be_bytes());
    long_section(0x40, 0x3001, 0, &b)
}

/// One packet's payload fill: a xorshift32 seeded by the counter.
fn fill(counter: u32, out: &mut [u8]) {
    let mut x = counter.wrapping_mul(2_654_435_761) | 1;
    for b in out {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *b = (x >> 8) as u8;
    }
}

/// Packets between repeats of the PSI/SI group of four.
const SI_PERIOD: u32 = 400;

/// The test stream.
pub struct TestStream {
    index: u32,
    counter: u32,
    cc: [u8; 5],
    tables: [Vec<u8>; 4],
}

impl Default for TestStream {
    fn default() -> Self {
        TestStream {
            index: 0,
            counter: 0,
            cc: [0; 5],
            tables: [pat(), pmt(), sdt(), nit()],
        }
    }
}

impl TsSource for TestStream {
    fn next_packet(&mut self, out: &mut [u8; TS_LEN]) {
        let slot = self.index % SI_PERIOD;
        self.index = self.index.wrapping_add(1);
        if slot < 4 {
            let pid = [PID_PAT, PID_PMT, PID_SDT, PID_NIT][slot as usize];
            let cc = &mut self.cc[slot as usize];
            *out = section_packet(pid, *cc, &self.tables[slot as usize]);
            *cc = (*cc + 1) & 0x0F;
            return;
        }
        out[0] = TS_SYNC;
        out[1] = (PID_DATA >> 8) as u8;
        out[2] = PID_DATA as u8;
        out[3] = 0x10 | self.cc[4];
        self.cc[4] = (self.cc[4] + 1) & 0x0F;
        out[4..8].copy_from_slice(&self.counter.to_be_bytes());
        fill(self.counter, &mut out[8..]);
        self.counter = self.counter.wrapping_add(1);
    }
}

/// If `p` is a good test-stream data packet, its counter. A receiver counts
/// these to measure packets lost or corrupted past the FEC.
pub fn check_test_payload(p: &[u8]) -> Option<u32> {
    if p.len() != TS_LEN
        || p[0] != TS_SYNC
        || u16::from(p[1] & 0x1F) << 8 | u16::from(p[2]) != PID_DATA
    {
        return None;
    }
    let counter = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
    let mut want = [0u8; TS_LEN - 8];
    fill(counter, &mut want);
    (p[8..] == want).then_some(counter)
}

/// A `.ts` file, looped. The file must be packet-aligned (starts on a 0x47).
pub struct FileTs {
    data: Vec<u8>,
    at: usize,
}

impl FileTs {
    pub fn open(path: &Path) -> Result<FileTs, String> {
        let mut data = Vec::new();
        std::fs::File::open(path)
            .and_then(|mut f| f.read_to_end(&mut data))
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        // Start on the first sync byte that has another a packet on.
        let start = (0..data.len().saturating_sub(TS_LEN))
            .find(|&i| data[i] == TS_SYNC && data[i + TS_LEN] == TS_SYNC)
            .ok_or_else(|| format!("{} holds no transport stream", path.display()))?;
        data.drain(..start);
        data.truncate(data.len() / TS_LEN * TS_LEN);
        if data.is_empty() {
            return Err(format!("{} holds no whole packet", path.display()));
        }
        Ok(FileTs { data, at: 0 })
    }
}

impl TsSource for FileTs {
    fn next_packet(&mut self, out: &mut [u8; TS_LEN]) {
        out.copy_from_slice(&self.data[self.at..self.at + TS_LEN]);
        self.at = (self.at + TS_LEN) % self.data.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_stream_is_valid_packets_with_checkable_payloads() {
        let mut s = TestStream::default();
        let mut p = [0u8; TS_LEN];
        let (mut si, mut data) = (0, 0);
        for n in 0..1000 {
            s.next_packet(&mut p);
            assert_eq!(p[0], TS_SYNC);
            match check_test_payload(&p) {
                Some(c) => {
                    assert_eq!(c, data, "packet {n}");
                    data += 1;
                }
                None => si += 1,
            }
        }
        // Four tables every 400 packets.
        assert_eq!(si, 12);
        assert_eq!(data, 988);
        // A flipped bit is no longer a good packet.
        s.next_packet(&mut p);
        p[100] ^= 1;
        assert_eq!(check_test_payload(&p), None);
    }

    #[test]
    fn the_si_sections_have_valid_crcs() {
        // A section's CRC-32 over the whole section, itself included, is 0.
        for t in [pat(), pmt(), sdt(), nit()] {
            assert_eq!(crc32_mpeg2(&t), 0, "table {:#x}", t[0]);
            // section_length counts what follows it.
            let len = usize::from(t[1] & 0x0F) << 8 | usize::from(t[2]);
            assert_eq!(len, t.len() - 3, "table {:#x}", t[0]);
        }
    }

    #[test]
    fn the_si_packets_parse_with_the_ts_crate() {
        // The receiver's own analyser reads the service and network back.
        let mut s = TestStream::default();
        let mut a = decdvbt_ts::TsAnalyser::new();
        let mut p = [0u8; TS_LEN];
        for _ in 0..8 {
            s.next_packet(&mut p);
            a.packet(&p);
        }
        let r = a.report();
        assert_eq!(r.programmes.len(), 1);
        let prog = &r.programmes[0];
        assert_eq!(prog.number, 1);
        assert_eq!(prog.pmt_pid, PID_PMT);
        assert_eq!(prog.name.as_deref(), Some("Test stream"));
        assert_eq!(prog.provider.as_deref(), Some("DecDVB-T"));
        assert_eq!(r.network.name.as_deref(), Some("DecDVB-T network"));
        assert_eq!(r.network.network_id, Some(0x3001));
        assert_eq!(r.bad_sections, 0);
    }
}
