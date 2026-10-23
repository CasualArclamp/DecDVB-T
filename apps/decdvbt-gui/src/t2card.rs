// made by CasualArclamp
//! What the signal card says of a DVB-T2 lock: the mode the L1 signalling
//! names, the PLP being decoded and how its FEC blocks are coming out.

use decdvbt_rx::t2::receiver::{FS_NATIVE, T2Status};

/// The card's mode line and detail rows for a T2 lock.
pub struct T2Card {
    /// "256-QAM 2/3 rotated": the constellation and code rate of the PLP.
    pub name: Option<String>,
    pub details: Vec<(String, String)>,
}

pub fn card(t2: &T2Status, stream_mbps: Option<f64>) -> T2Card {
    let kv = |k: &str, v: String| (k.to_string(), v);
    let mut details = Vec::new();
    if let Some(m) = &t2.mode {
        let g = m.guard.samples(m.fft);
        details.push(kv(
            "FFT size",
            format!("{}{}", m.fft.name(), if m.extended { " ext" } else { "" }),
        ));
        details.push(kv(
            "guard",
            format!("{} ({:.0} µs)", m.guard, g as f64 / FS_NATIVE * 1e6),
        ));
        details.push(kv("pilots", m.pp.to_string()));
        details.push(kv(
            "symbol",
            format!("{:.0} µs", (m.fft.n() + g) as f64 / FS_NATIVE * 1e6),
        ));
        details.push(kv("carriers", m.c_ps().to_string()));
    }
    let name = t2.plp.map(|p| {
        details.push(kv(
            "FEC",
            format!(
                "{}K LDPC · PLP {} · {} blocks a frame",
                if p.frame == decdvbt_core::FecFrame::Normal {
                    64
                } else {
                    16
                },
                t2.plp_id,
                t2.blocks_per_frame
            ),
        ));
        format!(
            "{} {}{}",
            p.qam.name(),
            p.rate,
            if p.rotated { " rotated" } else { "" }
        )
    });
    if t2.decoding {
        if let Some(r) = stream_mbps {
            details.push(kv("stream", format!("{r:.2} Mbit/s")));
        }
        let total = (t2.blocks_ok + t2.blocks_bad).max(1);
        details.push(kv(
            "LDPC blocks",
            format!(
                "{} lost of {} · {:.1} iterations",
                t2.blocks_bad,
                total,
                t2.ldpc_iterations as f64 / t2.blocks_ok.max(1) as f64
            ),
        ));
    }
    if t2.fec_dropped > 0 {
        details.push(kv(
            "FEC behind",
            format!("{} frames given up", t2.fec_dropped),
        ));
    }
    T2Card { name, details }
}
