//! End-to-end test for `cp --all-versions` of a single key (#762).
//!
//! Self-contained (its own helpers) so it compiles as an independent
//! integration-test crate. Skipped automatically when no S3 endpoint is
//! configured, exactly like the main `e2e.rs` suite, so `cargo test` still
//! passes on a bare host. Uses only short, bounded `Command` invocations
//! (no pipes, no signals) so it can never hang.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::prelude::*;

fn endpoint_configured() -> bool {
    std::env::var("AWS_ENDPOINT_URL").is_ok() || std::env::var("S3_ENDPOINT_URL").is_ok()
}

fn unique_bucket() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rs5cmd-test-cpver-{}-{}", std::process::id(), nanos)
}

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

/// `cp --all-versions s3://bucket/key localdir/` must download EVERY version of
/// the single key, writing each to a distinct local file (the version id is
/// appended to the base name) so the versions do not overwrite one another.
#[test]
fn cp_all_versions_of_single_key() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("v.txt");

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["bucket-version", "--set", "Enabled", &s3("")])
        .assert()
        .success();

    // Three distinct versions of the same key.
    let bodies = ["VERSION-ONE", "VERSION-TWO", "VERSION-THREE"];
    for body in bodies {
        std::fs::write(&f, body).unwrap();
        rs5cmd()
            .args(["cp", f.to_str().unwrap(), &s3("/k.txt")])
            .assert()
            .success();
    }

    // Collect the version ids via `--json ls --all-versions` (same path cp uses).
    let out = rs5cmd()
        .args(["--json", "ls", "--all-versions", &s3("/")])
        .output()
        .unwrap();
    assert!(out.status.success());
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut version_ids = Vec::new();
    for line in listing.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = v.get("version_id").and_then(|x| x.as_str()) {
                if !id.is_empty() {
                    version_ids.push(id.to_string());
                }
            }
        }
    }
    assert_eq!(
        version_ids.len(),
        3,
        "expected three versions, got {version_ids:?}"
    );

    // Download every version into a fresh local directory.
    let dest = tmp.path().join("downloaded");
    std::fs::create_dir(&dest).unwrap();
    let dest_arg = format!("{}/", dest.to_str().unwrap());
    rs5cmd()
        .args(["cp", "--all-versions", &s3("/k.txt"), &dest_arg])
        .assert()
        .success();

    // Exactly one file per version must exist, named `k.txt_<versionid>`, each
    // holding the body that version stored.
    let mut got_bodies = Vec::new();
    for id in &version_ids {
        let path = dest.join(format!("k.txt_{id}"));
        assert!(
            path.exists(),
            "expected a downloaded file for version {id} at {path:?}"
        );
        got_bodies.push(std::fs::read_to_string(&path).unwrap());
    }

    // The set of downloaded bodies must be exactly the three we wrote (the three
    // versions are distinct, so every body appears once).
    got_bodies.sort();
    let mut expected: Vec<String> = bodies.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(got_bodies, expected, "downloaded version bodies mismatch");

    // The destination directory must contain exactly the 3 version files.
    let n_files = std::fs::read_dir(&dest).unwrap().count();
    assert_eq!(n_files, 3, "expected exactly 3 downloaded version files");

    // Cleanup: remove every version by id (so the bucket can be deleted), then
    // rb. (Deleting each version individually fully empties a versioned bucket,
    // mirroring the existing `object_versioning_roundtrip` test.)
    for id in &version_ids {
        rs5cmd()
            .args(["rm", "--version-id", id, &s3("/k.txt")])
            .assert()
            .success();
    }
    rs5cmd().args(["rb", &s3("")]).assert().success();
}
