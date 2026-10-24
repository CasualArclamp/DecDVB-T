// made by CasualArclamp
//! The UK profile's resident programs (D-Book 7 §13.10.4; ETSI ES 202 184
//! §11.10.4), called by name with Call or Fork. A parameter is a generic
//! value; an output parameter is an indirect reference to the variable
//! that takes the result. Strings are bytes throughout.
//!
//! After MythTV's freemheg `Programs.cpp` (GPL-2.0-or-later), rewritten.

use std::rc::Rc;

use crate::asn1::Node;
use crate::engine::{Engine, Fetch, Files, Request};
use crate::obj::*;

impl Engine {
    /// Run resident program `t` with `params`; whether it succeeded.
    pub(crate) fn call_program(
        &mut self,
        t: &Ref,
        params: &[Node],
        g: &Rc<str>,
        files: &dyn Files,
    ) -> bool {
        let name = match self.objs.get(t).map(|o| &o.kind) {
            Some(Kind::Program { name }) => name.clone(),
            _ => return false,
        };
        if let Some(o) = self.objs.get(t)
            && !o.available
        {
            self.preparation(t, files);
        }
        let p = |i: usize| params.get(i);
        let int = |e: &Engine, i: usize| e.gint(p(i), g);
        let s = |e: &Engine, i: usize| e.gstr(p(i), g);
        let out = |e: &mut Engine, i: usize, v: Value| {
            if let Some(r) = p(i).and_then(|n| e.out_ref(n, g)) {
                e.set_var(&r, v);
            }
        };
        match name.as_str() {
            // GetCurrentDate: the MJD and the seconds since midnight, local.
            "GCD" => {
                let t = self.local_now();
                out(self, 0, Value::Int((t.div_euclid(86400) + 40587) as i32));
                out(self, 1, Value::Int(t.rem_euclid(86400) as i32));
                true
            }
            // FormatDate(format, date, seconds, out string).
            "FDa" => {
                let f = s(self, 0);
                let day = int(self, 1) as i64 - 40587;
                let secs = int(self, 2) as i64;
                out(self, 3, Value::Str(format_date(&f, day * 86400 + secs)));
                true
            }
            // GetDayOfWeek(date, out day): 0 is Sunday.
            "GDW" => {
                let d = (int(self, 0) as i64 - 40587 + 4).rem_euclid(7);
                out(self, 1, Value::Int(d as i32));
                true
            }
            // Random(n, out 1..n).
            "Rnd" => {
                let n = int(self, 0).max(1) as u64;
                self.rng = self
                    .rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                out(self, 1, Value::Int(((self.rng >> 33) % n) as i32 + 1));
                true
            }
            "CTC" => {
                let v = s(self, 0);
                out(self, 1, Value::ContentRef(v));
                true
            }
            "CTO" => {
                let gid = s(self, 0);
                let num = int(self, 1);
                let r = Ref {
                    group: canonical(&gid, g),
                    num,
                };
                out(self, 2, Value::ObjRef(r));
                true
            }
            "GSL" => {
                let n = char_len(&s(self, 0));
                out(self, 1, Value::Int(n as i32));
                true
            }
            // GetSubString(string, begin, end, out): 1-based, inclusive.
            "GSS" => {
                let st = s(self, 0);
                let (b, e) = (int(self, 1), int(self, 2));
                out(self, 3, Value::Str(substring(&st, b, e)));
                true
            }
            // SearchSubString(string, start, search, out position or -1).
            "SSS" => {
                let st = s(self, 0);
                let at = int(self, 1);
                let pat = s(self, 2);
                out(
                    self,
                    3,
                    Value::Int(search(&st, at, &pat).map_or(-1, |i| i as i32)),
                );
                true
            }
            // SearchAndExtractSubString(string, start, search, out text before
            // it, out the position after it, or -1).
            "SES" => {
                let st = s(self, 0);
                let at = int(self, 1).max(1);
                let pat = s(self, 2);
                match search(&st, at, &pat) {
                    Some(i) => {
                        out(self, 3, Value::Str(substring(&st, at, i as i32 - 1)));
                        out(self, 4, Value::Int(i as i32 + char_len(&pat) as i32));
                    }
                    None => {
                        out(self, 3, Value::Str(Vec::new()));
                        out(self, 4, Value::Int(-1));
                    }
                }
                true
            }
            // GetServiceIndex(reference, out index or -1).
            "GSI" => {
                let r = String::from_utf8_lossy(&s(self, 0)).into_owned();
                let i = self.service_index(&r);
                out(self, 1, Value::Int(i));
                true
            }
            "TIn" => {
                let i = int(self, 0);
                self.requests.push(Request::Tune(i));
                true
            }
            "TII" | "SBI" | "SSM" | "SWA" | "DBG" => true,
            // GetBasicSI(index, out network, original network, ts, service).
            "BSI" => {
                let i = int(self, 0);
                let Some(tri) = self.service_triplet(i) else {
                    return false;
                };
                out(self, 1, Value::Int(tri.0 as i32));
                out(self, 2, Value::Int(tri.0 as i32));
                out(self, 3, Value::Int(tri.1 as i32));
                out(self, 4, Value::Int(tri.2 as i32));
                true
            }
            "GBI" => {
                out(self, 0, Value::Bool(false));
                out(self, 1, Value::Str(Vec::new()));
                true
            }
            // CheckContentRef / CheckGroupIDRef(ref, out valid, out ref).
            "CCR" | "CGR" => {
                let r = s(self, 0);
                let path = canonical(&r, g);
                let valid = match files.fetch(&path) {
                    Fetch::Ready(_) => true,
                    Fetch::Missing => false,
                    Fetch::Pending => {
                        self.block_on(path);
                        return false;
                    }
                };
                out(self, 1, Value::Bool(valid));
                out(
                    self,
                    2,
                    if name == "CCR" {
                        Value::ContentRef(r)
                    } else {
                        Value::ObjRef(Ref {
                            group: path,
                            num: 0,
                        })
                    },
                );
                true
            }
            "VTG" => {
                let (x, y) = (int(self, 0), int(self, 1));
                out(self, 2, Value::Int(x));
                out(self, 3, Value::Int(y));
                true
            }
            // GetDisplayAspectRatio: 1 is 4:3, 2 is 16:9.
            "GDA" => {
                out(self, 0, Value::Int(2));
                true
            }
            // GetICStatus: 2, no interaction channel.
            "GIS" => {
                out(self, 0, Value::Int(2));
                true
            }
            // WhoAmI: manufacturer, model and version (D-Book 7 §13.10.4.24).
            "WAI" => {
                out(self, 0, Value::Str(b"DDT DecDVB-T 010".to_vec()));
                true
            }
            _ => {
                let n = name.clone();
                if self.tracing {
                    self.trace.push(format!("  (no resident program {n})"));
                }
                false
            }
        }
    }

