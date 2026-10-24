// made by CasualArclamp
//! Engine tests on small applications built here in BER.

use std::collections::HashMap;

use crate::engine::{Engine, Fetch, Files, key};
use crate::render::{Renderer, WIDTH};
use crate::tags::*;

/// A tiny BER writer: context tags (constructed), and the universal types.
fn tlv(first: u8, tag: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    if tag < 31 {
        v.push(first | tag as u8);
    } else {
        v.push(first | 0x1F);
        let mut groups = vec![(tag & 0x7F) as u8];
        let mut t = tag >> 7;
        while t > 0 {
            groups.push(0x80 | (t & 0x7F) as u8);
            t >>= 7;
        }
        v.extend(groups.iter().rev());
    }
    if body.len() < 128 {
        v.push(body.len() as u8);
    } else {
        v.push(0x82);
        v.extend_from_slice(&(body.len() as u16).to_be_bytes());
    }
    v.extend_from_slice(body);
    v
}

fn t(tag: u32, parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0xA0, tag, &parts.concat())
}

/// A primitive context tag (booleans, integers, strings by its tag).
fn p(tag: u32, body: &[u8]) -> Vec<u8> {
    tlv(0x80, tag, body)
}

fn int(v: i32) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().take(3).take_while(|&&x| x == 0).count();
    tlv(0x00, 2, &b[skip..])
}

fn s(v: &[u8]) -> Vec<u8> {
    tlv(0x00, 4, v)
}

fn boolean(b: bool) -> Vec<u8> {
    tlv(0x00, 1, &[if b { 0xFF } else { 0 }])
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x20, 16, &parts.concat())
}

fn ext(group: &str, n: i32) -> Vec<u8> {
    seq(&[s(group.as_bytes()), int(n)])
}

struct Map(HashMap<String, Vec<u8>>);

impl Files for Map {
    fn fetch(&self, path: &str) -> Fetch {
        match self.0.get(path) {
            Some(b) => Fetch::Ready(b.clone()),
            None => Fetch::Missing,
        }
    }
}

/// `/a` starts `/s/menu`; the scene has a red rectangle, a text, a counter
/// and links: the red key writes "Hello" into the text, a TestVariable on
/// the counter (through the activate–test–deactivate idiom) moves the
/// rectangle, and a timer counts.
fn files() -> Map {
    let app = t(
        C_APPLICATION,
        &[
            ext("/a", 0),
            t(C_ON_START_UP, &[t(C_TRANSITION_TO, &[ext("/s/menu", 0)])]),
            t(C_ITEMS, &[]),
        ],
    );
    let red = [0xFFu8, 0, 0, 0];
    let scene = t(
        C_SCENE,
        &[
            ext("/s/menu", 0),
            t(
                C_ITEMS,
                &[
                    t(
                        C_RECTANGLE,
                        &[
                            int(1),
                            t(C_ORIGINAL_BOX_SIZE, &[int(100), int(50)]),
                            t(C_ORIGINAL_POSITION, &[int(10), int(20)]),
                            t(C_ORIGINAL_REF_FILL_COLOUR, &[s(&red)]),
                        ],
                    ),
                    t(
                        C_TEXT,
                        &[
                            int(2),
                            t(C_ORIGINAL_CONTENT, &[s(b"")]),
                            t(C_ORIGINAL_BOX_SIZE, &[int(300), int(40)]),
                            t(C_ORIGINAL_POSITION, &[int(200), int(200)]),
                        ],
                    ),
                    t(
                        C_INTEGER_VARIABLE,
                        &[int(3), t(C_ORIGINAL_VALUE, &[int(0)])],
                    ),
                    // On red: "Hello", add 5 to the counter, test it.
                    t(
                        C_LINK,
                        &[
                            int(4),
                            t(
                                C_LINK_CONDITION,
                                &[int(0), tlv(0x00, 10, &[6]), int(key::RED)],
                            ),
                            t(
                                C_LINK_EFFECT,
                                &[
                                    t(C_SET_DATA, &[int(2), s(b"Hello")]),
                                    t(C_ADD, &[int(3), int(5)]),
                                    t(C_ACTIVATE, &[int(5)]),
                                    t(
                                        C_TEST_VARIABLE,
                                        &[int(3), int(1), t(C_NEW_GENERIC_INTEGER, &[int(5)])],
                                    ),
                                    t(C_DEACTIVATE, &[int(5)]),
                                    t(C_SET_TIMER, &[int(0), int(7), seq(&[int(100)])]),
                                ],
                            ),
                        ],
                    ),
                    // Fires only between the Activate and the Deactivate above.
                    t(
                        C_LINK,
                        &[
                            int(5),
                            p(C_INITIALLY_ACTIVE, &[0]),
                            t(
                                C_LINK_CONDITION,
                                &[int(3), tlv(0x00, 10, &[23]), boolean(true)],
                            ),
                            t(
                                C_LINK_EFFECT,
                                &[t(C_SET_POSITION, &[int(1), int(400), int(300)])],
                            ),
                        ],
                    ),
                    // The timer: the counter becomes 99.
                    t(
                        C_LINK,
                        &[
                            int(6),
                            t(C_LINK_CONDITION, &[int(0), tlv(0x00, 10, &[8]), int(7)]),
                            t(
                                C_LINK_EFFECT,
                                &[t(
                                    C_SET_VARIABLE,
                                    &[int(3), t(C_NEW_GENERIC_INTEGER, &[int(99)])],
                                )],
                            ),
                        ],
                    ),
                ],
            ),
            p(C_INPUT_EVENT_REGISTER, &[3]),
        ],
    );
    let mut m = HashMap::new();
    m.insert("/a".to_string(), app);
    m.insert("/s/menu".to_string(), scene);
    Map(m)
}

fn pixel(r: &Renderer, x: usize, y: usize) -> [u8; 4] {
    let i = (y * WIDTH + x) * 4;
    r.rgba[i..i + 4].try_into().unwrap()
}

#[test]
fn boots_draws_and_reacts_to_keys_and_timers() {
    let f = files();
    let mut e = Engine::new();
    e.boot(&f);
    assert_eq!(e.scene(), Some("/s/menu"));
    let mut r = Renderer::new();
    assert!(e.tick(0, &f));
    r.render(&e);
    assert_eq!(pixel(&r, 15, 25), [255, 0, 0, 255]);
    assert_eq!(pixel(&r, 5, 5)[3], 0, "uncovered pixels are clear");

    e.key(key::RED);
    e.tick(40, &f);
    let shown = e.shown().join("\n");
    assert!(shown.contains("\"Hello\""), "{shown}");
    // The TestEvent link ran while it was active: the rectangle moved.
    assert!(shown.contains("Rectangle at (400, 300)"), "{shown}");
    r.render(&e);
    assert_eq!(pixel(&r, 405, 305), [255, 0, 0, 255]);

    // 100 ms on, the timer the red key set fires.
    e.tick(200, &f);
    let var = crate::obj::Ref {
        group: "/s/menu".into(),
        num: 3,
    };
    assert_eq!(e.var(&var).map(|v| v.as_int()), Some(99), "the timer fired");
}

#[test]
fn a_missing_scene_is_an_engine_event_not_a_hang() {
    let mut f = files();
    f.0.remove("/s/menu");
    let mut e = Engine::new();
    e.boot(&f);
    assert_eq!(e.app(), Some("/a"));
    assert_eq!(e.scene(), None);
    for ms in (0..2000).step_by(40) {
        e.tick(ms, &f);
    }
    assert_eq!(e.scene(), None);
}
