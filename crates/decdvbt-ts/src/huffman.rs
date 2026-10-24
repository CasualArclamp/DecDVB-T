// made by CasualArclamp
//! DVB text compressed with the Freesat/Freeview Huffman tables: a string
//! whose first byte is 0x1F, the second the table (1 or 2), then a bit stream
//! in which each character's code depends on the one before it. The UK's
//! terrestrial guide (event names and descriptions) is sent this way.
//!
//! After MythTV's `freesat_huffman.cpp` (GPL-2.0-or-later, the MythTV
//! project); the tables are in [`crate::huffman_tables`].

use crate::huffman_tables::{Code, TABLE_1_CODES, TABLE_1_INDEX, TABLE_2_CODES, TABLE_2_INDEX};

/// Decodes to this: the end of the string.
const STOP: u8 = 0;
/// Decodes to this: raw bytes follow, eight bits each, until one is ASCII.
const ESCAPE: u8 = 1;

/// The bits of a byte string, MSB first, read 32 at a time.
struct Bits<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bits<'_> {
    /// The next 32 bits (zeros past the end), without taking them.
    fn peek(&self) -> u32 {
        let mut v = 0u32;
        for k in 0..32 {
            let bit = self.at + k;
            let b = self
                .bytes
                .get(bit / 8)
                .map_or(0, |&b| (b >> (7 - bit % 8)) & 1);
            v |= u32::from(b) << (31 - k);
        }
        v
    }

    fn take(&mut self, n: usize) {
        self.at += n;
    }

    fn left(&self) -> usize {
        (self.bytes.len() * 8).saturating_sub(self.at)
    }
}

/// The text of a 0x1F string (`b` from the 0x1F), or `None` if it is not one
/// this decoder knows. A code missing from the table ends the text with "…".
pub fn decode(b: &[u8]) -> Option<String> {
    if b.len() < 2 || b[0] != 0x1F {
        return None;
    }
    let (codes, index): (&[Code], &[u16; 129]) = match b[1] {
        1 => (&TABLE_1_CODES, &TABLE_1_INDEX),
        2 => (&TABLE_2_CODES, &TABLE_2_INDEX),
        _ => return None,
    };
    let mut bits = Bits {
        bytes: &b[2..],
        at: 0,
    };
    let mut out = Vec::with_capacity(b.len() * 2);
    let mut last = STOP; // the start has the context of the end
    let mut truncated = false;
    loop {
        if bits.left() == 0 {
            break;
        }
        let value = bits.peek();
        if last == ESCAPE {
            // A raw byte; the first ASCII one ends the escape and is the next
            // character's context (a control character ends the text).
            let ch = (value >> 24) as u8;
            bits.take(8);
            if ch & 0x80 == 0 {
                if ch < b' ' {
                    break;
                }
                last = ch;
            }
            out.push(ch);
            continue;
        }
        let ctx = last as usize;
        if ctx >= 128 {
            truncated = true;
            break;
        }
        let range = index[ctx] as usize..index[ctx + 1] as usize;
        let found = codes[range].iter().find(|(code, n, _)| {
            let mask = if *n >= 32 { u32::MAX } else { !(u32::MAX >> n) };
            value & mask == *code
        });
        let Some(&(_, n, ch)) = found else {
            truncated = true;
            break;
        };
        bits.take(n as usize);
        if ch == STOP {
            break;
        }
        if ch != ESCAPE {
            out.push(ch);
        }
        last = ch;
    }
    let mut s = match String::from_utf8(out) {
        Ok(s) => s,
        Err(e) => e.into_bytes().iter().map(|&c| c as char).collect(),
    };
    if truncated {
        s.push('…');
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode with the same tables (the test's own encoder): the inverse of
    /// the decoder, so a string round-trips.
    fn encode(text: &str, table: u8) -> Vec<u8> {
        let (codes, index): (&[Code], &[u16; 129]) = if table == 1 {
            (&TABLE_1_CODES, &TABLE_1_INDEX)
        } else {
            (&TABLE_2_CODES, &TABLE_2_INDEX)
        };
        let mut bits: Vec<bool> = Vec::new();
        let put = |v: u32, n: u8, bits: &mut Vec<bool>| {
            for k in 0..n {
                bits.push((v >> (31 - k)) & 1 == 1);
            }
        };
        let mut last = STOP as usize;
        for &ch in text.as_bytes().iter().chain(std::iter::once(&STOP)) {
            let range = index[last] as usize..index[last + 1] as usize;
            match codes[range.clone()].iter().find(|c| c.2 == ch) {
                Some(&(v, n, _)) => put(v, n, &mut bits),
                None => {
                    // Escape, then the raw byte.
                    let &(v, n, _) = codes[range].iter().find(|c| c.2 == ESCAPE).unwrap();
                    put(v, n, &mut bits);
                    put(u32::from(ch) << 24, 8, &mut bits);
                    if ch == STOP {
                        break;
                    }
                }
            }
            last = ch as usize;
        }
        let mut out = vec![0x1F, table];
        for chunk in bits.chunks(8) {
            let mut b = 0u8;
            for (k, &bit) in chunk.iter().enumerate() {
                b |= u8::from(bit) << (7 - k);
            }
            out.push(b);
        }
        out
    }

    #[test]
    fn text_round_trips_through_both_tables() {
        for table in [1, 2] {
            for text in [
                "Pointless",
                "BBC News at Six",
                "The latest national and international news, with Sophie Raworth.",
                "Film: 2001 - A Space Odyssey (1968)",
            ] {
                let enc = encode(text, table);
                assert!(enc.len() < text.len() + 2, "{text}: no smaller");
                assert_eq!(decode(&enc).as_deref(), Some(text), "table {table}");
            }
        }
    }

    #[test]
    fn not_a_huffman_string_is_not_decoded() {
        assert_eq!(decode(b"\x15plain"), None);
        assert_eq!(decode(&[0x1F, 3, 0, 0]), None);
    }
}
