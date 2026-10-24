// made by CasualArclamp
//! The MHEG-5 classes the UK profile uses, loaded from the parse tree
//! (ISO/IEC 13522-5 §§8–41; the attributes and their tags, Annex A). One
//! [`Obj`] per object: the common Root/Ingredient state, then what its class
//! adds ([`Kind`]). Actions stay as parse-tree nodes and are interpreted when
//! performed (`engine.rs`).
//!
//! The class layout follows MythTV's freemheg (`Groups.cpp`,
//! `Ingredients.cpp`, `Visible.cpp`, `Text.cpp`, `TokenGroup.cpp`,
//! `Variables.cpp`, `Link.cpp`, `Stream.cpp`; GPL-2.0-or-later), rewritten.

use std::rc::Rc;

use crate::asn1::Node;
use crate::tags::*;

/// An object's name: its group (an application's or scene's file) and its
/// number in it (0 is the group itself).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ref {
    pub group: Rc<str>,
    pub num: i32,
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({} {})", self.group, self.num)
    }
}

/// A group id as written in a file ("~//a", "DSM://s/x.mhg", "/s/x.mhg",
/// "x.mhg") made absolute against `base` (the group it was written in): the
/// carousel path, "/s/x.mhg".
pub fn canonical(id: &[u8], base: &str) -> Rc<str> {
    let s = String::from_utf8_lossy(id);
    let mut s: &str = &s;
    for p in ["DSM:", "dsm:", "~"] {
        if let Some(r) = s.strip_prefix(p) {
            s = r;
        }
    }
    let path = if s.starts_with('/') {
        s.trim_start_matches('/').to_string()
    } else {
        // Relative to the folder of the group it is named in.
        let dir = base
            .rsplit_once('/')
            .map_or("", |d| d.0)
            .trim_start_matches('/');
        if dir.is_empty() {
            s.to_string()
        } else {
            format!("{dir}/{s}")
        }
    };
    // "a/./b", "a/../b".
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/")).into()
}

/// An object reference in the tree: an internal reference (a number, in
/// `group`) or an external one (a group id and a number).
pub fn object_ref(n: &Node, group: &Rc<str>) -> Option<Ref> {
    match n {
        Node::Int(num) => Some(Ref {
            group: group.clone(),
            num: *num,
        }),
        Node::Seq(a) => match a.as_slice() {
            [Node::Str(g), Node::Int(num)] => Some(Ref {
                group: canonical(g, group),
                num: *num,
            }),
            [Node::Str(g)] => Some(Ref {
                group: canonical(g, group),
                num: 0,
            }),
            [Node::Int(num)] => Some(Ref {
                group: group.clone(),
                num: *num,
            }),
            _ => None,
        },
        Node::Tagged(C_OBJECT_REFERENCE, a) if a.len() == 1 => object_ref(&a[0], group),
        _ => None,
    }
}

/// A variable's value (ISO/IEC 13522-5 §§30–35).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i32),
    Str(Vec<u8>),
    ObjRef(Ref),
    ContentRef(Vec<u8>),
}

impl Value {
    /// The value as an integer: an octet string is read as decimal (its
    /// leading sign and digits), as freemheg's integer variables do.
    pub fn as_int(&self) -> i32 {
        match self {
            Value::Int(v) => *v,
            Value::Bool(b) => *b as i32,
            Value::Str(s) => {
                let t = String::from_utf8_lossy(s);
                let t = t.trim_start();
                let end = t
                    .char_indices()
                    .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
                    .map_or(t.len(), |x| x.0);
                t[..end].parse().unwrap_or(0)
            }
            _ => 0,
        }
    }

    pub fn as_bool(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(v) => *v != 0,
            _ => false,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Value::Str(s) | Value::ContentRef(s) => s,
            _ => &[],
        }
    }
}

/// A colour: RGB and transparency (0 opaque, 255 clear), the UK profile's
/// absolute colours (D-Book 7 §13.10.3; an index colour is taken as grey).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Colour {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub t: u8,
}

