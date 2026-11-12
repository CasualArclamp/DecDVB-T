// made by CasualArclamp
//! A radio on another machine: the client end of DecSAT's remote protocol
//! (`decsat-server`, see DecSAT's `docs/REMOTE.md`), in its IQ ("dumb")
//! mode. The server opens one of its radios and sends the samples, packed
//! to 4-16 bits and optionally zstd-compressed, over one TCP connection;
//! DecDVB-T decodes them as it would a radio of its own.
//!
//! From DecSAT's `decsat-remote` crate (`client.rs`, `wire.rs`, `iq.rs`,
//! `auth.rs`, `msg.rs`), cut down to this one mode: decoding on the server
//! (smart and shared mode) is DecSAT's own, and so is the server. Receive
//! only, as the server is.
//!
//! The password is proved by HMAC and never sent, but the link is not
//! encrypted: over the internet, carry it inside a VPN or an SSH tunnel.

pub mod auth;
pub mod iq;
pub mod msg;
pub mod wire;

use std::io::{self, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use decdvbt_core::{Error, Iq, Result};

use self::msg::{C2S, Hello, Mode, S2C};
pub use self::msg::{Gain, IqParams, LinkStats, RadioInfo, RadioRequest, ServerMode};
use self::wire::{Kind, MAGIC, PROTO};
use crate::IqSource;

/// The port a server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 5262;

const CONNECT: Duration = Duration::from_secs(5);
/// How long the server may take to open a radio.
const START: Duration = Duration::from_secs(15);
/// No frame from the server for this long and the link is presumed dead.
const IDLE: Duration = Duration::from_secs(15);
const PING: Duration = Duration::from_secs(2);
/// IQ blocks the reader may queue for the engine before dropping.
const IQ_QUEUE: usize = 256;
const IDLE_SAMPLES: usize = 256;

fn err(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("remote: {e}"))
}

/// How to reach a server.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// `HOST` or `HOST:PORT`.
    pub addr: String,
    /// Shown to other users on a shared server, and in the server's log.
    pub name: String,
    pub password: Option<String>,
    /// Replace whoever holds the server, if it allows that.
    pub takeover: bool,
}

/// What a server said about itself.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub mode: ServerMode,
    pub radios: Vec<RadioInfo>,
}

fn resolve(addr: &str) -> Result<SocketAddr> {
    let with_port = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(err)?
        .next()
        .ok_or_else(|| err(format!("cannot resolve {addr}")))
}

fn send(s: &mut TcpStream, m: &C2S) -> io::Result<()> {
    wire::write_frame(s, Kind::Control, &msg::encode(m), 0, &mut Vec::new()).map(|_| ())
}

fn read_s2c(s: &mut TcpStream) -> Result<S2C> {
    let (mut b, mut t) = (Vec::new(), Vec::new());
    let (kind, _) = wire::read_frame(s, &mut b, &mut t).map_err(err)?;
    if kind != Kind::Control {
        return Err(err("expected a control frame"));
    }
    msg::decode(&b).ok_or_else(|| err("bad message from the server"))
}

/// Connect and run the handshake, as far as the server's `Ready`.
fn handshake(o: &ConnectOptions) -> Result<(TcpStream, ServerInfo)> {
    let addr = resolve(&o.addr)?;
    let mut s =
        TcpStream::connect_timeout(&addr, CONNECT).map_err(|e| err(format!("{addr}: {e}")))?;
    let _ = s.set_nodelay(true);
    s.set_read_timeout(Some(START)).map_err(err)?;
    s.set_write_timeout(Some(START)).map_err(err)?;
    s.write_all(&MAGIC).map_err(err)?;
    send(
        &mut s,
        &C2S::Hello(Hello {
            proto: PROTO,
            version: concat!("DecDVB-T ", env!("CARGO_PKG_VERSION")).into(),
            name: o.name.clone(),
            takeover: o.takeover,
        }),
    )
    .map_err(err)?;
    let (name, version, mode, nonce) = match read_s2c(&mut s)? {
        S2C::ServerHello {
            proto,
            version,
            name,
            mode,
            nonce,
        } => {
            if proto != PROTO {
                return Err(err(format!(
                    "server speaks protocol {proto}, this client {PROTO}"
                )));
            }
            (name, version, mode, nonce)
        }
        S2C::Refused(why) => return Err(err(why)),
        _ => return Err(err("unexpected reply")),
    };
    if let Some(n) = nonce {
        let pw = o
            .password
            .as_deref()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| err("this server needs a password"))?;
        let client_nonce = auth::nonce();
        let proof = auth::proof(pw, &n, &client_nonce);
        send(
            &mut s,
            &C2S::Auth {
                client_nonce,
                proof,
            },
        )
        .map_err(err)?;
    }
    match read_s2c(&mut s)? {
        S2C::Ready { radios } => Ok((
            s,
            ServerInfo {
                name,
                version,
                mode,
                radios,
            },
        )),
        S2C::Refused(why) => Err(err(why)),
        _ => Err(err("unexpected reply")),
    }
}

