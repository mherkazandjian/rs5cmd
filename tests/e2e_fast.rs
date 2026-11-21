//! End-to-end tests for the io_uring fast path (`--fast`). These only compile
//! and run under `--features fast`, and require an S3-compatible endpoint with
//! io_uring available (the `test-fast` docker-compose service: hardened
//! io_uring seccomp profile + MinIO). They self-skip when no endpoint is set.

#![cfg(feature = "fast")]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assert_cmd::prelude::*;
use predicates::prelude::*;

fn endpoint() -> Option<String> {
    std::env::var("AWS_ENDPOINT_URL")
        .or_else(|_| std::env::var("S3_ENDPOINT_URL"))
        .ok()
}

fn unique_bucket() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rs5cmd-fast-{}-{}", std::process::id(), nanos)
}

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

/// Fast-path upload, download, remote→remote copy, and remote→remote move.
#[test]
fn fast_path_all_directions() {
    let Some(ep) = endpoint() else {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    };
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let g = || {
        let mut c = rs5cmd();
        c.args(["--endpoint-url", &ep]);
        c
    };

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"alpha").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"beta").unwrap();

    g().args(["mb", &s3("")]).assert().success();

    // Upload (local -> remote) via the fast path.
    let pattern = format!("{}/*.txt", tmp.path().to_str().unwrap());
    g().args(["cp", "--fast", &pattern, &s3("/up/")])
        .assert()
        .success();
    g().args(["ls", &s3("/up/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt").and(predicate::str::contains("b.txt")));

    // Download (remote -> local) via the fast path.
    let out = tmp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    g().args(["cp", "--fast", &s3("/up/*"), &format!("{}/", out.to_str().unwrap())])
        .assert()
        .success();
    assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(std::fs::read(out.join("b.txt")).unwrap(), b"beta");

    // Remote -> remote server-side copy via the fast path.
    g().args(["cp", "--fast", &s3("/up/*"), &s3("/copy/")])
        .assert()
        .success();
    g().args(["ls", &s3("/copy/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt").and(predicate::str::contains("b.txt")));

    // Remote -> remote move: copies then deletes the source objects.
    g().args(["mv", "--fast", &s3("/copy/*"), &s3("/moved/")])
        .assert()
        .success();
    g().args(["ls", &s3("/moved/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"));
    // The copy/ source prefix is now empty.
    g().args(["ls", &s3("/copy/")]).assert().failure();

    // Cleanup.
    g().args(["rm", &s3("/up/*")]).assert().success();
    g().args(["rm", &s3("/moved/*")]).assert().success();
    g().args(["rb", &s3("")]).assert().success();
}

/// An in-test server for one GET: it reads the request, then sends the status line, a
/// Content-Length of `total` and `sent` bytes of the body, then either holds the connection
/// open (`hold`) or closes it.
fn start_partial_server(total: usize, sent: usize, hold: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer_partially(stream, total, sent, hold));
        }
    });
    port
}

fn answer_partially(stream: TcpStream, total: usize, sent: usize, hold: bool) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line.trim_end().is_empty() {
            break;
        }
    }
    let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"e\"\r\n\r\n")
        .into_bytes();
    out.resize(out.len() + sent, b'a');
    let mut stream = stream;
    let _ = stream.write_all(&out);
    if hold {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// The byte ranges an in-test server was asked for, in order (`None`: no Range header).
type Ranges = Arc<Mutex<Vec<Option<(u64, u64)>>>>;

/// Serves `data` as every object, keep-alive, honouring `Range: bytes=A-B` (206 with
/// Content-Range, or 416 when no byte of it exists) and recording each request's range.
fn start_range_server(data: Vec<u8>) -> (u16, Ranges) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ranges: Ranges = Arc::default();
    let (data, seen) = (Arc::new(data), ranges.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (data, seen) = (data.clone(), seen.clone());
            std::thread::spawn(move || answer_ranges(stream, &data, &seen));
        }
    });
    (port, ranges)
}

fn answer_ranges(stream: TcpStream, data: &[u8], seen: &Ranges) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    loop {
        let mut range = None;
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let lower = header.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("range: bytes=") {
                let (a, b) = v.split_once('-').unwrap();
                range = Some((a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap()));
            }
        }
        seen.lock().unwrap().push(range);
        let total = data.len() as u64;
        let out = match range {
            Some((a, _)) if a >= total => {
                b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\n\r\n".to_vec()
            }
            Some((a, b)) => {
                let b = b.min(total - 1);
                let mut out = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                     Content-Range: bytes {a}-{b}/{total}\r\nETag: \"e\"\r\n\r\n",
                    b - a + 1
                )
                .into_bytes();
                out.extend_from_slice(&data[a as usize..=b as usize]);
                out
            }
            None => {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nETag: \"e\"\r\n\r\n"
                )
                .into_bytes();
                out.extend_from_slice(data);
                out
            }
        };
        if stream.write_all(&out).is_err() {
            return;
        }
    }
}

/// `rs5cmd --fast cp s3://bucket/object <dst>` against the in-test server on `port`.
fn fast_download(port: u16, dst: &std::path::Path) -> Command {
    fast_download_with(port, dst, &[])
}