impl Colour {
    pub const CLEAR: Colour = Colour {
        r: 0,
        g: 0,
        b: 0,
        t: 255,
    };
    pub const WHITE: Colour = Colour {
        r: 255,
        g: 255,
        b: 255,
        t: 0,
    };
    pub const BLACK: Colour = Colour {
        r: 0,
        g: 0,
        b: 0,
        t: 0,
    };

    pub fn from_bytes(b: &[u8]) -> Colour {
        let g = |i: usize| b.get(i).copied().unwrap_or(0);
        Colour {
            r: g(0),
            g: g(1),
            b: g(2),
            t: g(3),
        }
    }

    pub fn from_node(n: &Node) -> Option<Colour> {
        match n {
            Node::Str(s) => Some(Colour::from_bytes(s)),
            Node::Int(i) => {
                let v = (*i).clamp(0, 255) as u8;
                Some(Colour {
                    r: v,
                    g: v,
                    b: v,
                    t: 0,
                })
            }
            Node::Tagged(_, a) if a.len() == 1 => Colour::from_node(&a[0]),
            _ => None,
        }
    }
}

/// An ingredient's content (ISO/IEC 13522-5 §13.4): bytes in the file, or
/// a file named.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    None,
    Included(Vec<u8>),
    Referenced(Vec<u8>),
}

impl Content {
    fn from_node(n: Option<&Node>) -> Content {
        match n.and_then(|n| n.arg(0)) {
            Some(Node::Str(s)) => Content::Included(s.clone()),
            Some(Node::Seq(a)) => match a.first() {
                Some(Node::Str(s)) => Content::Referenced(s.clone()),
                Some(n) => n
                    .str()
                    .map_or(Content::None, |s| Content::Referenced(s.to_vec())),
                None => Content::None,
            },
            _ => Content::None,
        }
    }
}

/// A link's condition and effect (§23).
#[derive(Debug, Clone)]
pub struct Link {
    pub source: Ref,
    pub event: i32,
    pub data: Option<Value>,
    pub effect: Rc<[Node]>,
}

/// An application or a scene (§§9–11).
#[derive(Debug, Clone, Default)]
pub struct Group {
    pub is_app: bool,
    pub on_start_up: Rc<[Node]>,
    pub on_close_down: Rc<[Node]>,
    pub items: Vec<i32>,
    /// Scene: the input event register (D-Book 7 §13.5.2; 3 or 4 in UK use).
    pub input_register: i32,
    /// Application defaults (§10.2).
    pub default: Defaults,
    /// Timers: (id, fires at, engine ms).
    pub timers: Vec<(i32, u64)>,
    pub start_ms: u64,
}

/// The application's default attributes.
#[derive(Debug, Clone)]
pub struct Defaults {
    pub background: Colour,
    pub text_colour: Colour,
    pub font_attributes: Vec<u8>,
    pub line_colour: Colour,
    pub fill_colour: Colour,
    pub text_hook: i32,
    pub bitmap_hook: i32,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            background: Colour::CLEAR,
            text_colour: Colour::WHITE,
            font_attributes: b"plain.24.28.0".to_vec(),
            line_colour: Colour::WHITE,
            fill_colour: Colour::CLEAR,
            text_hook: 10,
            bitmap_hook: 4,
        }
    }
}

/// A decoded bitmap, RGBA.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// What a visible draws.
#[derive(Debug, Clone)]
pub enum Draw {
    Rectangle,
    Bitmap {
        tiling: bool,
        image: Option<Rc<Image>>,
        scale: Option<(i32, i32)>,
        offset: (i32, i32),
    },
    Text {
        font_attributes: Vec<u8>,
        h_just: i32,
        v_just: i32,
        wrap: bool,
    },
    Video {
        component_tag: i32,
        scale: Option<(i32, i32)>,
    },
    /// A visible this engine does not draw (slider, line art, hotspot…).
    Other,
}

/// A visible's state (§§24–29): place, size, colours, and its class's part.
#[derive(Debug, Clone)]
pub struct Visible {
    pub pos: (i32, i32),
    pub size: (i32, i32),
    pub line_width: i32,
    pub line_colour: Colour,
    pub fill_colour: Colour,
    /// A text's colours (`None`: the application's default).
    pub text_colour: Option<Colour>,
    pub background: Option<Colour>,
    pub transparency: i32,
    pub bordered: bool,
    pub draw: Draw,
    /// Interactible (§33): highlighted, interacting.
    pub highlight: bool,
}

