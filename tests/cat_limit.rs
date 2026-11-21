//! `cat` of a remote object goes through --limit-download like a download does (F05). An in-test
//! HTTP server serves a 1 MiB object; at 512 KB/s, cat cannot finish in much under 2 s.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::time::{Duration, Instant};

use assert_cmd::prelude::*;

const SIZE: usize = 1 << 20;

fn answer(stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).unwrap_or(0) == 0 {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line.trim_end().is_empty() {
            break;
        }
    }
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\nConnection: close\r\n\
         Last-Modified: Mon, 02 Nov 2026 10:00:00 GMT\r\nETag: \"e\"\r\n\r\n"
    );
    let mut out = head.into_bytes();
    if !request.starts_with("HEAD ") {
        out.extend((0..SIZE).map(|i| (i % 251) as u8));
    }
    let mut stream = stream;
    let _ = stream.write_all(&out);
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

#[test]
fn remote_cat_is_paced_by_limit_download() {
    let port = start_server();
    let start = Instant::now();
    let out = Command::cargo_bin("rs5cmd")
        .unwrap()
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_PROFILE")
        .args(["--endpoint-url", &format!("http://127.0.0.1:{port}")])
        .args(["--limit-download", "512KB", "cat", "s3://bucket/object"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let want: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
    assert!(out.stdout == want, "got {} bytes, want {SIZE}", out.stdout.len());
    assert!(elapsed >= Duration::from_millis(1500), "1 MiB at 512 KB/s took {elapsed:?}");
}
