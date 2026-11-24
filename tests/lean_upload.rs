//! Uploads on the lean data path (`--data-path lean`) against the SDK path, on an in-test S3
//! server that records every request and stores what it receives: the same requests (method,
//! path, query, the object's headers, the signed payload), the same stored bytes, the same error
//! messages; and the lean path's CRC32C, in a header for in-memory bodies and, with
//! `--checksum-trailers`, as an aws-chunked trailer for streamed ones.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use assert_cmd::prelude::*;

const MIB: usize = 1024 * 1024;

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    /// Sorted, without the SDK's `x-id`.
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    /// The payload, aws-chunked framing removed.
    body: Vec<u8>,
    /// The aws-chunked trailer's CRC32C, when the body had one.
    trailer_crc: Option<String>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn has(&self, param: &str) -> bool {
        self.query.iter().any(|(k, _)| k == param)
    }

    /// What both paths must send alike.
    fn shape(&self) -> (String, String, Vec<(String, String)>) {
        (self.method.clone(), self.path.clone(), self.query.clone())
    }
}

#[derive(Default)]
struct Store {
    log: Vec<Seen>,
    objects: HashMap<String, Vec<u8>>,
    parts: HashMap<u32, Vec<u8>>,
}

type Shared = Arc<Mutex<Store>>;

fn crc32c(data: &[u8]) -> String {
    use base64::Engine;
    let crc = crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, data) as u32;
    base64::engine::general_purpose::STANDARD.encode(crc.to_be_bytes())
}

/// The payload and trailer CRC32C of an aws-chunked body.
fn decode_aws_chunked(raw: &[u8]) -> (Vec<u8>, Option<String>) {
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let end = rest.windows(2).position(|w| w == b"\r\n").expect("chunk size line");
        let line = std::str::from_utf8(&rest[..end]).unwrap();
        let size = usize::from_str_radix(line.split(';').next().unwrap(), 16).unwrap();
        rest = &rest[end + 2..];
        if size == 0 {
            break;
        }
        // The SDK's 64 KiB chunks: servers cap the size (MinIO at 16 MiB).
        assert!(size <= 64 * 1024, "a chunk of {size} bytes");
        out.extend_from_slice(&rest[..size]);
        assert_eq!(&rest[size..size + 2], b"\r\n", "chunk end");
        rest = &rest[size + 2..];
    }
    let trailers = std::str::from_utf8(rest).unwrap();
    assert!(trailers.ends_with("\r\n\r\n"), "{trailers:?}");
    let crc = trailers
        .split("\r\n")
        .find_map(|l| l.strip_prefix("x-amz-checksum-crc32c:"))
        .map(|v| v.trim().to_string());
    (out, crc)
}

fn respond(mut stream: TcpStream, status: &str, headers: &[(&str, String)], body: &str) {
    let len = body.len();
    let mut out = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {len}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(body);
    let _ = stream.write_all(out.as_bytes());
}

