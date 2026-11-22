//! Local-only command-line tests: no S3 endpoint needed.

use std::fs;
use std::process::Command;

use assert_cmd::prelude::*;

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

#[test]
fn cp_bare_relative_dir_keeps_dir_name_and_source() {
    // #2: `cp data out/` mapped data/a.txt onto itself and truncated it.
    for src in ["data", "./data"] {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("data")).unwrap();
        fs::write(tmp.path().join("data/a.txt"), b"hello").unwrap();
        rs5cmd()
            .current_dir(tmp.path())
            .args(["cp", src, "out/"])
            .assert()
            .success();
        assert_eq!(fs::read(tmp.path().join("data/a.txt")).unwrap(), b"hello", "{src}");
        assert_eq!(fs::read(tmp.path().join("out/data/a.txt")).unwrap(), b"hello", "{src}");
    }
}

#[test]
fn mv_bare_relative_dir_moves_the_files() {
    // #2: `mv data2 out2/` deleted the file it had just truncated.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("data2")).unwrap();
    fs::write(tmp.path().join("data2/b.txt"), b"hello").unwrap();
    rs5cmd()
        .current_dir(tmp.path())
        .args(["mv", "data2", "out2/"])
        .assert()
        .success();
    assert_eq!(fs::read(tmp.path().join("out2/data2/b.txt")).unwrap(), b"hello");
    assert!(!tmp.path().join("data2/b.txt").exists());
}

/// #8: `tree` refuses --json (before any request) instead of printing a text tree.
#[test]
fn tree_refuses_json() {
    let out = rs5cmd()
        .args(["--json", "--endpoint-url", "http://127.0.0.1:9", "tree", "s3://bucket/"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tree has no JSON output") && stderr.contains("ls --json"), "{stderr}");
}

#[test]
fn copying_a_file_onto_itself_fails_and_keeps_it() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
    for cmd in ["cp", "mv"] {
        rs5cmd()
            .current_dir(tmp.path())
            .args([cmd, "a.txt", "./a.txt"])
            .assert()
            .failure();
        assert_eq!(fs::read(tmp.path().join("a.txt")).unwrap(), b"hello", "{cmd}");
    }
}

/// Without the `sso` feature (the default), a profile that needs IAM Identity Center fails at
/// once with an error that names the feature, instead of falling through to other credentials.
#[cfg(not(feature = "sso"))]
#[test]
fn sso_profile_without_the_sso_feature_names_the_feature() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    fs::write(
        &config,
        "[profile sso-test]\nsso_session = s\nsso_account_id = 111122223333\n\
         sso_role_name = r\nregion = us-east-1\n\n[sso-session s]\n\
         sso_start_url = https://example.invalid/start\nsso_region = us-east-1\n",
    )
    .unwrap();
    let out = rs5cmd()
        .env("HOME", tmp.path())
        .env("AWS_CONFIG_FILE", &config)
        .env("AWS_SHARED_CREDENTIALS_FILE", tmp.path().join("credentials"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_ENDPOINT_URL")
        .args(["--profile", "sso-test", "--endpoint-url", "http://127.0.0.1:9"])
        .args(["--retry-count", "0", "ls", "s3://bucket/"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sso") && stderr.contains("feature"), "{stderr}");
    // ... and says what to do instead (rs5cmd::error::SSO_HINT).
    let hint = "aws configure export-credentials";
    assert!(stderr.contains("error: ") && stderr.contains(hint), "{stderr}");
}