/// An action slot: its actions, or `None` where the slot is null.
pub type Slot = Option<Rc<[Node]>>;

/// A token group or list group (§§36–37).
#[derive(Debug, Clone, Default)]
pub struct TokenGroup {
    pub movement: Vec<Vec<i32>>,
    /// (item, its action slots).
    pub items: Vec<(Ref, Vec<Slot>)>,
    pub no_token_slots: Vec<Slot>,
    /// 1-based; 0 means none.
    pub token: i32,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Group(Box<Group>),
    Link(Link),
    Variable(Value),
    Program {
        name: String,
    },
    Visible(Box<Visible>),
    TokenGroup(TokenGroup),
    /// A stream and its components (§§20–22).
    Stream {
        components: Vec<i32>,
    },
    /// Audio and other ingredients that only need a running state here.
    Other,
}

/// One MHEG-5 object.
#[derive(Debug, Clone)]
pub struct Obj {
    pub id: Ref,
    /// The class's tag (C_TEXT, C_LINK…).
    pub class: u32,
    pub initially_active: bool,
    pub shared: bool,
    pub available: bool,
    pub running: bool,
    pub content: Content,
    /// The data the content gave (a text's bytes).
    pub data: Vec<u8>,
    pub content_hook: Option<i32>,
    pub kind: Kind,
    /// The definition, for Clone and for restoring the original values.
    pub def: Rc<Node>,
}

/// An action sequence's actions.
pub fn actions(n: Option<&Node>) -> Rc<[Node]> {
    n.map_or_else(|| Rc::from(Vec::new()), |n| Rc::from(n.args().to_vec()))
}

fn pair(n: Option<&Node>) -> Option<(i32, i32)> {
    let n = n?;
    Some((n.arg(0)?.int()?, n.arg(1)?.int()?))
}

/// The objects of a group file: the group, then its items (with a stream's
/// components after it). `group` is the group's canonical id.
pub fn load_group(root: &Node, group: &Rc<str>) -> Option<Vec<Obj>> {
    let class = root.tag()?;
    if class != C_APPLICATION && class != C_SCENE {
        return None;
    }
    let def = Rc::new(root.clone());
    let mut g = Group {
        is_app: class == C_APPLICATION,
        on_start_up: actions(root.get(C_ON_START_UP)),
        on_close_down: actions(root.get(C_ON_CLOSE_DOWN)),
        input_register: root.get_int(C_INPUT_EVENT_REGISTER).unwrap_or(3),
        ..Default::default()
    };
    if let Some(d) = root.get(C_DEFAULT_ATTRIBUTES) {
        let def = &mut g.default;
        if let Some(c) = d.get(C_BACKGROUND_COLOUR).and_then(Colour::from_node) {
            def.background = c;
        }
        if let Some(c) = d.get(C_TEXT_COLOUR).and_then(Colour::from_node) {
            def.text_colour = c;
        }
        if let Some(s) = d.get(C_FONT_ATTRIBUTES).and_then(|n| n.str()) {
            def.font_attributes = s.to_vec();
        }
        if let Some(h) = d.get_int(C_TEXT_CONTENT_HOOK) {
            def.text_hook = h;
        }
        if let Some(h) = d.get_int(C_BITMAP_CONTENT_HOOK) {
            def.bitmap_hook = h;
        }
    }
    let mut out = Vec::new();
    let mut items = Vec::new();
    for item in root.get(C_ITEMS).map_or(&[][..], |n| n.args()) {
        for o in load_ingredient(item, group) {
            items.push(o.id.num);
            out.push(o);
        }
    }
    g.items = items;
    out.insert(
        0,
        Obj {
            id: Ref {
                group: group.clone(),
                num: 0,
            },
            class,
            initially_active: false,
            shared: true,
            available: false,
            running: false,
            content: Content::None,
            data: Vec::new(),
            content_hook: None,
            kind: Kind::Group(Box::new(g)),
            def,
        },
    );
    Some(out)
}