/// One request per connection. `denied` refuses PutObject with 403 AccessDenied; uploads and
/// multipart uploads are stored; anything else is a missing key.
fn answer(stream: TcpStream, store: &Shared) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() || first.is_empty() {
        return;
    }
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let mut query: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (p.to_string(), String::new()),
        })
        .filter(|(k, _)| k != "x-id")
        .collect();
    query.sort();
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
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let len: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut raw = vec![0u8; len];
    if reader.read_exact(&mut raw).is_err() {
        return;
    }
    let chunked = header("content-encoding").is_some_and(|e| e.contains("aws-chunked"));
    let (body, trailer_crc) = if chunked {
        decode_aws_chunked(&raw)
    } else {
        (raw, None)
    };
    let seen = Seen {
        method: method.clone(),
        path: path.to_string(),
        query: query.clone(),
        headers: headers.clone(),
        body: body.clone(),
        trailer_crc: trailer_crc.clone(),
    };
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let mut st = store.lock().unwrap();
    st.log.push(seen);
    let checksummed = header("x-amz-checksum-crc32c").is_some() || trailer_crc.is_some();
    match method.as_str() {
        "PUT" if path == "/bucket/denied" => {
            drop(st);
            let body = "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>";
            respond(stream, "403 Forbidden", &[("Content-Type", "application/xml".into())], body);
        }
        "PUT" if q("partNumber").is_some() => {
            let n: u32 = q("partNumber").unwrap().parse().unwrap();
            let mut h = vec![("ETag", format!("\"p{n}\""))];
            if checksummed {
                h.push(("x-amz-checksum-crc32c", crc32c(&body)));
            }
            st.parts.insert(n, body);
            drop(st);
            respond(stream, "200 OK", &h, "");
        }
        "PUT" => {
            st.objects.insert(path.to_string(), body);
            drop(st);
            respond(stream, "200 OK", &[("ETag", "\"o\"".into())], "");
        }
        "POST" if q("uploads").is_some() => {
            drop(st);
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<InitiateMultipartUploadResult \
                 xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Bucket>bucket</Bucket>\
                 <Key>{}</Key><UploadId>up1</UploadId></InitiateMultipartUploadResult>",
                &path["/bucket/".len()..]
            );
            respond(stream, "200 OK", &[("Content-Type", "application/xml".into())], &body);
        }
        "POST" if q("uploadId").is_some() => {
            let mut numbers: Vec<u32> = st.parts.keys().copied().collect();
            numbers.sort();
            let object: Vec<u8> = numbers.iter().flat_map(|n| st.parts[n].clone()).collect();
            st.parts.clear();
            st.objects.insert(path.to_string(), object);
            drop(st);
            let body = "<CompleteMultipartUploadResult><ETag>\"m-3\"</ETag>\
                        </CompleteMultipartUploadResult>";
            respond(stream, "200 OK", &[("Content-Type", "application/xml".into())], body);
        }
        "DELETE" if q("uploadId").is_some() => {
            drop(st);
            respond(stream, "204 No Content", &[], "");
        }
        "HEAD" => {
            drop(st);
            respond(stream, "404 Not Found", &[], "");
        }
        _ => {
            drop(st);
            let body = "<Error><Code>NoSuchKey</Code><Message>no</Message></Error>";
            respond(stream, "404 Not Found", &[("Content-Type", "application/xml".into())], body);
        }
    }
}

fn start_server() -> (u16, Shared) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let store: Shared = Arc::default();
    let s = Arc::clone(&store);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let s = Arc::clone(&s);
            std::thread::spawn(move || answer(stream, &s));
        }
    });
    (port, store)
}

/// Runs rs5cmd against the server on `port`, with `--data-path` and `global` before `args`,
/// feeding `stdin` if given.
fn rs5cmd(
    port: u16,
    data_path: &str,
    global: &[&str],
    args: &[&str],
    stdin: Option<Vec<u8>>,
) -> std::process::Output {
    let mut cmd = Command::cargo_bin("rs5cmd").unwrap();
    cmd.env("AWS_ACCESS_KEY_ID", "test")
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
        .args(global)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        if let Some(data) = stdin {
            let _ = input.write_all(&data);
        }
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    out
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn ok(out: &std::process::Output, what: &str) {
    assert!(out.status.success(), "{what}: {}", String::from_utf8_lossy(&out.stderr));
}

/// The requests of a run, sorted (parts arrive in any order).
fn shapes(store: &Shared) -> Vec<(String, String, Vec<(String, String)>)> {
    let mut v: Vec<_> = store.lock().unwrap().log.iter().map(Seen::shape).collect();
    v.sort();
    v
}

fn requests(store: &Shared) -> Vec<Seen> {
    store.lock().unwrap().log.clone()
}

#[test]
fn a_small_upload_sends_the_sdks_request_with_a_crc32c_header() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("small");
    let body = data(1000);
    std::fs::write(&src, &body).unwrap();
    let mut puts = Vec::new();
    for path in ["sdk", "lean"] {
        let (port, store) = start_server();
        let args = [
            "cp",
            "--storage-class",
            "STANDARD_IA",
            "--content-type",
            "text/x-rs5cmd",
            src.to_str().unwrap(),
            "s3://bucket/small",
        ];
        ok(&rs5cmd(port, path, &[], &args, None), path);
        assert!(store.lock().unwrap().objects["/bucket/small"] == body, "{path}");
        let log = requests(&store);
        puts.push(log.into_iter().find(|s| s.method == "PUT").expect("a PUT"));
    }
    let (sdk, lean) = (&puts[0], &puts[1]);
    assert_eq!(sdk.shape(), lean.shape());
    // The signed payload too: the hash of the in-memory body on both.
    for h in ["content-type", "x-amz-storage-class", "content-length", "x-amz-content-sha256"] {
        assert_eq!(sdk.header(h), lean.header(h), "{h}");
    }
    assert_eq!(sdk.header("x-amz-checksum-crc32c"), None);
    assert_eq!(lean.header("x-amz-checksum-crc32c"), Some(crc32c(&body).as_str()));
}

