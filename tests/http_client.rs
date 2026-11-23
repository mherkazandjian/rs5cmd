//! rs5cmd's HTTP client (src/storage/s3.rs `build_http_client`), against in-test servers.
//!
//! - It speaks HTTP/1.1 only, even to a TLS server that offers HTTP/2: over HTTP/2 every part of
//!   a download would share one connection (#14).
//! - Without flags it trusts the platform's roots, `SSL_CERT_FILE` included, as the SDK's client
//!   did, and refuses a server they do not vouch for; with `--ca-certs-file`, the given bundle.
//! - A connection dropped before the response is retried, with or without `--no-verify-ssl`.
//! - Key exchange is classic (X25519) even with a server that prefers post-quantum, unless
//!   `--tls-post-quantum` is given; that flag prints one notice on stderr, none with `--json`.
//!
//! tests/data/tls holds a test CA (`ca.pem`) and a server certificate for 127.0.0.1 and
//! localhost signed by it (`server.pem`, key `server.key`), valid until 2126. The CA key was
//! not kept: to replace them, make a new CA and server certificate the same way.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use assert_cmd::prelude::*;

/// Three 5 MiB parts, the last one short.
const TOTAL: usize = 12 << 20;

fn byte(i: usize) -> u8 {
    (i % 251) as u8
}

fn tls_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/tls")
}

/// Answers one HTTP/1.1 request (HEAD, GET, ranged GET of a TOTAL-byte object) and closes.
fn answer<S: Read + Write>(stream: S) {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return;
    }
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end().to_ascii_lowercase();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("range:") {
            let (a, b) = v.trim().trim_start_matches("bytes=").split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            let b = b.parse::<usize>().map_or(TOTAL - 1, |b| b.min(TOTAL - 1));
            range = Some((a, b));
        }
    }
    let head = "Connection: close\r\nLast-Modified: Mon, 02 Nov 2026 10:00:00 GMT\r\nETag: \"e\"";
    let (status, extra, (a, b)) = match range {
        Some((a, b)) => {
            let extra = format!("Content-Range: bytes {a}-{b}/{TOTAL}\r\n");
            ("206 Partial Content", extra, (a, b))
        }
        None => ("200 OK", String::new(), (0, TOTAL - 1)),
    };
    let mut out = format!(
        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\n{head}\r\n\r\n",
        b + 1 - a
    )
    .into_bytes();
    if !request.starts_with("HEAD ") {
        out.extend((a..=b).map(byte));
    }
    let stream = reader.get_mut();
    let _ = stream.write_all(&out);
    let _ = stream.flush();
}

/// What each connection negotiated: the protocol (`None`: no ALPN) and the key exchange group.
type Negotiated = Arc<Mutex<Vec<(Option<Vec<u8>>, rustls::NamedGroup)>>>;

/// An HTTPS server offering HTTP/2 and HTTP/1.1, as an HTTP/2-capable front end does, and
/// preferring post-quantum key exchange. Returns its port and what each connection negotiated.
fn start_tls_server() -> (u16, Negotiated) {
    let open = |name: &str| BufReader::new(std::fs::File::open(tls_dir().join(name)).unwrap());
    let certs = rustls_pemfile::certs(&mut open("server.pem"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls_pemfile::private_key(&mut open("server.key")).unwrap().unwrap();
    let provider = Arc::new(rs5cmd::tls::provider_for(true));
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let config = Arc::new(config);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let negotiated: Negotiated = Arc::new(Mutex::new(Vec::new()));
    let seen = negotiated.clone();
    std::thread::spawn(move || {
        for mut tcp in listener.incoming().flatten() {
            let (config, seen) = (config.clone(), seen.clone());
            std::thread::spawn(move || {
                let mut conn = rustls::ServerConnection::new(config).unwrap();
                while conn.is_handshaking() {
                    if conn.complete_io(&mut tcp).is_err() {
                        return;
                    }
                }
                let group = conn.negotiated_key_exchange_group().unwrap().name();
                seen.lock().unwrap().push((conn.alpn_protocol().map(<[u8]>::to_vec), group));
                answer(rustls::StreamOwned::new(conn, tcp));
            });
        }
    });
    (port, negotiated)
}

/// A plain HTTP server that drops its first connection after reading the request.
fn start_dropping_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let count = connections.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                drop(stream);
                continue;
            }
            std::thread::spawn(move || answer::<TcpStream>(stream));
        }
    });
    (port, connections)
}

