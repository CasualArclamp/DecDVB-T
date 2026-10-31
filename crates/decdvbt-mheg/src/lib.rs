// made by CasualArclamp
//! MHEG-5, the UK Engine Profile (ETSI ES 202 184, the DTG D-Book 7): the
//! interactive ("Red Button") applications a service sends in its DSM-CC
//! object carousel, run and drawn on a 720x576 canvas.
//!
//! - [`asn1`]: the binary (ASN.1 BER) interchange form, read into a tree.
//! - [`obj`]: the classes, loaded from the tree.
//! - [`engine`]: applications, scenes, links, events and actions.
//! - [`programs`]: the resident programs.
//! - [`render`]: the canvas.
//! - [`ic`]: the interaction channel, files over HTTP(S).
//!
//! MythTV's freemheg (GPL-2.0-or-later, by David C. J. Matthews) is the
//! reference throughout; each module says what it follows.

pub mod asn1;
pub mod engine;
pub mod ic;
mod ic_tls;
mod list;
pub mod obj;
mod programs;
pub mod render;
pub mod tags;
#[cfg(test)]
mod tests;

pub use engine::{Engine, Fetch, Files, Request, Service, key};

/// What the engine says it is (WhoAmI: manufacturer, model, version; D-Book
/// 7 §13.10.4.24). Applications pick their files by it (Freeview's internet
/// channels give an unknown receiver their SD streams), and the interaction
/// channel's requests carry it ([`ic::USER_AGENT`]).
pub const WHO_AM_I: &str = "DDT DecDVB-T 010";
pub use render::Renderer;
