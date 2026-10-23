// made by CasualArclamp
//! Baseband frames (EN 302 755 §5.1): the 80-bit BBHEADER and the data field
//! of transport packets that follows it.
//!
//! In normal mode (NM) each user packet keeps its length (UPL = 1504 bits) but
//! its sync byte is replaced by the CRC-8 of the packet before it; in high
//! efficiency mode (HEM) the sync bytes are removed and packets are 187 bytes,
//! each optionally followed by a count of deleted null packets (the ISSY field
//! of the first packet of a frame, when ISSYI is set, takes the place of UPL
//! and SYNC in the header: the UK's multiplexes are HEM with ISSYI and 187-byte
//! units, which their SYNCD values show). Packets run on across frames: SYNCD
//! says where the first one that starts in a frame does.
//!
//! The header's CRC-8 (g = x^8 + x^7 + x^6 + x^4 + x^2 + 1) has its last bit
//! inverted in HEM, which is how a receiver tells the two apart.

/// Bytes of the BBHEADER.
pub const HEADER_BYTES: usize = 10;
pub const TS_LEN: usize = 188;
pub const TS_SYNC: u8 = 0x47;

/// CRC-8 over bytes, MSB first, register starting at zero.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0xD5
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BbHeader {
    /// 0 for a generic packetized stream, 1 continuous GS, 2 GSE, 3 TS.
    pub ts_gs: u8,
    /// Single input stream (true) or multiple.
    pub single: bool,
    /// Constant coding and modulation (true).
    pub ccm: bool,
    pub issyi: bool,
    pub npd: bool,
    pub ext: u8,
    /// The second MATYPE byte: the PLP's id or the input stream identifier.
    pub matype2: u8,
    /// User packet length, bits (0 in HEM for TS).
    pub upl: u16,
    /// Data field length, bits.
    pub dfl: u16,
    pub sync: u8,
    pub syncd: u16,
}

impl BbHeader {
    /// A transport stream in normal mode.
    pub fn ts_nm(plp_id: u8, dfl_bits: usize, syncd_bits: usize) -> BbHeader {
        BbHeader {
            ts_gs: 3,
            single: true,
            ccm: true,
            issyi: false,
            npd: false,
            ext: 0,
            matype2: plp_id,
            upl: 1504,
            dfl: dfl_bits as u16,
            sync: TS_SYNC,
            syncd: syncd_bits as u16,
        }
    }

    pub fn to_bytes(&self, hem: bool) -> [u8; HEADER_BYTES] {
        let mut h = [0u8; HEADER_BYTES];
        h[0] = (self.ts_gs << 6)
            | (u8::from(self.single) << 5)
            | (u8::from(self.ccm) << 4)
            | (u8::from(self.issyi) << 3)
            | (u8::from(self.npd) << 2)
            | (self.ext & 3);
        h[1] = self.matype2;
        h[2..4].copy_from_slice(&self.upl.to_be_bytes());
        h[4..6].copy_from_slice(&self.dfl.to_be_bytes());
        h[6] = self.sync;
        h[7..9].copy_from_slice(&self.syncd.to_be_bytes());
        h[9] = crc8(&h[..9]) ^ u8::from(hem);
        h
    }

    /// Read a header; the flag is true for high efficiency mode. `None` if the
    /// CRC fits neither.
    pub fn from_bytes(h: &[u8]) -> Option<(BbHeader, bool)> {
        if h.len() < HEADER_BYTES {
            return None;
        }
        let c = crc8(&h[..9]);
        let hem = match h[9] ^ c {
            0 => false,
            1 => true,
            _ => return None,
        };
        let m1 = h[0];
        Some((
            BbHeader {
                ts_gs: m1 >> 6,
                single: m1 & 0x20 != 0,
                ccm: m1 & 0x10 != 0,
                issyi: m1 & 0x08 != 0,
                npd: m1 & 0x04 != 0,
                ext: m1 & 3,
                matype2: h[1],
                upl: u16::from_be_bytes([h[2], h[3]]),
                dfl: u16::from_be_bytes([h[4], h[5]]),
                sync: h[6],
                syncd: u16::from_be_bytes([h[7], h[8]]),
            },
            hem,
        ))
    }
}