#[test]
fn a_multipart_upload_sends_the_sdks_requests() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big");
    let body = data(12 * MIB);
    std::fs::write(&src, &body).unwrap();
    let mut runs = Vec::new();
    for path in ["sdk", "lean"] {
        let (port, store) = start_server();
        let args = ["cp", "--part-size", "5", src.to_str().unwrap(), "s3://bucket/big"];
        ok(&rs5cmd(port, path, &[], &args, None), path);
        assert!(store.lock().unwrap().objects["/bucket/big"] == body, "{path}");
        let log = requests(&store);
        let parts: Vec<_> = log.iter().filter(|s| s.has("partNumber")).cloned().collect();
        assert_eq!(parts.len(), 3, "{path}");
        for p in &parts {
            // Streamed parts: no checksum without --checksum-trailers, an unsigned payload.
            assert_eq!(p.header("x-amz-checksum-crc32c"), None, "{path}");
            assert_eq!(p.header("x-amz-content-sha256"), Some("UNSIGNED-PAYLOAD"), "{path}");
        }
        let create = log.iter().find(|s| s.has("uploads")).unwrap();
        assert_eq!(create.header("x-amz-checksum-algorithm"), None, "{path}");
        runs.push(shapes(&store));
    }
    assert_eq!(runs[0], runs[1]);
}

/// `cp`'s object properties (#34) go with a lean upload as with the SDK's: on a PutObject and on
/// a multipart upload's Create, `--expires` as an HTTP date.
#[test]
fn object_properties_go_with_lean_uploads_as_with_the_sdks() {
    let dir = tempfile::tempdir().unwrap();
    let props = [
        "--cache-control",
        "max-age=60",
        "--expires",
        "2027-01-02T03:04:05Z",
        "--content-encoding",
        "identity",
        "--content-disposition",
        "attachment",
        "--metadata",
        "k1=v1",
        "--sse",
        "AES256",
        "--acl",
        "private",
        "--part-size",
        "5",
    ];
    let expected = [
        ("cache-control", "max-age=60"),
        ("expires", "Sat, 02 Jan 2027 03:04:05 GMT"),
        ("content-encoding", "identity"),
        ("content-disposition", "attachment"),
        ("x-amz-meta-k1", "v1"),
        ("x-amz-server-side-encryption", "AES256"),
        ("x-amz-acl", "private"),
    ];
    for (name, len) in [("small", 1000), ("big", 12 * MIB)] {
        let src = dir.path().join(name);
        std::fs::write(&src, data(len)).unwrap();
        let mut sent = Vec::new();
        for path in ["sdk", "lean"] {
            let (port, store) = start_server();
            let dst = format!("s3://bucket/{name}");
            let args: Vec<&str> = ["cp"]
                .into_iter()
                .chain(props)
                .chain([src.to_str().unwrap(), dst.as_str()])
                .collect();
            ok(&rs5cmd(port, path, &[], &args, None), path);
            // The PutObject, or the multipart upload's Create.
            let first = requests(&store)
                .into_iter()
                .find(|s| s.method == "PUT" && !s.has("partNumber") || s.has("uploads"))
                .expect("an upload");
            sent.push(first);
        }
        let (sdk, lean) = (&sent[0], &sent[1]);
        assert_eq!(sdk.shape(), lean.shape(), "{name}");
        for (h, v) in expected {
            assert_eq!(sdk.header(h), Some(v), "{name}: {h}");
            assert_eq!(lean.header(h), Some(v), "{name}: {h}");
        }
    }
}

