// made by CasualArclamp
//! MHEG-5's binary interchange form: ASN.1 BER (ISO/IEC 13522-5 Annex A,
//! ITU-T X.690) read into a tree with no grammar applied. A class's loader
//! then picks out the tagged items it knows ([`Node::get`]).
//!
//! How each context tag's contents are typed (boolean, enumerated, integer,
//! string, or nested items) follows MythTV's freemheg `ParseBinary.cpp`
//! (GPL-2.0-or-later; ISO/IEC 13522-5 Annex A gives the tags implicit types).

use crate::tags::*;

/// An item of the tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Bool(bool),
    Int(i32),
    Enum(i32),
    Str(Vec<u8>),
    Null,
    /// A SEQUENCE (universal tag 16).
    Seq(Vec<Node>),
    /// A context-specific tag and what it holds (one primitive, or items).
    Tagged(u32, Vec<Node>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub &'static str);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl Node {
    /// The items of a tagged node or sequence.
    pub fn args(&self) -> &[Node] {
        match self {
            Node::Tagged(_, a) | Node::Seq(a) => a,
            _ => &[],
        }
    }

    pub fn tag(&self) -> Option<u32> {
        match self {
            Node::Tagged(t, _) => Some(*t),
            _ => None,
        }
    }

    /// The first item tagged `tag` among this node's items.
    pub fn get(&self, tag: u32) -> Option<&Node> {
        self.args().iter().find(|n| n.tag() == Some(tag))
    }

    /// The `i`th item.
    pub fn arg(&self, i: usize) -> Option<&Node> {
        self.args().get(i)
    }

    /// An integer: the node, or the single item of a tagged node.
    pub fn int(&self) -> Option<i32> {
        match self {
            Node::Int(v) | Node::Enum(v) => Some(*v),
            Node::Bool(b) => Some(*b as i32),
            Node::Tagged(_, a) if a.len() == 1 => a[0].int(),
            _ => None,
        }
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Node::Bool(b) => Some(*b),
            Node::Int(v) => Some(*v != 0),
            Node::Tagged(_, a) if a.len() == 1 => a[0].bool(),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&[u8]> {
        match self {
            Node::Str(s) => Some(s),
            Node::Tagged(_, a) if a.len() == 1 => a[0].str(),
            _ => None,
        }
    }

    /// The tagged item's integer.
    pub fn get_int(&self, tag: u32) -> Option<i32> {
        self.get(tag).and_then(|n| n.int())
    }

    pub fn get_bool(&self, tag: u32) -> Option<bool> {
        self.get(tag).and_then(|n| n.bool())
    }
}

/// How a context tag's primitive contents are read (freemheg's switch):
/// boolean, enumerated, integer or string; `None` holds nested items.
fn context_kind(tag: u32) -> Option<u8> {
    match tag {
        C_MULTIPLE_SELECTION
        | C_OBSCURED_INPUT
        | C_INITIALLY_AVAILABLE
        | C_WRAP_AROUND
        | C_TEXT_WRAPPING
        | C_INITIALLY_ACTIVE
        | C_MOVING_CURSOR
        | C_SHARED
        | C_ENGINE_RESP
        | C_TILING
        | C_BORDERED_BOUNDING_BOX => Some(b'b'),
        C_INPUT_TYPE
        | C_SLIDER_STYLE
        | C_TERMINATION
        | C_ORIENTATION
        | C_HORIZONTAL_JUSTIFICATION
        | C_BUTTON_STYLE
        | C_START_CORNER
        | C_LINE_ORIENTATION
        | C_VERTICAL_JUSTIFICATION
        | C_STORAGE => Some(b'e'),
        C_INITIAL_PORTION
        | C_STEP_SIZE
        | C_INPUT_EVENT_REGISTER
        | C_INITIAL_VALUE
        | C_IP_CONTENT_HOOK
        | C_MAX_VALUE
        | C_MIN_VALUE
        | C_LINE_ART_CONTENT_HOOK
        | C_BITMAP_CONTENT_HOOK
        | C_TEXT_CONTENT_HOOK
        | C_STREAM_CONTENT_HOOK
        | C_MAX_LENGTH
        | C_CHARACTER_SET
        | C_ORIGINAL_TRANSPARENCY
        | C_ORIGINAL_GC_PRIORITY
        | C_LOOPING
        | C_ORIGINAL_LINE_STYLE
        | C_STANDARD_VERSION
        | C_ORIGINAL_LINE_WIDTH
        | C_CONTENT_HOOK
        | C_CONTENT_CACHE_PRIORITY
        | C_COMPONENT_TAG
        | C_ORIGINAL_VOLUME
        | C_PROGRAM_CONNECTION_TAG
        | C_CONTENT_SIZE => Some(b'i'),
        C_OBJECT_INFORMATION | C_CONTENT_REFERENCE | C_FONT_ATTRIBUTES | C_CHAR_LIST | C_NAME
        | C_ORIGINAL_LABEL => Some(b's'),
        _ => None,
    }
}

/// Parse one MHEG-5 object (an application or a scene) from its file.
pub fn parse(b: &[u8]) -> Result<Node, ParseError> {
    let mut at = 0;
    item(b, &mut at, 0)
}

/// A BER integer: big-endian two's complement.
fn int(b: &[u8]) -> i32 {
    let mut v: i32 = if b.first().is_some_and(|&c| c >= 0x80) {
        -1
    } else {
        0
    };
    for &c in b {
        v = v.wrapping_shl(8) | c as i32;
    }
    v
}

fn item(b: &[u8], at: &mut usize, depth: u32) -> Result<Node, ParseError> {
    if depth > 64 {
        return Err(ParseError("nested too deeply"));
    }
    let next = |at: &mut usize| -> Result<u8, ParseError> {
        let c = *b.get(*at).ok_or(ParseError("truncated"))?;
        *at += 1;
        Ok(c)
    };
    let c = next(at)?;
    let class = c & 0xC0;
    let mut tag = (c & 0x1F) as u32;
    if tag == 0x1F {
        tag = 0;
        loop {
            let c = next(at)?;
            tag = (tag << 7) | (c & 0x7F) as u32;
            if c & 0x80 == 0 {
                break;
            }
            if tag > 1 << 20 {
                return Err(ParseError("tag too long"));
            }
        }
    }
    let l = next(at)?;
    let len = if l & 0x80 == 0 {
        l as usize
    } else {
        let n = (l & 0x7F) as usize;
        if n == 0 || n > 4 {
            return Err(ParseError("indefinite or huge length"));
        }
        let mut v = 0usize;
        for _ in 0..n {
            v = (v << 8) | next(at)? as usize;
        }
        v
    };
    let end = at
        .checked_add(len)
        .filter(|&e| e <= b.len())
        .ok_or(ParseError("truncated"))?;
    let body = &b[*at..end];
    let items = |at: &mut usize| -> Result<Vec<Node>, ParseError> {
        let mut v = Vec::new();
        while *at < end {
            v.push(item(b, at, depth + 1)?);
        }
        Ok(v)
    };
    let node = match class {
        0x00 => match tag {
            1 => Node::Bool(int(body) != 0),
            2 => Node::Int(int(body)),
            10 => Node::Enum(int(body)),
            4 => Node::Str(body.to_vec()),
            5 => Node::Null,
            16 => Node::Seq(items(at)?),
            _ => return Err(ParseError("unknown universal tag")),
        },
        0x80 => match context_kind(tag) {
            Some(k) => {
                let v = match k {
                    _ if body.is_empty() && k != b's' => vec![],
                    b'b' => vec![Node::Bool(int(body) != 0)],
                    b'e' => vec![Node::Enum(int(body))],
                    b'i' => vec![Node::Int(int(body))],
                    _ => vec![Node::Str(body.to_vec())],
                };
                Node::Tagged(tag, v)
            }
            None => Node::Tagged(tag, items(at)?),
        },
        _ => return Err(ParseError("tag class")),
    };
    *at = end;
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_application_header() {
        // {:Application ("/a" 0) :Items ({:Text 2 :InitiallyActive false
        //  :Shared true})}, as the BBC's /a starts.
        let b = [
            0xA0, 0x17, 0x30, 0x07, 0x04, 0x02, b'/', b'a', 0x02, 0x01, 0x00, 0xA8, 0x0C, 0xBD,
            0x0A, 0x02, 0x01, 0x02, 0x9F, 0x38, 0x01, 0x00, 0x9F, 0x3B, 0x01, 0xFF,
        ];
        let n = parse(&b).unwrap();
        assert_eq!(n.tag(), Some(C_APPLICATION));
        assert_eq!(
            n.arg(0),
            Some(&Node::Seq(vec![Node::Str(b"/a".to_vec()), Node::Int(0)]))
        );
        let text = &n.get(C_ITEMS).unwrap().args()[0];
        assert_eq!(text.tag(), Some(C_TEXT));
        assert_eq!(text.arg(0), Some(&Node::Int(2)));
        assert_eq!(text.get_bool(C_INITIALLY_ACTIVE), Some(false));
        assert_eq!(text.get_bool(C_SHARED), Some(true));
    }

    #[test]
    fn integers_are_signed() {
        assert_eq!(int(&[0xFF]), -1);
        assert_eq!(int(&[0x00, 0xFF]), 255);
        assert_eq!(int(&[0x02, 0xD0]), 720);
    }
}
