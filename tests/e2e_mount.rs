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
