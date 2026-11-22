//! Listings and deletes on the lean data path (`--data-path lean`) against the SDK path, on an
//! in-test S3 server that pages its listings two entries at a time and records every request:
//! the same output, the same requests (method, path, query), and the same DeleteObjects body and
//! checksum.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

use assert_cmd::prelude::*;

const KEYS: &[&str] = &["a", "d/b", "d/c", "k&1", "z"];
const PAGE: usize = 2;

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    /// Sorted, without the SDK's `x-id`; values as sent (percent-encoded).
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The headers' names, sorted, but the SDK's own bookkeeping (user agent, invocation id,
    /// attempt count).
    fn names(&self) -> Vec<&str> {
        const SDK_ONLY: &[&str] = &[
            "user-agent",
            "x-amz-user-agent",
            "amz-sdk-invocation-id",
            "amz-sdk-request",
        ];
        let mut v: Vec<&str> = self
            .headers
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !SDK_ONLY.contains(k))
            .collect();
        v.sort();
        v
    }
}

type Log = Arc<Mutex<Vec<Seen>>>;

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The listing's entries after `start` (an index), as S3 groups them: keys under `prefix`, the
/// part up to the next `delimiter` folded into one common prefix.
fn entries(prefix: &str, delimiter: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    for k in KEYS.iter().filter(|k| k.starts_with(prefix)) {
        let rest = &k[prefix.len()..];
        match rest.find(delimiter).filter(|_| !delimiter.is_empty()) {
            Some(i) => {
                let p = format!("{prefix}{}", &rest[..i + delimiter.len()]);
                if !out.iter().any(|(dir, e)| *dir && *e == p) {
                    out.push((true, p));
                }
            }
            None => out.push((false, k.to_string())),
        }
    }
    out
}

fn list_v2(q: &dyn Fn(&str) -> Option<String>) -> String {
    let prefix = q("prefix").map(|p| decode(&p)).unwrap_or_default();
    let delimiter = q("delimiter").map(|d| decode(&d)).unwrap_or_default();
    let start: usize = q("continuation-token").map(|t| t[1..].parse().unwrap()).unwrap_or(0);
    let all = entries(&prefix, &delimiter);
    let page = &all[start..(start + PAGE).min(all.len())];
    let more = start + PAGE < all.len();
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult \
         xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>bucket</Name>",
    );
    x.push_str(&format!("<Prefix>{}</Prefix><EncodingType>url</EncodingType>", encode(&prefix)));
    x.push_str(&format!("<IsTruncated>{more}</IsTruncated>"));
    for (dir, e) in page {
        if *dir {
            x.push_str(&format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", encode(e)));
        } else {
            x.push_str(&format!(
                "<Contents><Key>{}</Key><LastModified>2026-11-02T10:00:00.000Z</LastModified>\
                 <ETag>&quot;e-{}&quot;</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass>\
                 </Contents>",
                encode(e),
                e.len(),
                e.len()
            ));
        }
    }
    if more {
        x.push_str(&format!("<NextContinuationToken>t{}</NextContinuationToken>", start + PAGE));
    }
    x.push_str("</ListBucketResult>");
    x
}

fn list_versions() -> String {
    let mut x = String::from("<ListVersionsResult><IsTruncated>false</IsTruncated>");
    for k in KEYS {
        x.push_str(&format!(
            "<Version><Key>{}</Key><VersionId>v-{}</VersionId><IsLatest>true</IsLatest>\
             <LastModified>2026-11-02T10:00:00.000Z</LastModified><ETag>&quot;e&quot;</ETag>\
             <Size>1</Size><StorageClass>STANDARD</StorageClass></Version>",
            encode(k),
            k.len()
        ));
    }
    x.push_str("<DeleteMarker><Key>gone</Key><VersionId>dm</VersionId><IsLatest>true</IsLatest>\
                <LastModified>2026-11-02T10:00:00.000Z</LastModified></DeleteMarker>");
    x.push_str("</ListVersionsResult>");
    x
}