fn rs5cmd(endpoint: &str) -> Command {
    let mut cmd = Command::cargo_bin("rs5cmd").unwrap();
    cmd.env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_PROFILE")
        .env_remove("SSL_CERT_FILE")
        .env_remove("SSL_CERT_DIR")
        .args(["--endpoint-url", endpoint]);
    cmd
}

/// Downloads the object over TLS and checks it; returns the key exchange group of every
/// connection and the command's stderr.
fn download_over_tls(trust: impl FnOnce(&mut Command)) -> (Vec<rustls::NamedGroup>, String) {
    let (port, negotiated) = start_tls_server();
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("object");
    let mut cmd = rs5cmd(&format!("https://127.0.0.1:{port}"));
    trust(&mut cmd);
    let out = cmd
        .args(["cp", "--part-size", "5", "s3://bucket/object", dst.to_str().unwrap()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{stderr}");
    let got = std::fs::read(&dst).unwrap();
    assert_eq!(got.len(), TOTAL);
    assert!(got.iter().enumerate().all(|(i, b)| *b == byte(i)), "wrong bytes");
    let negotiated = negotiated.lock().unwrap();
    assert!(!negotiated.is_empty());
    assert!(
        negotiated.iter().all(|(p, _)| p.as_deref() != Some(b"h2")),
        "negotiated HTTP/2: {negotiated:?}"
    );
    (negotiated.iter().map(|(_, g)| *g).collect(), stderr)
}

fn assert_all(groups: &[rustls::NamedGroup], want: rustls::NamedGroup) {
    assert!(groups.iter().all(|g| *g == want), "want {want:?}, negotiated {groups:?}");
}

#[test]
fn platform_roots_and_no_http2() {
    let (groups, stderr) = download_over_tls(|cmd| {
        cmd.env("SSL_CERT_FILE", tls_dir().join("ca.pem"));
    });
    assert_all(&groups, rustls::NamedGroup::X25519);
    assert!(!stderr.contains("post-quantum"), "{stderr}");
}

#[test]
fn ca_certs_file_and_no_http2() {
    let (groups, _) = download_over_tls(|cmd| {
        cmd.arg("--ca-certs-file").arg(tls_dir().join("ca.pem"));
    });
    assert_all(&groups, rustls::NamedGroup::X25519);
}

#[test]
fn tls_post_quantum_negotiates_x25519mlkem768_and_says_so_once() {
    let (groups, stderr) = download_over_tls(|cmd| {
        cmd.env("SSL_CERT_FILE", tls_dir().join("ca.pem")).arg("--tls-post-quantum");
    });
    assert_all(&groups, rustls::NamedGroup::X25519MLKEM768);
    assert_eq!(stderr.matches(rs5cmd::tls::POST_QUANTUM_NOTICE).count(), 1, "{stderr}");

    let (groups, stderr) = download_over_tls(|cmd| {
        cmd.env("SSL_CERT_FILE", tls_dir().join("ca.pem"))
            .args(["--json", "--tls-post-quantum"]);
    });
    assert_all(&groups, rustls::NamedGroup::X25519MLKEM768);
    assert!(!stderr.contains("post-quantum"), "{stderr}");
}

/// The trust store is read when the first TLS connection needs it, but still checked: a server
/// whose certificate does not chain to a trusted root is refused.
#[test]
fn untrusted_server_is_refused() {
    let (port, negotiated) = start_tls_server();
    let dir = tempfile::tempdir().unwrap();
    rs5cmd(&format!("https://127.0.0.1:{port}"))
        // The server's own certificate is not a CA, so nothing it presents chains to a root.
        .env("SSL_CERT_FILE", tls_dir().join("server.pem"))
        .args(["--retry-count", "0", "cp", "s3://bucket/object"])
        .arg(dir.path().join("object"))
        .assert()
        .failure();
    assert!(negotiated.lock().unwrap().is_empty(), "a handshake completed");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

fn cat_after_a_dropped_connection(flags: &[&str]) {
    let (port, connections) = start_dropping_server();
    let out = rs5cmd(&format!("http://127.0.0.1:{port}"))
        .args(flags)
        .args(["cat", "s3://bucket/object"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout.len(), TOTAL);
    assert!(out.stdout.iter().enumerate().all(|(i, b)| *b == byte(i)), "wrong bytes");
    assert!(connections.load(Ordering::SeqCst) >= 2);
}

#[test]
fn dropped_connection_is_retried() {
    cat_after_a_dropped_connection(&[]);
}

#[test]
fn dropped_connection_is_retried_without_verification() {
    cat_after_a_dropped_connection(&["--no-verify-ssl"]);
}