/// Ask a server what it is and which radios it has, then hang up.
pub fn probe(o: &ConnectOptions) -> Result<ServerInfo> {
    let (s, info) = handshake(o)?;
    let _ = s.shutdown(Shutdown::Both);
    Ok(info)
}

#[derive(Default)]
struct LinkState {
    stats: LinkStats,
    note: Option<String>,
    center_hz: f64,
    gains: Vec<Gain>,
    /// Why the link ended.
    ended: Option<String>,
}

/// The handle commands are sent through.
struct Link {
    tx: Mutex<TcpStream>,
    state: Mutex<LinkState>,
    closed: AtomicBool,
    bytes: AtomicU64,
}

impl Link {
    fn send(&self, m: &C2S) {
        let mut s = self.tx.lock().unwrap();
        if send(&mut s, m).is_err() {
            self.closed.store(true, Ordering::Relaxed);
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.tx.lock().unwrap().shutdown(Shutdown::Both);
    }

    fn end(&self, why: String) {
        self.state.lock().unwrap().ended.get_or_insert(why);
        self.closed.store(true, Ordering::Relaxed);
    }

    // Rust note: `self: Arc<Self>` is a method that takes ownership of one
    // reference-counted handle, which the pinging thread then keeps.
    fn ping_loop(self: Arc<Self>) {
        let mut t = 0;
        while !self.closed.load(Ordering::Relaxed) {
            std::thread::sleep(PING);
            t += 1;
            self.send(&C2S::Ping(t));
        }
    }
}

/// Live controls of a remote radio. Cloning shares them.
#[derive(Clone)]
pub struct RemoteControl {
    link: Arc<Link>,
}

impl RemoteControl {
    pub fn tune(&self, hz: f64) {
        self.link.send(&C2S::Tune { hz });
    }

    /// Set one of the radio's gain stages, by the name the server gave it.
    pub fn set_gain(&self, name: &str, value: f64) {
        {
            let mut st = self.link.state.lock().unwrap();
            if let Some(g) = st.gains.iter_mut().find(|g| g.name == name) {
                g.value = value;
            }
        }
        self.link.send(&C2S::Gain {
            name: name.into(),
            value,
        });
    }

    /// The radio's gain stages as the server described them, with the
    /// values last set.
    pub fn gains(&self) -> Vec<Gain> {
        self.link.state.lock().unwrap().gains.clone()
    }

    pub fn center_hz(&self) -> f64 {
        self.link.state.lock().unwrap().center_hz
    }

    pub fn stats(&self) -> LinkStats {
        self.link.state.lock().unwrap().stats
    }

    /// Bytes received since the connection opened.
    pub fn bytes_received(&self) -> u64 {
        self.link.bytes.load(Ordering::Relaxed)
    }

    /// The server's latest message for the user, once.
    pub fn take_note(&self) -> Option<String> {
        self.link.state.lock().unwrap().note.take()
    }