fn visible(n: &Node, draw: Draw) -> Visible {
    let size = pair(n.get(C_ORIGINAL_BOX_SIZE)).unwrap_or((0, 0));
    let pos = pair(n.get(C_ORIGINAL_POSITION)).unwrap_or((0, 0));
    Visible {
        pos,
        size,
        line_width: n.get_int(C_ORIGINAL_LINE_WIDTH).unwrap_or(1),
        line_colour: n
            .get(C_ORIGINAL_REF_LINE_COLOUR)
            .and_then(Colour::from_node)
            .unwrap_or(Colour::WHITE),
        fill_colour: n
            .get(C_ORIGINAL_REF_FILL_COLOUR)
            .and_then(Colour::from_node)
            .unwrap_or(Colour::CLEAR),
        text_colour: n.get(C_TEXT_COLOUR).and_then(Colour::from_node),
        background: n.get(C_BACKGROUND_COLOUR).and_then(Colour::from_node),
        transparency: n.get_int(C_ORIGINAL_TRANSPARENCY).unwrap_or(0),
        bordered: n.get_bool(C_BORDERED_BOUNDING_BOX).unwrap_or(true),
        draw,
        highlight: false,
    }
}

/// An ingredient (and, for a stream, its components).
pub fn load_ingredient(n: &Node, group: &Rc<str>) -> Vec<Obj> {
    let Some(class) = n.tag() else {
        return vec![];
    };
    let Some(num) = n.arg(0).and_then(|a| a.int()) else {
        return vec![];
    };
    let id = Ref {
        group: group.clone(),
        num,
    };
    let mut extra = Vec::new();
    let kind = match class {
        C_LINK => {
            let c = n.get(C_LINK_CONDITION);
            let source = c
                .and_then(|c| c.arg(0))
                .and_then(|s| object_ref(s, group))
                .unwrap_or(id.clone());
            let event = c.and_then(|c| c.arg(1)).and_then(|e| e.int()).unwrap_or(0);
            let data = c.and_then(|c| c.arg(2)).and_then(|d| match d {
                Node::Bool(b) => Some(Value::Bool(*b)),
                Node::Int(i) | Node::Enum(i) => Some(Value::Int(*i)),
                Node::Str(s) => Some(Value::Str(s.clone())),
                _ => None,
            });
            Kind::Link(Link {
                source,
                event,
                data,
                effect: actions(n.get(C_LINK_EFFECT)),
            })
        }
        C_BOOLEAN_VARIABLE
        | C_INTEGER_VARIABLE
        | C_OCTET_STRING_VARIABLE
        | C_OBJECT_REF_VARIABLE
        | C_CONTENT_REF_VARIABLE => {
            Kind::Variable(original_value(class, n.get(C_ORIGINAL_VALUE), group))
        }
        C_RESIDENT_PROGRAM | C_REMOTE_PROGRAM | C_INTERCHANGED_PROGRAM => Kind::Program {
            name: String::from_utf8_lossy(n.get(C_NAME).and_then(|s| s.str()).unwrap_or(b""))
                .into_owned(),
        },
        C_RECTANGLE => Kind::Visible(Box::new(visible(n, Draw::Rectangle))),
        C_BITMAP => Kind::Visible(Box::new(visible(
            n,
            Draw::Bitmap {
                tiling: n.get_bool(C_TILING).unwrap_or(false),
                image: None,
                scale: None,
                offset: (0, 0),
            },
        ))),
        C_TEXT | C_HYPER_TEXT | C_ENTRY_FIELD => Kind::Visible(Box::new(visible(
            n,
            Draw::Text {
                font_attributes: n
                    .get(C_FONT_ATTRIBUTES)
                    .and_then(|s| s.str())
                    .map(|s| s.to_vec())
                    .unwrap_or_default(),
                h_just: n.get_int(C_HORIZONTAL_JUSTIFICATION).unwrap_or(1),
                v_just: n.get_int(C_VERTICAL_JUSTIFICATION).unwrap_or(1),
                wrap: n.get_bool(C_TEXT_WRAPPING).unwrap_or(false),
            },
        ))),
        C_LINE_ART | C_DYNAMIC_LINE_ART | C_SLIDER | C_HOTSPOT | C_PUSH_BUTTON
        | C_SWITCH_BUTTON => Kind::Visible(Box::new(visible(n, Draw::Other))),
        C_TOKEN_GROUP | C_LIST_GROUP => {
            let slots = |s: Option<&Node>| -> Vec<Slot> {
                s.map_or(&[][..], |s| s.args())
                    .iter()
                    .map(|a| match a {
                        Node::Null => None,
                        a => Some(Rc::from(a.args().to_vec())),
                    })
                    .collect()
            };
            let items = n
                .get(C_TOKEN_GROUP_ITEMS)
                .map_or(&[][..], |s| s.args())
                .iter()
                .filter_map(|it| Some((object_ref(it.arg(0)?, group)?, slots(it.arg(1)))))
                .collect();
            let movement = n
                .get(C_MOVEMENT_TABLE)
                .map_or(&[][..], |s| s.args())
                .iter()
                .map(|row| row.args().iter().filter_map(|x| x.int()).collect())
                .collect();
            Kind::TokenGroup(TokenGroup {
                movement,
                items,
                no_token_slots: slots(n.get(C_NO_TOKEN_ACTION_SLOTS)),
                token: 1,
            })
        }
        C_STREAM => {
            let mut comps = Vec::new();
            for c in n.get(C_MULTIPLEX).map_or(&[][..], |s| s.args()) {
                for mut o in load_ingredient(c, group) {
                    if o.class == C_VIDEO {
                        o.initially_active = c.get_bool(C_INITIALLY_ACTIVE).unwrap_or(true);
                    }
                    comps.push(o.id.num);
                    extra.push(o);
                }
            }
            Kind::Stream { components: comps }
        }
        C_VIDEO => Kind::Visible(Box::new(visible(
            n,
            Draw::Video {
                component_tag: n.get_int(C_COMPONENT_TAG).unwrap_or(-1),
                scale: None,
            },
        ))),
        _ => Kind::Other,
    };
    let initially_active = match class {
        C_RESIDENT_PROGRAM | C_REMOTE_PROGRAM | C_INTERCHANGED_PROGRAM => false,
        _ => n.get_bool(C_INITIALLY_ACTIVE).unwrap_or(true),
    };
    let mut out = vec![Obj {
        id,
        class,
        initially_active,
        shared: n.get_bool(C_SHARED).unwrap_or(false),
        available: false,
        running: false,
        content: Content::from_node(n.get(C_ORIGINAL_CONTENT)),
        data: Vec::new(),
        content_hook: n.get_int(C_CONTENT_HOOK),
        kind,
        def: Rc::new(n.clone()),
    }];
    out.extend(extra);
    out
}

