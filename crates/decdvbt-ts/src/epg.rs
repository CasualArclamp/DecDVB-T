// made by CasualArclamp
//! The programme guide: the events the EIT lists for each service, present and
//! following (table 0x4E) and the schedule (0x50–0x5F), with their times as
//! seconds since 1970 so a viewer can lay them out on a clock.
//!
//! The tables of this multiplex (0x4E, 0x50–0x5F) and of the others the
//! network carries (0x4F, 0x60–0x6F): on Freeview every multiplex sends the
//! whole network's guide, eight days of it.
//!
//! EN 300 468 §5.2.4 (EIT), §6.2.15 (extended event), §6.2.37 (short event),
//! §6.2.9 (content), §6.2.28 (parental rating), §6.2.20 (local time offset),
//! §6.2.8 (component); TS 102 323 §12.1 (content identifier, CRIDs); the DTG
//! D-Book's guidance descriptor (0x89, private data specifier 0x233A).

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
    /// Every content byte (EN 300 468 table 29: genre nibble, then the
    /// detail nibble), in the order sent.
    pub content: Vec<u8>,
    /// The event is UK Freeview's (original network 0x233A), whose content
    /// bytes are the D-Book's genres, not EN 300 468's (see [`EpgEvent::kinds`]).
    pub freeview: bool,
    /// The episode's, the series' and a recommendation's CRIDs as sent (on
    /// Freeview relative, "/m/KEIS": [`Epg::full_crid`] adds the authority).
    pub episode_crid: Option<String>,
    pub series_crid: Option<String>,
    pub recommendation_crid: Option<String>,
    /// From the component descriptors: high definition video, subtitles,
    /// subtitles for the hard of hearing, audio description, a signer in
    /// vision, surround sound.
    pub hd: bool,
    pub subtitles: bool,
    pub hard_of_hearing: bool,
    pub audio_description: bool,
    pub signed: bool,
    pub surround: bool,
    /// The broadcaster's warning ("Contains strong language"), D-Book.
    pub guidance: Option<String>,
    /// The extended event descriptors' items: (what, value), e.g. ("Director",
    /// "…").
    pub items: Vec<(String, String)>,
}

impl EpgEvent {
    pub fn end(&self) -> i64 {
        self.start + i64::from(self.duration_s)
    }

