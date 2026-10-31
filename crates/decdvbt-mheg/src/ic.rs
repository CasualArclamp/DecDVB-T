// made by CasualArclamp
//! The interaction channel (ETSI ES 202 184, the IC part of the UK profile):
//! applications and their files fetched over HTTP(S), as Freeview's
//! internet-delivered channels need (the broadcast carousel names an
//! application by URL; that application names its scenes, images and
//! stream on the same server).
//!
//! [`Http`] fetches on its own threads and keeps what it got; [`IcFiles`]
//! puts it beside the carousel behind the engine's [`Files`]: a URL is
//! fetched (once) if the carousel's `/auth.servers` lists its server, and
//! reads as [`Fetch::Pending`] until it is in, so the engine waits for it as
//! it waits for a carousel module.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine::{Fetch, Files};

/// What the interaction channel's requests say they come from: this
/// receiver, by its WhoAmI (never a television's).
pub const USER_AGENT: &str = "DecDVB-T MHEG-5 (DDT DecDVB-T 010)";

/// The largest file taken (applications, scenes and images are small).
const MAX_BODY: u64 = 8 << 20;

/// A stream the application asked to play ([`crate::Request::IcStream`]),
/// opened for reading: a GET that follows redirects, its body read as it
/// comes (an MPEG transport stream for as long as the server sends).
pub fn open_stream(url: &str) -> Result<Box<dyn std::io::Read + Send>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(15)))
        .user_agent(USER_AGENT)
        .build()
        .into();
    let r = agent.get(url).call().map_err(|e| e.to_string())?;
    // `into_body().into_reader()` gives the body as a plain `Read`;
    // `with_config().limit` lifts ureq's default cap (a live stream has no end).
    Ok(Box::new(
        r.into_body().into_with_config().limit(u64::MAX).reader(),
    ))
}
/// Threads fetching at once.
const WORKERS: usize = 4;

#[derive(Clone)]
enum Got {
    Pending,
    Ready(Arc<Vec<u8>>),
    Missing,
}

/// One fetch, for the trace / the GUI: the URL and how it went.
#[derive(Debug, Clone)]
pub struct Logged {
    pub url: String,
    /// "200 1467 bytes", "refused: not in auth.servers", an error.
    pub outcome: String,
}

struct Shared {
    got: HashMap<String, Got>,
    log: Vec<Logged>,
    /// The carousel's TLS certificates (its auth.tls.N files): an https
    /// server showing one of them is taken ([`crate::ic_tls`]).
    tls_certs: Option<Vec<Vec<u8>>>,
}

/// A request for the workers: a GET, or a POST of a form.
struct Job {
    url: String,
    form: Option<Vec<u8>>,
    /// What the answer is filed under ([`post_key`] for a POST).
    key: String,
}

/// A POST's answer is filed under its URL and form both.
fn post_key(url: &str, form: &[u8]) -> String {
    format!("POST {url}\n{}", String::from_utf8_lossy(form))
}

/// HTTP(S) GETs on background threads, each URL once; POSTs each time.
pub struct Http {
    shared: Arc<Mutex<Shared>>,
    // `Option` so `Drop` can close the channel (the workers then end).
    tx: Option<Sender<Job>>,
}

