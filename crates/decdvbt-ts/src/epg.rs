// made by CasualArclamp
//! The programme guide: the events the EIT lists for each service, present and
//! following (table 0x4E) and the schedule (0x50–0x5F), with their times as
//! seconds since 1970 so a viewer can lay them out on a clock.
//!
//! EN 300 468 §5.2.4 (EIT), §6.2.15 (extended event), §6.2.37 (short event),
//! §6.2.9 (content), §6.2.28 (parental rating), §6.2.20 (local time offset).

use std::collections::BTreeMap;

use crate::psi::{bcd, descriptors, dvb_text};

/// Events kept in all: a stream that lists more than a week of every channel
/// of a multiplex would still fit; one that goes on forever does not.
const MAX_EVENTS: usize = 60_000;
/// How long past an event's end it is kept, seconds.
const KEEP_AFTER_END: i64 = 3600;

/// One event of a service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpgEvent {
    pub event_id: u16,
    /// Start, seconds since 1970 UTC.
    pub start: i64,
    pub duration_s: u32,
    /// The running status: 1 not running, 2 starts in a few seconds, 3
    /// pausing, 4 running (0 undefined).
    pub running: u8,
    pub free_ca: bool,
    pub name: String,
    pub text: String,
    /// The extended event descriptors' text, by descriptor number.
    pub extended: BTreeMap<u8, String>,
    /// The first content nibble (EN 300 468 table 28): 1 movie/drama, 2 news,
    /// 3 show, 4 sports, 5 children's, 6 music, 7 arts, 8 social, 9 science,
    /// 10 leisure.
    pub genre: Option<u8>,
    /// The minimum age a parental rating gives, years.
    pub min_age: Option<u8>,
}

impl EpgEvent {
    pub fn end(&self) -> i64 {
        self.start + i64::from(self.duration_s)
    }

    /// The long description, or the short one if there is no long.
    pub fn description(&self) -> String {
        let long: String = self.extended.values().map(String::as_str).collect();
        if long.is_empty() {
            self.text.clone()
        } else {
            long
        }
    }
}

/// The name of a content nibble.
pub fn genre_name(nibble: u8) -> &'static str {
    match nibble {
        1 => "Movie / drama",
        2 => "News / current affairs",
        3 => "Show / game show",
        4 => "Sports",
        5 => "Children's",
        6 => "Music / ballet / dance",
        7 => "Arts / culture",
        8 => "Social / political",
        9 => "Education / science",
        10 => "Leisure / hobbies",
        11 => "Special characteristics",
        _ => "",
    }
}

/// Every event heard, by service.
#[derive(Debug, Clone, Default)]
pub struct Epg {
    /// Service id → event id → event.
    pub services: BTreeMap<u16, BTreeMap<u16, EpgEvent>>,
    /// Counts the changes, so a viewer can tell when to look again.
    pub version: u64,
    /// The stream's clock, seconds since 1970, when it has said.
    pub now: Option<i64>,
    count: usize,
}

impl Epg {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> usize {
        self.count
    }

    /// A schedule or present/following section's events (`body` after the
    /// section header: transport stream id, original network id, last
    /// section of the segment, last table id, then the events).
    pub fn section(&mut self, service: u16, body: &[u8]) {
        let mut at = 6;
        while at + 12 <= body.len() {
            let event_id = u16::from_be_bytes([body[at], body[at + 1]]);
            let start = &body[at + 2..at + 7];
            let dur = &body[at + 7..at + 10];
            let running = body[at + 10] >> 5;
            let free_ca = body[at + 10] & 0x10 != 0;
            let dl = u16::from_be_bytes([body[at + 10] & 0x0F, body[at + 11]]) as usize;
            let desc = &body[(at + 12).min(body.len())..(at + 12 + dl).min(body.len())];
            at += 12 + dl;
            if start == [0xFF; 5] {
                continue; // no start time (a reference event)
            }
            let mut ev = EpgEvent {
                event_id,
                start: mjd_unix(start),
                duration_s: (bcd(&dur[0..1]) * 3600 + bcd(&dur[1..2]) * 60 + bcd(&dur[2..3]))
                    as u32,
                running,
                free_ca,
                ..Default::default()
            };
            if self.now.is_some_and(|n| ev.end() + KEEP_AFTER_END < n) {
                continue; // over and gone
            }
            for (tag, d) in descriptors(desc) {
                match tag {
                    0x4D if d.len() >= 4 => {
                        // short_event_descriptor: language, name, text.
                        let nl = d[3] as usize;
                        ev.name = dvb_text(&d[4..(4 + nl).min(d.len())]);
                        if 4 + nl < d.len() {
                            let tl = d[4 + nl] as usize;
                            ev.text =
                                dvb_text(&d[(5 + nl).min(d.len())..(5 + nl + tl).min(d.len())]);
                        }
                    }
                    0x4E if d.len() >= 6 => {
                        // extended_event_descriptor: number, language, the
                        // items (skipped), text.
                        let items = d[4] as usize;
                        if let Some(&tl) = d.get(5 + items) {
                            let text = &d
                                [(6 + items).min(d.len())..(6 + items + tl as usize).min(d.len())];
                            ev.extended.insert(d[0] >> 4, dvb_text(text));
                        }
                    }
                    0x54 if !d.is_empty() => ev.genre = Some(d[0] >> 4),
                    0x55 if d.len() >= 4 => {
                        // The first rating: 0x01..=0x0F is an age of 3 more.
                        let r = d[3];
                        ev.min_age = (1..=0x0F).contains(&r).then(|| r + 3);
                    }
                    _ => {}
                }
            }
            self.insert(service, ev);
        }
    }