/// A variable's original value.
pub fn original_value(class: u32, n: Option<&Node>, group: &Rc<str>) -> Value {
    let v = n.and_then(|n| n.arg(0));
    match class {
        C_BOOLEAN_VARIABLE => Value::Bool(v.and_then(|v| v.bool()).unwrap_or(false)),
        C_INTEGER_VARIABLE => Value::Int(v.and_then(|v| v.int()).unwrap_or(0)),
        C_OCTET_STRING_VARIABLE => Value::Str(v.and_then(|v| v.str()).unwrap_or(b"").to_vec()),
        C_OBJECT_REF_VARIABLE => {
            Value::ObjRef(v.and_then(|v| object_ref(v, group)).unwrap_or(Ref {
                group: group.clone(),
                num: 0,
            }))
        }
        _ => Value::ContentRef(v.and_then(|v| v.str()).unwrap_or(b"").to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_ids() {
        let base = "/s/init.mhg";
        assert_eq!(&*canonical(b"~//a", base), "/a");
        assert_eq!(&*canonical(b"DSM://s/x.mhg", base), "/s/x.mhg");
        assert_eq!(&*canonical(b"/g/r.png", base), "/g/r.png");
        assert_eq!(&*canonical(b"menu.mhg", base), "/s/menu.mhg");
        assert_eq!(&*canonical(b"../g/x.png", base), "/g/x.png");
    }
}