    /// Local time now, Unix seconds plus the UK offset.
    fn local_now(&self) -> i64 {
        let unix = self.clock_unix.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        });
        unix + self.local_offset_s
    }

    /// GetServiceIndex: a service's place in the receiver's list, or -1
    /// (D-Book 7 §13.10.4.11). "rec://svc/cur" and "rec://svc/def" are this
    /// service; "dvb://onid.tsid.sid" is hexadecimal, an empty field matching
    /// any (the BBC writes "dvb://233a..1044").
    fn service_index(&self, r: &str) -> i32 {
        let here = (
            self.service.original_network_id,
            self.service.transport_stream_id,
            self.service.service_id,
        );
        let want: Option<[Option<u16>; 3]> = if r.starts_with("rec://svc/") {
            Some([Some(here.0), Some(here.1), Some(here.2)])
        } else if let Some(t) = r.strip_prefix("dvb://") {
            let f: Vec<&str> = t.split('.').collect();
            (f.len() == 3).then(|| {
                let h = |x: &str| {
                    (!x.is_empty())
                        .then(|| u16::from_str_radix(x, 16).ok())
                        .flatten()
                };
                [h(f[0]), h(f[1]), h(f[2])]
            })
        } else {
            None
        };
        let Some(w) = want else { return -1 };
        let matches = |&(o, t, s): &(u16, u16, u16)| {
            w[0].is_none_or(|x| x == o)
                && w[1].is_none_or(|x| x == t)
                && w[2].is_none_or(|x| x == s)
        };
        if w[2].is_none() {
            return -1;
        }
        self.service
            .services
            .iter()
            .position(matches)
            .map_or(-1, |i| i as i32)
    }

    fn service_triplet(&self, i: i32) -> Option<(u16, u16, u16)> {
        self.service.services.get(usize::try_from(i).ok()?).copied()
    }
}

