// made by CasualArclamp
//! The MHEG-5 engine: the running application and scene, their objects'
//! life cycle (preparation, activation, deactivation, destruction), links
//! and events, and the elementary actions (ISO/IEC 13522-5 §§7, 46; the UK
//! profile, ETSI ES 202 184 / D-Book 7 §13).
//!
//! The behaviour follows MythTV's freemheg `Engine.cpp` (GPL-2.0-or-later),
//! rewritten: actions run off a stack, a link fired by a synchronous event
//! (IsRunning, TestEvent…) runs its effect before the actions after the one
//! that caused it, asynchronous events (ContentAvailable, UserInput,
//! TimerFired…) queue and are taken one at a time once the stack is empty.
//! Where freemheg blocks for a file the carousel has not yet delivered (a
//! scene being transitioned to), this engine parks the action and tries
//! again on the next [`Engine::tick`].
//!
//! Cost: nothing runs between ticks; a tick with no timer due, no content
//! awaited and no event queued is a few compares.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use crate::asn1::{self, Node};
use crate::obj::*;
use crate::tags::*;

/// Event types (ISO/IEC 13522-5 Annex A, EventType; 32 and 33 are the UK
/// profile's, ES 202 184 §11.8).
pub mod ev {
    pub const IS_AVAILABLE: i32 = 1;
    pub const CONTENT_AVAILABLE: i32 = 2;
    pub const IS_DELETED: i32 = 3;
    pub const IS_RUNNING: i32 = 4;
    pub const IS_STOPPED: i32 = 5;
    pub const USER_INPUT: i32 = 6;
    pub const ANCHOR_FIRED: i32 = 7;
    pub const TIMER_FIRED: i32 = 8;
    pub const ASYNC_STOPPED: i32 = 9;
    pub const INTERACTION_COMPLETED: i32 = 10;
    pub const TOKEN_MOVED_FROM: i32 = 11;
    pub const TOKEN_MOVED_TO: i32 = 12;
    pub const STREAM_EVENT: i32 = 13;
    pub const STREAM_PLAYING: i32 = 14;
    pub const STREAM_STOPPED: i32 = 15;
    pub const COUNTER_TRIGGER: i32 = 16;
    pub const HIGHLIGHT_ON: i32 = 17;
    pub const HIGHLIGHT_OFF: i32 = 18;
    pub const TEST_EVENT: i32 = 23;
    pub const FIRST_ITEM_PRESENTED: i32 = 24;
    pub const LAST_ITEM_PRESENTED: i32 = 25;
    pub const HEAD_ITEMS: i32 = 26;
    pub const TAIL_ITEMS: i32 = 27;
    pub const ITEM_SELECTED: i32 = 28;
    pub const ITEM_DESELECTED: i32 = 29;
    pub const ENGINE_EVENT: i32 = 31;
    pub const FOCUS_MOVED: i32 = 32;
}

/// UK remote keys, the UserInput event's data (D-Book 7 §13.5.2, Table 13-5).
pub mod key {
    pub const UP: i32 = 1;
    pub const DOWN: i32 = 2;
    pub const LEFT: i32 = 3;
    pub const RIGHT: i32 = 4;
    /// 0 to 9 are 5 to 14.
    pub const DIGIT0: i32 = 5;
    pub const SELECT: i32 = 15;
    pub const CANCEL: i32 = 16;
    pub const RED: i32 = 100;
    pub const GREEN: i32 = 101;
    pub const YELLOW: i32 = 102;
    pub const BLUE: i32 = 103;
    pub const TEXT: i32 = 104;
}

/// What a file request finds.
pub enum Fetch {
    Ready(Vec<u8>),
    /// Not in yet (the carousel's module is still coming).
    Pending,
    Missing,
}

/// Where the engine's files come from (an object carousel).
pub trait Files {
    fn fetch(&self, path: &str) -> Fetch;
    /// POST a form to `url` (ReturnData, over the interaction channel); the
    /// answer once, then the next call posts again. A carousel cannot.
    fn post(&self, _url: &str, _form: &[u8]) -> Fetch {
        Fetch::Missing
    }
}

impl Files for decdvbt_ts::ObjectCarousel {
    fn fetch(&self, path: &str) -> Fetch {
        match self.lookup(path) {
            decdvbt_ts::Lookup::File(b) => Fetch::Ready(b.to_vec()),
            decdvbt_ts::Lookup::Pending => Fetch::Pending,
            _ => Fetch::Missing,
        }
    }
}

/// What the receiver tells the engine about where it is (the resident
/// programs that read the SI, D-Book 7 §13.10.4).
#[derive(Debug, Clone, Default)]
pub struct Service {
    pub original_network_id: u16,
    pub transport_stream_id: u16,
    pub service_id: u16,
    /// The services the receiver can tune, this one among them, as
    /// (original network, transport stream, service): a service's index is
    /// its place here (GetServiceIndex, TuneIndex).
    pub services: Vec<(u16, u16, u16)>,
}

/// One frame of the action stack: a sequence, where it has got to, and the
/// group its internal references are in.
struct Frame {
    group: Rc<str>,
    seq: Rc<[Node]>,
    at: usize,
}

struct Event {
    source: Ref,
    kind: i32,
    data: Option<Value>,
}

/// A request the engine makes of the receiver.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Tune to the service of this index (TuneIndex, `TIn`).
    Tune(i32),
    /// The video component to show, and its rectangle on the 720x576
    /// canvas (None: full screen).
    Video(Option<(i32, i32, i32, i32)>),
    /// Play this stream from the interaction channel in place of the
    /// broadcast (an MPEG transport stream over HTTP; Freeview's internet
    /// channels); None: back to the broadcast.
    IcStream(Option<String>),
}

/// How long after the last application quits the boot application starts.
const REBOOT_DELAY_MS: u64 = 1000;

/// How long a missing file takes to be reported (engine events 2 and 3).
const MISSING_DELAY_MS: u64 = 500;