/// Makes BBFRAMEs from transport packets in normal mode.
#[derive(Default)]
pub struct NmPacker {
    /// The bytes of the packet cut off at the last frame's end.
    pending: Vec<u8>,
    /// The CRC-8 of the packet before the next one.
    crc: u8,
}

impl NmPacker {
    /// A BBFRAME of `k_bch_bytes` bytes, pulling packets from `next`.
    pub fn frame(
        &mut self,
        plp_id: u8,
        k_bch_bytes: usize,
        mut next: impl FnMut(&mut [u8; TS_LEN]),
    ) -> Vec<u8> {
        let dfl = k_bch_bytes - HEADER_BYTES;
        let syncd = self.pending.len() * 8;
        let mut field = std::mem::take(&mut self.pending);
        let mut packet = [0u8; TS_LEN];
        while field.len() < dfl {
            next(&mut packet);
            let mut up = packet;
            up[0] = self.crc;
            self.crc = crc8(&packet[1..]);
            field.extend_from_slice(&up);
        }
        self.pending = field.split_off(dfl);
        let h = BbHeader::ts_nm(plp_id, dfl * 8, syncd);
        let mut out = h.to_bytes(false).to_vec();
        out.extend_from_slice(&field);
        out
    }
}

/// Recovers transport packets from BBFRAMEs.
#[derive(Default)]
pub struct Unpacker {
    partial: Vec<u8>,
    /// Frames that did not continue the last one (a lost frame in between).
    pub resyncs: u64,
    /// Packets whose CRC-8 (in the next packet's sync byte) did not fit.
    pub crc_errors: u64,
    /// The last packet's CRC-8, to check the next's first byte against.
    last_crc: Option<u8>,
}

impl Unpacker {
    /// Forget a packet cut by a lost frame.
    pub fn reset(&mut self) {
        self.partial.clear();
        self.last_crc = None;
    }

    /// The packets of a BBFRAME (header and data field), pushed to `out` as
    /// 188-byte packets with their sync byte restored. Returns the header.
    pub fn push(&mut self, bbframe: &[u8], out: &mut Vec<[u8; TS_LEN]>) -> Option<BbHeader> {
        let (h, hem) = BbHeader::from_bytes(bbframe)?;
        let dfl = h.dfl as usize / 8;
        let field = bbframe.get(HEADER_BYTES..HEADER_BYTES + dfl)?;
        if h.ts_gs != 3 {
            return Some(h); // not a transport stream
        }
        if hem {
            return self.push_hem(&h, field, out).then_some(h);
        }
        let upl = h.upl as usize / 8;
        if upl != TS_LEN {
            return Some(h);
        }
        let syncd = h.syncd as usize / 8;
        let mut at = 0;
        if !self.partial.is_empty() {
            // The first `syncd` bytes finish the packet begun before.
            if self.partial.len() + syncd == TS_LEN && syncd <= field.len() {
                self.partial.extend_from_slice(&field[..syncd]);
                let p = std::mem::take(&mut self.partial);
                self.emit(&p, &h, out);
            } else {
                self.partial.clear();
                self.last_crc = None;
                self.resyncs += 1;
            }
        }
        at += syncd.min(field.len());
        if h.syncd == u16::MAX {
            return Some(h);
        }
        while at + TS_LEN <= field.len() {
            self.emit(&field[at..at + TS_LEN], &h, out);
            at += TS_LEN;
        }
        self.partial.extend_from_slice(&field[at..]);
        Some(h)
    }

    fn emit(&mut self, up: &[u8], h: &BbHeader, out: &mut Vec<[u8; TS_LEN]>) {
        let mut p = [0u8; TS_LEN];
        p.copy_from_slice(up);
        if let Some(c) = self.last_crc
            && p[0] != c
        {
            self.crc_errors += 1;
        }
        self.last_crc = Some(crc8(&p[1..]));
        p[0] = h.sync;
        out.push(p);
    }