    pub fn ended(&self) -> Option<String> {
        self.link.state.lock().unwrap().ended.clone()
    }
}

fn control_message(link: &Link, m: S2C) {
    let mut st = link.state.lock().unwrap();
    match m {
        S2C::Stats(s) => st.stats = s,
        S2C::Note(n) => st.note = Some(n),
        S2C::Tuned { center_hz } => st.center_hz = center_hz,
        S2C::Stopped(why) | S2C::Refused(why) => {
            st.ended.get_or_insert(why);
        }
        _ => {}
    }
}

/// Raw IQ from a remote radio, as an [`IqSource`].
pub struct RemoteIqSource {
    rx: Receiver<Vec<Iq>>,
    held: Vec<Iq>,
    held_at: usize,
    rate: f64,
    description: String,
    dropped: Arc<AtomicU64>,
    control: RemoteControl,
}

impl RemoteIqSource {
    /// Connect, have the server open `radio` and start its stream. A
    /// `radio.key` left empty takes the first radio the server offers.
    pub fn connect(
        o: &ConnectOptions,
        mut radio: RadioRequest,
        p: IqParams,
    ) -> Result<RemoteIqSource> {
        if !iq::BITS.contains(&p.bits) {
            return Err(err(format!(
                "{} bits a sample is not offered (4, 6, 8, 10, 12 or 16)",
                p.bits
            )));
        }
        let (mut s, info) = handshake(o)?;
        if info.mode == ServerMode::Shared {
            return Err(err("a shared server decodes for its users and sends no IQ"));
        }
        if radio.key.is_empty() {
            let first = info
                .radios
                .first()
                .ok_or_else(|| err("the server offers no radio"))?;
            radio.key = first.key.clone();
        }
        let known = info.radios.iter().find(|r| r.key == radio.key);
        if known.is_none() && !info.radios.is_empty() {
            let keys: Vec<&str> = info.radios.iter().map(|r| r.key.as_str()).collect();
            return Err(err(format!(
                "the server has no radio {:?} (it offers {})",
                radio.key,
                keys.join(", ")
            )));
        }
        send(
            &mut s,
            &C2S::Start {
                radio: Some(radio),
                mode: Mode::Dumb(p),
            },
        )
        .map_err(err)?;
        let (rate, center, description, gains) = loop {
            match read_s2c(&mut s)? {
                S2C::Started {
                    sample_rate,
                    center_hz,
                    description,
                    gains,
                } => break (sample_rate, center_hz, description, gains),
                S2C::Stopped(why) | S2C::Refused(why) => return Err(err(why)),
                _ => {}
            }
        };
        s.set_read_timeout(Some(IDLE)).map_err(err)?;
        let link = Arc::new(Link {
            tx: Mutex::new(s.try_clone().map_err(err)?),
            state: Mutex::new(LinkState {
                center_hz: center,
                gains,
                ..Default::default()
            }),
            closed: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
        });
        {
            let l = Arc::clone(&link);
            std::thread::Builder::new()
                .name("remote-ping".into())
                .spawn(move || l.ping_loop())
                .map_err(err)?;
        }
        let (tx, rx) = mpsc::sync_channel(IQ_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        {
            let (link, dropped) = (Arc::clone(&link), Arc::clone(&dropped));
            std::thread::Builder::new()
                .name("remote-iq-rx".into())
                .spawn(move || iq_reader(&mut s, &link, &tx, &dropped))
                .map_err(err)?;
        }
        Ok(RemoteIqSource {
            rx,
            held: Vec::new(),
            held_at: 0,
            rate,
            description: format!("{description} on {} ({})", o.addr, info.name),
            dropped,
            control: RemoteControl { link },
        })
    }

    pub fn control(&self) -> RemoteControl {
        self.control.clone()
    }

    /// A counter of the samples lost so far on this end plus the gaps the
    /// server left, for the dropped-buffer readout.
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn iq_reader(s: &mut TcpStream, link: &Link, tx: &SyncSender<Vec<Iq>>, dropped: &AtomicU64) {
    let (mut b, mut t) = (Vec::new(), Vec::new());
    let mut next = 0u64;
    loop {
        let (kind, n) = match wire::read_frame(s, &mut b, &mut t) {
            Ok(f) => f,
            Err(e) => {
                link.end(e.to_string());
                return;
            }
        };
        link.bytes.fetch_add(n as u64, Ordering::Relaxed);
        match kind {
            Kind::Iq => {
                let Some((first, count)) = iq::peek(&b) else {
                    continue;
                };
                if first > next {
                    dropped.fetch_add(first - next, Ordering::Relaxed);
                }
                next = first + count as u64;
                let mut out = Vec::with_capacity(count);
                if iq::unpack(&b, &mut out).is_none() {
                    continue;
                }
                match tx.try_send(out) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        dropped.fetch_add(count as u64, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        link.close();
                        return;
                    }
                }
            }
            Kind::Control => {
                if let Some(m) = msg::decode::<S2C>(&b) {
                    control_message(link, m);
                }
            }
            // Smart mode's frames: not asked for, so not expected.
            _ => {}
        }
    }
}

impl Drop for RemoteIqSource {
    fn drop(&mut self) {
        self.control.link.close();
    }
}

impl IqSource for RemoteIqSource {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        out.clear();
        let cap = out.capacity().max(4096);
        while out.len() < cap {
            if self.held_at >= self.held.len() {
                let wait = if out.is_empty() {
                    Duration::from_millis(100)
                } else {
                    Duration::ZERO
                };
                match self.rx.recv_timeout(wait) {
                    Ok(b) => {
                        self.held = b;
                        self.held_at = 0;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        let why = self.control.ended().unwrap_or_else(|| "link closed".into());
                        return Err(err(why));
                    }
                }
            }
            let n = (cap - out.len()).min(self.held.len() - self.held_at);
            out.extend_from_slice(&self.held[self.held_at..self.held_at + n]);
            self.held_at += n;
        }
        if out.is_empty() {
            // Keeps the engine answering its commands through a stall
            // (`Ok(0)` would mean the end of the stream).
            out.resize(IDLE_SAMPLES, Iq::new(0.0, 0.0));
        }
        Ok(out.len())
    }

