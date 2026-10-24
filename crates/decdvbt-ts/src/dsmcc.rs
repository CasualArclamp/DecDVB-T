// made by CasualArclamp
//! DSM-CC object carousels (ISO/IEC 13818-6; their use in DVB: EN 301 192
//! §8 and TR 101 202): the file system the UK's interactive services (MHEG-5,
//! "Red Button") are broadcast in.
//!
//! A carousel is sent as sections on one or more PIDs:
//!
//! - **DSI** (table 0x3B, message 0x1006): the root, the *service gateway*,
//!   as an object reference;
//! - **DII** (table 0x3B, message 0x1002): the modules, their sizes and
//!   versions, and whether each is zlib-compressed;
//! - **DDB** (table 0x3C, message 0x1003): a block of a module.
//!
//! A complete module is a run of BIOP messages ([`crate::biop`]), each an
//! object: a directory, a file or a stream (event) object. Directories bind
//! names to object references (module, object key), so the tree is walked
//! from the service gateway on demand.
//!
//! Cost: only the carousel's own PIDs are looked at; a DDB block of a module
//! that is already complete is dropped after reading its 8-byte header, before
//! the CRC. So a carousel that has all come in costs a few compares a packet.
//!
//! The layout follows MythTV's DSM-CC code (`libmythtv/dsmcc*.cpp`, after
//! libdsmcc; GPL-2.0-or-later), rewritten.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use decdvbt_core::crc::crc32_mpeg2;

use crate::TS_LEN;
use crate::biop::{self, Binding, Object, ObjectKind, ObjectRef, Tap};
use crate::psi::Programme;
use crate::section::SectionAssembler;

/// dsmccMessageHeader's protocolDiscriminator and dsmccType (ISO/IEC 13818-6
/// §2.1, Table 2-1; type 0x03 is U-N download).
const PROTOCOL: u8 = 0x11;
const TYPE_DOWNLOAD: u8 = 0x03;
/// messageId (ISO/IEC 13818-6 §7.3, Table 7-4).
const MSG_DII: u16 = 0x1002;
const MSG_DDB: u16 = 0x1003;
const MSG_DSI: u16 = 0x1006;
/// The data broadcast id of the UK MHEG-5 profile (D-Book 7 §8.1.2).
pub const DATA_BROADCAST_UK_MHEG: u16 = 0x0106;
/// Modules are capped (a broadcaster's carousel modules are tens of kB).
const MAX_MODULE: u32 = 16 << 20;

/// One module of the carousel, as the DII describes it and DDBs fill it.
struct Module {
    version: u8,
    size: u32,
    block_size: u16,
    /// zlib-compressed, and its size when inflated (EN 301 192 §9.3.4,
    /// compressed_module_descriptor tag 0x09).
    original_size: Option<u32>,
    /// Blocks seen (one flag a block).
    have: Vec<bool>,
    missing: usize,
    data: Vec<u8>,
    /// Once complete: the module's objects by key.
    objects: Option<HashMap<Vec<u8>, Object>>,
}

impl Module {
    fn new(version: u8, size: u32, block_size: u16, original_size: Option<u32>) -> Self {
        let blocks = if block_size == 0 {
            0
        } else {
            (size as usize).div_ceil(block_size as usize)
        };
        Module {
            version,
            size,
            block_size,
            original_size,
            have: vec![false; blocks],
            missing: blocks,
            data: Vec::new(),
            objects: if blocks == 0 {
                Some(HashMap::new())
            } else {
                None
            },
        }
    }
}

/// A stream event's trigger as it came (EN 301 192 §8.3, stream event
/// descriptor tag 0x1A in a table-0x3D section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    /// The component tag of the stream it came on.
    pub component_tag: u8,
    pub event_id: u16,
    /// The NPT it fires at (33 bits; "now" in UK use).
    pub npt: u64,
    pub private_data: Vec<u8>,
}

/// What a path names in the carousel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<'a> {
    File(&'a [u8]),
    Directory(Vec<String>),
    /// A stream event object: event names by id, and the component tag
    /// their triggers come on.
    Events {
        names: Vec<(String, u16)>,
        component_tag: Option<u8>,
    },
    /// A stream object, or anything else that is not a file or a directory.
    Other,
    /// Part of the path's modules have not all come in yet.
    Pending,
    NotFound,
}

