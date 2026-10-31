// made by CasualArclamp
//! HTTPS to a server whose certificate the broadcast carries.
//!
//! A UK carousel names the TLS certificates its applications' servers use in
//! `auth.tls.1`, `auth.tls.2`… (the interaction channel of ETSI ES 202 184):
//! the receiver trusts those, not the web's certificate authorities.
//! Freeview's (Synapse's) is a self-signed `*.synapsetv.com` certificate
//! marked as a CA, which a web client rejects ("CA used as end entity"), so
//! these servers are reached here: the connection is taken only if the
//! server shows exactly one of the carousel's certificates, and the
//! handshake's signature is checked with its key, as usual.
//!
//! The HTTP is HTTP/1.1 with `Connection: close`: one request a connection,
//! the answer read to its end (a Content-Length or chunked body).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// The certificates in an `auth.tls.N` file: a 16-bit count, then each
/// certificate as a 24-bit length and its DER.
pub fn parse_auth_tls(b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Some(n) = b.get(..2).map(|c| u16::from_be_bytes([c[0], c[1]])) else {
        return out;
    };
    let mut at = 2;
    for _ in 0..n {
        let Some(l) = b.get(at..at + 3) else { break };
        let len = (l[0] as usize) << 16 | (l[1] as usize) << 8 | l[2] as usize;
        let Some(der) = b.get(at + 3..at + 3 + len) else {
            break;
        };
        out.push(der.to_vec());
        at += 3 + len;
    }
    out
}

/// Takes a server only if it shows one of `certs` (byte for byte).
#[derive(Debug)]
struct Pinned {
    certs: Vec<Vec<u8>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self.certs.iter().any(|c| c[..] == end_entity[..]) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        match verify_rsa(message, cert, dss, true) {
            Some(r) => r,
            None => rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms),
        }
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // TLS 1.3 signs with RSA-PSS only (RFC 8446 §4.2.3).
        match verify_rsa(message, cert, dss, false) {
            Some(r) => r,
            None => rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms),
        }
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// One DER element at the start of `b`: (tag, contents, whole length).
/// Short and long lengths (X.690 §8.1.3).
fn tlv(b: &[u8]) -> Option<(u8, &[u8], usize)> {
    let tag = *b.first()?;
    let first = *b.get(1)? as usize;
    let (len, head) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let len = b
            .get(2..2 + n)?
            .iter()
            .fold(0usize, |a, &x| a << 8 | x as usize);
        (len, 2 + n)
    };
    Some((tag, b.get(head..head + len)?, head + len))
}

/// A certificate's SubjectPublicKeyInfo, whole (RFC 5280 §4.1: in
/// tbsCertificate after the optional [0] version, the serial number, the
/// signature algorithm, the issuer, the validity and the subject).
fn spki_of(cert: &[u8]) -> Option<&[u8]> {
    let (_, cert_body, _) = tlv(cert)?;
    let (_, mut rest, _) = tlv(cert_body)?;
    if rest.first() == Some(&0xa0) {
        rest = &rest[tlv(rest)?.2..];
    }
    for _ in 0..5 {
        rest = &rest[tlv(rest)?.2..];
    }
    let (tag, _, whole) = tlv(rest)?;
    (tag == 0x30).then(|| &rest[..whole])
}

/// A handshake signature made with an RSA key, checked with RustCrypto's
/// `rsa` (ring refuses keys under 2048 bits; Freeview's server has a
/// 1024-bit one). None: not an RSA scheme (rustls's own check then).
fn verify_rsa(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
    pkcs1_allowed: bool,
) -> Option<Result<HandshakeSignatureValid, rustls::Error>> {
    use SignatureScheme as S;
    use rsa::pkcs8::DecodePublicKey;
    use rsa::{Pkcs1v15Sign, Pss, RsaPublicKey};
    use sha2::{Digest, Sha256, Sha384, Sha512};

    let scheme = dss.scheme;
    let is_pkcs1 = matches!(
        scheme,
        S::RSA_PKCS1_SHA256 | S::RSA_PKCS1_SHA384 | S::RSA_PKCS1_SHA512
    );
    let is_pss = matches!(
        scheme,
        S::RSA_PSS_SHA256 | S::RSA_PSS_SHA384 | S::RSA_PSS_SHA512
    );
    if !is_pkcs1 && !is_pss {
        return None;
    }
    let bad = || rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature);
    if is_pkcs1 && !pkcs1_allowed {
        return Some(Err(bad()));
    }
    let Some(key) = spki_of(cert).and_then(|s| RsaPublicKey::from_public_key_der(s).ok()) else {
        return Some(Err(bad()));
    };
    let sig = dss.signature();
    let ok = match scheme {
        S::RSA_PKCS1_SHA256 => {
            key.verify(Pkcs1v15Sign::new::<Sha256>(), &Sha256::digest(message), sig)
        }
        S::RSA_PKCS1_SHA384 => {
            key.verify(Pkcs1v15Sign::new::<Sha384>(), &Sha384::digest(message), sig)
        }
        S::RSA_PKCS1_SHA512 => {
            key.verify(Pkcs1v15Sign::new::<Sha512>(), &Sha512::digest(message), sig)
        }
        S::RSA_PSS_SHA256 => key.verify(Pss::new::<Sha256>(), &Sha256::digest(message), sig),
        S::RSA_PSS_SHA384 => key.verify(Pss::new::<Sha384>(), &Sha384::digest(message), sig),
        _ => key.verify(Pss::new::<Sha512>(), &Sha512::digest(message), sig),
    };
    Some(
        ok.map(|_| HandshakeSignatureValid::assertion())
            .map_err(|_| bad()),
    )
}