    fn sample_rate(&self) -> f64 {
        self.rate
    }

    fn center_freq(&self) -> f64 {
        self.control.center_hz()
    }

    fn describe(&self) -> String {
        self.description.clone()
    }

    /// Asks the server to retune; what is still on its way from the old
    /// frequency is thrown away here. A capture played by the server stays
    /// where it is (the server says so in a note).
    fn retune(&mut self, hz: f64) -> Result<bool> {
        if let Some(why) = self.control.ended() {
            return Err(err(why));
        }
        self.control.tune(hz);
        self.control.link.state.lock().unwrap().center_hz = hz;
        while self.rx.try_recv().is_ok() {}
        self.held.clear();
        self.held_at = 0;
        Ok(true)
    }

    fn is_live(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;

    use super::*;

    /// What the fake server saw, for the test to check.
    #[derive(Default)]
    struct Seen {
        hello: Option<Hello>,
        radio: Option<RadioRequest>,
        params: Option<IqParams>,
        tuned: Vec<f64>,
        gains: Vec<(String, f64)>,
    }

    fn read_c2s(s: &mut TcpStream) -> Option<C2S> {
        let (mut b, mut t) = (Vec::new(), Vec::new());
        let (kind, _) = wire::read_frame(s, &mut b, &mut t).ok()?;
        assert_eq!(kind, Kind::Control);
        msg::decode(&b)
    }

    fn send_s2c(s: &mut TcpStream, m: &S2C) {
        // The client may have hung up already (a refusal it did not wait for).
        let _ = wire::write_frame(s, Kind::Control, &msg::encode(m), 0, &mut Vec::new());
    }

    fn ramp(first: u64, n: usize) -> Vec<Iq> {
        (0..n)
            .map(|i| {
                let v = ((first as usize + i) % 100) as f32 - 50.0;
                Iq::new(v / 128.0, -v / 128.0)
            })
            .collect()
    }

    /// A stand-in for `decsat-server` in exclusive mode with one radio: the
    /// handshake (with a password if given), `Started`, then `blocks` blocks
    /// of a known ramp, answering `Tune` and `Gain` as the real one does.
    /// One block is left out so the client has a gap to count.
    fn fake_server(
        password: Option<&'static str>,
        mode: ServerMode,
        blocks: u64,
    ) -> (String, std::thread::JoinHandle<Seen>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = std::thread::spawn(move || {
            let mut seen = Seen::default();
            let (mut s, _) = l.accept().unwrap();
            let mut magic = [0u8; 4];
            s.read_exact(&mut magic).unwrap();
            assert_eq!(magic, MAGIC);
            let Some(C2S::Hello(h)) = read_c2s(&mut s) else {
                panic!("no hello");
            };
            assert_eq!(h.proto, PROTO);
            seen.hello = Some(h);
            let nonce = password.map(|_| auth::nonce());
            send_s2c(
                &mut s,
                &S2C::ServerHello {
                    proto: PROTO,
                    version: "test".into(),
                    name: "bench".into(),
                    mode,
                    nonce,
                },
            );
            if let (Some(pw), Some(n)) = (password, nonce) {
                match read_c2s(&mut s) {
                    Some(C2S::Auth {
                        client_nonce,
                        proof,
                    }) if auth::verify(pw, &n, &client_nonce, &proof) => {}
                    _ => {
                        send_s2c(&mut s, &S2C::Refused("wrong password".into()));
                        return seen;
                    }
                }
            }
            send_s2c(
                &mut s,
                &S2C::Ready {
                    radios: vec![RadioInfo {
                        key: "fake".into(),
                        name: "Fake radio".into(),
                        min_hz: 1e6,
                        max_hz: 6e9,
                        rates: vec![2e6, 8e6],
                        gains: vec![Gain {
                            name: "lna".into(),
                            min: 0.0,
                            max: 40.0,
                            step: 8.0,
                            value: 16.0,
                        }],
                    }],
                },
            );
            let Some(C2S::Start { radio, mode }) = read_c2s(&mut s) else {
                return seen;
            };
            let radio = radio.expect("a radio");
            let Mode::Dumb(p) = mode else {
                panic!("only IQ mode is asked for")
            };
            send_s2c(
                &mut s,
                &S2C::Started {
                    sample_rate: radio.sample_rate,
                    center_hz: radio.center_hz,
                    description: "Fake radio".into(),
                    gains: vec![Gain {
                        name: "lna".into(),
                        min: 0.0,
                        max: 40.0,
                        step: 8.0,
                        value: 16.0,
                    }],
                },
            );
            seen.radio = Some(radio);
            seen.params = Some(p);
            let (mut packed, mut scratch) = (Vec::new(), Vec::new());
            for b in 0..blocks {
                if b == 2 {
                    continue; // a block the link "lost"
                }
                iq::pack(&ramp(b * 2000, 2000), b * 2000, p.bits, &mut packed);
                wire::write_frame(&mut s, Kind::Iq, &packed, p.zstd, &mut scratch).unwrap();
            }
            send_s2c(&mut s, &S2C::Note("hello from the server".into()));
            // Then serve commands until the client hangs up.
            while let Some(m) = read_c2s(&mut s) {
                match m {
                    C2S::Tune { hz } => {
                        seen.tuned.push(hz);
                        send_s2c(&mut s, &S2C::Tuned { center_hz: hz });
                    }
                    C2S::Gain { name, value } => seen.gains.push((name, value)),
                    C2S::Ping(t) => send_s2c(&mut s, &S2C::Pong(t)),
                    _ => {}
                }
            }
            seen
        });
        (addr, h)
    }

    fn request(key: &str) -> RadioRequest {
        RadioRequest {
            key: key.into(),
            center_hz: 100e6,
            sample_rate: 2e6,
            gains: vec![("lna".into(), 24.0)],
        }
    }

    /// Why a connection was refused (it must be).
    fn refusal(o: &ConnectOptions, key: &str, p: IqParams) -> String {
        match RemoteIqSource::connect(o, request(key), p) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("connected"),
        }
    }

