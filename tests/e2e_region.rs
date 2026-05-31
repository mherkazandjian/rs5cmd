//! End-to-end regression tests for the per-side region/endpoint feature
//! (#858/#816/#514/#702/#700/#671), run against the docker-compose MinIO.
//!
//! IMPORTANT / HONEST SCOPE: MinIO is SINGLE-REGION and a single endpoint, so a
//! TRUE cross-region or cross-endpoint copy CANNOT be exercised here. These
//! tests are regression guards proving that:
//!   1. the new `--source-region` / `--destination-region` /
//!      `--source-endpoint-url` / `--destination-endpoint-url` flags parse and
//!      propagate (the binary accepts them and still runs), and
//!   2. a normal s3->s3 `cp`/`mv`/`sync` against MinIO STILL works when those
//!      flags drive the two-client (download+upload) code path — i.e. pointing
//!      both sides at the SAME MinIO endpoint with the SAME region exercises the
//!      cross-client `client_copy_to` plumbing without needing a second region.
//!
//! The flag-parse/propagation unit coverage lives in `src/storage/mod.rs`
//! (`options_per_side_*` tests) and `src/command/mod.rs`. These e2e tests add
//! the live MinIO round-trip.
//!
//! Gated the same way as `e2e.rs`: only meaningful inside the compose `test`
//! service where MinIO is up. The harness always passes `--endpoint-url`, so a
//! plain build (no MinIO) would fail to connect — which is why these run only
//! under docker compose.

#![allow(clippy::all)]

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

