//! The lean data path (`--data-path lean`) against the SDK path, on an in-test S3 server that
//! records every request: the same files, the same requests (method, path, range), the same
//! retries and the same error messages; and lean requests signed with SigV4.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

use assert_cmd::prelude::*;

const TOTAL: usize = 12 * 1024 * 1024;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn byte(i: usize) -> u8 {
    (i % 251) as u8
}

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    /// Without the query string (the SDK adds `x-id=GetObject`).
    path: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Answers one request per connection: `object` (TOTAL bytes, ranges honoured), `flaky` (503
/// SlowDown on its first GET, then like `object`), anything else 404 NoSuchKey.
fn answer(stream: TcpStream, log: &Log) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() || first.is_empty() {
        return;
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let path = target.split('?').next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let seen = Seen {
        method: method.clone(),
        path: path.clone(),
        headers,
    };
    let range = seen.header("range").map(str::to_string);
    let flaky_before = {
        let mut log = log.lock().unwrap();
        let before = log
            .iter()
            .filter(|s| s.method == "GET" && s.path == "/bucket/flaky")
            .count();
        log.push(seen);
        before
    };
    let common = "Connection: close\r\nLast-Modified: Mon, 02 Nov 2026 10:00:00 GMT\r\nETag: \"e\"";
    let known = path == "/bucket/object" || path == "/bucket/flaky";
    let mut out = if path == "/bucket/flaky" && method == "GET" && flaky_before == 0 {
        let body = "<Error><Code>SlowDown</Code><Message>slow down</Message></Error>";
        format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/xml\r\n\
             Content-Length: {}\r\n{common}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    } else if !known {
        if method == "HEAD" {
            format!("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n{common}\r\n\r\n").into_bytes()
        } else {
            let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>NoSuchKey</Code>\
                        <Message>The specified key does not exist.</Message></Error>";
            format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/xml\r\n\
                 Content-Length: {}\r\n{common}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes()
        }
    } else if method == "HEAD" {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\n{common}\r\n\r\n").into_bytes()
    } else if let Some(r) = range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
        let (a, b) = r.split_once('-').unwrap();
        let a: usize = a.parse().unwrap();
        let b: usize = b.parse::<usize>().unwrap().min(TOTAL - 1);
        let mut out = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/{TOTAL}\r\n\
             Content-Length: {}\r\n{common}\r\n\r\n",
            b - a + 1
        )
        .into_bytes();
        out.extend((a..=b).map(byte));
        out
    } else {
        let mut out =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\n{common}\r\n\r\n").into_bytes();
        out.extend((0..TOTAL).map(byte));
        out
    };
    let mut stream = stream;
    let _ = stream.write_all(&out);
    out.clear();
}

fn start_server() -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let l = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let l = Arc::clone(&l);
            std::thread::spawn(move || answer(stream, &l));
        }
    });
    (port, log)
}

fn rs5cmd(port: u16, data_path: &str, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("rs5cmd")
        .unwrap()
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env_remove("AWS_SESSION_TOKEN")
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_PROFILE")
        .env_remove("RS5CMD_DATA_PATH")
        .args([
            "--endpoint-url",
            &format!("http://127.0.0.1:{port}"),
            "--data-path",
            data_path,
        ])
        .args(args)
        .output()
        .unwrap()
}

/// (method, path, range) of each request, sorted (parts arrive in any order).
fn shape(log: &Log) -> Vec<(String, String, Option<String>)> {
    let mut v: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .map(|s| (s.method.clone(), s.path.clone(), s.header("range").map(str::to_string)))
        .collect();
    v.sort();
    v
}

fn expected() -> Vec<u8> {
    (0..TOTAL).map(byte).collect()
}

