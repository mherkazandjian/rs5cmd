//! End-to-end tests for s5cmd/s3cmd migration compatibility: `--limitrate`,
//! `--credentials-file`, the `--s3cfg` translator, and the `import-s3cfg`
//! subcommand. Skipped automatically when no S3 endpoint is configured, so
//! `cargo test` still passes on a bare host.

use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assert_cmd::prelude::*;
use predicates::prelude::*;

fn endpoint_configured() -> bool {
    std::env::var("AWS_ENDPOINT_URL").is_ok() || std::env::var("S3_ENDPOINT_URL").is_ok()
}

/// The MinIO endpoint the dev container points at (host form for rewriting into
/// a `.s3cfg`). Falls back to the docker-compose default.
fn endpoint() -> String {
    std::env::var("AWS_ENDPOINT_URL")
        .or_else(|_| std::env::var("S3_ENDPOINT_URL"))
        .unwrap_or_else(|_| "http://minio:9000".to_string())
}

fn unique_bucket() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rs5cmd-compat-{}-{}", std::process::id(), nanos)
}

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

/// `--limitrate` throttles a download to roughly the configured bytes/second.
#[test]
fn limitrate_throttles_download() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("blob.bin");
    // 12 MiB payload — larger than the 8 MiB part size, so the download issues
    // multiple ranged GETs. The aggregate limiter "charges forward" (the first
    // acquire is free), so a multi-part transfer is needed to observe the
    // throttle: at 4 MB/s a 12 MiB download takes ~2 s.
    std::fs::write(&local, vec![7u8; 12 * 1024 * 1024]).unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    // Upload unthrottled (setup).
    rs5cmd()
        .args(["cp", local.to_str().unwrap(), &s3("/blob.bin")])
        .assert()
        .success();

    // Download with a 4 MB/s cap and time it.
    let out = tmp.path().join("out.bin");
    let start = Instant::now();
    rs5cmd()
        .args([
            "cp",
            "--limitrate",
            "4MB",
            &s3("/blob.bin"),
            out.to_str().unwrap(),
        ])
        .assert()
        .success();
    let elapsed = start.elapsed();

    assert_eq!(std::fs::read(&out).unwrap().len(), 12 * 1024 * 1024);
    assert!(
        elapsed >= Duration::from_millis(1200),
        "download finished too fast ({elapsed:?}); --limitrate did not throttle"
    );

    rs5cmd().args(["rm", &s3("/blob.bin")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

/// `--credentials-file` authenticates from a non-default credentials file (with
/// the ambient `AWS_*` credential env vars removed so the file is the only
/// source).
#[test]
fn credentials_file_is_used() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    // Only meaningful when the ambient creds are MinIO's well-known pair.
    let (Ok(ak), Ok(sk)) = (
        std::env::var("AWS_ACCESS_KEY_ID"),
        std::env::var("AWS_SECRET_ACCESS_KEY"),
    ) else {
        eprintln!("skipping: ambient AWS credentials not set");
        return;
    };

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    let creds = tmp.path().join("creds");
    std::fs::write(
        &creds,
        format!("[default]\naws_access_key_id = {ak}\naws_secret_access_key = {sk}\n"),
    )
    .unwrap();

    // With the env creds removed, the listing only succeeds via the file.
    rs5cmd()
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .args([
            "ls",
            "--credentials-file",
            creds.to_str().unwrap(),
            &s3("/"),
        ])
        .assert()
        .success();

    rs5cmd().args(["rb", &s3("")]).assert().success();
}

/// Writes a `.s3cfg` describing the MinIO endpoint and verifies that `--s3cfg`
/// translates it (endpoint + credentials + addressing) well enough to connect,
/// with the ambient endpoint/credential env vars removed.
#[test]
fn s3cfg_translation_connects() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let (Ok(ak), Ok(sk)) = (
        std::env::var("AWS_ACCESS_KEY_ID"),
        std::env::var("AWS_SECRET_ACCESS_KEY"),
    ) else {
        eprintln!("skipping: ambient AWS credentials not set");
        return;
    };

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();

    // Seed a bucket + object using the ambient environment.
    rs5cmd().args(["mb", &s3("")]).assert().success();
    let local = tmp.path().join("o.txt");
    std::fs::write(&local, b"via s3cfg").unwrap();
    rs5cmd()
        .args(["cp", local.to_str().unwrap(), &s3("/o.txt")])
        .assert()
        .success();

    // host_base wants a bare host[:port]; strip the scheme from the endpoint.
    let ep = endpoint();
    let https = ep.starts_with("https://");
    let host = ep
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let s3cfg = tmp.path().join("s3cfg");
    std::fs::write(
        &s3cfg,
        format!(
            "[default]\n\
             access_key = {ak}\n\
             secret_key = {sk}\n\
             host_base = {host}\n\
             host_bucket = {host}/%(bucket)s\n\
             use_https = {}\n",
            if https { "True" } else { "False" }
        ),
    )
    .unwrap();

    rs5cmd()
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .args(["--s3cfg", s3cfg.to_str().unwrap(), "ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("o.txt"));

    rs5cmd().args(["rm", &s3("/o.txt")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

/// `import-s3cfg` persists a `.s3cfg` to `~/.rs5cmd` + `~/.aws/credentials`, and
/// a subsequent bare command works against the endpoint without `--s3cfg`.
#[test]
fn import_s3cfg_then_use_without_flag() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let (Ok(ak), Ok(sk)) = (
        std::env::var("AWS_ACCESS_KEY_ID"),
        std::env::var("AWS_SECRET_ACCESS_KEY"),
    ) else {
        eprintln!("skipping: ambient AWS credentials not set");
        return;
    };

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // Seed a bucket + object using the ambient environment.
    rs5cmd().args(["mb", &s3("")]).assert().success();
    let local = tmp.path().join("o.txt");
    std::fs::write(&local, b"imported").unwrap();
    rs5cmd()
        .args(["cp", local.to_str().unwrap(), &s3("/o.txt")])
        .assert()
        .success();

    let ep = endpoint();
    let https = ep.starts_with("https://");
    let host = ep
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let s3cfg = tmp.path().join("s3cfg");
    std::fs::write(
        &s3cfg,
        format!(
            "[default]\n\
             access_key = {ak}\n\
             secret_key = {sk}\n\
             host_base = {host}\n\
             host_bucket = {host}/%(bucket)s\n\
             use_https = {}\n",
            if https { "True" } else { "False" }
        ),
    )
    .unwrap();

    // Import with HOME pointed at the temp dir.
    rs5cmd()
        .env("HOME", &home)
        .args(["import-s3cfg", s3cfg.to_str().unwrap()])
        .assert()
        .success();

    assert!(home.join(".rs5cmd").is_file(), "~/.rs5cmd was not written");
    assert!(
        home.join(".aws").join("credentials").is_file(),
        "~/.aws/credentials was not written"
    );

    // A bare `ls` (no --s3cfg, env endpoint/creds removed) now works via the
    // auto-loaded ~/.rs5cmd + ~/.aws/credentials.
    rs5cmd()
        .env("HOME", &home)
        .env_remove("AWS_ENDPOINT_URL")
        .env_remove("S3_ENDPOINT_URL")
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("o.txt"));

    rs5cmd().args(["rm", &s3("/o.txt")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}
