//! Downloads are written to a temporary file and renamed only when complete (F03). A part that
//! fails leaves no file behind and an existing destination as it was. An in-test HTTP server
//! serves the first 5 MiB part of a 6 MiB object and denies the second part.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;

use assert_cmd::prelude::*;

const TOTAL: usize = 6 << 20;
const PART: usize = 5 << 20;

fn answer(stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return;
    }
    let mut range = String::new();
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
            range = v.trim().to_string();
        }
    }
    let head = "Connection: close\r\nLast-Modified: Mon, 02 Nov 2026 10:00:00 GMT\r\nETag: \"e\"";
    let mut out = if request.starts_with("HEAD ") {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\n{head}\r\n\r\n").into_bytes()
    } else if range.starts_with("bytes=0-") {
        let mut r = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{TOTAL}\r\n\
             Content-Length: {PART}\r\n{head}\r\n\r\n",
            PART - 1
        )
        .into_bytes();
        r.resize(r.len() + PART, b'a');
        r
    } else {
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                    <Error><Code>AccessDenied</Code><Message>denied</Message></Error>";
        format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/xml\r\n\
             Content-Length: {}\r\n{head}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    let mut stream = stream;
    let _ = stream.write_all(&out);
    out.clear();
}

fn start_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer(stream));
        }
    });
    port
}

fn download(port: u16, dst: &std::path::Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("rs5cmd")
        .unwrap()
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_PROFILE")
        .args([
            "--endpoint-url",
            &format!("http://127.0.0.1:{port}"),
            "cp",
            "--part-size",
            "5",
            "s3://bucket/object",
            dst.to_str().unwrap(),
        ])
        .assert()
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn failed_part_leaves_no_file() {
    let port = start_server();
    let dir = tempfile::tempdir().unwrap();
    download(port, &dir.path().join("object")).failure();
    assert!(names(dir.path()).is_empty(), "{:?}", names(dir.path()));
}

#[test]
fn failed_part_keeps_existing_destination() {
    let port = start_server();
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("object");
    std::fs::write(&dst, b"old").unwrap();
    download(port, &dst).failure();
    assert_eq!(std::fs::read(&dst).unwrap(), b"old");
    assert_eq!(names(dir.path()), ["object"]);
}