impl Http {
    /// `user_agent` is what the requests say they are (the receiver's
    /// WhoAmI, so a server can tell it from a television).
    pub fn new(user_agent: &str) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            got: HashMap::new(),
            log: Vec::new(),
            tls_certs: None,
        }));
        let (tx, rx) = channel::<Job>();
        // One receiver shared by the workers: `Arc<Mutex<..>>` lets several
        // threads take turns at it.
        let rx = Arc::new(Mutex::new(rx));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .user_agent(user_agent)
            .http_status_as_error(false)
            .build()
            .into();
        for _ in 0..WORKERS {
            let (rx, shared, agent) = (rx.clone(), shared.clone(), agent.clone());
            let ua = user_agent.to_string();
            std::thread::Builder::new()
                .name("mheg-ic".into())
                .spawn(move || worker(&rx, &shared, &agent, &ua))
                .expect("start an IC fetch thread");
        }
        Http {
            shared,
            tx: Some(tx),
        }
    }

    /// The file at `url`: asked for the first time, it is fetched.
    pub fn fetch(&self, url: &str) -> Fetch {
        let mut s = self.shared.lock().unwrap();
        match s.got.get(url) {
            Some(Got::Ready(b)) => Fetch::Ready(b.to_vec()),
            Some(Got::Missing) => Fetch::Missing,
            Some(Got::Pending) => Fetch::Pending,
            None => {
                s.got.insert(url.to_string(), Got::Pending);
                drop(s);
                self.send(Job {
                    url: url.to_string(),
                    form: None,
                    key: url.to_string(),
                });
                Fetch::Pending
            }
        }
    }

    /// POST `form` to `url`: Pending until the answer is in, which is given
    /// once (the next call with the same form posts again).
    pub fn post(&self, url: &str, form: &[u8]) -> Fetch {
        let key = post_key(url, form);
        let mut s = self.shared.lock().unwrap();
        match s.got.get(&key) {
            Some(Got::Pending) => Fetch::Pending,
            Some(Got::Ready(_)) | Some(Got::Missing) => match s.got.remove(&key) {
                Some(Got::Ready(b)) => Fetch::Ready(b.to_vec()),
                _ => Fetch::Missing,
            },
            None => {
                s.got.insert(key.clone(), Got::Pending);
                drop(s);
                self.send(Job {
                    url: url.to_string(),
                    form: Some(form.to_vec()),
                    key,
                });
                Fetch::Pending
            }
        }
    }

    fn send(&self, job: Job) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(job);
        }
    }

    /// Note a URL that was not fetched (and say why).
    fn refuse(&self, url: &str, why: &str) {
        let mut s = self.shared.lock().unwrap();
        if !s.got.contains_key(url) {
            s.got.insert(url.to_string(), Got::Missing);
            s.log.push(Logged {
                url: url.to_string(),
                outcome: why.to_string(),
            });
        }
    }

    /// The fetches so far, oldest first.
    pub fn log(&self) -> Vec<Logged> {
        self.shared.lock().unwrap().log.clone()
    }

    /// Whether any fetch is still going.
    pub fn busy(&self) -> bool {
        let s = self.shared.lock().unwrap();
        s.got.values().any(|g| matches!(g, Got::Pending))
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        // Closing the channel ends the workers once their fetch is done.
        self.tx = None;
    }
}

/// One request through ureq (the web's certificate authorities for https):
/// (status, body).
fn web_request(agent: &ureq::Agent, job: &Job) -> Result<(u16, Vec<u8>), String> {
    let answer = match &job.form {
        None => agent.get(&job.url).call(),
        Some(f) => agent
            .post(&job.url)
            .content_type("application/x-www-form-urlencoded")
            .send(&f[..]),
    };
    let mut r = answer.map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let body = r
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| format!("body: {e}"))?;
    Ok((status, body))
}

fn worker(rx: &Mutex<Receiver<Job>>, shared: &Mutex<Shared>, agent: &ureq::Agent, ua: &str) {
    loop {
        // The lock is held only while waiting for the next job.
        let job = match rx.lock().unwrap().recv() {
            Ok(j) => j,
            Err(_) => return,
        };
        let certs = shared.lock().unwrap().tls_certs.clone().unwrap_or_default();
        // A server whose certificate the carousel carries first; then the
        // web's way (a server with an ordinary certificate).
        let answer = if job.url.starts_with("https://") && !certs.is_empty() {
            crate::ic_tls::request(&job.url, job.form.as_deref(), ua, &certs, MAX_BODY as usize)
                .or_else(|pinned| {
                    web_request(agent, &job).map_err(|web| format!("{pinned}; {web}"))
                })
        } else {
            web_request(agent, &job)
        };
        let (got, outcome) = match answer {
            Ok((status, b)) if (200..300).contains(&status) => {
                let o = format!("{status} {} bytes", b.len());
                (Got::Ready(Arc::new(b)), o)
            }
            Ok((status, b)) => (
                Got::Missing,
                format!(
                    "{status} {:?}",
                    String::from_utf8_lossy(&b[..b.len().min(200)])
                ),
            ),
            Err(e) => (Got::Missing, e),
        };
        let mut s = shared.lock().unwrap();
        s.got.insert(job.key, got);
        let url = match &job.form {
            None => job.url,
            Some(f) => format!("POST {} [{}]", job.url, String::from_utf8_lossy(f)),
        };
        s.log.push(Logged { url, outcome });
    }
}