/// An object carousel being received.
pub struct ObjectCarousel {
    pub carousel_id: u32,
    /// The PIDs the carousel's streams are on, with their component tags.
    streams: Vec<(u16, Option<u8>)>,
    sections: SectionAssembler,
    done: Vec<Vec<u8>>,
    /// The service gateway (from the DSI).
    gateway: Option<ObjectRef>,
    /// The DSI's transaction id: a new one starts the carousel again.
    dsi_transaction: Option<u32>,
    modules: BTreeMap<u16, Module>,
    events: Vec<StreamEvent>,
    /// Counts up whenever a module completes or is replaced (an engine
    /// reloads what it holds when it changes).
    pub generation: u64,
    pub bad_sections: u64,
}

impl ObjectCarousel {
    /// A carousel `carousel_id` sent on `streams` (PID, component tag).
    pub fn new(carousel_id: u32, streams: Vec<(u16, Option<u8>)>) -> Self {
        ObjectCarousel {
            carousel_id,
            streams,
            sections: SectionAssembler::default(),
            done: Vec::new(),
            gateway: None,
            dsi_transaction: None,
            modules: BTreeMap::new(),
            events: Vec::new(),
            generation: 0,
            bad_sections: 0,
        }
    }

    /// The UK MHEG-5 carousel of a programme, from its PMT: the stream with
    /// a carousel id (and the MHEG data broadcast id, if any stream says
    /// one), and every DSM-CC stream of the programme beside it, as modules
    /// may come on any of them.
    pub fn for_programme(p: &Programme) -> Option<Self> {
        let dsmcc = |t: u8| matches!(t, 0x0B..=0x0D);
        let boot = p
            .streams
            .iter()
            .filter(|e| dsmcc(e.stream_type) && e.carousel_id.is_some())
            .max_by_key(|e| e.data_broadcast_id == Some(DATA_BROADCAST_UK_MHEG))?;
        let streams = p
            .streams
            .iter()
            .filter(|e| dsmcc(e.stream_type))
            .map(|e| (e.pid, e.component_tag))
            .collect();
        Some(Self::new(boot.carousel_id?, streams))
    }