    fn insert(&mut self, service: u16, ev: EpgEvent) {
        let events = self.services.entry(service).or_default();
        match events.get_mut(&ev.event_id) {
            Some(old) => {
                // The extended text arrives in pieces over several sections
                // of the same version: keep what the old one had of it.
                let mut ev = ev;
                for (k, v) in &old.extended {
                    ev.extended.entry(*k).or_insert_with(|| v.clone());
                }
                if *old != ev {
                    *old = ev;
                    self.version += 1;
                }
            }
            None => {
                if self.count >= MAX_EVENTS {
                    return;
                }
                events.insert(ev.event_id, ev);
                self.count += 1;
                self.version += 1;
            }
        }
    }

    /// Forget the events long over.
    pub fn prune(&mut self) {
        let Some(now) = self.now else { return };
        let mut removed = 0;
        for events in self.services.values_mut() {
            let before = events.len();
            events.retain(|_, e| e.end() + KEEP_AFTER_END >= now);
            removed += before - events.len();
        }
        if removed > 0 {
            self.count -= removed;
            self.version += 1;
        }
    }

    /// What is on at `t` on `service` and what follows it, by the schedule.
    pub fn now_next(&self, service: u16, t: i64) -> (Option<&EpgEvent>, Option<&EpgEvent>) {
        let Some(events) = self.services.get(&service) else {
            return (None, None);
        };
        let now = events.values().find(|e| e.start <= t && t < e.end());
        let next = events
            .values()
            .filter(|e| e.start > t)
            .min_by_key(|e| e.start);
        (now, next)
    }

    /// The events of `service` that overlap `from..to`, in order.
    pub fn between(&self, service: u16, from: i64, to: i64) -> Vec<&EpgEvent> {
        let mut v: Vec<&EpgEvent> = self
            .services
            .get(&service)
            .map(|m| {
                m.values()
                    .filter(|e| e.start < to && e.end() > from)
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|e| e.start);
        v
    }
}

/// The local time offset descriptor's first entry (EN 300 468 §6.2.20): the
/// offset now and the next, and when the change falls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalTime {
    pub offset_min: i32,
    /// When the offset changes (summer time begins or ends), seconds since 1970.
    pub change: i64,
    pub next_offset_min: i32,
}

impl LocalTime {
    /// The offset from UTC at `t`, minutes.
    pub fn offset_at(&self, t: i64) -> i32 {
        if t >= self.change {
            self.next_offset_min
        } else {
            self.offset_min
        }
    }

    /// The descriptors of a TOT, if one carries an offset.
    pub(crate) fn from_descriptors(desc: &[u8]) -> Option<LocalTime> {
        descriptors(desc).find_map(|(tag, d)| {
            // country (3), region and polarity (1), offset (2 BCD), change
            // time (5), next offset (2 BCD).
            if tag != 0x58 || d.len() < 13 {
                return None;
            }
            let minutes = |b: &[u8], neg: bool| {
                let m = (bcd(&b[0..1]) * 60 + bcd(&b[1..2])) as i32;
                if neg { -m } else { m }
            };
            let neg = d[3] & 1 != 0;
            Some(LocalTime {
                offset_min: minutes(&d[4..6], neg),
                change: mjd_unix(&d[6..11]),
                next_offset_min: minutes(&d[11..13], neg),
            })
        })
    }
}

/// A 16-bit MJD and 24-bit BCD UTC time (EN 300 468 Annex C) as seconds since
/// 1970.
pub fn mjd_unix(b: &[u8]) -> i64 {
    let mjd = i64::from(u16::from_be_bytes([b[0], b[1]]));
    // MJD 40587 is 1970-01-01.
    (mjd - 40587) * 86400 + (bcd(&b[2..3]) * 3600 + bcd(&b[3..4]) * 60 + bcd(&b[4..5])) as i64
}