/// Strings are octet strings: lengths and positions count bytes (as in
/// freemheg; the BBC keeps colours as raw bytes inside its strings).
fn char_len(s: &[u8]) -> usize {
    s.len()
}

/// Bytes `b` to `e` (1-based, inclusive), clamped.
fn substring(s: &[u8], b: i32, e: i32) -> Vec<u8> {
    let b = (b.max(1) - 1) as usize;
    let e = (e.max(0) as usize).min(s.len());
    if b >= e {
        return Vec::new();
    }
    s[b..e].to_vec()
}

/// Where `pat` is in `s` from byte `at` (1-based) on.
fn search(s: &[u8], at: i32, pat: &[u8]) -> Option<usize> {
    let start = (at.max(1) - 1) as usize;
    if start > s.len() {
        return None;
    }
    if pat.is_empty() {
        return Some(start + 1);
    }
    s[start..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|i| start + i + 1)
}

/// FormatDate's codes (D-Book 7 Table 13-20): %Y %y %X %x %D %d %H %h %I %i
/// %M %m %S %s %A %a %%.
fn format_date(f: &[u8], t: i64) -> Vec<u8> {
    let (y, mo, d, h, mi, s) = civil(t);
    let mut out = String::new();
    let mut it = f.iter();
    while let Some(&c) = it.next() {
        if c != b'%' {
            out.push(c as char);
            continue;
        }
        let Some(&k) = it.next() else { break };
        let h12 = if h % 12 == 0 { 12 } else { h % 12 };
        match k {
            b'Y' => out.push_str(&format!("{y:04}")),
            b'y' => out.push_str(&format!("{:02}", y % 100)),
            b'X' => out.push_str(&format!("{mo:02}")),
            b'x' => out.push_str(&mo.to_string()),
            b'D' => out.push_str(&format!("{d:02}")),
            b'd' => out.push_str(&d.to_string()),
            b'H' => out.push_str(&format!("{h:02}")),
            b'h' => out.push_str(&h.to_string()),
            b'I' => out.push_str(&format!("{h12:02}")),
            b'i' => out.push_str(&h12.to_string()),
            b'M' => out.push_str(&format!("{mi:02}")),
            b'm' => out.push_str(&mi.to_string()),
            b'S' => out.push_str(&format!("{s:02}")),
            b's' => out.push_str(&s.to_string()),
            b'A' => out.push_str(if h < 12 { "AM" } else { "PM" }),
            b'a' => out.push_str(if h < 12 { "am" } else { "pm" }),
            b'%' => out.push('%'),
            k => {
                out.push('%');
                out.push(k as char);
            }
        }
    }
    out.into_bytes()
}

/// Unix seconds as (year, month, day, hour, minute, second).
fn civil(t: i64) -> (i64, i64, i64, i64, i64, i64) {
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // Howard Hinnant's days-to-civil.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    (y, m, d, secs / 3600, secs / 60 % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings() {
        assert_eq!(substring(b"Hello world", 7, 11), b"world");
        assert_eq!(search(b"a,b,c", 1, b","), Some(2));
        assert_eq!(search(b"a,b,c", 3, b","), Some(4));
        assert_eq!(search(b"abc", 1, b"x"), None);
    }

    #[test]
    fn dates() {
        // 2026-10-24 10:05:09 UTC.
        let t = 1_792_836_309;
        assert_eq!(
            format_date(b"%Y-%X-%D %H:%M:%S %a", t),
            b"2026-10-24 10:05:09 am"
        );
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
    }
}