/// As `fast_download`, with `extra` arguments to `cp`.
fn fast_download_with(port: u16, dst: &std::path::Path, extra: &[&str]) -> Command {
    let mut c = rs5cmd();
    c.env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_REGION", "us-east-1")
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_PROFILE")
        .args(["--endpoint-url", &format!("http://127.0.0.1:{port}")])
        .args(["--retry-count", "0", "cp", "--fast"])
        .args(extra)
        .arg("s3://bucket/object")
        .arg(dst);
    c
}

/// A `--fast` download larger than a part is fetched as part-sized ranges, written at their
/// offsets. Fetched whole, monoio-http hands over the body as one buffer: the whole object in
/// memory (F43), and one write too long for io_uring at 4 GiB (#15).
#[test]
fn fast_download_fetches_an_object_larger_than_a_part_in_ranges() {
    const MIB: u64 = 1 << 20;
    let data: Vec<u8> = (0..12 * MIB).map(|i| (i % 251) as u8).collect();
    let (port, ranges) = start_range_server(data.clone());
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("object");
    fast_download_with(port, &dst, &["--part-size", "5"]).assert().success();
    assert!(std::fs::read(&dst).unwrap() == data, "downloaded bytes differ");
    let want = vec![
        Some((0, 5 * MIB - 1)),
        Some((5 * MIB, 10 * MIB - 1)),
        Some((10 * MIB, 12 * MIB - 1)),
    ];
    assert_eq!(*ranges.lock().unwrap(), want);
}

/// An empty object answers any range with 416; the `--fast` download then GETs it whole and
/// writes an empty file.
#[test]
fn fast_download_of_an_empty_object_falls_back_to_a_plain_get() {
    let (port, ranges) = start_range_server(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("object");
    fast_download(port, &dst).assert().success();
    assert_eq!(std::fs::metadata(&dst).unwrap().len(), 0);
    assert_eq!(*ranges.lock().unwrap(), vec![Some((0, (8 << 20) - 1)), None]);
}

fn entries(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// SIGTERM in the middle of a `--fast` download ends the run at once with 130 and leaves no
/// file behind. The fast path's transfers used to hold the task that answers signals, so the
/// run went on until every transfer finished (#16).
#[test]
fn fast_sigterm_ends_the_run_and_leaves_no_file() {
    let port = start_partial_server(64 << 20, 64 << 10, true);
    let dir = tempfile::tempdir().unwrap();
    let mut child = fast_download(port, &dir.path().join("object")).spawn().unwrap();
    // The download has started once its temporary file exists.
    let start = Instant::now();
    while entries(dir.path()).is_empty() {
        assert!(start.elapsed() < Duration::from_secs(20), "the download did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = child.id().to_string();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    let signalled = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if signalled.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("still running 10 s after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(130));
    assert!(entries(dir.path()).is_empty(), "left behind: {:?}", entries(dir.path()));
}

/// A `--fast` download whose body ends early fails and leaves no file: the body is written as it
/// arrives, into a temporary file renamed only once complete.
#[test]
fn fast_short_download_leaves_no_file() {
    let port = start_partial_server(1 << 20, 1000, false);
    let dir = tempfile::tempdir().unwrap();
    fast_download(port, &dir.path().join("object")).assert().failure();
    assert!(entries(dir.path()).is_empty(), "left behind: {:?}", entries(dir.path()));
}

/// With `--fast`, a file larger than a part goes to the default path's multipart upload instead
/// of being read whole; it downloads back on the fast path, the body streamed to the file.
#[test]
fn fast_path_leaves_files_larger_than_a_part_to_multipart() {
    let Some(ep) = endpoint() else {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    };
    let bucket = unique_bucket();
    let g = || {
        let mut c = rs5cmd();
        c.args(["--endpoint-url", &ep]);
        c
    };
    let tmp = tempfile::tempdir().unwrap();
    let data: Vec<u8> = (0..(6u32 << 20)).map(|i| (i % 251) as u8).collect();
    std::fs::write(tmp.path().join("big"), &data).unwrap();
    std::fs::write(tmp.path().join("small"), b"small").unwrap();
    g().args(["mb", &format!("s3://{bucket}")]).assert().success();

    let pattern = format!("{}/*", tmp.path().to_str().unwrap());
    g().args(["cp", "--fast", "--part-size", "5", &pattern, &format!("s3://{bucket}/up/")])
        .assert()
        .success();
    // A multipart upload's ETag ends in -<parts>.
    g().args(["ls", "-e", &format!("s3://{bucket}/up/big")])
        .assert()
        .success()
        .stdout(predicate::str::contains("-2"));

    let out = tmp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    g().args(["cp", "--fast", &format!("s3://{bucket}/up/*")])
        .arg(format!("{}/", out.display()))
        .assert()
        .success();
    assert_eq!(std::fs::read(out.join("big")).unwrap(), data);
    assert_eq!(std::fs::read(out.join("small")).unwrap(), b"small");

    g().args(["rm", &format!("s3://{bucket}/up/*")]).assert().success();
    g().args(["rb", &format!("s3://{bucket}")]).assert().success();
}