    /// High efficiency mode: 187-byte packets (sync byte removed), each
    /// followed by a count of deleted null packets if NPD. Deleted null
    /// packets are put back, ahead of the packet they were deleted before.
    fn push_hem(&mut self, h: &BbHeader, field: &[u8], out: &mut Vec<[u8; TS_LEN]>) -> bool {
        let unit = 187 + usize::from(h.npd);
        let syncd = h.syncd as usize / 8;
        let mut at = 0;
        if !self.partial.is_empty() {
            if self.partial.len() + syncd == unit && syncd <= field.len() {
                self.partial.extend_from_slice(&field[..syncd]);
                let p = std::mem::take(&mut self.partial);
                Self::emit_hem(&p, h, out);
            } else {
                self.partial.clear();
                self.resyncs += 1;
            }
        }
        at += syncd.min(field.len());
        while at + unit <= field.len() {
            Self::emit_hem(&field[at..at + unit], h, out);
            at += unit;
        }
        self.partial.extend_from_slice(&field[at..]);
        true
    }

    fn emit_hem(unit: &[u8], h: &BbHeader, out: &mut Vec<[u8; TS_LEN]>) {
        if h.npd {
            let dnp = unit[unit.len() - 1];
            for _ in 0..dnp {
                let mut null = [0xFFu8; TS_LEN];
                null[0] = h.sync.max(TS_SYNC);
                null[1] = 0x1F;
                null[2] = 0xFF;
                null[3] = 0x10;
                out.push(null);
            }
        }
        let mut p = [0u8; TS_LEN];
        p[0] = TS_SYNC;
        p[1..].copy_from_slice(&unit[..187]);
        out.push(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_round_trips_in_both_modes() {
        let h = BbHeader::ts_nm(0, 42960, 1000);
        for hem in [false, true] {
            let b = h.to_bytes(hem);
            let (back, m) = BbHeader::from_bytes(&b).unwrap();
            assert_eq!(back, h);
            assert_eq!(m, hem);
        }
        let mut bad = h.to_bytes(false);
        bad[3] ^= 0x10;
        assert!(BbHeader::from_bytes(&bad).is_none());
    }

    #[test]
    fn the_crc_8_matches_the_standards_check_value() {
        // CRC-8 with g = 0xD5, init 0, no reflection: the catalogue's
        // CRC-8/DVB-S2, check value 0xBC.
        assert_eq!(crc8(b"123456789"), 0xBC);
    }

    fn packets(n: usize) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|i| {
                let mut p = [0u8; TS_LEN];
                p[0] = TS_SYNC;
                p[1] = (i >> 8) as u8 & 0x1F;
                p[2] = i as u8;
                for (j, b) in p.iter_mut().enumerate().skip(3) {
                    *b = (i * 7 + j * 13) as u8;
                }
                p
            })
            .collect()
    }

    #[test]
    fn packets_survive_frames_that_cut_them() {
        let src = packets(200);
        let mut it = src.iter().cycle();
        let mut packer = NmPacker::default();
        let mut un = Unpacker::default();
        let mut got = Vec::new();
        // 5380-byte frames: 28 whole packets and 106 bytes of a 29th.
        for _ in 0..12 {
            let f = packer.frame(0, 5380, |p| *p = *it.next().unwrap());
            assert_eq!(f.len(), 5380);
            un.push(&f, &mut got).unwrap();
        }
        assert!(got.len() >= 12 * 28);
        for (i, p) in got.iter().enumerate() {
            assert_eq!(p, &src[i % 200], "packet {i}");
        }
        assert_eq!(un.crc_errors, 0);
        assert_eq!(un.resyncs, 0);
    }

    #[test]
    fn a_lost_frame_is_a_resync_not_garbage() {
        let src = packets(300);
        let mut it = src.iter().cycle();
        let mut packer = NmPacker::default();
        let mut un = Unpacker::default();
        let mut got = Vec::new();
        let frames: Vec<Vec<u8>> = (0..6)
            .map(|_| packer.frame(0, 5380, |p| *p = *it.next().unwrap()))
            .collect();
        for (i, f) in frames.iter().enumerate() {
            if i == 2 {
                continue;
            }
            un.push(f, &mut got).unwrap();
        }
        assert_eq!(un.resyncs, 1);
        // Everything that came out is a whole, correct packet.
        for p in &got {
            assert_eq!(p[0], TS_SYNC);
            let i = (usize::from(p[1] & 0x1F) << 8) | usize::from(p[2]);
            assert_eq!(p, &src[i]);
        }
    }
}