pub struct Engine {
    pub(crate) objs: HashMap<Ref, Obj>,
    /// The application stack (Spawn pushes); the last is running.
    apps: Vec<Rc<str>>,
    scene: Option<Rc<str>>,
    /// Running links, in the order they were activated.
    links: Vec<Ref>,
    stack: Vec<Frame>,
    events: VecDeque<Event>,
    /// Visibles, bottom first.
    pub(crate) display: Vec<Ref>,
    /// Ingredients waiting for their content: (object, path).
    awaiting: Vec<(Ref, Rc<str>)>,
    /// A file an action is waiting for (the action stays on the stack).
    blocked: Option<Rc<str>>,
    in_transition: bool,
    /// Events to raise later: (at, event).
    delayed: Vec<(u64, Event)>,
    /// When to boot again after the last application quit.
    reboot_at: Option<u64>,
    now_ms: u64,
    pub(crate) persistent: HashMap<Vec<u8>, Vec<Value>>,
    lock: i32,
    pub(crate) dirty: bool,
    pub service: Service,
    pub requests: Vec<Request>,
    /// Keys to give the application once it has a scene.
    early_keys: Vec<i32>,
    /// What the engine did, for `--trace` (capped).
    pub trace: Vec<String>,
    pub tracing: bool,
    /// Print the trace as it happens, too.
    pub trace_stderr: bool,
    /// The engine stopped (the last application quit, or it could not boot).
    pub finished: bool,
    /// The name of the file that failed to boot or the like.
    pub error: Option<String>,
    /// The time from the stream (TDT), Unix seconds; the PC's clock if None.
    pub clock_unix: Option<i64>,
    /// Local time's offset from UTC, seconds (the TOT's, in the UK 0 or 3600).
    pub local_offset_s: i64,
    pub(crate) rng: u64,
    /// The receiver has an interaction channel (the files go through an
    /// [`crate::ic::IcFiles`]): GetICStatus says so and URLs are fetched.
    pub ic: bool,
    /// Groups whose close-down is running.
    closing: Vec<Ref>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            objs: HashMap::new(),
            apps: Vec::new(),
            scene: None,
            links: Vec::new(),
            stack: Vec::new(),
            events: VecDeque::new(),
            display: Vec::new(),
            awaiting: Vec::new(),
            blocked: None,
            in_transition: false,
            delayed: Vec::new(),
            reboot_at: None,
            now_ms: 0,
            persistent: HashMap::new(),
            lock: 0,
            dirty: true,
            service: Service::default(),
            requests: Vec::new(),
            early_keys: Vec::new(),
            trace: Vec::new(),
            tracing: false,
            trace_stderr: false,
            finished: false,
            error: None,
            clock_unix: None,
            local_offset_s: 0,
            rng: 0x853C_49E6_748F_EA9B,
            ic: false,
            closing: Vec::new(),
        }
    }

    fn log(&mut self, s: impl FnOnce() -> String) {
        if self.tracing {
            if self.trace.len() > 20000 {
                self.trace.drain(..10000);
            }
            let s = s();
            if self.trace_stderr {
                eprintln!("{s}");
            }
            self.trace.push(s);
        }
    }

    /// Start the carousel's boot application: "/a", else "/startup"
    /// (D-Book 7 §15.1.2). The engine waits for it if it is not in yet.
    pub fn boot(&mut self, files: &dyn Files) {
        let app: Rc<str> = match files.fetch("/a") {
            Fetch::Missing if !matches!(files.fetch("/startup"), Fetch::Missing) => {
                "/startup".into()
            }
            _ => "/a".into(),
        };
        self.stack.push(Frame {
            group: app.clone(),
            seq: Rc::from(vec![Node::Tagged(
                C_LAUNCH,
                vec![Node::Seq(vec![
                    Node::Str(app.as_bytes().to_vec()),
                    Node::Int(0),
                ])],
            )]),
            at: 0,
        });
        self.run(files);
    }

    /// What applications stored under "pst://" names (the receiver's
    /// non-volatile store: Freeview's internet channels keep the device id
    /// the server gave them there), as bytes for the receiver to keep.
    pub fn save_nonvolatile(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let put = |out: &mut Vec<u8>, b: &[u8]| {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        };
        let mut names: Vec<&Vec<u8>> = self
            .persistent
            .keys()
            .filter(|k| k.starts_with(b"pst://"))
            .collect();
        names.sort();
        for name in names {
            let vals = &self.persistent[name];
            put(&mut out, name);
            out.extend_from_slice(&(vals.len() as u32).to_le_bytes());
            for v in vals {
                match v {
                    Value::Bool(b) => out.extend_from_slice(&[0, *b as u8]),
                    Value::Int(i) => {
                        out.push(1);
                        out.extend_from_slice(&i.to_le_bytes());
                    }
                    Value::Str(s) => {
                        out.push(2);
                        put(&mut out, s);
                    }
                    Value::ContentRef(s) => {
                        out.push(3);
                        put(&mut out, s);
                    }
                    Value::ObjRef(r) => {
                        out.push(4);
                        put(&mut out, r.group.as_bytes());
                        out.extend_from_slice(&r.num.to_le_bytes());
                    }
                }
            }
        }
        out
    }

    /// Restore what [`Engine::save_nonvolatile`] gave (anything malformed
    /// is dropped from there on).
    pub fn load_nonvolatile(&mut self, b: &[u8]) {
        // A small reader over the bytes; `?` ends the load at a short read.
        fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
            let (h, t) = (b.get(..n)?, b.get(n..)?);
            *b = t;
            Some(h)
        }
        fn u32_of(b: &mut &[u8]) -> Option<u32> {
            Some(u32::from_le_bytes(take(b, 4)?.try_into().ok()?))
        }
        fn bytes(b: &mut &[u8]) -> Option<Vec<u8>> {
            let n = u32_of(b)? as usize;
            Some(take(b, n)?.to_vec())
        }
        fn entry(b: &mut &[u8]) -> Option<(Vec<u8>, Vec<Value>)> {
            let name = bytes(b)?;
            let n = u32_of(b)?;
            let mut vals = Vec::new();
            for _ in 0..n.min(1024) {
                vals.push(match take(b, 1)?[0] {
                    0 => Value::Bool(take(b, 1)?[0] != 0),
                    1 => Value::Int(u32_of(b)? as i32),
                    2 => Value::Str(bytes(b)?),
                    3 => Value::ContentRef(bytes(b)?),
                    4 => {
                        let g = String::from_utf8(bytes(b)?).ok()?;
                        let num = u32_of(b)? as i32;
                        Value::ObjRef(Ref {
                            group: g.into(),
                            num,
                        })
                    }
                    _ => return None,
                });
            }
            Some((name, vals))
        }
        let mut b = b;
        while !b.is_empty() {
            let Some((name, vals)) = entry(&mut b) else {
                break;
            };
            if name.starts_with(b"pst://") {
                self.persistent.insert(name, vals);
            }
        }
    }

    /// The application is running and has a scene.
    pub fn has_scene(&self) -> bool {
        self.scene.is_some()
    }

    /// Nothing to do until a key, a timer or the carousel: no action waiting
    /// for a file, no content awaited, no event queued.
    pub fn is_idle(&self) -> bool {
        self.stack.is_empty()
            && self.blocked.is_none()
            && self.awaiting.is_empty()
            && self.events.is_empty()
            && self.delayed.is_empty()
    }

    /// The visibles being drawn, bottom first: (object, class, position,
    /// size), for traces.
    pub fn shown(&self) -> Vec<String> {
        self.display
            .iter()
            .filter_map(|r| {
                let o = self.objs.get(r)?;
                let Kind::Visible(v) = &o.kind else {
                    return None;
                };
                o.running.then(|| {
                    format!(
                        "{r} {} at {:?} size {:?}{}",
                        tag_name(o.class),
                        v.pos,
                        v.size,
                        match &v.draw {
                            Draw::Text { .. } => format!(
                                " {:?}",
                                String::from_utf8_lossy(&o.data[..o.data.len().min(60)])
                            ),
                            Draw::Bitmap { image, .. } =>
                                format!(" image {:?}", image.as_ref().map(|i| (i.w, i.h))),
                            Draw::Rectangle => format!(
                                " fill {:?} line {} {:?}",
                                v.fill_colour, v.line_width, v.line_colour
                            ),
                            _ => String::new(),
                        }
                    )
                })
            })
            .collect()
    }

    pub fn app(&self) -> Option<&str> {
        self.apps.last().map(|a| &**a)
    }

    pub fn scene(&self) -> Option<&str> {
        self.scene.as_deref()
    }

    /// Time passes (`now_ms` counts up from any origin): timers fire, content
    /// that has come in is given to its objects, events are processed.
    /// Returns whether the picture changed.
    pub fn tick(&mut self, now_ms: u64, files: &dyn Files) -> bool {
        self.now_ms = now_ms;
        if self.reboot_at.is_some_and(|t| t <= now_ms) && self.apps.is_empty() {
            self.reboot_at = None;
            self.boot(files);
        }
        let mut fired = Vec::new();
        for g in self.groups() {
            if let Some(Obj {
                kind: Kind::Group(grp),
                ..
            }) = self.objs.get_mut(&g)
            {
                grp.timers.retain(|&(id, at)| {
                    if at <= now_ms {
                        fired.push((g.clone(), id));
                        false
                    } else {
                        true
                    }
                });
            }
        }
        for (g, id) in fired {
            self.async_event(g, ev::TIMER_FIRED, Some(Value::Int(id)));
        }
        if !self.delayed.is_empty() {
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.delayed)
                .into_iter()
                .partition(|d| d.0 <= now_ms);
            self.delayed = later;
            self.events.extend(due.into_iter().map(|d| d.1));
        }
        if !self.awaiting.is_empty() {
            self.deliver_content(files);
        }
        if self.scene.is_some() && self.events.is_empty() && self.stack.is_empty() {
            for k in std::mem::take(&mut self.early_keys) {
                self.key(k);
            }
        }
        self.run(files);
        std::mem::take(&mut self.dirty) && self.lock <= 0
    }

    /// A key on the remote (UK codes, [`key`]).
    pub fn key(&mut self, code: i32) {
        let Some(scene) = self.scene.clone() else {
            if !self.apps.is_empty() {
                self.early_keys.push(code);
            }
            return;
        };
        let source = Ref {
            group: scene.clone(),
            num: 0,
        };
        let register = match self.objs.get(&source) {
            Some(Obj {
                kind: Kind::Group(g),
                ..
            }) => g.input_register,
            _ => 3,
        };
        // Register 5 has no numbers and no Text key (D-Book 7 Table 13-6).
        let allowed = match register {
            5 => !(key::DIGIT0..key::DIGIT0 + 10).contains(&code) && code != key::TEXT,
            _ => true,
        };
        if allowed {
            self.log(|| format!("key {code}"));
            self.async_event(source, ev::USER_INPUT, Some(Value::Int(code)));
        }
    }

    /// A stream event fired (from the carousel's stream event descriptors):
    /// the stream event objects listening on `component_tag` with an event
    /// of this id give their StreamEvent with its name.
    pub fn stream_event(&mut self, name: &[u8], component: Option<Ref>) {
        let streams: Vec<Ref> = match component {
            Some(r) => vec![r],
            None => self
                .objs
                .values()
                .filter(|o| o.class == C_STREAM && o.running)
                .map(|o| o.id.clone())
                .collect(),
        };
        for s in streams {
            self.async_event(s, ev::STREAM_EVENT, Some(Value::Str(name.to_vec())));
        }
    }

    /// Wait for a file before going on (the action is tried again).
    pub(crate) fn block_on(&mut self, path: Rc<str>) {
        self.blocked = Some(path);
    }

    fn groups(&self) -> Vec<Ref> {
        self.apps
            .iter()
            .chain(self.scene.iter())
            .map(|g| Ref {
                group: g.clone(),
                num: 0,
            })
            .collect()
    }

    // ---- events and links ----

    fn async_event(&mut self, source: Ref, kind: i32, data: Option<Value>) {
        self.events.push_back(Event { source, kind, data });
    }

    /// An event: a synchronous one fires its links now (their effects go on
    /// top of the action stack), an asynchronous one is queued.
    pub(crate) fn event(&mut self, source: Ref, kind: i32, data: Option<Value>) {
        use ev::*;
        match kind {
            CONTENT_AVAILABLE
            | USER_INPUT
            | ANCHOR_FIRED
            | TIMER_FIRED
            | ASYNC_STOPPED
            | INTERACTION_COMPLETED
            | STREAM_EVENT
            | STREAM_PLAYING
            | STREAM_STOPPED
            | COUNTER_TRIGGER
            | ENGINE_EVENT
            | FOCUS_MOVED => self.async_event(source, kind, data),
            _ => self.check_links(&source, kind, data.as_ref()),
        }
    }

    fn check_links(&mut self, source: &Ref, kind: i32, data: Option<&Value>) {
        let links = self.links.clone();
        for l in links {
            let Some(o) = self.objs.get(&l) else { continue };
            let Kind::Link(link) = &o.kind else { continue };
            if !o.running || link.event != kind || &link.source != source {
                continue;
            }
            let matches = match (&link.data, data) {
                (None, _) => true,
                (Some(want), Some(got)) => match (want, got) {
                    (Value::Bool(a), b) => *a == b.as_bool(),
                    (Value::Int(a), b) => *a == b.as_int(),
                    (a, b) => a.as_bytes() == b.as_bytes(),
                },
                (Some(_), None) => false,
            };
            if matches {
                let (group, seq) = (l.group.clone(), link.effect.clone());
                self.log(|| format!("link {l} fires on {source} event {kind}"));
                self.stack.push(Frame { group, seq, at: 0 });
            }
        }
    }

    /// Run actions, then queued events, until there is nothing to do (or an
    /// action waits for a file).
    fn run(&mut self, files: &dyn Files) {
        let mut guard = 0u32;
        loop {
            self.run_actions(files);
            if self.blocked.is_some() || self.finished {
                return;
            }
            let Some(e) = self.events.pop_front() else {
                return;
            };
            self.log(|| format!("event {} from {}", e.kind, e.source));
            self.check_links(&e.source, e.kind, e.data.as_ref());
            guard += 1;
            if guard > 100_000 {
                // A runaway application (events that make events forever).
                self.events.clear();
                return;
            }
        }
    }

    fn run_actions(&mut self, files: &dyn Files) {
        let mut guard = 0u32;
        while let Some(top) = self.stack.last_mut() {
            if top.at >= top.seq.len() {
                self.stack.pop();
                continue;
            }
            let (group, seq, at) = (top.group.clone(), top.seq.clone(), top.at);
            top.at += 1;
            let depth = self.stack.len();
            self.blocked = None;
            self.perform(&seq[at], &group, files);
            if self.blocked.is_some() {
                // Try the action again later.
                if let Some(f) = self.stack.get_mut(depth - 1)
                    && Rc::ptr_eq(&f.seq, &seq)
                {
                    f.at = at;
                }
                return;
            }
            guard += 1;
            if guard > 1_000_000 {
                self.stack.clear();
                return;
            }
        }
    }

    // ---- the life cycle ----

    fn set_running(&mut self, r: &Ref, on: bool) {
        if let Some(o) = self.objs.get_mut(r) {
            o.running = on;
        }
    }

    pub(crate) fn preparation(&mut self, r: &Ref, files: &dyn Files) {
        let Some(o) = self.objs.get_mut(r) else {
            return;
        };
        if o.available {
            return;
        }
        o.available = true;
        // Variables start from their original value.
        if let Kind::Variable(v) = &mut o.kind {
            *v = original_value(o.class, o.def.get(C_ORIGINAL_VALUE), &r.group);
        }
        let visible = matches!(o.kind, Kind::Visible(_));
        let content = o.content.clone();
        if let Kind::Group(g) = &o.kind {
            let items = g.items.clone();
            for num in items {
                let ir = Ref {
                    group: r.group.clone(),
                    num,
                };
                let prep = self.objs.get(&ir).is_some_and(|i| {
                    i.initially_active
                        || (matches!(i.kind, Kind::Program { .. })
                            && i.def.get_bool(C_INITIALLY_AVAILABLE).unwrap_or(true))
                });
                if prep {
                    self.preparation(&ir, files);
                }
            }
        }
        if visible && !self.display.contains(r) {
            self.display.push(r.clone());
        }
        self.event(r.clone(), ev::IS_AVAILABLE, None);
        self.content_preparation(r, content, files);
    }

    /// Give an ingredient its content, or ask the carousel for it.
    fn content_preparation(&mut self, r: &Ref, content: Content, files: &dyn Files) {
        match content {
            Content::None => {}
            Content::Included(b) => self.content_arrived(r, b),
            // A stream's "rec://svc/def" or "dvb://…" names the broadcast,
            // not a file; a stream's URL is played, not fetched. Other
            // URLs are files over the interaction channel.
            Content::Referenced(path)
                if path.windows(3).any(|w| w == b"://")
                    && !path.starts_with(b"DSM:")
                    && (!(path.starts_with(b"http://") || path.starts_with(b"https://"))
                        || self.objs.get(r).is_some_and(|o| o.class == C_STREAM)) => {}
            Content::Referenced(path) => {
                let path = canonical(&path, &r.group);
                self.awaiting.retain(|(o, _)| o != r);
                let f = files.fetch(&path);
                self.log(|| {
                    let s = match &f {
                        Fetch::Ready(b) => format!("{} bytes", b.len()),
                        Fetch::Pending => "pending".into(),
                        Fetch::Missing => "missing".into(),
                    };
                    format!("    {r}: content {path}: {s}")
                });
                match f {
                    Fetch::Ready(b) => self.content_arrived(r, b),
                    Fetch::Pending => self.awaiting.push((r.clone(), path)),
                    Fetch::Missing => {
                        self.log(|| format!("{r}: content {path} is not in the carousel"));
                        // ContentRefError (D-Book 7 §13.4.5, engine event 3).
                        self.engine_event(3);
                    }
                }
            }
        }
    }

    fn deliver_content(&mut self, files: &dyn Files) {
        let waiting = std::mem::take(&mut self.awaiting);
        for (r, path) in waiting {
            match files.fetch(&path) {
                Fetch::Ready(b) => self.content_arrived(&r, b),
                Fetch::Pending => self.awaiting.push((r, path)),
                Fetch::Missing => {}
            }
        }
    }

    fn content_arrived(&mut self, r: &Ref, b: Vec<u8>) {
        let Some(o) = self.objs.get_mut(r) else {
            return;
        };
        if !o.available {
            return;
        }
        if let Kind::Visible(v) = &mut o.kind
            && let Draw::Bitmap { image, .. } = &mut v.draw
        {
            *image = crate::render::decode_bitmap(&b).map(Rc::new);
        }
        o.data = b;
        if o.running {
            self.dirty = true;
        }
        self.event(r.clone(), ev::CONTENT_AVAILABLE, None);
    }

    pub(crate) fn activation(&mut self, r: &Ref, files: &dyn Files) {
        let Some(o) = self.objs.get(r) else {
            self.log(|| format!("activate: no object {r}"));
            return;
        };
        if o.running {
            return;
        }
        if !o.available {
            self.preparation(r, files);
        }
        let Some(o) = self.objs.get(r) else { return };
        match &o.kind {
            Kind::Group(g) => {
                let (start, items) = (g.on_start_up.clone(), g.items.clone());
                self.stack.push(Frame {
                    group: r.group.clone(),
                    seq: start,
                    at: 0,
                });
                self.run_actions(files);
                for num in items {
                    let ir = Ref {
                        group: r.group.clone(),
                        num,
                    };
                    if self.objs.get(&ir).is_some_and(|i| i.initially_active) {
                        self.activation(&ir, files);
                    }
                }
                if let Some(Obj {
                    kind: Kind::Group(g),
                    ..
                }) = self.objs.get_mut(r)
                {
                    g.start_ms = self.now_ms;
                }
                self.set_running(r, true);
                self.dirty = true;
                self.event(r.clone(), ev::IS_RUNNING, None);
                return;
            }
            Kind::Link(_) => {
                if !self.links.contains(r) {
                    self.links.push(r.clone());
                }
            }
            Kind::Stream { components } => {
                let comps = components.clone();
                if let Some(url) = ic_stream_url(&o.content) {
                    self.requests.push(Request::IcStream(Some(url)));
                }
                self.set_running(r, true);
                for num in comps {
                    let cr = Ref {
                        group: r.group.clone(),
                        num,
                    };
                    if self.objs.get(&cr).is_some_and(|i| i.initially_active) {
                        self.activation(&cr, files);
                    }
                }
                self.event(r.clone(), ev::STREAM_PLAYING, None);
            }
            Kind::TokenGroup(tg) => {
                let (items, token) = (
                    tg.items.iter().map(|i| i.0.clone()).collect::<Vec<_>>(),
                    tg.token,
                );
                for i in items {
                    self.activation(&i, files);
                }
                self.event(r.clone(), ev::TOKEN_MOVED_TO, Some(Value::Int(token)));
                // A ListGroup then shows the items in its cells only.
                self.list_activated(r, files);
            }
            Kind::Visible(v) => {
                if matches!(v.draw, Draw::Video { .. }) {
                    self.video_request(r);
                }
                self.dirty = true;
            }
            _ => {}
        }
        self.set_running(r, true);
        self.event(r.clone(), ev::IS_RUNNING, None);
    }

    pub(crate) fn deactivation(&mut self, r: &Ref, files: &dyn Files) {
        let Some(o) = self.objs.get(r) else { return };
        if !o.running || self.closing.contains(r) {
            return;
        }
        match &o.kind {
            Kind::Group(g) => {
                let close = g.on_close_down.clone();
                self.stack.push(Frame {
                    group: r.group.clone(),
                    seq: close,
                    at: 0,
                });
                // A close-down that stops its own group (NHK WORLD's scene
                // does, on an error) must not start the close-down again.
                self.closing.push(r.clone());
                self.run_actions(files);
                self.closing.retain(|c| c != r);
                if let Some(Obj {
                    kind: Kind::Group(g),
                    ..
                }) = self.objs.get_mut(r)
                {
                    g.timers.clear();
                }
            }
            Kind::Link(_) => self.links.retain(|l| l != r),
            Kind::Stream { components } => {
                if ic_stream_url(&o.content).is_some() {
                    self.requests.push(Request::IcStream(None));
                }
                for num in components.clone() {
                    let cr = Ref {
                        group: r.group.clone(),
                        num,
                    };
                    self.deactivation(&cr, files);
                }
                self.event(r.clone(), ev::STREAM_STOPPED, None);
            }
            Kind::TokenGroup(tg) => {
                let token = tg.token;
                self.event(r.clone(), ev::TOKEN_MOVED_FROM, Some(Value::Int(token)));
                self.list_deactivated(r, files);
            }
            Kind::Visible(v) => {
                if matches!(v.draw, Draw::Video { .. }) {
                    self.requests.push(Request::Video(None));
                }
                self.dirty = true;
            }
            _ => {}
        }
        self.set_running(r, false);
        self.event(r.clone(), ev::IS_STOPPED, None);
    }

    pub(crate) fn destruction(&mut self, r: &Ref, files: &dyn Files) {
        let Some(o) = self.objs.get(r) else { return };
        if !o.available {
            return;
        }
        if o.running {
            self.deactivation(r, files);
        }
        let Some(o) = self.objs.get(r) else { return };
        if let Kind::Group(g) = &o.kind {
            for num in g.items.clone().into_iter().rev() {
                let ir = Ref {
                    group: r.group.clone(),
                    num,
                };
                self.destruction(&ir, files);
            }
        }
        self.display.retain(|d| d != r);
        self.awaiting.retain(|(o, _)| o != r);
        if let Some(o) = self.objs.get_mut(r) {
            o.available = false;
            o.data.clear();
            if let Kind::Visible(v) = &mut o.kind
                && let Draw::Bitmap { image, .. } = &mut v.draw
            {
                *image = None;
            }
        }
        self.dirty = true;
        self.event(r.clone(), ev::IS_DELETED, None);
    }

    fn video_request(&mut self, r: &Ref) {
        if let Some(Obj {
            kind: Kind::Visible(v),
            ..
        }) = self.objs.get(r)
        {
            let full = v.pos == (0, 0) && v.size == (720, 576);
            self.requests.push(Request::Video(
                (!full).then_some((v.pos.0, v.pos.1, v.size.0, v.size.1)),
            ));
        }
    }

    // ---- groups ----

    /// Load a group file: `Err(true)` while it is still to come.
    fn load(&mut self, path: &Rc<str>, files: &dyn Files) -> Result<Vec<Obj>, bool> {
        match files.fetch(path) {
            Fetch::Ready(b) => {
                let root = asn1::parse(&b).map_err(|e| {
                    self.error = Some(format!("{path}: {e}"));
                    false
                })?;
                // The group is named by its path, whatever its header says,
                // so that references to it match.
                load_group(&root, path).ok_or(false)
            }
            Fetch::Pending => Err(true),
            Fetch::Missing => Err(false),
        }
    }

    fn insert(&mut self, objs: Vec<Obj>) {
        for o in objs {
            self.objs.insert(o.id.clone(), o);
        }
    }

    /// Remove a group's objects (after its destruction).
    fn remove_group(&mut self, g: &Rc<str>) {
        self.objs.retain(|r, _| &r.group != g);
        self.links.retain(|r| &r.group != g);
        self.display.retain(|r| &r.group != g);
        self.awaiting.retain(|(r, _)| &r.group != g);
    }

    /// An engine event (a file not found and the like), raised after a
    /// delay: a receiver takes a while to know a file is not in the
    /// carousel, and applications retry at once on the error.
    fn engine_event(&mut self, n: i32) {
        if let Some(app) = self.apps.last().cloned() {
            let e = Event {
                source: Ref { group: app, num: 0 },
                kind: ev::ENGINE_EVENT,
                data: Some(Value::Int(n)),
            };
            self.delayed.push((self.now_ms + MISSING_DELAY_MS, e));
        }
    }

    fn transition_to(&mut self, target: &Ref, files: &dyn Files) {
        if self.in_transition {
            return;
        }
        let objs = match self.load(&target.group, files) {
            Ok(o) => o,
            Err(true) => {
                self.blocked = Some(target.group.clone());
                return;
            }
            Err(false) => {
                self.log(|| format!("transition: {} is not there", target.group));
                self.engine_event(2); // GroupIDRefError (D-Book 7 §13.4.5)
                return;
            }
        };
        if objs.first().is_some_and(|o| o.class != C_SCENE) {
            return;
        }
        self.log(|| format!("transition to {}", target.group));
        self.stack.clear();
        self.in_transition = true;
        if let Some(old) = self.scene.take() {
            let r = Ref {
                group: old.clone(),
                num: 0,
            };
            self.scene = Some(old.clone());
            self.deactivation(&r, files);
            self.destruction(&r, files);
            self.scene = None;
            // Events from the old scene go with it.
            let shared = |e: &Event, objs: &HashMap<Ref, Obj>| {
                e.source.group != old || objs.get(&e.source).is_some_and(|o| o.shared)
            };
            let objs_ref = &self.objs;
            self.events.retain(|e| shared(e, objs_ref));
            self.remove_group(&old);
        }
        self.requests.push(Request::Video(None));
        let g = target.group.clone();
        self.insert(objs);
        self.scene = Some(g.clone());
        let r = Ref { group: g, num: 0 };
        self.preparation(&r, files);
        self.activation(&r, files);
        self.in_transition = false;
        self.dirty = true;
    }

    fn launch(&mut self, target: &Ref, spawn: bool, files: &dyn Files) {
        if self.in_transition {
            return;
        }
        let objs = match self.load(&target.group, files) {
            Ok(o) => o,
            Err(true) => {
                self.blocked = Some(target.group.clone());
                return;
            }
            Err(false) => {
                if self.apps.is_empty() {
                    self.error = Some(format!("cannot boot {}", target.group));
                    self.finished = true;
                } else {
                    self.engine_event(2);
                }
                return;
            }
        };
        if objs.first().is_some_and(|o| o.class != C_APPLICATION) {
            return;
        }
        self.log(|| format!("launch {}", target.group));
        self.stack.clear();
        if !spawn {
            self.quit_app(files);
        } else if let Some(sc) = self.scene.take() {
            // The spawning application keeps its state but loses its scene.
            let r = Ref {
                group: sc.clone(),
                num: 0,
            };
            self.scene = Some(sc.clone());
            self.deactivation(&r, files);
            self.destruction(&r, files);
            self.scene = None;
            self.remove_group(&sc);
        }
        let g = target.group.clone();
        self.insert(objs);
        self.apps.push(g.clone());
        set_ic_root(&g);
        let r = Ref { group: g, num: 0 };
        self.preparation(&r, files);
        self.activation(&r, files);
    }

    /// Stop the running application (and its scene).
    fn quit_app(&mut self, files: &dyn Files) {
        if let Some(sc) = self.scene.take() {
            let r = Ref {
                group: sc.clone(),
                num: 0,
            };
            self.scene = Some(sc.clone());
            self.deactivation(&r, files);
            self.destruction(&r, files);
            self.scene = None;
            self.remove_group(&sc);
        }
        if let Some(app) = self.apps.last().cloned() {
            let r = Ref {
                group: app.clone(),
                num: 0,
            };
            self.deactivation(&r, files);
            self.destruction(&r, files);
            self.remove_group(&app);
            self.apps.pop();
            set_ic_root(self.apps.last().map_or("", |a| a));
        }
        self.events.clear();
        self.delayed.clear();
        self.dirty = true;
    }

    // ---- values ----

    pub(crate) fn var(&self, r: &Ref) -> Option<&Value> {
        match &self.objs.get(r)?.kind {
            Kind::Variable(v) => Some(v),
            _ => None,
        }
    }

    pub(crate) fn set_var(&mut self, r: &Ref, v: Value) {
        if let Some(Obj {
            kind: Kind::Variable(old),
            ..
        }) = self.objs.get_mut(r)
        {
            // A variable keeps its type: an integer set from a boolean and
            // the like is converted.
            *old = match (&*old, v) {
                (Value::Int(_), v) => Value::Int(v.as_int()),
                (Value::Bool(_), v) => Value::Bool(v.as_bool()),
                (Value::Str(_), Value::Int(i)) => Value::Str(i.to_string().into_bytes()),
                (Value::Str(_), Value::ContentRef(s)) => Value::Str(s),
                (Value::ContentRef(_), Value::Str(s)) => Value::ContentRef(s),
                (_, v) => v,
            };
        } else {
            self.log(|| format!("set_var: {r} is not a variable"));
        }
    }

    /// GenericObjectReference: a reference, or an indirect one (an object
    /// reference variable's value).
    pub(crate) fn gref(&self, n: &Node, g: &Rc<str>) -> Option<Ref> {
        match n {
            Node::Tagged(C_INDIRECTREFERENCE, a) => match self.var(&object_ref(a.first()?, g)?)? {
                Value::ObjRef(r) => Some(r.clone()),
                _ => None,
            },
            Node::Tagged(C_NEW_GENERIC_OBJECT_REF, a) => self.gref(a.first()?, g),
            n => object_ref(n, g),
        }
    }

    /// The variable an indirect reference names (an output parameter).
    pub(crate) fn out_ref(&self, n: &Node, g: &Rc<str>) -> Option<Ref> {
        match n {
            Node::Tagged(C_INDIRECTREFERENCE, a) => object_ref(a.first()?, g),
            Node::Tagged(t, a)
                if (C_NEW_GENERIC_BOOLEAN..=C_NEW_GENERIC_CONTENT_REF).contains(t) =>
            {
                self.out_ref(a.first()?, g)
            }
            n => object_ref(n, g),
        }
    }

    /// A generic value of any type.
    pub(crate) fn gvalue(&self, n: &Node, g: &Rc<str>) -> Option<Value> {
        match n {
            Node::Bool(b) => Some(Value::Bool(*b)),
            Node::Int(i) | Node::Enum(i) => Some(Value::Int(*i)),
            Node::Str(s) => Some(Value::Str(s.clone())),
            Node::Tagged(C_INDIRECTREFERENCE, a) => self.var(&object_ref(a.first()?, g)?).cloned(),
            Node::Tagged(C_NEW_GENERIC_OBJECT_REF, a) => {
                Some(Value::ObjRef(self.gref(a.first()?, g)?))
            }
            Node::Tagged(C_NEW_GENERIC_CONTENT_REF, a) | Node::Tagged(C_CONTENT_REFERENCE, a) => {
                match self.gvalue(a.first()?, g)? {
                    Value::Str(s) | Value::ContentRef(s) => Some(Value::ContentRef(s)),
                    v => Some(v),
                }
            }
            Node::Tagged(C_NEW_GENERIC_BOOLEAN, a) => {
                Some(Value::Bool(self.gvalue(a.first()?, g)?.as_bool()))
            }
            Node::Tagged(C_NEW_GENERIC_INTEGER, a) => {
                Some(Value::Int(self.gvalue(a.first()?, g)?.as_int()))
            }
            Node::Tagged(C_NEW_GENERIC_OCTETSTRING, a) => match self.gvalue(a.first()?, g)? {
                Value::ContentRef(s) => Some(Value::Str(s)),
                v => Some(v),
            },
            Node::Seq(a) if a.len() == 2 => object_ref(n, g)
                .map(Value::ObjRef)
                .or_else(|| self.gvalue(&a[0], g)),
            _ => None,
        }
    }

    pub(crate) fn gint(&self, n: Option<&Node>, g: &Rc<str>) -> i32 {
        n.and_then(|n| self.gvalue(n, g)).map_or(0, |v| v.as_int())
    }

    pub(crate) fn gbool(&self, n: Option<&Node>, g: &Rc<str>) -> bool {
        n.and_then(|n| self.gvalue(n, g))
            .is_some_and(|v| v.as_bool())
    }

    pub(crate) fn gstr(&self, n: Option<&Node>, g: &Rc<str>) -> Vec<u8> {
        match n.and_then(|n| self.gvalue(n, g)) {
            Some(Value::Str(s)) | Some(Value::ContentRef(s)) => s,
            Some(Value::Int(i)) => i.to_string().into_bytes(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn visible_mut(&mut self, r: &Ref) -> Option<&mut Visible> {
        match &mut self.objs.get_mut(r)?.kind {
            Kind::Visible(v) => Some(v),
            _ => None,
        }
    }

    fn colour(&self, n: Option<&Node>, g: &Rc<str>) -> Colour {
        match n {
            Some(Node::Tagged(C_NEW_ABSOLUTE_COLOUR, a)) => {
                Colour::from_bytes(&self.gstr(a.first(), g))
            }
            Some(Node::Tagged(C_NEW_COLOUR_INDEX, a)) => {
                let v = self.gint(a.first(), g).clamp(0, 255) as u8;
                Colour {
                    r: v,
                    g: v,
                    b: v,
                    t: 0,
                }
            }
            Some(n) => Colour::from_node(n).unwrap_or(Colour::CLEAR),
            None => Colour::CLEAR,
        }
    }

    // ---- the actions ----

    fn perform(&mut self, action: &Node, g: &Rc<str>, files: &dyn Files) {
        let Node::Tagged(tag, a) = action else { return };
        let tag = *tag;
        let target = a.first().and_then(|t| self.gref(t, g));
        let arg = |i: usize| a.get(i);
        if self.tracing {
            let t = target.clone();
            self.log(|| {
                format!(
                    "  {} {}",
                    tag_name(tag),
                    t.map_or("?".into(), |t| t.to_string())
                )
            });
        }
        let Some(t) = target else { return };
        // Run, Stop, Preload, Unload and SetData are the Ingredient class's
        // actions (ISO/IEC 13522-5): an application or scene (object 0)
        // ignores them. NHK WORLD's app frees an empty slot of its image
        // table with them, through a reference whose number is 0.
        if t.num == 0 && matches!(tag, C_RUN | C_STOP | C_PRELOAD | C_UNLOAD | C_SET_DATA) {
            self.log(|| "    (not for a group: ignored)".into());
            return;
        }
        if matches!(
            tag,
            C_ADD_ITEM
                | C_DEL_ITEM
                | C_GET_LIST_SIZE
                | C_GET_FIRST_ITEM
                | C_GET_LIST_ITEM
                | C_GET_ITEM_STATUS
                | C_GET_CELL_ITEM
                | C_SELECT_ITEM
                | C_DESELECT_ITEM
                | C_TOGGLE_ITEM
                | C_SCROLL_ITEMS
                | C_SET_FIRST_ITEM
        ) && self.list_action(tag, &t, a, g, files)
        {
            return;
        }
        match tag {
            C_ACTIVATE | C_RUN => self.activation(&t, files),
            C_DEACTIVATE | C_STOP => self.deactivation(&t, files),
            C_PRELOAD => self.preparation(&t, files),
            C_UNLOAD => self.destruction(&t, files),
            C_ADD | C_SUBTRACT | C_MULTIPLY | C_DIVIDE | C_MODULO => {
                let x = self.gint(arg(1), g);
                let v = self.var(&t).map_or(0, |v| v.as_int());
                let r = match tag {
                    C_ADD => v.wrapping_add(x),
                    C_SUBTRACT => v.wrapping_sub(x),
                    C_MULTIPLY => v.wrapping_mul(x),
                    C_DIVIDE if x != 0 => v.wrapping_div(x),
                    C_MODULO if x != 0 => v.rem_euclid(x),
                    _ => v,
                };
                self.set_var(&t, Value::Int(r));
            }
            C_APPEND => {
                let mut s = self.var(&t).map_or(Vec::new(), |v| v.as_bytes().to_vec());
                s.extend(self.gstr(arg(1), g));
                self.set_var(&t, Value::Str(s));
            }
            C_SET_VARIABLE => {
                if let Some(v) = arg(1).and_then(|n| self.gvalue(n, g)) {
                    self.log(|| format!("    = {}", show_value(&v)));
                    self.set_var(&t, v);
                }
            }
            C_TEST_VARIABLE => {
                let op = arg(1).and_then(|n| n.int()).unwrap_or(1);
                let Some(v) = self.var(&t).cloned() else {
                    return;
                };
                let Some(w) = arg(2).and_then(|n| self.gvalue(n, g)) else {
                    return;
                };
                let ord = match (&v, &w) {
                    (Value::Int(_), _) | (Value::Bool(_), _) => v.as_int().cmp(&w.as_int()),
                    (Value::ObjRef(a), Value::ObjRef(b)) => {
                        if a == b {
                            std::cmp::Ordering::Equal
                        } else {
                            std::cmp::Ordering::Less
                        }
                    }
                    _ => v.as_bytes().cmp(w.as_bytes()),
                };
                use std::cmp::Ordering::*;
                // Operators: ISO/IEC 13522-5 §31.4.1 (1 equal … 6 greater or equal).
                let r = match op {
                    1 => ord == Equal,
                    2 => ord != Equal,
                    3 => ord == Less,
                    4 => ord != Greater,
                    5 => ord == Greater,
                    6 => ord != Less,
                    _ => false,
                };
                self.event(t, ev::TEST_EVENT, Some(Value::Bool(r)));
            }
            C_CALL | C_FORK => {
                let ok_var = arg(1).and_then(|n| object_ref(n, g));
                let params = arg(2).map_or(&[][..], |p| p.args());
                let ok = self.call_program(&t, params, g, files);
                if self.tracing {
                    let shown: Vec<String> = params
                        .iter()
                        .map(|p| self.gvalue(p, g).map_or("?".into(), |v| show_value(&v)))
                        .collect();
                    self.log(|| format!("    -> {ok} ({})", shown.join(", ")));
                }
                if let Some(v) = ok_var {
                    self.set_var(&v, Value::Bool(ok));
                }
                if tag == C_FORK {
                    self.event(t, ev::ASYNC_STOPPED, None);
                }
            }
            C_TRANSITION_TO => self.transition_to(&t, files),
            C_LAUNCH => self.launch(&t, false, files),
            C_SPAWN => self.launch(&t, true, files),
            C_QUIT => {
                let spawned = self.apps.len() > 1;
                self.quit_app(files);
                if spawned {
                    // The spawner restarts (its OnRestart).
                    if let Some(app) = self.apps.last().cloned() {
                        let r = Ref { group: app, num: 0 };
                        if let Some(Obj { def, .. }) = self.objs.get(&r) {
                            let seq = actions(def.get(C_ON_RESTART));
                            self.stack.push(Frame {
                                group: r.group.clone(),
                                seq,
                                at: 0,
                            });
                        }
                    }
                } else {
                    // The receiver starts the boot application again (D-Book
                    // 7 §15.1: an application that quits hands back to the
                    // auto-boot one).
                    self.reboot_at = Some(self.now_ms + REBOOT_DELAY_MS);
                }
            }
            C_SET_TIMER => {
                let id = self.gint(arg(1), g);
                let new = arg(2).map(|n| {
                    (
                        self.gint(n.arg(0), g),
                        n.arg(1).is_some_and(|b| self.gbool(Some(b), g)),
                    )
                });
                let now = self.now_ms;
                if let Some(Obj {
                    kind: Kind::Group(grp),
                    ..
                }) = self.objs.get_mut(&t)
                {
                    grp.timers.retain(|x| x.0 != id);
                    if let Some((ms, absolute)) = new {
                        let at = if absolute {
                            grp.start_ms + ms.max(0) as u64
                        } else {
                            now + ms.max(0) as u64
                        };
                        if !(absolute && at < now) {
                            grp.timers.push((id, at));
                        }
                    }
                }
            }
            C_SEND_EVENT => {
                let src = arg(1).and_then(|n| self.gref(n, g)).unwrap_or(t);
                let kind = arg(2).and_then(|n| n.int()).unwrap_or(0);
                let data = arg(3).and_then(|n| self.gvalue(n, g));
                self.event(src, kind, data);
            }
            C_SET_DATA => {
                let content = match arg(1) {
                    Some(Node::Seq(s)) => Content::Referenced(self.gstr(s.first(), g)),
                    Some(n) => Content::Included(self.gstr(Some(n), g)),
                    None => return,
                };
                self.log(|| format!("    content {content:?}"));
                // The old content's ContentAvailable, if still queued, is
                // for content that is gone: an application that reads the
                // data on that event would read the old file as the new.
                self.events
                    .retain(|e| !(e.source == t && e.kind == ev::CONTENT_AVAILABLE));
                if let Some(o) = self.objs.get_mut(&t) {
                    // A running stream given a new URL plays it.
                    if o.class == C_STREAM
                        && o.running
                        && let Some(url) = ic_stream_url(&content)
                    {
                        self.requests.push(Request::IcStream(Some(url)));
                    }
                    o.content = content.clone();
                    if o.available {
                        self.content_preparation(&t, content, files);
                    }
                }
            }
            C_GET_TEXT_DATA => {
                if let Some(v) = arg(1).and_then(|n| object_ref(n, g)) {
                    let d = self.objs.get(&t).map_or(Vec::new(), |o| o.data.clone());
                    self.log(|| format!("    = {}", show_value(&Value::Str(d.clone()))));
                    self.set_var(&v, Value::Str(d));
                }
            }
            C_GET_AVAILABILITY_STATUS | C_GET_RUNNING_STATUS => {
                if let Some(v) = arg(1).and_then(|n| object_ref(n, g)) {
                    let o = self.objs.get(&t);
                    let b = if tag == C_GET_RUNNING_STATUS {
                        o.is_some_and(|o| o.running)
                    } else {
                        o.is_some_and(|o| o.available)
                    };
                    self.set_var(&v, Value::Bool(b));
                }
            }
            C_SET_POSITION
            | C_SET_BOX_SIZE
            | C_SCALE_BITMAP
            | C_SCALE_VIDEO
            | C_SET_BITMAP_DECODE_OFFSET
            | C_SET_VIDEO_DECODE_OFFSET => {
                let (x, y) = (self.gint(arg(1), g), self.gint(arg(2), g));
                if let Some(v) = self.visible_mut(&t) {
                    match tag {
                        C_SET_POSITION => v.pos = (x, y),
                        C_SET_BOX_SIZE => v.size = (x, y),
                        _ => match &mut v.draw {
                            Draw::Bitmap { scale, .. } if tag == C_SCALE_BITMAP => {
                                *scale = Some((x, y))
                            }
                            Draw::Bitmap { offset, .. } if tag == C_SET_BITMAP_DECODE_OFFSET => {
                                *offset = (x, y)
                            }
                            Draw::Video { scale, .. } if tag == C_SCALE_VIDEO => {
                                *scale = Some((x, y))
                            }
                            _ => {}
                        },
                    }
                    self.dirty = true;
                }
                if matches!(tag, C_SET_POSITION | C_SET_BOX_SIZE | C_SCALE_VIDEO)
                    && self
                        .objs
                        .get(&t)
                        .is_some_and(|o| o.class == C_VIDEO && o.running)
                {
                    self.video_request(&t);
                }
            }
            C_GET_POSITION | C_GET_BOX_SIZE | C_GET_BITMAP_DECODE_OFFSET => {
                let pair = match self.objs.get(&t).map(|o| &o.kind) {
                    Some(Kind::Visible(v)) if tag == C_GET_POSITION => v.pos,
                    Some(Kind::Visible(v)) if tag == C_GET_BOX_SIZE => v.size,
                    Some(Kind::Visible(v)) => match &v.draw {
                        Draw::Bitmap { offset, .. } => *offset,
                        _ => return,
                    },
                    _ => return,
                };
                if let Some(x) = arg(1).and_then(|n| object_ref(n, g)) {
                    self.set_var(&x, Value::Int(pair.0));
                }
                if let Some(y) = arg(2).and_then(|n| object_ref(n, g)) {
                    self.set_var(&y, Value::Int(pair.1));
                }
            }
            C_BRING_TO_FRONT | C_SEND_TO_BACK | C_PUT_BEFORE | C_PUT_BEHIND => {
                let Some(i) = self.display.iter().position(|d| d == &t) else {
                    return;
                };
                let other = arg(1).and_then(|n| self.gref(n, g));
                let item = self.display.remove(i);
                let at = match tag {
                    C_BRING_TO_FRONT => self.display.len(),
                    C_SEND_TO_BACK => 0,
                    _ => match other.and_then(|o| self.display.iter().position(|d| d == &o)) {
                        Some(j) if tag == C_PUT_BEFORE => j + 1,
                        Some(j) => j,
                        None => i.min(self.display.len()),
                    },
                };
                self.display.insert(at, item);
                self.dirty = true;
            }
            C_SET_FILL_COLOUR | C_SET_LINE_COLOUR | C_SET_TEXT_COLOUR | C_SET_BACKGROUND_COLOUR => {
                let c = self.colour(arg(1), g);
                if let Some(v) = self.visible_mut(&t) {
                    match tag {
                        C_SET_FILL_COLOUR => v.fill_colour = c,
                        C_SET_LINE_COLOUR => v.line_colour = c,
                        C_SET_TEXT_COLOUR => v.text_colour = Some(c),
                        _ => v.background = Some(c),
                    }
                    self.dirty = true;
                }
            }
            C_SET_LINE_WIDTH => {
                let w = self.gint(arg(1), g);
                if let Some(v) = self.visible_mut(&t) {
                    v.line_width = w;
                    self.dirty = true;
                }
            }
            C_SET_TRANSPARENCY => {
                let w = self.gint(arg(1), g);
                if let Some(v) = self.visible_mut(&t) {
                    v.transparency = w;
                    self.dirty = true;
                }
            }
            C_SET_FONT_ATTRIBUTES => {
                let s = self.gstr(arg(1), g);
                if let Some(v) = self.visible_mut(&t)
                    && let Draw::Text {
                        font_attributes, ..
                    } = &mut v.draw
                {
                    *font_attributes = s;
                    self.dirty = true;
                }
            }
            C_SET_HIGHLIGHT_STATUS => {
                let on = self.gbool(arg(1), g);
                if let Some(v) = self.visible_mut(&t) {
                    v.highlight = on;
                }
                self.event(
                    t,
                    if on {
                        ev::HIGHLIGHT_ON
                    } else {
                        ev::HIGHLIGHT_OFF
                    },
                    None,
                );
            }
            C_MOVE | C_MOVE_TO => {
                let n = self.gint(arg(1), g);
                let Some(Obj {
                    kind: Kind::TokenGroup(tg),
                    running,
                    ..
                }) = self.objs.get_mut(&t)
                else {
                    return;
                };
                let old = tg.token;
                let new = if tag == C_MOVE_TO {
                    n
                } else {
                    // The movement table's row n, at the token's column.
                    tg.movement
                        .get((n - 1).max(0) as usize)
                        .and_then(|row| row.get((old - 1).max(0) as usize))
                        .copied()
                        .unwrap_or(old)
                };
                let running = *running;
                if new != old {
                    tg.token = new;
                    if running {
                        self.event(t.clone(), ev::TOKEN_MOVED_FROM, Some(Value::Int(old)));
                        self.event(t, ev::TOKEN_MOVED_TO, Some(Value::Int(new)));
                    }
                }
            }
            C_GET_TOKEN_POSITION => {
                let pos = match self.objs.get(&t).map(|o| &o.kind) {
                    Some(Kind::TokenGroup(tg)) => tg.token,
                    _ => return,
                };
                if let Some(v) = arg(1).and_then(|n| object_ref(n, g)) {
                    self.set_var(&v, Value::Int(pos));
                }
            }
            C_CALL_ACTION_SLOT => {
                let slot = self.gint(arg(1), g);
                let seq = match self.objs.get(&t).map(|o| &o.kind) {
                    Some(Kind::TokenGroup(tg)) => {
                        let slots = if tg.token == 0 {
                            Some(&tg.no_token_slots)
                        } else {
                            tg.items.get((tg.token - 1) as usize).map(|i| &i.1)
                        };
                        slots
                            .and_then(|s| s.get((slot - 1).max(0) as usize))
                            .cloned()
                            .flatten()
                    }
                    _ => None,
                };
                if let Some(seq) = seq {
                    self.stack.push(Frame {
                        group: t.group.clone(),
                        seq,
                        at: 0,
                    });
                }
            }
            C_GET_ENGINE_SUPPORT => {
                let f = self.gstr(arg(1), g);
                let ok = engine_support(&String::from_utf8_lossy(&f), self.ic);
                if let Some(v) = arg(2).and_then(|n| object_ref(n, g)) {
                    self.set_var(&v, Value::Bool(ok));
                }
            }
            C_STORE_PERSISTENT | C_READ_PERSISTENT => {
                let ok_var = arg(1).and_then(|n| object_ref(n, g));
                let vars: Vec<Ref> = arg(2)
                    .map_or(&[][..], |n| n.args())
                    .iter()
                    .filter_map(|n| object_ref(n, g))
                    .collect();
                let name = self.gstr(arg(3), g);
                let ok = if tag == C_STORE_PERSISTENT {
                    let vals = vars.iter().filter_map(|v| self.var(v).cloned()).collect();
                    self.persistent.insert(name, vals);
                    true
                } else if let Some(vals) = self.persistent.get(&name).cloned() {
                    for (r, v) in vars.iter().zip(vals) {
                        self.set_var(r, v);
                    }
                    true
                } else {
                    false
                };
                if let Some(v) = ok_var {
                    self.set_var(&v, Value::Bool(ok));
                }
            }
            C_LOCK_SCREEN => self.lock += 1,
            C_UNLOCK_SCREEN => {
                self.lock = (self.lock - 1).max(0);
                self.dirty = true;
            }
            C_SET_INPUT_REGISTER => {
                let reg = self.gint(arg(1), g);
                if let Some(Obj {
                    kind: Kind::Group(grp),
                    ..
                }) = self.objs.get_mut(&t)
                {
                    grp.input_register = reg;
                }
            }
            C_CLONE => self.clone_object(&t, arg(1).and_then(|n| object_ref(n, g))),
            _ => self.log(|| format!("  (not done: {})", tag_name(tag))),
        }
    }

    /// Clone: a copy of an ingredient with the group's next free number,
    /// its reference put in the variable given.
    fn clone_object(&mut self, t: &Ref, var: Option<Ref>) {
        let Some(o) = self.objs.get(t).cloned() else {
            return;
        };
        let num = self
            .objs
            .keys()
            .filter(|r| r.group == t.group)
            .map(|r| r.num)
            .max()
            .unwrap_or(0)
            + 1;
        let id = Ref {
            group: t.group.clone(),
            num,
        };
        let mut c = o;
        c.id = id.clone();
        c.available = false;
        c.running = false;
        self.objs.insert(id.clone(), c);
        if let Some(v) = var {
            self.set_var(&v, Value::ObjRef(id));
        }
    }
}

/// A stream's content when it is a URL over the interaction channel (not
/// "rec://svc/def" or "dvb://…", the broadcast).
fn ic_stream_url(c: &Content) -> Option<String> {
    let Content::Referenced(p) = c else {
        return None;
    };
    let s = String::from_utf8_lossy(p);
    is_url(&s).then(|| s.into_owned())
}

/// The current source's root follows the running application: its URL's
/// folder if it came over the interaction channel, else none (the carousel).
fn set_ic_root(app: &str) {
    let root = is_url(app).then(|| url_dir(app).to_string());
    IC_ROOT.with(|r| *r.borrow_mut() = root);
}

/// A value, short, for traces.
fn show_value(v: &Value) -> String {
    match v {
        Value::Str(s) | Value::ContentRef(s) => {
            let t = String::from_utf8_lossy(&s[..s.len().min(80)]).into_owned();
            format!("{t:?}")
        }
        v => format!("{v:?}"),
    }
}

/// GetEngineSupport (ISO/IEC 13522-5 §46.30; the UK strings, D-Book 7
/// §13.12.1): what this engine says it has.
fn engine_support(f: &str, ic: bool) -> bool {
    let (name, arg) = match f.split_once('(') {
        Some((n, a)) => (n, a.trim_end_matches(')')),
        None => (f, ""),
    };
    match name {
        "ApplicationStacking" | "ASt" | "Cloning" | "Clo" => true,
        "SceneAspectRatio" | "SAR" => matches!(arg, "4,3" | "16,9"),
        "SceneCoordinateSystem" | "SCS" => arg == "720,576",
        "MultipleAudioStreams" | "MAS" | "MultipleVideoStreams" | "MVS" => {
            matches!(arg, "0" | "1")
        }
        "OverlappingVisibles" | "OvV" => true,
        "UniversalEngineProfile" | "UEP" | "UKEngineProfile" | "UEP2" => {
            matches!(arg, "1" | "")
        }
        "DownloadableFont" | "DLF" | "HDExtension" | "HDE" | "HDGraphicsPlaneExtension" | "HDG" => {
            false
        }
        // The interaction channel's profile 1 (files and streams over HTTP).
        "ICProfile" | "ICP" => ic && matches!(arg, "0" | "1" | ""),
        "IPStreaming" => false,
        "PNGBitmap" | "PNG" | "BitmapScaling" | "BSc" | "VideoScaling" | "VSc"
        | "BitmapDecodeOffset" | "BDO" | "VideoDecodeOffset" | "VDO" => true,
        "UKEngineProfileVersion" | "UniversalEngineProfileVersion" => true,
        _ => false,
    }
}