    /// The PIDs this carousel is read from.
    pub fn pids(&self) -> impl Iterator<Item = u16> + '_ {
        self.streams.iter().map(|s| s.0)
    }

    /// Look at a transport packet (any PID; others are ignored at once).
    pub fn packet(&mut self, p: &[u8; TS_LEN]) {
        let pid = u16::from_be_bytes([p[1] & 0x1F, p[2]]);
        if !self.streams.iter().any(|s| s.0 == pid) {
            return;
        }
        let tei = p[1] & 0x80 != 0;
        let afc = (p[3] >> 4) & 3;
        if tei || afc & 1 == 0 || p[3] >> 6 != 0 {
            return;
        }
        let at = 4 + if afc & 2 != 0 { 1 + p[4] as usize } else { 0 };
        if at >= TS_LEN {
            return;
        }
        let mut done = std::mem::take(&mut self.done);
        done.clear();
        self.sections
            .feed(pid, p[1] & 0x40 != 0, &p[at..], &mut done);
        for s in &done {
            self.section(pid, s);
        }
        self.done = done;
    }

    /// One complete DSM-CC section (ISO/IEC 13818-6 §9.2.2).
    pub fn section(&mut self, pid: u16, s: &[u8]) {
        if s.len() < 12 {
            return;
        }
        let tid = s[0];
        // A DDB's header first: a block of a module already in is dropped
        // here, before the CRC.
        if tid == 0x3C && !self.ddb_wanted(s) {
            return;
        }
        if !matches!(tid, 0x3B..=0x3D) {
            return;
        }
        // section_syntax_indicator 1: a CRC; 0: a checksum (not checked).
        if s[1] & 0x80 != 0 && crc32_mpeg2(s) != 0 {
            self.bad_sections += 1;
            return;
        }
        let body = &s[8..s.len() - 4];
        match tid {
            0x3B => self.user_network(body),
            0x3C => self.ddb(body),
            _ => {
                let tag = self.streams.iter().find(|x| x.0 == pid).and_then(|x| x.1);
                self.descriptors(tag.unwrap_or(0), body);
            }
        }
    }

    /// Stream events triggered since the last call.
    pub fn take_events(&mut self) -> Vec<StreamEvent> {
        std::mem::take(&mut self.events)
    }

    /// Whether this DDB section carries a block not yet in.
    fn ddb_wanted(&self, s: &[u8]) -> bool {
        // section header (8), dsmccDownloadDataHeader (12 + adaptation),
        // then moduleId, moduleVersion, reserved, blockNumber.
        let Some(h) = s.get(8..20) else { return false };
        if h[0] != PROTOCOL || h[1] != TYPE_DOWNLOAD {
            return false;
        }
        let download_id = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
        if download_id != self.carousel_id {
            return false;
        }
        let at = 20 + h[9] as usize;
        let Some(b) = s.get(at..at + 6) else {
            return false;
        };
        let module = u16::from_be_bytes([b[0], b[1]]);
        let block = u16::from_be_bytes([b[4], b[5]]) as usize;
        match self.modules.get(&module) {
            Some(m) => m.version == b[2] && m.have.get(block) == Some(&false),
            None => false,
        }
    }

    /// A DSI or DII (userNetworkMessage, table 0x3B).
    fn user_network(&mut self, b: &[u8]) {
        let Some((msg, transaction, payload)) = message_header(b) else {
            return;
        };
        match msg {
            MSG_DSI => self.dsi(transaction, payload),
            MSG_DII => self.dii(payload),
            _ => {}
        }
    }

    /// DownloadServerInitiate (ISO/IEC 13818-6 §7.3.6; TR 101 202 §4.7.5:
    /// the private data is BIOP::ServiceGatewayInfo).
    fn dsi(&mut self, transaction: u32, p: &[u8]) {
        if self.dsi_transaction == Some(transaction) {
            return;
        }
        let mut r = biop::Reader::new(p);
        let parse = |r: &mut biop::Reader| -> Option<ObjectRef> {
            r.skip(20)?; // serverId
            let compat = r.u16()? as usize;
            r.skip(compat)?;
            let _private_len = r.u16()?;
            biop::ior(r)
        };
        let Some(gw) = parse(&mut r) else { return };
        if gw.carousel_id != self.carousel_id {
            return;
        }
        // A new DSI: everything may have changed (the DIIs say what has).
        if self.dsi_transaction.is_some() && self.gateway.as_ref() != Some(&gw) {
            self.modules.clear();
            self.generation += 1;
        }
        self.dsi_transaction = Some(transaction);
        self.gateway = Some(gw);
    }

    /// DownloadInfoIndication (ISO/IEC 13818-6 §7.3.2; the module info is
    /// BIOP::ModuleInfo, TR 101 202 §4.7.5.2).
    fn dii(&mut self, p: &[u8]) {
        let mut r = biop::Reader::new(p);
        let Some(download_id) = r.u32() else { return };
        if download_id != self.carousel_id {
            return;
        }
        type Entry = (u16, u32, u8, Option<u32>);
        let parse = |r: &mut biop::Reader| -> Option<(u16, Vec<Entry>)> {
            let block_size = r.u16()?;
            r.skip(1 + 1 + 4 + 4)?; // window, ack period, tC window, tC scenario
            let compat = r.u16()? as usize;
            r.skip(compat)?;
            let n = r.u16()?;
            let mut out = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let id = r.u16()?;
                let size = r.u32()?;
                let version = r.u8()?;
                let info = r.lv8()?;
                out.push((id, size, version, module_compression(info)));
            }
            Some((block_size, out))
        };
        let Some((block_size, list)) = parse(&mut r) else {
            return;
        };
        for (id, size, version, compressed) in list {
            if size > MAX_MODULE {
                continue;
            }
            let fresh = match self.modules.get(&id) {
                Some(m) => m.version != version || m.size != size,
                None => true,
            };
            if fresh {
                let had = self
                    .modules
                    .insert(id, Module::new(version, size, block_size, compressed));
                if had.is_some() || size == 0 {
                    self.generation += 1;
                }
            }
        }
    }

    /// DownloadDataBlock (ISO/IEC 13818-6 §7.3.7).
    fn ddb(&mut self, b: &[u8]) {
        let Some((msg, download_id, p)) = message_header(b) else {
            return;
        };
        if msg != MSG_DDB || download_id != self.carousel_id || p.len() < 6 {
            return;
        }
        let module = u16::from_be_bytes([p[0], p[1]]);
        let version = p[2];
        let block = u16::from_be_bytes([p[4], p[5]]) as usize;
        let data = &p[6..];
        let Some(m) = self.modules.get_mut(&module) else {
            return;
        };
        if m.version != version || m.have.get(block) != Some(&false) {
            return;
        }
        let at = block * m.block_size as usize;
        let end = (at + data.len()).min(m.size as usize);
        if at >= end {
            return;
        }
        if m.data.len() < m.size as usize {
            m.data.resize(m.size as usize, 0);
        }
        m.data[at..end].copy_from_slice(&data[..end - at]);
        m.have[block] = true;
        m.missing -= 1;
        if m.missing == 0 {
            complete(m);
            self.generation += 1;
        }
    }

    /// A table-0x3D section's descriptors: stream event descriptors fire.
    fn descriptors(&mut self, tag: u8, mut d: &[u8]) {
        while d.len() >= 2 && d.len() >= 2 + d[1] as usize {
            let (t, v) = (d[0], &d[2..2 + d[1] as usize]);
            if t == 0x1A && v.len() >= 10 {
                // stream_event_descriptor (ISO/IEC 13818-6 §8.3.3):
                // eventId(16), reserved(31), eventNPT(33), private data.
                let raw = u64::from_be_bytes([v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9]]);
                let npt = raw & ((1 << 33) - 1);
                self.events.push(StreamEvent {
                    component_tag: tag,
                    event_id: u16::from_be_bytes([v[0], v[1]]),
                    npt,
                    private_data: v.get(10..).unwrap_or(&[]).to_vec(),
                });
            }
            d = &d[2 + d[1] as usize..];
        }
    }

    /// The carousel has its root (a DSI came).
    pub fn has_gateway(&self) -> bool {
        self.gateway.is_some()
    }

    /// Modules complete and known.
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .modules
            .values()
            .filter(|m| m.objects.is_some())
            .count();
        (done, self.modules.len())
    }

    /// Bytes of module data received (inflated, for complete modules).
    pub fn bytes(&self) -> usize {
        self.modules.values().map(|m| m.data.len()).sum()
    }

    fn object(&self, r: &ObjectRef) -> Result<&Object, bool> {
        if r.carousel_id != self.carousel_id {
            return Err(false);
        }
        let m = self.modules.get(&r.module_id).ok_or(true)?;
        let objs = m.objects.as_ref().ok_or(true)?;
        objs.get(&r.key).ok_or(false)
    }

    fn data(&self, r: &ObjectRef) -> &[u8] {
        self.modules.get(&r.module_id).map_or(&[], |m| &m.data)
    }

    /// The object a path names. `Err(true)` while part of it is still to
    /// come, `Err(false)` if it is not there.
    fn walk(&self, path: &str) -> Result<(ObjectRef, &Object), bool> {
        let mut at = self.gateway.clone().ok_or(true)?;
        let mut obj = self.object(&at)?;
        for name in path.split('/').filter(|s| !s.is_empty()) {
            let ObjectKind::Directory(bindings) = &obj.kind else {
                return Err(false);
            };
            let b = bindings.iter().find(|b| b.name == name).ok_or(false)?;
            at = b.target.clone();
            obj = self.object(&at)?;
        }
        Ok((at, obj))
    }

    /// What `path` names ("/", "/a", "scenes/menu"; leading slashes are
    /// optional).
    pub fn lookup(&self, path: &str) -> Lookup<'_> {
        match self.walk(path) {
            Err(true) => Lookup::Pending,
            Err(false) => Lookup::NotFound,
            Ok((at, obj)) => match &obj.kind {
                ObjectKind::File(range) => Lookup::File(&self.data(&at)[range.clone()]),
                ObjectKind::Directory(b) => {
                    Lookup::Directory(b.iter().map(|b| b.name.clone()).collect())
                }
                ObjectKind::StreamEvent { names, taps } => Lookup::Events {
                    names: names.clone(),
                    component_tag: event_tag(taps),
                },
                ObjectKind::Stream { .. } => Lookup::Other,
            },
        }
    }

    /// A file's contents, if it is in.
    pub fn file(&self, path: &str) -> Option<&[u8]> {
        match self.lookup(path) {
            Lookup::File(b) => Some(b),
            _ => None,
        }
    }

    /// Every file and directory reachable from the root, as (path, kind,
    /// size), and how many bindings point into modules not yet in.
    pub fn tree(&self) -> (Vec<(String, &'static str, usize)>, usize) {
        let mut out = Vec::new();
        let mut pending = 0;
        let Some(gw) = self.gateway.clone() else {
            return (out, 1);
        };
        let mut stack = vec![(String::new(), gw, 0)];
        while let Some((path, r, depth)) = stack.pop() {
            let obj = match self.object(&r) {
                Ok(o) => o,
                Err(p) => {
                    pending += p as usize;
                    continue;
                }
            };
            match &obj.kind {
                ObjectKind::Directory(bindings) => {
                    out.push((format!("{path}/"), "dir", bindings.len()));
                    if depth < 32 {
                        for Binding { name, target, .. } in bindings.iter().rev() {
                            stack.push((format!("{path}/{name}"), target.clone(), depth + 1));
                        }
                    }
                }
                ObjectKind::File(range) => out.push((path, "fil", range.len())),
                ObjectKind::Stream { .. } => out.push((path, "str", 0)),
                ObjectKind::StreamEvent { names, .. } => out.push((path, "ste", names.len())),
            }
        }
        (out, pending)
    }

    /// Write the files that are in under `dir` (directories made as
    /// needed; a stream event object becomes a `.events` text file). Returns
    /// the files written.
    pub fn dump(&self, dir: &Path) -> std::io::Result<usize> {
        let (tree, _) = self.tree();
        let mut n = 0;
        for (path, kind, _) in tree {
            // Names come off the air: keep them inside `dir`.
            let rel: std::path::PathBuf = path
                .split('/')
                .filter(|c| !c.is_empty() && *c != "." && *c != "..")
                .map(|c| c.replace(['\\', ':', '*', '?', '"', '<', '>', '|'], "_"))
                .collect();
            let full = dir.join(&rel);
            match (kind, self.lookup(&path)) {
                ("dir", _) => std::fs::create_dir_all(&full)?,
                (_, Lookup::File(b)) => {
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&full, b)?;
                    n += 1;
                }
                (
                    _,
                    Lookup::Events {
                        names,
                        component_tag,
                    },
                ) => {
                    let mut s = format!("stream events on component tag {component_tag:?}\n");
                    for (name, id) in names {
                        s.push_str(&format!("{id:5} {name}\n"));
                    }
                    let mut f = full.into_os_string();
                    f.push(".events");
                    std::fs::write(f, s)?;
                }
                _ => {}
            }
        }
        Ok(n)
    }
}