fn bin() -> String {
    env!("CARGO_BIN_EXE_rs5cmd").to_string()
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_bucket() -> String {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("rs5cmd-region-{}-{}", std::process::id(), n)
}

fn endpoint() -> String {
    std::env::var("RS5CMD_TEST_ENDPOINT").unwrap_or_else(|_| "http://minio:9000".to_string())
}

/// Runs the binary with MinIO credentials and the SHARED `--endpoint-url`, plus
/// any extra args. Returns (success, stdout, stderr).
fn run_raw(extra: &[&str]) -> (bool, String, String) {
    let mut cmd = Command::new(bin());
    cmd.env("AWS_ACCESS_KEY_ID", "minioadmin");
    cmd.env("AWS_SECRET_ACCESS_KEY", "minioadmin");
    cmd.env("AWS_REGION", "us-east-1");
    cmd.arg("--endpoint-url").arg(endpoint());
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("failed to run rs5cmd binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn run_ok(extra: &[&str]) -> String {
    let (ok, stdout, stderr) = run_raw(extra);
    assert!(
        ok,
        "command failed: args={extra:?}\nstdout={stdout}\nstderr={stderr}"
    );
    stdout
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::var("RS5CMD_TEST_TMP").unwrap_or_else(|_| "/tmp".to_string());
    let dir = std::path::PathBuf::from(base).join(format!(
        "rs5cmd-region-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_bucket() -> String {
    let b = unique_bucket();
    run_ok(&["mb", &format!("s3://{b}")]);
    b
}

/// The per-side flags parse and the binary accepts them (here on a trivial `ls`,
/// which never actually splits into two clients but proves clap wiring).
#[test]
fn test_per_side_flags_parse() {
    // No bucket needed: just prove the global flags are accepted on a command.
    // `ls` of all buckets must still succeed with the extra (unused-for-ls)
    // per-side flags present.
    let (ok, stdout, stderr) = run_raw(&[
        "--source-region",
        "us-east-1",
        "--destination-region",
        "us-east-1",
        "--source-endpoint-url",
        &endpoint(),
        "--destination-endpoint-url",
        &endpoint(),
        "ls",
    ]);
    assert!(ok, "per-side flags rejected: stdout={stdout}\nstderr={stderr}");
}

/// s3->s3 `cp` STILL works when the per-side region+endpoint flags drive the
/// two-client (download+upload) copy path. Both sides point at the SAME MinIO,
/// so this is a single-region exercise of the cross-client `client_copy_to`
/// plumbing — NOT a true cross-region copy (MinIO can't do that).
#[test]
fn test_cp_s3_to_s3_two_client_same_endpoint() {
    let bucket = make_bucket();
    let dir = tempdir();
    let src = dir.join("payload.txt");
    let body = "two-client copy regression payload";
    std::fs::write(&src, body.as_bytes()).unwrap();

    // Seed an object.
    run_ok(&["cp", src.to_str().unwrap(), &format!("s3://{bucket}/a.txt")]);

    // s3->s3 copy with DIFFERING per-side flags set (same endpoint/region, but
    // present), which makes `sides_differ()` true and routes through the
    // download+upload two-client path. The destination endpoint/region are set
    // explicitly so the source side falls back to --endpoint-url/AWS_REGION.
    run_ok(&[
        "--destination-region",
        "us-east-1",
        "--destination-endpoint-url",
        &endpoint(),
        "cp",
        &format!("s3://{bucket}/a.txt"),
        &format!("s3://{bucket}/b.txt"),
    ]);

    // Verify the destination object exists and round-trips with identical bytes.
    let out = run_ok(&["ls", &format!("s3://{bucket}/")]);
    assert!(out.contains("b.txt"), "copy target missing: {out}");

    let back = dir.join("b.txt");
    run_ok(&[
        "cp",
        &format!("s3://{bucket}/b.txt"),
        back.to_str().unwrap(),
    ]);
    let got = std::fs::read_to_string(&back).unwrap();
    assert_eq!(got, body, "two-client copy corrupted the body");
}

/// Regression guard: a plain s3->s3 `cp` with NO per-side flags must STILL take
/// the single-client server-side `CopyObject` fast path and succeed unchanged.
#[test]
fn test_cp_s3_to_s3_single_client_fast_path_unchanged() {
    let bucket = make_bucket();
    let dir = tempdir();
    let src = dir.join("fast.txt");
    let body = "single-client fast path payload";
    std::fs::write(&src, body.as_bytes()).unwrap();

    run_ok(&["cp", src.to_str().unwrap(), &format!("s3://{bucket}/x.txt")]);
    run_ok(&[
        "cp",
        &format!("s3://{bucket}/x.txt"),
        &format!("s3://{bucket}/y.txt"),
    ]);

    let back = dir.join("y.txt");
    run_ok(&[
        "cp",
        &format!("s3://{bucket}/y.txt"),
        back.to_str().unwrap(),
    ]);
    assert_eq!(std::fs::read_to_string(&back).unwrap(), body);
}

/// s3->s3 `sync` STILL works when per-side flags drive the two-client path
/// (same endpoint, so single-region exercise only).
#[test]
fn test_sync_s3_to_s3_two_client_same_endpoint() {
    let src_bucket = make_bucket();
    let dst_bucket = make_bucket();
    let dir = tempdir();

    // Seed two objects in the source bucket.
    for name in ["one.txt", "sub/two.txt"] {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, name.as_bytes()).unwrap();
        run_ok(&[
            "cp",
            p.to_str().unwrap(),
            &format!("s3://{src_bucket}/{name}"),
        ]);
    }

    // Sync across buckets with per-side flags set (same MinIO), exercising the
    // cross-client copy path in sync.
    run_ok(&[
        "--source-endpoint-url",
        &endpoint(),
        "--destination-endpoint-url",
        &endpoint(),
        "--destination-region",
        "us-east-1",
        "sync",
        &format!("s3://{src_bucket}/*"),
        &format!("s3://{dst_bucket}/"),
    ]);

    let out = run_ok(&["ls", &format!("s3://{dst_bucket}/")]);
    assert!(out.contains("one.txt"), "sync missing one.txt: {out}");
    // Recursive contents land under their relative layout.
    let out_sub = run_ok(&["ls", &format!("s3://{dst_bucket}/sub/")]);
    assert!(out_sub.contains("two.txt"), "sync missing sub/two.txt: {out_sub}");
}

/// s3->s3 `mv` (copy + delete source) STILL works through the two-client path.
#[test]
fn test_mv_s3_to_s3_two_client_same_endpoint() {
    let bucket = make_bucket();
    let dir = tempdir();
    let src = dir.join("movable.txt");
    let body = "movable payload";
    std::fs::write(&src, body.as_bytes()).unwrap();

    run_ok(&["cp", src.to_str().unwrap(), &format!("s3://{bucket}/m1.txt")]);

    run_ok(&[
        "--destination-endpoint-url",
        &endpoint(),
        "--destination-region",
        "us-east-1",
        "mv",
        &format!("s3://{bucket}/m1.txt"),
        &format!("s3://{bucket}/m2.txt"),
    ]);

    // Destination present, source gone.
    let out = run_ok(&["ls", &format!("s3://{bucket}/")]);
    assert!(out.contains("m2.txt"), "mv target missing: {out}");
    assert!(!out.contains("m1.txt"), "mv did not delete source: {out}");
}