/// The calendar date and time of `t` seconds since 1970 (UTC):
/// year, month, day, hour, minute, second.
pub fn civil(t: i64) -> (i32, u32, u32, u32, u32, u32) {
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400) as u32);
    // Days since 1970 to a proleptic Gregorian date (Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day, secs / 3600, secs % 3600 / 60, secs % 60)
}

/// The day of the week of `t`: 0 Monday … 6 Sunday.
pub fn weekday(t: i64) -> u32 {
    // 1970-01-01 was a Thursday.
    (t.div_euclid(86400) + 3).rem_euclid(7) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psi::section;

    fn event(id: u16, mjd: u16, hms: [u8; 3], dur: [u8; 3], name: &str, long: &str) -> Vec<u8> {
        let mut e = vec![(id >> 8) as u8, id as u8, (mjd >> 8) as u8, mjd as u8];
        e.extend_from_slice(&hms);
        e.extend_from_slice(&dur);
        let mut d = vec![0x4D, (5 + name.len()) as u8];
        d.extend_from_slice(b"eng");
        d.push(name.len() as u8);
        d.extend_from_slice(name.as_bytes());
        d.push(0);
        // An extended event: descriptor 0 of 0, no items.
        d.extend_from_slice(&[0x4E, (6 + long.len()) as u8, 0x00]);
        d.extend_from_slice(b"eng");
        d.push(0);
        d.push(long.len() as u8);
        d.extend_from_slice(long.as_bytes());
        d.extend_from_slice(&[0x54, 2, 0x40, 0x00]); // sports
        d.extend_from_slice(&[0x55, 4, b'G', b'B', b'R', 0x09]); // 12+
        let dl = d.len();
        e.extend_from_slice(&[0x80 | (dl >> 8) as u8 & 0x0F, dl as u8]); // running 4
        e.extend_from_slice(&d);
        e
    }

    #[test]
    fn the_schedule_gives_events_with_times_text_genre_and_age() {
        let mut body = vec![0, 1, 0, 1, 0, 0x50];
        // MJD 60000 = 2023-02-25.
        body.extend(event(
            1,
            60000,
            [0x12, 0x00, 0x00],
            [0x01, 0x30, 0x00],
            "News",
            "The day's news.",
        ));
        body.extend(event(
            2,
            60000,
            [0x13, 0x30, 0x00],
            [0x00, 0x30, 0x00],
            "Match",
            "Football.",
        ));
        let sec = section(0x50, 7, 0, &body);
        let mut epg = Epg::new();
        epg.section(7, &sec[8..sec.len() - 4]);
        let t0 = 1_677_326_400; // 2023-02-25 12:00:00
        let ev = &epg.services[&7][&1];
        assert_eq!((ev.start, ev.duration_s), (t0, 5400));
        assert_eq!(ev.name, "News");
        assert_eq!(ev.description(), "The day's news.");
        assert_eq!(ev.genre, Some(4));
        assert_eq!(ev.min_age, Some(12));
        assert_eq!(ev.running, 4);
        let (now, next) = epg.now_next(7, t0 + 600);
        assert_eq!(now.map(|e| e.event_id), Some(1));
        assert_eq!(next.map(|e| e.event_id), Some(2));
        assert_eq!(epg.between(7, t0 + 5000, t0 + 6000).len(), 2);
        // The same section again changes nothing.
        let v = epg.version;
        epg.section(7, &sec[8..sec.len() - 4]);
        assert_eq!((epg.version, epg.events()), (v, 2));
        // An hour after the last one has ended, they go.
        epg.now = Some(t0 + 3 * 3600 + 3600 + 60);
        epg.prune();
        assert_eq!(epg.events(), 0);
    }

    #[test]
    fn dates_and_the_local_offset() {
        assert_eq!(civil(1_677_326_400), (2023, 2, 25, 12, 0, 0));
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(-1), (1969, 12, 31, 23, 59, 59));
        assert_eq!(weekday(1_677_326_400), 5); // a Saturday
        // UK: +0:00 now, +1:00 from 2023-03-26 01:00 UTC (MJD 60029).
        let d = [
            b'G', b'B', b'R', 0x02, 0x00, 0x00, 0xEA, 0x7D, 0x01, 0x00, 0x00, 0x01, 0x00,
        ];
        let mut desc = vec![0x58, d.len() as u8];
        desc.extend_from_slice(&d);
        let lt = LocalTime::from_descriptors(&desc).unwrap();
        assert_eq!(lt.offset_at(1_677_326_400), 0);
        assert_eq!(lt.offset_at(1_679_792_400 + 1), 60);
    }
}