/// The component tag a stream event object's triggers come on (its tap of
/// use STR_EVENT_USE 0x000C or STR_STATUS_AND_EVENT_USE 0x000D, TR 101 202
/// Table 4.3).
fn event_tag(taps: &[Tap]) -> Option<u8> {
    taps.iter()
        .find(|t| matches!(t.use_, 0x000C | 0x000D))
        .map(|t| t.assoc_tag as u8)
}

/// dsmccMessageHeader or dsmccDownloadDataHeader (ISO/IEC 13818-6 §2.1,
/// §7.2.2): the message id, the transaction (or download) id, and what
/// follows the adaptation header.
fn message_header(b: &[u8]) -> Option<(u16, u32, &[u8])> {
    if b.len() < 12 || b[0] != PROTOCOL || b[1] != TYPE_DOWNLOAD {
        return None;
    }
    let msg = u16::from_be_bytes([b[2], b[3]]);
    let id = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let adaptation = b[9] as usize;
    let length = u16::from_be_bytes([b[10], b[11]]) as usize;
    let end = (12 + length).min(b.len());
    let start = 12 + adaptation;
    (start <= end).then(|| (msg, id, &b[start..end]))
}

/// A BIOP::ModuleInfo's compressed_module_descriptor, if any: the size
/// inflated (TR 101 202 §4.7.5.2; EN 301 192 §9.3.4).
fn module_compression(info: &[u8]) -> Option<u32> {
    let mut r = biop::Reader::new(info);
    r.skip(12)?; // moduleTimeOut, blockTimeOut, minBlockTime
    let taps = r.u8()?;
    for _ in 0..taps {
        r.skip(6)?;
        let sel = r.u8()? as usize;
        r.skip(sel)?;
    }
    let n = r.u8()? as usize;
    let mut d = r.bytes(n)?;
    while d.len() >= 2 && d.len() >= 2 + d[1] as usize {
        if d[0] == 0x09 && d[1] >= 5 {
            return Some(u32::from_be_bytes([d[3], d[4], d[5], d[6]]));
        }
        d = &d[2 + d[1] as usize..];
    }
    None
}