/// Every key of a DeleteObjects body, reported deleted.
fn deleted(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let mut x = String::from("<DeleteResult>");
    for part in body.split("<Key>").skip(1) {
        let key = &part[..part.find("</Key>").unwrap()];
        x.push_str(&format!("<Deleted><Key>{key}</Key></Deleted>"));
    }
    x.push_str("</DeleteResult>");
    x
}

fn answer(mut stream: TcpStream, log: &Log) {
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
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let q = |name: &str| query.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let (status, xml) = if path.trim_end_matches('/') != "/bucket" {
        ("404 Not Found", "<Error><Code>NoSuchKey</Code><Message>no</Message></Error>".into())
    } else if method == "GET" && q("list-type").is_some() {
        ("200 OK", list_v2(&q))
    } else if method == "GET" && q("versions").is_some() {
        ("200 OK", list_versions())
    } else if method == "POST" && q("delete").is_some() {
        ("200 OK", deleted(&body))
    } else {
        ("400 Bad Request", "<Error><Code>InvalidRequest</Code><Message>?</Message></Error>".into())
    };
    log.lock().unwrap().push(Seen {
        method,
        path: path.to_string(),
        query: query.clone(),
        headers,
        body,
    });
    let out = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/xml\r\n\
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

/// stdout's lines, sorted; and the requests, sorted.
type Run = (Vec<String>, Vec<(String, String, Vec<(String, String)>)>);

fn run(args: &[&str], data_path: &str) -> (Run, Vec<Seen>) {
    let (port, log) = start_server();
    let out = rs5cmd(port, data_path, args);
    assert!(
        out.status.success(),
        "{data_path} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    let seen = log.lock().unwrap().clone();
    let mut shapes: Vec<_> = seen
        .iter()
        .map(|s| (s.method.clone(), s.path.clone(), s.query.clone()))
        .collect();
    shapes.sort();
    ((lines, shapes), seen)
}

#[test]
fn listings_page_and_print_as_on_the_sdk_path() {
    for args in [
        &["ls", "s3://bucket/"][..],
        &["ls", "s3://bucket/d/"][..],
        &["ls", "s3://bucket/*"][..],
        &["ls", "--all-versions", "s3://bucket/"][..],
    ] {
        let (sdk, _) = run(args, "sdk");
        let (lean, _) = run(args, "lean");
        assert_eq!(sdk, lean, "{args:?}");
        assert!(!sdk.0.is_empty(), "{args:?}: printed nothing");
    }
    // The key with an escaped character comes back decoded.
    let ((lines, shapes), _) = run(&["ls", "s3://bucket/*"], "lean");
    assert!(lines.iter().any(|l| l.ends_with("k&1")), "{lines:?}");
    let pages = shapes.iter().filter(|(_, _, q)| q.iter().any(|(k, _)| k == "list-type"));
    assert!(pages.count() > 1, "paged: {shapes:?}");
}

#[test]
fn rm_sends_the_sdks_delete_objects() {
    let (sdk, sdk_seen) = run(&["rm", "s3://bucket/*"], "sdk");
    let (lean, lean_seen) = run(&["rm", "s3://bucket/*"], "lean");
    assert_eq!(sdk, lean);
    assert_eq!(sdk.0.len(), KEYS.len(), "{:?}", sdk.0);
    let delete = |seen: &[Seen]| {
        seen.iter()
            .find(|s| s.method == "POST")
            .cloned()
            .expect("a DeleteObjects request")
    };
    let (a, b) = (delete(&sdk_seen), delete(&lean_seen));
    assert_eq!(String::from_utf8_lossy(&a.body), String::from_utf8_lossy(&b.body));
    assert_eq!(a.names(), b.names());
    let checksum = ["x-amz-checksum-crc32", "x-amz-sdk-checksum-algorithm"];
    for h in checksum.into_iter().chain(["x-amz-content-sha256", "content-type"]) {
        assert_eq!(a.header(h), b.header(h), "{h}");
    }
    assert!(b.header("x-amz-checksum-crc32").is_some());
    assert_eq!(b.header("x-amz-sdk-checksum-algorithm"), Some("CRC32"));
}
