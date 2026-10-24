// made by CasualArclamp
//! BIOP, the object carousel's messages (TR 101 202 §4.7; ISO/IEC 13818-6
//! §11.3): a module is a run of BIOP messages, each one object (a directory,
//! a file, a stream or a stream event object) under an object key.
//! References between them are IORs carrying a BIOP profile body
//! (ObjectLocation: carousel, module, key; and a ConnBinder's taps).
//!
//! After MythTV's `dsmccbiop.cpp` (GPL-2.0-or-later), rewritten.

use std::collections::HashMap;
use std::ops::Range;

/// "BIOP" (TR 101 202 Table 4.4, magic).
const MAGIC: u32 = 0x4249_4F50;
/// IOR profile and component tags (TR 101 202 Table 4.10 to 4.12).
const TAG_BIOP: u32 = 0x4953_4F06;
const TAG_OBJECT_LOCATION: u32 = 0x4953_4F50;

/// A big-endian reader that says `None` past the end (the `?` operator
/// then unwinds a whole parse at the first short field).
pub struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, at: 0 }
    }

    pub fn pos(&self) -> usize {
        self.at
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }

    /// Bytes after a one-byte length.
    pub fn lv8(&mut self) -> Option<&'a [u8]> {
        let n = self.u8()? as usize;
        self.bytes(n)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.bytes(n).map(|_| ())
    }

    pub fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    pub fn u16(&mut self) -> Option<u16> {
        self.bytes(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.bytes(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Where an object is: BIOP::ObjectLocation (TR 101 202 Table 4.11).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub carousel_id: u32,
    pub module_id: u16,
    pub key: Vec<u8>,
}

/// A tap (TR 101 202 §4.7.2.5): a stream named by its association tag
/// (the component tag, in UK use).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tap {
    pub id: u16,
    pub use_: u16,
    pub assoc_tag: u16,
}

/// A directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    /// The kind the binding names ("fil", "dir", "str", "ste").
    pub kind: String,
    pub target: ObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectKind {
    Directory(Vec<Binding>),
    /// The file's contents within its module.
    File(Range<usize>),
    Stream {
        taps: Vec<Tap>,
    },
    /// A stream event object: its events' names and ids, and the taps.
    StreamEvent {
        names: Vec<(String, u16)>,
        taps: Vec<Tap>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub kind: ObjectKind,
}

/// An IOR (TR 101 202 §4.7.3.1): the BIOP profile's object location. Other
/// profiles (Lite Options, for objects of other services) give `None`.
pub fn ior(r: &mut Reader) -> Option<ObjectRef> {
    let type_len = r.u32()? as usize;
    r.skip(type_len)?;
    // type_id is padded to four bytes (TR 101 202 Table 4.5, alignment_gap).
    r.skip((4 - type_len % 4) % 4)?;
    let profiles = r.u32()?;
    let mut found = None;
    for _ in 0..profiles {
        let tag = r.u32()?;
        let len = r.u32()? as usize;
        let body = r.bytes(len)?;
        if tag == TAG_BIOP && found.is_none() {
            found = profile_body(body);
        }
    }
    found
}

/// BIOPProfileBody (TR 101 202 Table 4.7): byte order, then the lite
/// components.
fn profile_body(b: &[u8]) -> Option<ObjectRef> {
    let mut r = Reader::new(b);
    r.skip(1)?; // byte order: big-endian
    let n = r.u8()?;
    let mut loc = None;
    for _ in 0..n {
        let tag = r.u32()?;
        let len = r.u8()? as usize;
        let c = r.bytes(len)?;
        if tag == TAG_OBJECT_LOCATION {
            let mut c = Reader::new(c);
            let carousel_id = c.u32()?;
            let module_id = c.u16()?;
            c.skip(2)?; // version 1.0
            let klen = c.u8()? as usize;
            loc = Some(ObjectRef {
                carousel_id,
                module_id,
                key: c.bytes(klen)?.to_vec(),
            });
        }
        // The ConnBinder (0x49534F40) names the DII the module is in; every
        // DII of the carousel is read anyway, so it is not needed here.
    }
    loc
}

fn taps(r: &mut Reader) -> Option<Vec<Tap>> {
    let n = r.u8()?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let id = r.u16()?;
        let use_ = r.u16()?;
        let assoc_tag = r.u16()?;
        let sel = r.u8()? as usize;
        r.skip(sel)?;
        out.push(Tap {
            id,
            use_,
            assoc_tag,
        });
    }
    Some(out)
}

fn text(b: &[u8]) -> String {
    let b = b.strip_suffix(&[0]).unwrap_or(b);
    String::from_utf8_lossy(b).into_owned()
}

/// Every object in a module, by key. A message that does not parse ends the
/// run (what came before it is kept).
pub fn objects(m: &[u8]) -> HashMap<Vec<u8>, Object> {
    let mut out = HashMap::new();
    let mut at = 0;
    while at + 12 <= m.len() {
        let Some((len, key, obj)) = message(&m[at..], at) else {
            break;
        };
        if let Some(obj) = obj {
            out.insert(key, obj);
        }
        at += len;
    }
    out
}

/// One BIOP message at the start of `b` (which is at `base` in its module):
/// its length, key and object (TR 101 202 Tables 4.4 to 4.17).
fn message(b: &[u8], base: usize) -> Option<(usize, Vec<u8>, Option<Object>)> {
    let mut r = Reader::new(b);
    if r.u32()? != MAGIC {
        return None;
    }
    r.skip(4)?; // version 1.0, byte order, message type
    let size = r.u32()? as usize;
    let total = 12 + size;
    let key = r.lv8()?.to_vec();
    let kind_len = r.u32()? as usize;
    let kind = r.bytes(kind_len)?;
    let info_len = r.u16()? as usize;
    let info = r.bytes(info_len)?;
    let contexts = r.u8()?;
    for _ in 0..contexts {
        r.skip(4)?;
        let n = r.u16()? as usize;
        r.skip(n)?;
    }
    let body_len = r.u32()? as usize;
    let body_at = r.pos();
    let body = r.bytes(body_len)?;
    if total > b.len() {
        return None;
    }
    let mut br = Reader::new(body);
    let obj = match kind {
        b"fil\0" | b"fil" => {
            let n = br.u32()? as usize;
            let start = base + body_at + 4;
            (n <= body.len() - 4).then(|| ObjectKind::File(start..start + n))
        }
        b"dir\0" | b"srg\0" | b"dir" | b"srg" => Some(ObjectKind::Directory(bindings(&mut br)?)),
        b"str\0" | b"str" => Some(ObjectKind::Stream {
            taps: taps(&mut br).unwrap_or_default(),
        }),
        b"ste\0" | b"ste" => stream_event(info, &mut br),
        _ => None,
    };
    Some((total, key, obj.map(|kind| Object { kind })))
}

/// A directory's bindings (TR 101 202 Table 4.15).
fn bindings(r: &mut Reader) -> Option<Vec<Binding>> {
    let n = r.u16()?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let comps = r.u8()?;
        let (mut name, mut kind) = (String::new(), String::new());
        for i in 0..comps {
            let id = r.lv8()?;
            let k = r.lv8()?;
            if i + 1 == comps {
                name = text(id);
                kind = text(k);
            }
        }
        r.skip(1)?; // binding type
        let target = ior(r);
        let info = r.u16()? as usize;
        r.skip(info)?;
        if let Some(target) = target {
            out.push(Binding { name, kind, target });
        }
    }
    Some(out)
}