/// A module has all its blocks: inflate it if it is compressed and read its
/// objects.
fn complete(m: &mut Module) {
    if let Some(size) = m.original_size {
        match miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(
            &m.data,
            (size as usize).min(MAX_MODULE as usize) + 16,
        ) {
            Ok(d) => m.data = d,
            Err(_) => m.data.clear(),
        }
    }
    m.objects = Some(biop::objects(&m.data));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::biop::tests::{dir_message, file_message, ior};

    /// A DSM-CC section (table `tid`, extension `ext`) with its CRC.
    pub fn dsmcc_section(tid: u8, ext: u16, body: &[u8]) -> Vec<u8> {
        let len = 5 + body.len() + 4;
        let mut s = vec![
            tid,
            0xB0 | ((len >> 8) as u8 & 0x0F),
            len as u8,
            (ext >> 8) as u8,
            ext as u8,
            0xC1,
            0,
            0,
        ];
        s.extend_from_slice(body);
        let crc = crc32_mpeg2(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        s
    }

    fn header(msg: u16, id: u32, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![PROTOCOL, TYPE_DOWNLOAD];
        b.extend_from_slice(&msg.to_be_bytes());
        b.extend_from_slice(&id.to_be_bytes());
        b.push(0xFF);
        b.push(0); // no adaptation
        b.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        b.extend_from_slice(payload);
        b
    }

    pub fn dsi(carousel: u32, module: u16, key: &[u8]) -> Vec<u8> {
        let mut p = vec![0xFF; 20];
        p.extend_from_slice(&0u16.to_be_bytes());
        let ior = ior(b"srg\0", carousel, module, key);
        p.extend_from_slice(&((ior.len() + 4) as u16).to_be_bytes());
        p.extend_from_slice(&ior);
        p.extend_from_slice(&[0, 0, 0, 0]); // no taps, contexts, user info
        dsmcc_section(0x3B, 0, &header(MSG_DSI, 0x8000_0000, &p))
    }

    /// A DII naming `modules` (id, size, version, compressed original size).
    pub fn dii(carousel: u32, block: u16, modules: &[(u16, u32, u8, Option<u32>)]) -> Vec<u8> {
        let mut p = carousel.to_be_bytes().to_vec();
        p.extend_from_slice(&block.to_be_bytes());
        p.extend_from_slice(&[0; 10]);
        p.extend_from_slice(&0u16.to_be_bytes());
        p.extend_from_slice(&(modules.len() as u16).to_be_bytes());
        for &(id, size, v, comp) in modules {
            p.extend_from_slice(&id.to_be_bytes());
            p.extend_from_slice(&size.to_be_bytes());
            p.push(v);
            let mut info = vec![0u8; 12];
            info.push(1); // one tap: BIOP_OBJECT_USE
            info.extend_from_slice(&[0, 0, 0, 0x17, 0, 0x65, 0]);
            match comp {
                Some(orig) => {
                    info.push(7);
                    info.extend_from_slice(&[0x09, 5, 0x08]);
                    info.extend_from_slice(&orig.to_be_bytes());
                }
                None => info.push(0),
            }
            p.push(info.len() as u8);
            p.extend_from_slice(&info);
        }
        p.extend_from_slice(&0u16.to_be_bytes());
        dsmcc_section(0x3B, 2, &header(MSG_DII, 0x8000_0002, &p))
    }

    pub fn ddbs(
        carousel: u32,
        module: u16,
        version: u8,
        block: usize,
        data: &[u8],
    ) -> Vec<Vec<u8>> {
        data.chunks(block)
            .enumerate()
            .map(|(n, c)| {
                let mut p = module.to_be_bytes().to_vec();
                p.push(version);
                p.push(0xFF);
                p.extend_from_slice(&(n as u16).to_be_bytes());
                p.extend_from_slice(c);
                dsmcc_section(0x3C, module, &header(MSG_DDB, carousel, &p))
            })
            .collect()
    }

    /// A small carousel: `/` holds `a` (a file) and `scenes/` (a directory in
    /// a second, compressed module) holding `menu`.
    fn carousel_sections() -> Vec<Vec<u8>> {
        let c = 7;
        let mut m1 = dir_message(
            b"srg\0",
            &[1],
            &[
                ("a", b"fil\0", c, 1, &[2]),
                ("scenes", b"dir\0", c, 2, &[1]),
            ],
        );
        m1.extend(file_message(&[2], b"{:Application ('/a' 0)}"));
        let mut m2 = dir_message(b"dir\0", &[1], &[("menu", b"fil\0", c, 2, &[3])]);
        m2.extend(file_message(&[3], &[0xAB; 3000]));
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&m2, 6);
        let mut v = vec![
            dsi(c, 1, &[1]),
            dii(
                c,
                1000,
                &[
                    (1, m1.len() as u32, 0, None),
                    (2, z.len() as u32, 3, Some(m2.len() as u32)),
                ],
            ),
        ];
        v.extend(ddbs(c, 1, 0, 1000, &m1));
        v.extend(ddbs(c, 2, 3, 1000, &z));
        v
    }

    #[test]
    fn builds_the_tree() {
        let mut oc = ObjectCarousel::new(7, vec![(100, Some(0x65))]);
        let secs = carousel_sections();
        // Blocks before the DII are dropped; they come round again.
        for s in secs.iter().rev() {
            oc.section(100, s);
        }
        assert_eq!(oc.lookup("/a"), Lookup::Pending);
        for s in &secs {
            oc.section(100, s);
        }
        assert_eq!(oc.progress(), (2, 2));
        assert_eq!(oc.file("/a"), Some(&b"{:Application ('/a' 0)}"[..]));
        assert_eq!(oc.file("scenes/menu").map(|b| b.len()), Some(3000));
        assert_eq!(
            oc.lookup("/"),
            Lookup::Directory(vec!["a".into(), "scenes".into()])
        );
        assert_eq!(oc.lookup("/b"), Lookup::NotFound);
        let (tree, pending) = oc.tree();
        assert_eq!(pending, 0);
        let paths: Vec<_> = tree.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(paths, ["/", "/a", "/scenes/", "/scenes/menu"]);
        // A complete module's blocks are dropped before the CRC.
        let g = oc.generation;
        for s in &secs[2..] {
            oc.section(100, s);
        }
        assert_eq!(oc.generation, g);
    }

    #[test]
    fn new_module_version_replaces_it() {
        let mut oc = ObjectCarousel::new(7, vec![(100, None)]);
        for s in carousel_sections() {
            oc.section(100, &s);
        }
        let m1 = {
            let mut m = dir_message(b"srg\0", &[1], &[("a", b"fil\0", 7, 1, &[2])]);
            m.extend(file_message(&[2], b"new"));
            m
        };
        oc.section(100, &dii(7, 1000, &[(1, m1.len() as u32, 1, None)]));
        assert_eq!(oc.lookup("/a"), Lookup::Pending);
        for s in ddbs(7, 1, 1, 1000, &m1) {
            oc.section(100, &s);
        }
        assert_eq!(oc.file("a"), Some(&b"new"[..]));
    }

    #[test]
    fn from_packets() {
        let mut oc = ObjectCarousel::new(7, vec![(0x1C21, Some(0x65))]);
        let mut cc = 0u8;
        for s in carousel_sections() {
            // Split each section over packets the way a multiplexer would.
            for (n, chunk) in s.chunks(183).enumerate() {
                let mut p = [0xFFu8; TS_LEN];
                p[0] = 0x47;
                p[1] = if n == 0 { 0x40 } else { 0 } | 0x1C;
                p[2] = 0x21;
                p[3] = 0x10 | cc;
                cc = (cc + 1) & 15;
                if n == 0 {
                    p[4] = 0;
                    p[5..5 + chunk.len()].copy_from_slice(chunk);
                } else {
                    p[4..4 + chunk.len()].copy_from_slice(chunk);
                }
                oc.packet(&p);
            }
        }
        assert_eq!(oc.file("/a").map(|b| b.len()), Some(23));
    }

    #[test]
    fn stream_event_descriptor() {
        let mut oc = ObjectCarousel::new(7, vec![(100, Some(0x77))]);
        let mut d = vec![0x1A, 13, 0x00, 0x05, 0xFF, 0xFF, 0xFF, 0xFE, 0, 0, 0, 0];
        d.extend_from_slice(&[0x00, b'h', b'i']);
        oc.section(100, &dsmcc_section(0x3D, 5, &d));
        let ev = oc.take_events();
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].component_tag, ev[0].event_id), (0x77, 5));
        assert_eq!(ev[0].private_data, [0, b'h', b'i']);
    }
}