/// The servers a carousel lets its applications reach: its `/auth.servers`,
/// one URL prefix a line (Freeview's start each with '!' and group them
/// with '#' lines). None: the file is not there (or not in yet).
fn allowed_servers(carousel: &dyn Files) -> Option<Vec<String>> {
    let Fetch::Ready(b) = carousel.fetch("/auth.servers") else {
        return None;
    };
    Some(
        String::from_utf8_lossy(&b)
            .lines()
            .filter_map(|l| {
                let l = l
                    .trim()
                    .trim_start_matches(|c: char| !c.is_ascii_alphabetic());
                crate::obj::is_url(l).then(|| l.to_string())
            })
            .collect(),
    )
}

/// Whether `url` falls under one of the allowed prefixes ("http://h/"
/// allows everything on h; a port written or not is the same server).
fn allowed(url: &str, servers: &[String]) -> bool {
    let strip_port = |u: &str| -> String {
        let Some(i) = u.find("://") else {
            return u.to_string();
        };
        let rest = &u[i + 3..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        let host = &rest[..host_end];
        let host = host.split(':').next().unwrap_or(host);
        format!("{}{host}{}", &u[..i + 3], &rest[host_end..])
    };
    let u = strip_port(url);
    servers.iter().any(|s| u.starts_with(&strip_port(s)))
}

/// The carousel with the interaction channel beside it.
pub struct IcFiles<'a> {
    pub carousel: &'a dyn Files,
    pub http: &'a Http,
}

impl IcFiles<'_> {
    /// Whether the carousel lets applications reach `url` (noted if not).
    /// The first time, the carousel's TLS certificates are taken too.
    fn may_reach(&self, url: &str) -> bool {
        {
            let mut s = self.http.shared.lock().unwrap();
            if s.tls_certs.is_none() {
                let mut certs = Vec::new();
                // auth.tls.1, auth.tls.2…: the first missing one ends them.
                for i in 1..=16 {
                    match self.carousel.fetch(&format!("/auth.tls.{i}")) {
                        Fetch::Ready(b) => certs.extend(crate::ic_tls::parse_auth_tls(&b)),
                        _ => break,
                    }
                }
                s.tls_certs = Some(certs);
            }
        }
        match allowed_servers(self.carousel) {
            Some(s) if allowed(url, &s) => true,
            Some(_) => {
                self.http.refuse(url, "refused: not in auth.servers");
                false
            }
            None => {
                self.http
                    .refuse(url, "refused: the carousel has no auth.servers");
                false
            }
        }
    }
}

impl Files for IcFiles<'_> {
    fn fetch(&self, path: &str) -> Fetch {
        if !crate::obj::is_url(path) {
            return self.carousel.fetch(path);
        }
        if self.may_reach(path) {
            self.http.fetch(path)
        } else {
            Fetch::Missing
        }
    }

    fn post(&self, url: &str, form: &[u8]) -> Fetch {
        if crate::obj::is_url(url) && self.may_reach(url) {
            self.http.post(url, form)
        } else {
            Fetch::Missing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_is_the_who_am_i() {
        assert!(USER_AGENT.contains(crate::WHO_AM_I));
    }

    #[test]
    fn servers() {
        let s = vec![
            "http://cdn.example.com/".to_string(),
            "https://api.example.com:443/".to_string(),
        ];
        assert!(allowed("http://cdn.example.com/apps/nhk/cont/a", &s));
        assert!(allowed("http://cdn.example.com:80/x", &s));
        assert!(allowed("https://api.example.com/m/?op=x", &s));
        assert!(!allowed("http://api.example.com/m/", &s));
        assert!(!allowed("http://cdn.example.com.evil.net/", &s));
    }
}
