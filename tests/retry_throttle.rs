//! `429 Too Many Requests` is retried with backoff, as `503 SlowDown` is (#32). An in-test HTTP
//! server answers the first requests of each kind with `429 TooManyRequests` (the code MinIO and
//! other S3-compatible servers send), then succeeds.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use assert_cmd::prelude::*;

#[derive(Default)]
struct Seen {
    puts: AtomicUsize,
    gets: AtomicUsize,
}

const LIST: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
    <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bucket</Name>\
    <Prefix></Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>\
    <Contents><Key>k.txt</Key><LastModified>2026-11-02T10:00:00.000Z</LastModified>\
    <ETag>\"e\"</ETag><Size>3</Size><StorageClass>STANDARD</StorageClass></Contents>\
    </ListBucketResult>";

const THROTTLED: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error>\
    <Code>TooManyRequests</Code><Message>Please reduce your request rate.</Message></Error>";

fn answer(stream: TcpStream, seen: &Seen, fails: usize) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return;
    }
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end().to_ascii_lowercase();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let count = if request.starts_with("PUT ") { &seen.puts } else { &seen.gets };
    let n = count.fetch_add(1, Ordering::SeqCst) + 1;
    let (status, body) = if n <= fails {
        ("429 Too Many Requests", THROTTLED)
    } else if request.starts_with("PUT ") {
        ("200 OK", "")
    } else {
        ("200 OK", LIST)
    };
    let out = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nETag: \"e\"\r\nContent-Type: application/xml\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = stream;
    let _ = stream.write_all(out.as_bytes());
}

/// A server that throttles the first `fails` PUTs and the first `fails` GETs.
fn start_server(fails: usize) -> (u16, Arc<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Seen::default());
    let s = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let s = Arc::clone(&s);
            std::thread::spawn(move || answer(stream, &s, fails));
        }
    });
    (port, seen)
}

fn rs5cmd(port: u16, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("rs5cmd").unwrap();
    cmd.env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_PROFILE")
        .env_remove("RS5CMD_DATA_PATH")
        .args(["--endpoint-url", &format!("http://127.0.0.1:{port}")])
        .args(args);
    cmd
}

#[test]
fn an_upload_and_a_listing_are_retried_after_429() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, b"abc").unwrap();
    let (port, seen) = start_server(2);
    rs5cmd(port, &["cp", f.to_str().unwrap(), "s3://bucket/k.txt"])
        .assert()
        .success();
    assert_eq!(seen.puts.load(Ordering::SeqCst), 3, "two 429s, then the upload");
    rs5cmd(port, &["ls", "s3://bucket/*"])
        .assert()
        .success()
        .stdout(predicates::str::contains("k.txt"));
    assert_eq!(seen.gets.load(Ordering::SeqCst), 3, "two 429s, then the listing");
}

#[test]
fn retry_count_still_caps_the_attempts_after_429() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, b"abc").unwrap();
    let (port, seen) = start_server(usize::MAX);
    rs5cmd(port, &["--retry-count", "1", "cp", f.to_str().unwrap(), "s3://bucket/k.txt"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("TooManyRequests"));
    assert_eq!(seen.puts.load(Ordering::SeqCst), 2, "the attempt and one retry");
}
