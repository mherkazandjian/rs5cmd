//! End-to-end test for the FUSE `mount` command.
//!
//! Compiled only with `--features mount` on Linux, and skipped at runtime when
//! no S3 endpoint is configured or `/dev/fuse` is unavailable — so `cargo test`
//! stays green on any host and in CI without FUSE.
#![cfg(all(feature = "mount", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use assert_cmd::prelude::*;
use predicates::prelude::*;

fn endpoint_configured() -> bool {
    std::env::var("AWS_ENDPOINT_URL").is_ok() || std::env::var("S3_ENDPOINT_URL").is_ok()
}

fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
}

fn unique_bucket() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rs5cmd-mount-{}-{}", std::process::id(), nanos)
}

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

/// Unmounts and reaps the mount process when dropped, even on test failure.
struct MountGuard {
    mnt: PathBuf,
    child: Child,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let _ = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.mnt)
            .status();
        let _ = self.child.wait();
    }
}

fn wait_for_mount(mnt: &Path) -> bool {
    let needle = format!(" {} fuse", mnt.display());
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            if mounts.lines().any(|l| l.contains(&needle)) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn mount_read_write_lifecycle() {
    if !endpoint_configured() || !fuse_available() {
        eprintln!("skipping mount e2e: no S3 endpoint or /dev/fuse");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    rs5cmd().args(["mb", &s3("")]).assert().success();

    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();

    let child = rs5cmd()
        .args(["mount", &s3(""), mnt.to_str().unwrap()])
        .spawn()
        .unwrap();
    let guard = MountGuard {
        mnt: mnt.clone(),
        child,
    };
    assert!(wait_for_mount(&mnt), "mount point did not appear");

    // Write through the mount; verify the object exists in S3.
    std::fs::write(mnt.join("hello.txt"), b"hello world").unwrap();
    rs5cmd()
        .args(["cat", &s3("/hello.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));

    // Read it back through the mount.
    assert_eq!(
        std::fs::read(mnt.join("hello.txt")).unwrap(),
        b"hello world"
    );

    // mkdir + nested write.
    std::fs::create_dir(mnt.join("sub")).unwrap();
    std::fs::write(mnt.join("sub").join("n.txt"), b"nested").unwrap();
    rs5cmd()
        .args(["cat", &s3("/sub/n.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("nested"));

    // Directory listing reflects both entries.
    let names: Vec<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.contains(&"hello.txt".to_string()), "names: {names:?}");
    assert!(names.contains(&"sub".to_string()), "names: {names:?}");

    // A larger write to exercise streaming/readback beyond a single chunk.
    let big: Vec<u8> = (0..(2 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
    std::fs::write(mnt.join("big.bin"), &big).unwrap();
    assert_eq!(std::fs::read(mnt.join("big.bin")).unwrap(), big);

    // Unlink and confirm the object is gone.
    std::fs::remove_file(mnt.join("hello.txt")).unwrap();
    rs5cmd().args(["cat", &s3("/hello.txt")]).assert().failure();

    // Unmount before tearing down the bucket.
    drop(guard);

    let _ = rs5cmd().args(["rm", &s3("/*")]).ok();
    let _ = rs5cmd().args(["rb", &s3("")]).ok();
}

/// Spawns `rs5cmd mount` (with optional extra flags) and waits for it to appear.
fn spawn_mount(bucket: &str, mnt: &Path, extra: &[&str]) -> MountGuard {
    let mut args: Vec<String> = vec![
        "mount".into(),
        format!("s3://{bucket}"),
        mnt.to_str().unwrap().into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    let child = rs5cmd().args(&args).spawn().unwrap();
    let guard = MountGuard {
        mnt: mnt.to_path_buf(),
        child,
    };
    assert!(wait_for_mount(mnt), "mount point did not appear");
    guard
}

/// Regression: a single read that spans more chunks than the cache budget
/// (`buffer_size / chunk_size`) must return the full, correct bytes — it used to
/// fail with EIO ("chunk missing after fetch").
#[test]
fn mount_read_spans_more_chunks_than_buffer() {
    if !endpoint_configured() || !fuse_available() {
        eprintln!("skipping mount e2e: no S3 endpoint or /dev/fuse");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    rs5cmd().args(["mb", &s3("")]).assert().success();

    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    // 4 KiB chunks with an 8 KiB buffer = a 2-chunk cache; a kernel read of up
    // to 128 KiB spans ~32 chunks.
    let guard = spawn_mount(
        &bucket,
        &mnt,
        &["--vfs-read-chunk-size", "4096", "--buffer-size", "8192"],
    );

    let data: Vec<u8> = (0..(256 * 1024)).map(|i| (i % 251) as u8).collect();
    std::fs::write(mnt.join("d.bin"), &data).unwrap();
    assert_eq!(
        std::fs::read(mnt.join("d.bin")).unwrap(),
        data,
        "read spanning more chunks than the buffer must not fail or truncate"
    );

    drop(guard);
    let _ = rs5cmd().args(["rm", &s3("/*")]).ok();
    let _ = rs5cmd().args(["rb", &s3("")]).ok();
}

/// Regression: `mkdir` over an existing name returns EEXIST, and a directory
/// rename moves the whole subtree (no orphaned keys).
#[test]
fn mount_namespace_ops() {
    if !endpoint_configured() || !fuse_available() {
        eprintln!("skipping mount e2e: no S3 endpoint or /dev/fuse");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    rs5cmd().args(["mb", &s3("")]).assert().success();

    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let guard = spawn_mount(&bucket, &mnt, &[]);

    // mkdir, then mkdir again -> EEXIST.
    std::fs::create_dir(mnt.join("d")).unwrap();
    let again = std::fs::create_dir(mnt.join("d"));
    assert_eq!(
        again.unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists,
        "second mkdir should be EEXIST"
    );

    // Nested content, then a directory rename (copy whole subtree + delete).
    std::fs::create_dir(mnt.join("d").join("sub")).unwrap();
    std::fs::write(mnt.join("d").join("sub").join("f.txt"), b"deep").unwrap();
    std::fs::rename(mnt.join("d"), mnt.join("renamed")).unwrap();

    assert!(!mnt.join("d").exists(), "old directory should be gone");
    assert_eq!(
        std::fs::read(mnt.join("renamed").join("sub").join("f.txt")).unwrap(),
        b"deep",
        "nested file should survive the directory rename"
    );
    rs5cmd()
        .args(["cat", &s3("/renamed/sub/f.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("deep"));

    drop(guard);
    let _ = rs5cmd().args(["rm", &s3("/*")]).ok();
    let _ = rs5cmd().args(["rb", &s3("")]).ok();
}
