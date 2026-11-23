//! What `cp` asks of the new object (#34): `--cache-control`, `--expires`, `--content-encoding`,
//! `--content-disposition`, `--metadata`, `--sse` and `--sse-kms-key-id` reach a single upload, a
//! multipart upload and a server-side copy as their S3 headers, and `--metadata-directive` (or a
//! flag that sets the copy's metadata) decides the copy's directive. An in-test server records
//! every request's headers.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

use assert_cmd::prelude::*;

/// One recorded request: method, path with query, lower-case headers.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn answer(stream: TcpStream, log: &Log) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            return;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let seen = Seen { method, target, headers };
    if seen.header("expect") == Some("100-continue") {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let len: usize = seen.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let xml = if seen.method == "POST" && seen.target.contains("uploads") {
        "<InitiateMultipartUploadResult><Bucket>bucket</Bucket><Key>k</Key>\
         <UploadId>u1</UploadId></InitiateMultipartUploadResult>"
    } else if seen.method == "POST" {
        "<CompleteMultipartUploadResult><Bucket>bucket</Bucket><Key>k</Key>\
         <ETag>\"e\"</ETag></CompleteMultipartUploadResult>"
    } else if seen.header("x-amz-copy-source").is_some() {
        "<CopyObjectResult><ETag>\"e\"</ETag>\
         <LastModified>2026-11-02T10:00:00.000Z</LastModified></CopyObjectResult>"
    } else {
        ""
    };
    log.lock().unwrap().push(seen);
    let out = format!(
        "HTTP/1.1 200 OK\r\nConnection: close\r\nETag: \"e\"\r\nContent-Type: application/xml\r\n\
         Content-Length: {}\r\n\r\n{xml}",
        xml.len()
    );
    let _ = stream.write_all(out.as_bytes());
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

const PROPERTIES: &[&str] = &[
    "--cache-control",
    "public, max-age=345600",
    "--expires",
    "2030-01-02T03:04:05Z",
    "--content-encoding",
    "gzip",
    "--content-disposition",
    "attachment; filename=\"f.txt\"",
    "--metadata",
    "a=b",
    "--metadata",
    "c=d=e",
    "--sse",
    "aws:kms",
    "--sse-kms-key-id",
    "key1",
];

fn assert_properties(r: &Seen) {
    let want = [
        ("cache-control", "public, max-age=345600"),
        ("expires", "Wed, 02 Jan 2030 03:04:05 GMT"),
        ("content-encoding", "gzip"),
        ("content-disposition", "attachment; filename=\"f.txt\""),
        ("x-amz-meta-a", "b"),
        ("x-amz-meta-c", "d=e"),
        ("x-amz-server-side-encryption", "aws:kms"),
        ("x-amz-server-side-encryption-aws-kms-key-id", "key1"),
    ];
    for (name, value) in want {
        assert_eq!(r.header(name), Some(value), "{name} in {} {}", r.method, r.target);
    }
}

fn args<'a>(head: &[&'a str], tail: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["cp"];
    v.extend_from_slice(head);
    v.extend_from_slice(PROPERTIES);
    v.extend_from_slice(tail);
    v
}

#[test]
fn a_single_upload_sends_the_object_properties() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, b"abc").unwrap();
    let (port, log) = start_server();
    rs5cmd(port, &args(&[], &[f.to_str().unwrap(), "s3://bucket/k.txt"])).assert().success();
    let log = log.lock().unwrap();
    let put = log.iter().find(|r| r.method == "PUT").expect("a PUT");
    assert_properties(put);
}

#[test]
fn a_multipart_upload_sends_them_when_it_starts() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.bin");
    std::fs::write(&f, vec![b'x'; 6 << 20]).unwrap();
    let (port, log) = start_server();
    let tail = [f.to_str().unwrap(), "s3://bucket/k.bin"];
    rs5cmd(port, &args(&["--part-size", "5"], &tail)).assert().success();
    let log = log.lock().unwrap();
    let create = log
        .iter()
        .find(|r| r.method == "POST" && r.target.contains("uploads"))
        .expect("a CreateMultipartUpload");
    assert_properties(create);
    assert_eq!(log.iter().filter(|r| r.target.contains("partNumber")).count(), 2);
}