#[test]
fn checksum_trailers_frame_streamed_uploads_as_aws_chunked() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big");
    let mid = dir.path().join("mid");
    let body = data(12 * MIB);
    std::fs::write(&big, &body).unwrap();
    std::fs::write(&mid, &body[..MIB]).unwrap();
    let (port, store) = start_server();
    let trailers = ["--checksum-trailers"];
    let args = ["cp", "--part-size", "5", big.to_str().unwrap(), "s3://bucket/big"];
    ok(&rs5cmd(port, "lean", &trailers, &args, None), "multipart");
    let args = ["cp", mid.to_str().unwrap(), "s3://bucket/mid"];
    ok(&rs5cmd(port, "lean", &trailers, &args, None), "single");
    let st = store.lock().unwrap();
    assert!(st.objects["/bucket/big"] == body);
    assert!(st.objects["/bucket/mid"] == body[..MIB]);
    let create = st.log.iter().find(|s| s.has("uploads")).unwrap();
    assert_eq!(create.header("x-amz-checksum-algorithm"), Some("CRC32C"));
    let streamed: Vec<_> = st
        .log
        .iter()
        .filter(|s| s.method == "PUT")
        .collect();
    assert_eq!(streamed.len(), 4, "three parts and one object");
    for s in streamed {
        assert_eq!(s.header("content-encoding"), Some("aws-chunked"), "{:?}", s.query);
        assert_eq!(s.header("x-amz-trailer"), Some("x-amz-checksum-crc32c"));
        assert_eq!(
            s.header("x-amz-content-sha256"),
            Some("STREAMING-UNSIGNED-PAYLOAD-TRAILER")
        );
        let decoded = s.body.len().to_string();
        assert_eq!(s.header("x-amz-decoded-content-length"), Some(decoded.as_str()));
        assert_eq!(s.trailer_crc.as_deref(), Some(crc32c(&s.body).as_str()));
    }
    let complete = st.log.iter().find(|s| s.method == "POST" && s.has("uploadId")).unwrap();
    let xml = String::from_utf8_lossy(&complete.body);
    assert_eq!(xml.matches("<ChecksumCRC32C>").count(), 3, "{xml}");
}

#[test]
fn pipe_parts_carry_their_crc32c_in_a_header() {
    let body = data(12 * MIB);
    let mut runs = Vec::new();
    for path in ["sdk", "lean"] {
        let (port, store) = start_server();
        let args = ["pipe", "--part-size", "5", "s3://bucket/piped"];
        ok(&rs5cmd(port, path, &[], &args, Some(body.clone())), path);
        assert!(store.lock().unwrap().objects["/bucket/piped"] == body, "{path}");
        let log = requests(&store);
        let create = log.iter().find(|s| s.has("uploads")).unwrap();
        let complete = log
            .iter()
            .find(|s| s.method == "POST" && s.has("uploadId"))
            .unwrap();
        let listed = String::from_utf8_lossy(&complete.body).matches("<ChecksumCRC32C>").count();
        let parts: Vec<_> = log.iter().filter(|s| s.has("partNumber")).collect();
        assert_eq!(parts.len(), 3, "{path}");
        if path == "lean" {
            assert_eq!(create.header("x-amz-checksum-algorithm"), Some("CRC32C"));
            for p in &parts {
                assert_eq!(p.header("x-amz-checksum-crc32c"), Some(crc32c(&p.body).as_str()));
            }
            assert_eq!(listed, 3);
        } else {
            assert_eq!(create.header("x-amz-checksum-algorithm"), None);
            assert_eq!(listed, 0);
        }
        runs.push(shapes(&store));
    }
    assert_eq!(runs[0], runs[1]);
}

#[test]
fn a_refused_upload_fails_with_the_sdks_message() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("denied");
    std::fs::write(&src, data(100)).unwrap();
    let mut stderr = Vec::new();
    for path in ["sdk", "lean"] {
        let (port, store) = start_server();
        let args = ["cp", src.to_str().unwrap(), "s3://bucket/denied"];
        let out = rs5cmd(port, path, &[], &args, None);
        assert!(!out.status.success(), "{path}");
        let puts = requests(&store).iter().filter(|s| s.method == "PUT").count();
        assert_eq!(puts, 1, "{path}: not retried");
        stderr.push(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    assert!(stderr[1].contains("AccessDenied"), "{}", stderr[1]);
    assert_eq!(stderr[0], stderr[1]);
}