/// A stream event object (TR 101 202 Table 4.17): the info holds
/// DSM::Stream::Info_T and the event names, the body the taps and the ids.
fn stream_event(info: &[u8], body: &mut Reader) -> Option<ObjectKind> {
    let mut r = Reader::new(info);
    let d = r.u8()? as usize;
    r.skip(d + 8 + 3)?; // description, duration, audio/video/data flags
    let n = r.u16()?;
    let mut names = Vec::with_capacity(n as usize);
    for _ in 0..n {
        names.push(text(r.lv8()?));
    }
    let taps = taps(body)?;
    let ids = body.u8()?;
    let mut out = Vec::with_capacity(names.len());
    for name in names.into_iter().take(ids as usize) {
        out.push((name, body.u16()?));
    }
    Some(ObjectKind::StreamEvent { names: out, taps })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An IOR to (carousel, module, key) of `kind`.
    pub fn ior(kind: &[u8], carousel: u32, module: u16, key: &[u8]) -> Vec<u8> {
        let mut v = (kind.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(&1u32.to_be_bytes());
        let mut loc = carousel.to_be_bytes().to_vec();
        loc.extend_from_slice(&module.to_be_bytes());
        loc.extend_from_slice(&[1, 0, key.len() as u8]);
        loc.extend_from_slice(key);
        let mut pb = vec![0, 1];
        pb.extend_from_slice(&TAG_OBJECT_LOCATION.to_be_bytes());
        pb.push(loc.len() as u8);
        pb.extend_from_slice(&loc);
        v.extend_from_slice(&TAG_BIOP.to_be_bytes());
        v.extend_from_slice(&(pb.len() as u32).to_be_bytes());
        v.extend_from_slice(&pb);
        v
    }

    fn message(key: &[u8], kind: &[u8], info: &[u8], body: &[u8]) -> Vec<u8> {
        let mut rest = vec![key.len() as u8];
        rest.extend_from_slice(key);
        rest.extend_from_slice(&(kind.len() as u32).to_be_bytes());
        rest.extend_from_slice(kind);
        rest.extend_from_slice(&(info.len() as u16).to_be_bytes());
        rest.extend_from_slice(info);
        rest.push(0);
        rest.extend_from_slice(&(body.len() as u32).to_be_bytes());
        rest.extend_from_slice(body);
        let mut m = MAGIC.to_be_bytes().to_vec();
        m.extend_from_slice(&[1, 0, 0, 0]);
        m.extend_from_slice(&(rest.len() as u32).to_be_bytes());
        m.extend(rest);
        m
    }

    pub fn file_message(key: &[u8], content: &[u8]) -> Vec<u8> {
        let mut body = (content.len() as u32).to_be_bytes().to_vec();
        body.extend_from_slice(content);
        message(key, b"fil\0", &(content.len() as u64).to_be_bytes(), &body)
    }

    /// A directory entry: name, kind, carousel, module, key.
    pub type Entry<'a> = (&'a str, &'a [u8], u32, u16, &'a [u8]);

    /// A directory: entries (name, kind, carousel, module, key).
    pub fn dir_message(kind: &[u8], key: &[u8], entries: &[Entry]) -> Vec<u8> {
        let mut body = (entries.len() as u16).to_be_bytes().to_vec();
        for &(name, k, c, m, key) in entries {
            body.push(1);
            body.push(name.len() as u8 + 1);
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.push(k.len() as u8);
            body.extend_from_slice(k);
            body.push(1); // nobject
            body.extend(ior(k, c, m, key));
            body.extend_from_slice(&0u16.to_be_bytes());
        }
        message(key, kind, &[], &body)
    }

    #[test]
    fn stream_event_object() {
        let mut info = vec![0u8]; // no description
        info.extend_from_slice(&[0; 8 + 3]);
        info.extend_from_slice(&2u16.to_be_bytes());
        for n in ["start", "stop"] {
            info.push(n.len() as u8);
            info.extend_from_slice(n.as_bytes());
        }
        let mut body = vec![1, 0, 0, 0, 0x0D, 0, 0x77, 0];
        body.push(2);
        body.extend_from_slice(&[0, 1, 0, 2]);
        let m = message(&[9], b"ste\0", &info, &body);
        let objs = objects(&m);
        let ObjectKind::StreamEvent { names, taps } = &objs[&vec![9u8]].kind else {
            panic!()
        };
        assert_eq!(names, &[("start".to_string(), 1), ("stop".to_string(), 2)]);
        assert_eq!(taps[0].assoc_tag, 0x77);
    }
}