#[test]
fn a_ranged_download_sends_the_sdks_requests_signed() {
    let mut shapes = Vec::new();
    let mut payload_hashes = Vec::new();
    for path in ["sdk", "lean"] {
        let (port, log) = start_server();
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("object");
        let out = rs5cmd(
            port,
            path,
            &["cp", "--part-size", "5", "s3://bucket/object", dst.to_str().unwrap()],
        );
        assert!(out.status.success(), "{path}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(std::fs::read(&dst).unwrap() == expected(), "{path}: wrong content");
        if path == "lean" {
            for s in log.lock().unwrap().iter() {
                let auth = s.header("authorization").unwrap_or("");
                assert!(
                    auth.starts_with("AWS4-HMAC-SHA256 Credential=test/")
                        && auth.contains("/us-east-1/s3/aws4_request"),
                    "{s:?}"
                );
                assert_eq!(s.header("x-amz-content-sha256"), Some(EMPTY_SHA256), "{s:?}");
                assert!(s.header("x-amz-date").is_some(), "{s:?}");
            }
        }
        let mut hashes: Vec<_> = log
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.header("x-amz-content-sha256").map(str::to_string))
            .collect();
        hashes.dedup();
        payload_hashes.push(hashes);
        shapes.push(shape(&log));
    }
    assert_eq!(shapes[0], shapes[1]);
    assert_eq!(payload_hashes[0], payload_hashes[1], "the payload hash the SDK signs");
    assert_eq!(shapes[1].len(), 3, "{:?}", shapes[1]);
}

/// stderr with the run's own temporary directory replaced, so two runs compare.
fn scrub(stderr: &[u8], dir: &std::path::Path) -> String {
    String::from_utf8_lossy(stderr).replace(dir.to_str().unwrap(), "<dir>")
}

#[test]
fn a_missing_key_fails_with_the_sdks_message() {
    let mut stderr = Vec::new();
    for path in ["sdk", "lean"] {
        let (port, log) = start_server();
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("x");
        let out = rs5cmd(port, path, &["cp", "s3://bucket/missing", dst.to_str().unwrap()]);
        assert!(!out.status.success(), "{path}");
        assert!(!dst.exists(), "{path}");
        stderr.push(scrub(&out.stderr, dir.path()));
        assert_eq!(log.lock().unwrap().len(), 1, "{path}: not retried");
    }
    assert!(stderr[1].contains("NoSuchKey"), "{}", stderr[1]);
    assert_eq!(stderr[0], stderr[1]);
}

#[test]
fn a_throttled_request_is_retried_as_the_sdk_retries_it() {
    for path in ["sdk", "lean"] {
        let (port, log) = start_server();
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("flaky");
        let out = rs5cmd(
            port,
            path,
            &["cp", "--part-size", "16", "s3://bucket/flaky", dst.to_str().unwrap()],
        );
        assert!(out.status.success(), "{path}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(std::fs::read(&dst).unwrap() == expected(), "{path}: wrong content");
        let gets = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.method == "GET")
            .count();
        assert_eq!(gets, 2, "{path}: one 503, one retry");
    }
}

/// `sync` of one object asks HeadObject first (`stat`), then downloads it; a missing one fails
/// the same way on both paths.
#[test]
fn head_object_matches_the_sdk() {
    let mut runs = Vec::new();
    for path in ["sdk", "lean"] {
        let (port, log) = start_server();
        let dir = tempfile::tempdir().unwrap();
        // Not empty: on this base a sync into an empty local directory fails (#4).
        std::fs::write(dir.path().join("keep"), b"").unwrap();
        let ok = rs5cmd(
            port,
            path,
            &["sync", "s3://bucket/object", &format!("{}/", dir.path().display())],
        );
        assert!(ok.status.success(), "{path}: {}", String::from_utf8_lossy(&ok.stderr));
        assert!(std::fs::read(dir.path().join("object")).unwrap() == expected());
        assert!(log.lock().unwrap().iter().any(|s| s.method == "HEAD"), "{path}");
        let missing = rs5cmd(
            port,
            path,
            &["sync", "s3://bucket/gone", &format!("{}/", dir.path().display())],
        );
        assert!(!missing.status.success(), "{path}");
        runs.push((shape(&log), scrub(&missing.stderr, dir.path())));
    }
    assert_eq!(runs[0], runs[1]);
}