    /// The names of the event's kinds of programme, from its content bytes,
    /// each once.
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = Vec::new();
        for &c in &self.content {
            let n = if self.freeview {
                freeview_genre_name(c >> 4)
            } else {
                content_name(c)
            };
            if !n.is_empty() && !v.contains(&n) {
                v.push(n);
            }
        }
        v
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

/// The name of a content byte's detail (EN 300 468 table 29), or of its
/// genre if the detail is general or not one the table names.
pub fn content_name(byte: u8) -> &'static str {
    let name = match byte {
        0x10 => "Movie / drama",
        0x11 => "Detective / thriller",
        0x12 => "Adventure / western / war",
        0x13 => "Science fiction / fantasy / horror",
        0x14 => "Comedy",
        0x15 => "Soap / melodrama / folklore",
        0x16 => "Romance",
        0x17 => "Serious / classical / religious / historical drama",
        0x18 => "Adult movie / drama",
        0x20 => "News / current affairs",
        0x21 => "News / weather report",
        0x22 => "News magazine",
        0x23 => "Documentary",
        0x24 => "Discussion / interview / debate",
        0x30 => "Show / game show",
        0x31 => "Game show / quiz / contest",
        0x32 => "Variety show",
        0x33 => "Talk show",
        0x40 => "Sports",
        0x41 => "Special event",
        0x42 => "Sports magazine",
        0x43 => "Football / soccer",
        0x44 => "Tennis / squash",
        0x45 => "Team sports",
        0x46 => "Athletics",
        0x47 => "Motor sport",
        0x48 => "Water sport",
        0x49 => "Winter sports",
        0x4A => "Equestrian",
        0x4B => "Martial sports",
        0x50 => "Children's / youth",
        0x51 => "Pre-school children's",
        0x52 => "Entertainment for 6 to 14",
        0x53 => "Entertainment for 10 to 16",
        0x54 => "Informational / educational / school",
        0x55 => "Cartoons / puppets",
        0x60 => "Music / ballet / dance",
        0x61 => "Rock / pop",
        0x62 => "Serious / classical music",
        0x63 => "Folk / traditional music",
        0x64 => "Jazz",
        0x65 => "Musical / opera",
        0x66 => "Ballet",
        0x70 => "Arts / culture",
        0x71 => "Performing arts",
        0x72 => "Fine arts",
        0x73 => "Religion",
        0x74 => "Popular culture / traditional arts",
        0x75 => "Literature",
        0x76 => "Film / cinema",
        0x77 => "Experimental film / video",
        0x78 => "Broadcasting / press",
        0x79 => "New media",
        0x7A => "Arts / culture magazine",
        0x7B => "Fashion",
        0x80 => "Social / political / economics",
        0x81 => "Magazine / report / documentary",
        0x82 => "Economics / social advisory",
        0x83 => "Remarkable people",
        0x90 => "Education / science / factual",
        0x91 => "Nature / animals / environment",
        0x92 => "Technology / natural sciences",
        0x93 => "Medicine / physiology / psychology",
        0x94 => "Foreign countries / expeditions",
        0x95 => "Social / spiritual sciences",
        0x96 => "Further education",
        0x97 => "Languages",
        0xA0 => "Leisure / hobbies",
        0xA1 => "Tourism / travel",
        0xA2 => "Handicraft",
        0xA3 => "Motoring",
        0xA4 => "Fitness and health",
        0xA5 => "Cooking",
        0xA6 => "Advertisement / shopping",
        0xA7 => "Gardening",
        0xB0 => "Original language",
        0xB1 => "Black and white",
        0xB2 => "Unpublished",
        0xB3 => "Live broadcast",
        _ => "",
    };
    if name.is_empty() {
        genre_name(byte >> 4)
    } else {
        name
    }
}

/// UK Freeview's genres (the DTG D-Book's use of the content descriptor's
/// first nibble; the second is 0): not EN 300 468's table 29.
pub fn freeview_genre_name(nibble: u8) -> &'static str {
    match nibble {
        0x1 => "Movie",
        0x2 | 0x7 | 0x8 => "News and factual",
        0x3 | 0x6 => "Entertainment",
        0x4 => "Sport",
        0x5 => "Children's",
        0x9 => "Education",
        0xA => "Lifestyle",
        0xF => "Drama",
        _ => "",
    }
}