#[test]
fn a_copy_sends_them_and_replaces_the_metadata() {
    let (port, log) = start_server();
    rs5cmd(port, &args(&[], &["s3://bucket/src.txt", "s3://bucket/dst.txt"])).assert().success();
    let log = log.lock().unwrap();
    let copy = log.iter().find(|r| r.header("x-amz-copy-source").is_some()).expect("a copy");
    assert_properties(copy);
    assert_eq!(copy.header("x-amz-metadata-directive"), Some("REPLACE"));
}

#[test]
fn the_copy_directive_is_the_flags_choice_and_otherwise_left_to_s3() {
    let directive = |extra: &[&str]| {
        let (port, log) = start_server();
        let mut a = vec!["cp"];
        a.extend_from_slice(extra);
        a.extend_from_slice(&["s3://bucket/src.txt", "s3://bucket/dst.txt"]);
        rs5cmd(port, &a).assert().success();
        let log = log.lock().unwrap();
        let copy = log.iter().find(|r| r.header("x-amz-copy-source").is_some()).unwrap();
        copy.header("x-amz-metadata-directive").map(str::to_string)
    };
    // No flag: S3's default (COPY), the source's metadata is kept.
    assert_eq!(directive(&[]), None);
    // Storage class, ACL and encryption apply under COPY too.
    assert_eq!(directive(&["--storage-class", "STANDARD_IA", "--sse", "AES256"]), None);
    assert_eq!(directive(&["--content-type", "text/plain"]).as_deref(), Some("REPLACE"));
    let copy = ["--metadata-directive", "copy", "--metadata", "a=b"];
    assert_eq!(directive(&copy).as_deref(), Some("COPY"));
    assert_eq!(directive(&["--metadata-directive", "REPLACE"]).as_deref(), Some("REPLACE"));
}

#[test]
fn a_directive_is_refused_for_an_upload_or_a_download() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, b"abc").unwrap();
    let (port, log) = start_server();
    let up = ["cp", "--metadata-directive", "REPLACE", f.to_str().unwrap(), "s3://bucket/k.txt"];
    rs5cmd(port, &up)
        .assert()
        .failure()
        .stderr(predicates::str::contains("metadata directive is not supported for upload"));
    let dl = tmp.path().join("dl.txt");
    let down = ["cp", "--metadata-directive", "COPY", "s3://bucket/k.txt", dl.to_str().unwrap()];
    rs5cmd(port, &down)
        .assert()
        .failure()
        .stderr(predicates::str::contains("metadata directive is not supported for download"));
    assert!(log.lock().unwrap().is_empty(), "nothing was sent");
}

#[test]
fn a_bad_expires_or_metadata_is_refused_before_anything_is_sent() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, b"abc").unwrap();
    let (port, log) = start_server();
    let src = f.to_str().unwrap();
    for (flags, message) in [
        (["--expires", "tomorrow"], "not an RFC 3339 time"),
        (["--metadata", "novalue"], "use key=value"),
    ] {
        let mut a = vec!["cp"];
        a.extend_from_slice(&flags);
        a.extend_from_slice(&[src, "s3://bucket/k.txt"]);
        rs5cmd(port, &a).assert().failure().stderr(predicates::str::contains(message));
    }
    let dup = ["cp", "--metadata", "a=1", "--metadata", "a=2", src, "s3://bucket/k.txt"];
    rs5cmd(port, &dup)
        .assert()
        .failure()
        .stderr(predicates::str::contains("already defined"));
    assert!(log.lock().unwrap().is_empty(), "nothing was sent");
}
