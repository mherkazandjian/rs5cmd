//! End-to-end test for `rm --all-versions` (#34): every version of one key, delete markers
//! included, or of every key a wildcard matches; each deleted version is printed with its id.
//!
//! Self-contained (its own helpers) so it compiles as an independent integration-test crate.
//! Skipped automatically when no S3 endpoint is configured, like the main `e2e.rs` suite.
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::prelude::*;

mod common;
use common::endpoint_configured;

fn unique_bucket() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("rs5cmd-test-rmver-{}-{}", std::process::id(), nanos)
}

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

/// The `(key, version id)` of every version and delete marker in the bucket.
fn versions(bucket: &str) -> Vec<(String, String)> {
    let out = rs5cmd()
        .args(["--json", "ls", "--all-versions", &format!("s3://{bucket}/*")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| {
            let key = v["key"].as_str().unwrap_or_default();
            let key = key.rsplit_once(&format!("{bucket}/")).map_or(key, |(_, k)| k);
            (key.to_string(), v["version_id"].as_str().unwrap_or_default().to_string())
        })
        .collect()
}

#[test]
fn rm_all_versions_of_a_key_and_of_a_wildcard() {
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
    let put = |key: &str, body: &str| {
        std::fs::write(&f, body).unwrap();
        rs5cmd().args(["cp", f.to_str().unwrap(), &s3(key)]).assert().success();
    };
    for body in ["one", "two", "three"] {
        put("/k.txt", body);
    }
    put("/k.txt2", "neighbour");
    put("/d/x.txt", "x1");
    put("/d/x.txt", "x2");
    // A plain rm of a versioned key leaves its versions and adds a delete marker.
    rs5cmd().args(["rm", &s3("/k.txt")]).assert().success();
    let count = |key: &str| versions(&bucket).iter().filter(|(k, _)| k == key).count();
    assert_eq!(count("k.txt"), 4, "three versions and a delete marker");

    rs5cmd()
        .args(["rm", "--all-versions", "--version-id", "x", &s3("/k.txt")])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not allowed to combine"));

    let out = rs5cmd().args(["rm", "--all-versions", &s3("/k.txt")]).output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.lines().count(), 4, "{stdout}");
    for line in stdout.lines() {
        let mut words = line.split_whitespace();
        assert_eq!(words.next(), Some("rm"), "{line}");
        assert_eq!(words.next(), Some(s3("/k.txt").as_str()), "{line}");
        assert!(words.next().is_some_and(|id| !id.is_empty()), "a version id: {line}");
    }
    assert_eq!(count("k.txt"), 0);
    assert_eq!(count("k.txt2"), 1, "a key that only starts with the target is kept");

    let out = rs5cmd()
        .args(["--json", "rm", "--all-versions", &s3("/d/*")])
        .output()
        .unwrap();
    assert!(out.status.success());
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["version_id"].as_str().map(str::to_string))
        .collect();
    assert_eq!(ids.len(), 2, "both versions of d/x.txt: {ids:?}");
    assert_eq!(count("d/x.txt"), 0);

    rs5cmd().args(["rm", "--all-versions", &s3("/*")]).assert().success();
    assert!(versions(&bucket).is_empty());
    rs5cmd().args(["rb", &s3("")]).assert().success();
}