/// A Freeview genre nibble as the nearest EN 300 468 one (for colours and
/// the like that go by the standard's).
fn freeview_as_standard(nibble: u8) -> u8 {
    match nibble {
        0x6 => 0x3,
        0x7 | 0x8 => 0x2,
        0xF => 0x1,
        n => n,
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
    /// Each service's default CRID authority (the SDT's default authority
    /// descriptor, "fp.bbc.co.uk"), for the relative CRIDs of its events.
    pub authorities: BTreeMap<u16, String>,
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
        // The original network: UK Freeview's genres are its own.
        let freeview = body.len() >= 4 && u16::from_be_bytes([body[2], body[3]]) == 0x233A;
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
                freeview,
                ..Default::default()
            };
            if self.now.is_some_and(|n| ev.end() + KEEP_AFTER_END < n) {
                continue; // over and gone
            }
            // The private data specifier in force (0x233A: the UK's DTG).
            let mut pds = 0u32;
            for (tag, d) in descriptors(desc) {
                match tag {
                    0x5F if d.len() >= 4 => pds = u32::from_be_bytes([d[0], d[1], d[2], d[3]]),
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
                        // items (what, value), text.
                        let items = d[4] as usize;
                        let list = &d[5..(5 + items).min(d.len())];
                        let mut k = 0;
                        while k < list.len() {
                            let dl = list[k] as usize;
                            let what = list.get(k + 1..k + 1 + dl).unwrap_or(&[]);
                            let il = list.get(k + 1 + dl).copied().unwrap_or(0) as usize;
                            let value = list.get(k + 2 + dl..k + 2 + dl + il).unwrap_or(&[]);
                            ev.items.push((dvb_text(what), dvb_text(value)));
                            k += 2 + dl + il;
                        }
                        if let Some(&tl) = d.get(5 + items) {
                            let text = &d
                                [(6 + items).min(d.len())..(6 + items + tl as usize).min(d.len())];
                            ev.extended.insert(d[0] >> 4, dvb_text(text));
                        }
                    }
                    0x54 if !d.is_empty() => {
                        // content_descriptor: (nibbles, user byte) pairs.
                        ev.genre = Some(if freeview {
                            freeview_as_standard(d[0] >> 4)
                        } else {
                            d[0] >> 4
                        });
                        ev.content.extend(d.iter().step_by(2));
                    }
                    0x50 if d.len() >= 2 => {
                        // component_descriptor: what the event's streams are
                        // (EN 300 468 table 26).
                        let (content, kind) = (d[0] & 0x0F, d[1]);
                        match content {
                            0x1 if (0x09..=0x10).contains(&kind) => ev.hd = true,
                            0x5 if (0x0B..=0x10).contains(&kind) => ev.hd = true,
                            0x9 => ev.hd = true, // HEVC: HD or UHD
                            0x3 if (0x10..=0x15).contains(&kind) => ev.subtitles = true,
                            0x3 if (0x20..=0x25).contains(&kind) => {
                                ev.subtitles = true;
                                ev.hard_of_hearing = true;
                            }
                            0x3 if kind == 0x30 || kind == 0x31 => ev.signed = true,
                            0x2 | 0x6 if matches!(kind, 0x40 | 0x44 | 0x47 | 0x48) => {
                                ev.audio_description = true
                            }
                            0x6 if matches!(kind, 0x05 | 0x07) => ev.surround = true,
                            0x4 | 0x7 => ev.surround = true, // AC-3, DTS
                            _ => {}
                        }
                    }
                    0x76 => {
                        // content_identifier_descriptor: CRIDs, by type (the
                        // low nibble: 1 the item, 2 the series, 3 a
                        // recommendation; Freeview sends 0x31..0x33).
                        let mut k = 0;
                        while k < d.len() {
                            let (kind, location) = (d[k] >> 2, d[k] & 0x03);
                            if location == 0 {
                                let cl = d.get(k + 1).copied().unwrap_or(0) as usize;
                                let crid = d
                                    .get(k + 2..k + 2 + cl)
                                    .map(|c| c.iter().map(|&b| b as char).collect::<String>());
                                match kind & 0x0F {
                                    1 => ev.episode_crid = crid,
                                    2 => ev.series_crid = crid,
                                    3 => ev.recommendation_crid = crid,
                                    _ => {}
                                }
                                k += 2 + cl;
                            } else {
                                // A reference into a CRID table: not sent here.
                                k += 3;
                            }
                        }
                    }
                    0x89 if pds == 0x233A && !d.is_empty() => {
                        // guidance_descriptor (D-Book): type 0 is a language
                        // and the text, type 1 a mode bit first.
                        let text = match d[0] & 0x03 {
                            0 => d.get(4..),
                            1 => d.get(5..),
                            _ => None,
                        };
                        ev.guidance = text.map(dvb_text).filter(|t| !t.is_empty());
                    }
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

    /// A CRID with its authority: a relative one ("/m/KEIS") takes the
    /// service's default authority (TS 102 323 §6.3.1).
    pub fn full_crid(&self, service: u16, crid: &str) -> String {
        if crid.starts_with("crid://") {
            return crid.to_string();
        }
        match self.authorities.get(&service) {
            Some(a) => format!("crid://{a}{crid}"),
            None => crid.to_string(),
        }
    }

    /// Every showing of an episode (by its full CRID) across the services,
    /// in time order.
    pub fn showings(&self, episode: &str) -> Vec<(u16, &EpgEvent)> {
        self.matching(episode, |e| e.episode_crid.as_deref())
    }

    /// Every event of a series (by its full CRID), in time order.
    pub fn series_events(&self, series: &str) -> Vec<(u16, &EpgEvent)> {
        self.matching(series, |e| e.series_crid.as_deref())
    }

    fn matching<'a>(
        &'a self,
        full: &str,
        crid: impl Fn(&EpgEvent) -> Option<&str>,
    ) -> Vec<(u16, &'a EpgEvent)> {
        let mut v: Vec<(u16, &EpgEvent)> = self
            .services
            .iter()
            .flat_map(|(&s, m)| m.values().map(move |e| (s, e)))
            .filter(|(s, e)| {
                crid(e).is_some_and(|c| self.full_crid(*s, c).eq_ignore_ascii_case(full))
            })
            .collect();
        v.sort_by_key(|(_, e)| e.start);
        v
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

    /// A Freeview event as the UK sends it: components, CRIDs, a D-Book
    /// guidance warning behind its private data specifier, a D-Book genre.
    #[test]
    fn freeview_details_are_read() {
        let mut e = vec![0, 9, 0xEA, 0x60, 0x18, 0x00, 0x00, 0x00, 0x45, 0x00];
        let mut d = vec![0x4D, 5 + 4, b'e', b'n', b'g', 4];
        d.extend_from_slice(b"Film");
        d.push(0);
        // Components: H.264 HD video, DVB subtitles for the hard of hearing,
        // HE-AAC audio description.
        for (c, t) in [(0x05u8, 0x0Bu8), (0x03, 0x20), (0x06, 0x47)] {
            d.extend_from_slice(&[0x50, 6, 0xF0 | c, t, 1, b'e', b'n', b'g']);
        }
        // CRIDs: the item (0x31) and the series (0x32), relative.
        d.extend_from_slice(&[0x76, 2 + 7 + 2 + 7, 0x31 << 2, 7]);
        d.extend_from_slice(b"/m/KEIS");
        d.extend_from_slice(&[0x32 << 2, 7]);
        d.extend_from_slice(b"/m-XW9N");
        // Guidance: only after the DTG's private data specifier.
        d.extend_from_slice(&[0x5F, 4, 0x00, 0x00, 0x23, 0x3A]);
        let warn = b"Contains strong language";
        d.extend_from_slice(&[0x89, (4 + warn.len()) as u8, 0xFC, b'e', b'n', b'g']);
        d.extend_from_slice(warn);
        d.extend_from_slice(&[0x54, 2, 0xF0, 0x00]); // Freeview: drama
        e.extend_from_slice(&[0x80 | (d.len() >> 8) as u8, d.len() as u8]);
        e.extend_from_slice(&d);
        // Network 0x233A: Freeview.
        let mut body = vec![0x10, 0x01, 0x23, 0x3A, 0, 0x60];
        body.extend(e);
        let sec = section(0x60, 4164, 0, &body);
        let mut epg = Epg::new();
        epg.authorities.insert(4164, "fp.bbc.co.uk".into());
        epg.section(4164, &sec[8..sec.len() - 4]);
        let ev = &epg.services[&4164][&9];
        assert!(ev.hd && ev.subtitles && ev.hard_of_hearing && ev.audio_description);
        assert!(!ev.signed && !ev.surround);
        assert_eq!(ev.guidance.as_deref(), Some("Contains strong language"));
        assert_eq!(ev.kinds(), ["Drama"]);
        assert_eq!(ev.genre, Some(1));
        let full = epg.full_crid(4164, ev.episode_crid.as_deref().unwrap());
        assert_eq!(full, "crid://fp.bbc.co.uk/m/KEIS");
        assert_eq!(epg.showings("CRID://fp.bbc.co.uk/m/KEIS").len(), 1);
        assert_eq!(epg.series_events("crid://fp.bbc.co.uk/m-XW9N").len(), 1);
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