    /// The whole session against the fake server, at every sample width,
    /// with and without zstd and a password.
    #[test]
    fn iq_arrives_in_order_and_commands_reach_the_server() {
        for (bits, zstd, password) in [
            (8, 0, None),
            (8, 3, Some("hunter2")),
            (12, 1, None),
            (16, 0, Some("x")),
        ] {
            let (addr, server) = fake_server(password, ServerMode::Exclusive, 6);
            let o = ConnectOptions {
                addr,
                name: "tester".into(),
                password: password.map(String::from),
                takeover: false,
            };
            let p = IqParams {
                bits,
                zstd,
                slice: None,
            };
            // An empty key takes the server's first radio.
            let mut src = RemoteIqSource::connect(&o, request(""), p).expect("connect");
            assert_eq!(src.sample_rate(), 2e6);
            assert_eq!(src.center_freq(), 100e6);
            assert!(src.is_live());
            assert!(src.describe().contains("Fake radio"), "{}", src.describe());
            let c = src.control();
            assert_eq!(c.gains()[0].name, "lna");

            // Five blocks of 2000 arrive (one of the six was left out).
            let mut got = Vec::new();
            let mut buf = Vec::with_capacity(4096);
            let t0 = std::time::Instant::now();
            while got.len() < 10_000 && t0.elapsed() < Duration::from_secs(10) {
                src.read(&mut buf).unwrap();
                // Idle filler (zeros while nothing has arrived) is not signal.
                if buf.len() == IDLE_SAMPLES && buf.iter().all(|s| s.re == 0.0 && s.im == 0.0) {
                    continue;
                }
                got.extend_from_slice(&buf);
            }
            assert_eq!(got.len(), 10_000, "{bits} bits");
            let mut want = ramp(0, 4000);
            want.extend(ramp(6000, 6000));
            for (i, (a, b)) in got.iter().zip(&want).enumerate() {
                assert!(
                    (a - b).norm() < 1e-3,
                    "{bits} bits, sample {i}: {a} against {b}"
                );
            }
            assert_eq!(src.dropped(), 2000, "the block the server left out");

            assert!(src.retune(145.5e6).unwrap());
            c.set_gain("lna", 32.0);
            assert_eq!(c.gains()[0].value, 32.0);
            // The note was sent after the IQ, so it is in by now or soon.
            let t0 = std::time::Instant::now();
            let mut note = None;
            while note.is_none() && t0.elapsed() < Duration::from_secs(5) {
                note = c.take_note();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(note.as_deref(), Some("hello from the server"));
            // (A ramp compresses to very little under zstd.)
            assert!(c.bytes_received() > 500);
            // Let the commands cross before hanging up.
            std::thread::sleep(Duration::from_millis(100));
            drop(src);
            let seen = server.join().unwrap();
            assert_eq!(seen.hello.unwrap().name, "tester");
            let r = seen.radio.unwrap();
            assert_eq!(r.key, "fake");
            assert_eq!(r.gains, [("lna".to_string(), 24.0)]);
            assert_eq!(seen.params.unwrap(), p);
            assert_eq!(seen.tuned, [145.5e6]);
            assert_eq!(seen.gains, [("lna".to_string(), 32.0)]);
        }
    }

    #[test]
    fn a_wrong_or_missing_password_is_refused() {
        for given in [Some("wrong"), None] {
            let (addr, server) = fake_server(Some("right"), ServerMode::Exclusive, 0);
            let o = ConnectOptions {
                addr,
                password: given.map(String::from),
                ..Default::default()
            };
            let e = refusal(&o, "fake", IqParams::default());
            assert!(e.contains("password"), "{e}");
            let _ = server.join();
        }
    }

    #[test]
    fn an_unknown_radio_a_shared_server_and_odd_bits_are_refused() {
        let (addr, server) = fake_server(None, ServerMode::Exclusive, 0);
        let o = ConnectOptions {
            addr,
            ..Default::default()
        };
        let e = refusal(&o, "hackrf", IqParams::default());
        assert!(e.contains("fake"), "{e}");
        let _ = server.join();

        let (addr, server) = fake_server(None, ServerMode::Shared, 0);
        let o = ConnectOptions {
            addr,
            ..Default::default()
        };
        let e = refusal(&o, "fake", IqParams::default());
        assert!(e.contains("shared"), "{e}");
        let _ = server.join();

        let p = IqParams {
            bits: 7,
            ..Default::default()
        };
        assert!(refusal(&o, "fake", p).contains("bits"));
    }

    #[test]
    fn probe_lists_the_radios() {
        let (addr, server) = fake_server(None, ServerMode::Exclusive, 0);
        let info = probe(&ConnectOptions {
            addr,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(info.name, "bench");
        assert_eq!(info.radios[0].key, "fake");
        assert_eq!(info.radios[0].rates, [2e6, 8e6]);
        let _ = server.join();
    }

    #[test]
    fn nobody_listening_is_an_error_not_a_hang() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        let o = ConnectOptions {
            addr,
            ..Default::default()
        };
        assert!(probe(&o).is_err());
    }
}
