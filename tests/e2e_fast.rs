//! End-to-end tests for the io_uring fast path (`--fast`). These only compile
//! and run under `--features fast`, and require an S3-compatible endpoint with
//! io_uring available (the `test-fast` docker-compose service: hardened
//! io_uring seccomp profile + MinIO). They self-skip when no endpoint is set.

#![cfg(feature = "fast")]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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