/// A GET (`form` None) or a form POST to an https URL, the server taken
/// only if it shows one of `certs`: (status, body).
pub fn request(
    url: &str,
    form: Option<&[u8]>,
    user_agent: &str,
    certs: &[Vec<u8>],
    max_body: usize,
) -> Result<(u16, Vec<u8>), String> {
    let rest = url.strip_prefix("https://").ok_or("not https")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| "bad port")?),
        None => (authority, 443),
    };

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Pinned {
        certs: certs.to_vec(),
        algorithms: provider.signature_verification_algorithms,
    };
    let config =
        rustls::ClientConfig::builder_with_provider(provider.clone() as Arc<CryptoProvider>)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            // "dangerous" is rustls's name for any verifier of one's own; this
            // one is stricter than the usual (one exact certificate).
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
    let name = ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;

    let tcp = TcpStream::connect((host, port)).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    // `StreamOwned` runs the TLS over the socket as one Read + Write.
    let mut tls = rustls::StreamOwned::new(conn, tcp);

    let mut req = format!(
        "{} {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: {user_agent}\r\n\
         Accept: */*\r\nConnection: close\r\n",
        if form.is_some() { "POST" } else { "GET" }
    );
    if let Some(f) = form {
        req.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            f.len()
        ));
    }
    req.push_str("\r\n");
    tls.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    if let Some(f) = form {
        tls.write_all(f).map_err(|e| e.to_string())?;
    }
    tls.flush().map_err(|e| e.to_string())?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > max_body + 65536 {
                    return Err("answer too long".into());
                }
            }
            // Servers often close without TLS's close_notify: the answer
            // is complete if its length says so (checked below).
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    parse_response(&raw)
}

/// An HTTP/1.1 answer: (status, body).
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no header")?;
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("no status")?;
    let mut length = None;
    let mut chunked = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "content-length" {
            length = v.parse::<usize>().ok();
        } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }
    let body = &raw[head_end + 4..];
    let body = if chunked {
        dechunk(body)?
    } else if let Some(n) = length {
        if body.len() < n {
            return Err("answer cut short".into());
        }
        body[..n].to_vec()
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// A chunked body (RFC 9112 §7.1) put back together.
fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").ok_or("bad chunk")?;
        let size_text = String::from_utf8_lossy(&b[..eol]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "bad chunk size")?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        let chunk = b.get(..size).ok_or("chunk cut short")?;
        out.extend_from_slice(chunk);
        b = b.get(size + 2..).ok_or("chunk cut short")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_tls_layout() {
        let mut f = vec![0, 2];
        for der in [&b"\x30\x03abc"[..], &b"\x30\x01z"[..]] {
            f.extend_from_slice(&[0, 0, der.len() as u8]);
            f.extend_from_slice(der);
        }
        assert_eq!(
            parse_auth_tls(&f),
            vec![b"\x30\x03abc".to_vec(), b"\x30\x01z".to_vec()]
        );
        assert!(parse_auth_tls(&[0]).is_empty());
    }

    #[test]
    fn responses() {
        let r = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_response(r).unwrap(), (200, b"hello".to_vec()));
        let r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\n\r\n";
        assert_eq!(parse_response(r).unwrap(), (200, b"hello".to_vec()));
        let r = b"HTTP/1.1 404 Not Found\r\n\r\nnope";
        assert_eq!(parse_response(r).unwrap(), (404, b"nope".to_vec()));
    }
}
