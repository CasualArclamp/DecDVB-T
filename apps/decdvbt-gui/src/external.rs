// made by CasualArclamp
//! Open a service in VLC, through the HTTP output.
//!
//! VLC gets the server's URL and `--program` for the service (a DVB-T
//! multiplex carries a dozen, and VLC would otherwise take the first). It is
//! looked for where its installer puts it, then on the PATH; if it is not
//! found the URL is reported to open by hand.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

fn candidates() -> Vec<PathBuf> {
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|d| {
            PathBuf::from(d)
                .join("VideoLAN")
                .join("VLC")
                .join("vlc.exe")
        })
        .collect()
}

/// The URL a local player should open for a server on `addr`: a server
/// listening on every interface is reached here through the loopback.
pub fn http_url(addr: SocketAddr) -> String {
    let ip = if addr.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        addr.ip()
    };
    format!("http://{}/", SocketAddr::new(ip, addr.port()))
}

/// Start VLC on `url`, playing service `program`.
pub fn vlc(url: &str, program: u16) -> Result<(), String> {
    let args = [url.to_string(), format!("--program={program}")];
    for exe in candidates() {
        if exe.exists() {
            return std::process::Command::new(&exe)
                .args(&args)
                .spawn()
                .map(|_| ())
                .map_err(|e| format!("{}: {e}", exe.display()));
        }
    }
    std::process::Command::new("vlc")
        .args(&args)
        .spawn()
        .map(|_| ())
        .map_err(|_| format!("VLC not found: open {url} in it (service {program})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_reach_the_server_locally() {
        assert_eq!(
            http_url("0.0.0.0:8001".parse().unwrap()),
            "http://127.0.0.1:8001/"
        );
        assert_eq!(
            http_url("192.168.1.5:8001".parse().unwrap()),
            "http://192.168.1.5:8001/"
        );
    }
}
