//! End-to-end tests driving the `rs5cmd` binary against an S3-compatible
//! endpoint (MinIO in docker-compose). Skipped automatically when no endpoint
//! is configured, so `cargo test` still passes on a bare host.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::prelude::*;
use predicates::prelude::*;

fn endpoint_configured() -> bool {
    std::env::var("AWS_ENDPOINT_URL").is_ok() || std::env::var("S3_ENDPOINT_URL").is_ok()
}

fn unique_bucket() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rs5cmd-test-{}-{}", std::process::id(), nanos)
}

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

#[test]
fn full_object_lifecycle() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("hello.txt");
    std::fs::write(&local, b"rs5cmd e2e payload").unwrap();

    // make bucket
    rs5cmd().args(["mb", &s3("")]).assert().success();

    // upload
    rs5cmd()
        .args(["cp", local.to_str().unwrap(), &s3("/hello.txt")])
        .assert()
        .success();

    // list shows the key and its size
    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello.txt"))
        .stdout(predicate::str::contains("18"));

    // cat returns the contents
    rs5cmd()
        .args(["cat", &s3("/hello.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("rs5cmd e2e payload"));

    // download to a new local file
    let out = tmp.path().join("out.txt");
    rs5cmd()
        .args(["cp", &s3("/hello.txt"), out.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(std::fs::read(&out).unwrap(), b"rs5cmd e2e payload");

    // remove the object, then the bucket
    rs5cmd().args(["rm", &s3("/hello.txt")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn wildcard_upload_and_prefix_delete() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"a").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"b").unwrap();
    std::fs::write(tmp.path().join("c.log"), b"c").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    let pattern = format!("{}/*.txt", tmp.path().to_str().unwrap());
    rs5cmd()
        .args(["cp", &pattern, &s3("/up/")])
        .assert()
        .success();

    // Only the two .txt files land directly under up/ (relative path == base).
    rs5cmd()
        .args(["ls", &s3("/up/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"))
        .stdout(predicate::str::contains("b.txt"))
        .stdout(predicate::str::contains("c.log").not());

    rs5cmd().args(["rm", &s3("/up/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn sync_size_only_is_idempotent_and_deletes() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"aaaa").unwrap();
    std::fs::write(dir.join("b.txt"), b"bbbb").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // Initial sync uploads both files.
    rs5cmd()
        .args(["sync", "--size-only", dir.to_str().unwrap(), &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("cp "));

    // Second sync with no changes copies nothing (size-only => deterministic).
    rs5cmd()
        .args(["sync", "--size-only", dir.to_str().unwrap(), &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("cp ").not());

    // Introduce a stray remote object and change a file's size; sync --delete
    // should re-copy the changed file and remove the stray.
    rs5cmd()
        .args(["cp", dir.join("a.txt").to_str().unwrap(), &s3("/m/src/stray.txt")])
        .assert()
        .success();
    std::fs::write(dir.join("a.txt"), b"aaaaCHANGED").unwrap();

    rs5cmd()
        .args(["sync", "--size-only", "--delete", dir.to_str().unwrap(), &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("cp ").and(predicate::str::contains("stray.txt").count(1)))
        .stdout(predicate::str::contains("rm "));

    rs5cmd().args(["rm", &s3("/m/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn sync_max_delete_aborts_without_touching_anything() {
    // --max-delete must abort the whole sync (no copies, no deletes) when the
    // delete set exceeds the cap, and leave the destination intact.
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    // An empty source directory against a destination with 3 objects: a plain
    // `--delete` would remove all 3. `--max-delete 2` must refuse.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("empty");
    std::fs::create_dir(&dir).unwrap();

    let seed = tmp.path().join("seed.txt");
    std::fs::write(&seed, b"x").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    for name in ["one.txt", "two.txt", "three.txt"] {
        rs5cmd()
            .args(["cp", seed.to_str().unwrap(), &s3(&format!("/m/{name}"))])
            .assert()
            .success();
    }

    // Over the cap (3 > 2): aborts with a non-zero exit and deletes nothing.
    rs5cmd()
        .args([
            "sync",
            "--delete",
            "--max-delete",
            "2",
            &format!("{}/", dir.to_str().unwrap()),
            &s3("/m/"),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("max-delete"));

    // All three objects must still be present (nothing was deleted).
    rs5cmd()
        .args(["ls", &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("one.txt"))
        .stdout(predicate::str::contains("two.txt"))
        .stdout(predicate::str::contains("three.txt"));

    // At the cap (3 <= 3): proceeds and deletes all three. The sync's own
    // output reports the three deletions (we assert on that rather than a
    // follow-up `ls`, since `ls` on the now-empty prefix exits non-zero with
    // "no object found").
    rs5cmd()
        .args([
            "sync",
            "--delete",
            "--max-delete",
            "3",
            &format!("{}/", dir.to_str().unwrap()),
            &s3("/m/"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("one.txt"))
        .stdout(predicate::str::contains("two.txt"))
        .stdout(predicate::str::contains("three.txt"))
        .stdout(predicate::str::contains("rm "));

    // The prefix is now empty: `ls` finds nothing.
    rs5cmd()
        .args(["ls", &s3("/m/")])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no object found"));

    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn s3_to_s3_multipart_copy_for_large_source() {
    // Server-side copy must fall back to multipart UploadPartCopy for sources
    // over the 5 GiB CopyObject limit (s5cmd PR#856). We can't make a 5 GiB
    // object on MinIO cheaply, so RS5CMD_MULTIPART_COPY_THRESHOLD=1 forces the
    // multipart path, and --part-size 5 makes the ~11 MiB source copy in three
    // ranged parts. The copy must be byte-identical to the source.
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    let tmp = tempfile::tempdir().unwrap();
    // ~11 MiB with a position-dependent pattern so a mis-ordered or mis-ranged
    // part would corrupt the result and fail the byte comparison.
    let n = 11 * 1024 * 1024 + 123;
    let mut data = vec![0u8; n];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let local = tmp.path().join("big.bin");
    std::fs::write(&local, &data).unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", local.to_str().unwrap(), &s3("/big.bin")])
        .assert()
        .success();

    // Force the multipart-copy path with 5 MiB parts (=> 3 UploadPartCopy parts).
    rs5cmd()
        .env("RS5CMD_MULTIPART_COPY_THRESHOLD", "1")
        .args(["cp", "--part-size", "5", &s3("/big.bin"), &s3("/big-copy.bin")])
        .assert()
        .success();

    // Download the copy and verify it is byte-identical to the source.
    let out = tmp.path().join("out.bin");
    rs5cmd()
        .args(["cp", &s3("/big-copy.bin"), out.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(std::fs::read(&out).unwrap(), data);

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn du_counts_objects_with_wildcard() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("x.bin"), vec![0u8; 100]).unwrap();
    std::fs::write(tmp.path().join("y.bin"), vec![0u8; 200]).unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    let pattern = format!("{}/*", tmp.path().to_str().unwrap());
    rs5cmd().args(["cp", &pattern, &s3("/d/")]).assert().success();

    // 300 bytes across 2 objects.
    rs5cmd()
        .args(["du", &s3("/d/*")])
        .assert()
        .success()
        .stdout(predicate::str::contains("300 bytes in 2 objects"));

    rs5cmd().args(["rm", &s3("/d/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn large_object_multipart_roundtrip() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    // 12 MiB > default 8 MiB part size => multipart upload (2 parts) and ranged
    // download (2 ranges). Byte value depends on position so a mis-ordered or
    // truncated part is detected.
    let size = 12 * 1024 * 1024usize;
    let mut data = vec![0u8; size];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big.bin");
    std::fs::write(&src, &data).unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // Upload (multipart) then download (ranged), with a small part size to force
    // multiple parts regardless of defaults.
    rs5cmd()
        .args([
            "cp", "--part-size", "5", "--concurrency", "4",
            src.to_str().unwrap(), &s3("/big.bin"),
        ])
        .assert()
        .success();

    let out = tmp.path().join("big.out");
    rs5cmd()
        .args([
            "cp", "--part-size", "5", "--concurrency", "4",
            &s3("/big.bin"), out.to_str().unwrap(),
        ])
        .assert()
        .success();

    let got = std::fs::read(&out).unwrap();
    assert_eq!(got.len(), size, "downloaded size mismatch");
    assert!(got == data, "downloaded bytes differ from source");

    rs5cmd().args(["rm", &s3("/big.bin")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn run_batch_from_file() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"alpha").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"beta").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // A command file with a comment and two uploads.
    let cmds = tmp.path().join("cmds.txt");
    std::fs::write(
        &cmds,
        format!(
            "# upload two files\ncp {} {}\ncp {} {}\n",
            tmp.path().join("a.txt").to_str().unwrap(),
            s3("/a.txt"),
            tmp.path().join("b.txt").to_str().unwrap(),
            s3("/b.txt"),
        ),
    )
    .unwrap();

    rs5cmd().args(["run", cmds.to_str().unwrap()]).assert().success();
    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt").and(predicate::str::contains("b.txt")));

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn pipe_multipart_large_stdin() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    rs5cmd().args(["mb", &s3("")]).assert().success();

    // 12 MiB > 5 MiB part size => multipart (3 parts).
    let size = 12 * 1024 * 1024usize;
    let data = vec![7u8; size];
    assert_cmd::Command::cargo_bin("rs5cmd")
        .unwrap()
        .args(["pipe", "--part-size", "5", "--concurrency", "4", &s3("/big.bin")])
        .write_stdin(data)
        .assert()
        .success();

    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("12582912"));

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn select_multi_object_wildcard() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("d1.json"), b"{\"n\":1}\n{\"n\":2}\n").unwrap();
    std::fs::write(tmp.path().join("d2.json"), b"{\"n\":3}\n").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", tmp.path().join("d1.json").to_str().unwrap(), &s3("/data/d1.json")])
        .assert()
        .success();
    rs5cmd()
        .args(["cp", tmp.path().join("d2.json").to_str().unwrap(), &s3("/data/d2.json")])
        .assert()
        .success();

    // Wildcard select across both objects => 3 rows.
    rs5cmd()
        .args([
            "select", "-e", "SELECT * FROM s3object s",
            "--input-format", "json", "--json-type", "lines",
            &s3("/data/*"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"n\":1").and(predicate::str::contains("\"n\":3")));

    rs5cmd().args(["rm", &s3("/data/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn json_output_mode() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("j.txt");
    std::fs::write(&f, b"hi").unwrap();

    // mb emits a JSON object with operation+success.
    rs5cmd()
        .args(["--json", "mb", &s3("")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"operation\":\"mb\"").and(predicate::str::contains("\"success\":true")));

    rs5cmd()
        .args(["--json", "cp", f.to_str().unwrap(), &s3("/j.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"operation\":\"cp\"").and(predicate::str::contains("\"destination\"")));

    // ls emits a per-object JSON record with key/size/type.
    rs5cmd()
        .args(["--json", "ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"key\":\"j.txt\"").and(predicate::str::contains("\"size\":2")));

    rs5cmd().args(["--json", "rm", &s3("/j.txt")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn list_objects_v1_fallback() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("v.txt"), b"v1").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", tmp.path().join("v.txt").to_str().unwrap(), &s3("/v.txt")])
        .assert()
        .success();

    // V1 listing path must enumerate the same objects.
    rs5cmd()
        .args(["--use-list-objects-v1", "ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("v.txt"));

    rs5cmd().args(["rm", &s3("/v.txt")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn head_and_presign_json() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("h.txt"), b"hi").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", tmp.path().join("h.txt").to_str().unwrap(), &s3("/h.txt")])
        .assert()
        .success();

    rs5cmd()
        .args(["--json", "head", &s3("/h.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"key\"").and(predicate::str::contains("\"size\":2")));

    rs5cmd()
        .args(["--json", "presign", &s3("/h.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"url\"").and(predicate::str::contains("X-Amz-")));

    rs5cmd().args(["rm", &s3("/h.txt")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn object_versioning_roundtrip() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("v.txt");

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd().args(["bucket-version", "--set", "Enabled", &s3("")]).assert().success();
    // Confirm versioning reads back as Enabled.
    rs5cmd()
        .args(["bucket-version", &s3("")])
        .assert()
        .success()
        .stdout(predicate::str::contains("Enabled"));

    // Two versions of the same key.
    std::fs::write(&f, b"VERSION-ONE").unwrap();
    rs5cmd().args(["cp", f.to_str().unwrap(), &s3("/k.txt")]).assert().success();
    std::fs::write(&f, b"VERSION-TWO").unwrap();
    rs5cmd().args(["cp", f.to_str().unwrap(), &s3("/k.txt")]).assert().success();

    // List all versions as JSON and collect the version ids.
    let out = rs5cmd()
        .args(["--json", "ls", "--all-versions", &s3("/")])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut version_ids = Vec::new();
    for line in stdout.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = v.get("version_id").and_then(|x| x.as_str()) {
                version_ids.push(id.to_string());
            }
        }
    }
    assert_eq!(version_ids.len(), 2, "expected two versions, got {version_ids:?}");

    // cat each version id; the two contents must be the two distinct payloads.
    let mut contents = std::collections::BTreeSet::new();
    for id in &version_ids {
        let o = rs5cmd()
            .args(["cat", "--version-id", id, &s3("/k.txt")])
            .output()
            .unwrap();
        assert!(o.status.success());
        contents.insert(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    let expected: std::collections::BTreeSet<String> =
        ["VERSION-ONE".to_string(), "VERSION-TWO".to_string()].into_iter().collect();
    assert_eq!(contents, expected, "version-specific cat returned wrong contents");

    // Delete every version explicitly (version-id deletes fully empty a versioned
    // bucket, leaving no delete markers), then remove the bucket.
    for id in &version_ids {
        rs5cmd()
            .args(["rm", "--version-id", id, &s3("/k.txt")])
            .assert()
            .success();
    }
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn move_local_to_s3_deletes_source() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("m.txt");
    std::fs::write(&f, b"move me").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // mv uploads then deletes the local source.
    rs5cmd()
        .args(["mv", f.to_str().unwrap(), &s3("/m.txt")])
        .assert()
        .success();
    assert!(!f.exists(), "local source should be deleted after mv");

    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("m.txt"));

    // mv back to local deletes the remote object.
    let back = tmp.path().join("back.txt");
    rs5cmd()
        .args(["mv", &s3("/m.txt"), back.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(std::fs::read(&back).unwrap(), b"move me");
    // Remote object is gone: ls of the now-empty prefix errors "no object found".
    rs5cmd().args(["ls", &s3("/m.txt")]).assert().failure();

    rs5cmd().args(["rb", &s3("")]).assert().success();
}

// `mv --remove-empty-dirs` should prune the source directories a local->remote
// move empties, bounded at the move source root; without the flag they remain
// (#846).
#[test]
fn mv_remove_empty_dirs_prunes_emptied_source_dirs() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }

    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    rs5cmd().args(["mb", &s3("")]).assert().success();

    let tmp = tempfile::tempdir().unwrap();

    // Nested tree:  root/sub/inner/c.txt  — moving the file empties inner and
    // sub, up to but not including root.
    let root = tmp.path().join("root");
    let inner = root.join("sub").join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let file = inner.join("c.txt");
    std::fs::write(&file, b"hello c").unwrap();

    // Move everything under root into the bucket WITH pruning enabled.
    let pattern = format!("{}/*", root.to_str().unwrap());
    rs5cmd()
        .args(["mv", "--remove-empty-dirs", &pattern, &s3("/on/")])
        .assert()
        .success();

    assert!(!file.exists(), "moved file should be gone");
    assert!(
        !root.join("sub").join("inner").exists(),
        "emptied inner dir should be pruned with --remove-empty-dirs"
    );
    assert!(
        !root.join("sub").exists(),
        "emptied sub dir should be pruned with --remove-empty-dirs"
    );
    // The move source root itself must never be removed.
    assert!(root.exists(), "move source root must not be pruned");

    // --- Control: identical shape, WITHOUT the flag — dirs must remain. ---
    let root2 = tmp.path().join("root2");
    let inner2 = root2.join("sub").join("inner");
    std::fs::create_dir_all(&inner2).unwrap();
    let file2 = inner2.join("d.txt");
    std::fs::write(&file2, b"hello d").unwrap();

    let pattern2 = format!("{}/*", root2.to_str().unwrap());
    rs5cmd()
        .args(["mv", &pattern2, &s3("/off/")])
        .assert()
        .success();

    assert!(!file2.exists(), "moved file should be gone");
    assert!(
        root2.join("sub").join("inner").exists(),
        "without --remove-empty-dirs the emptied dirs must remain"
    );

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn proxy_socks5_transfer() {
    // Routes a full upload/list/download through a SOCKS5 proxy (--proxy). Only
    // runs when RS5CMD_TEST_SOCKS5 is set (the `test-proxy` compose service);
    // self-skips elsewhere. (s5cmd #823)
    let proxy = match std::env::var("RS5CMD_TEST_SOCKS5") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!("skipping: RS5CMD_TEST_SOCKS5 not set");
            return;
        }
    };
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("p.txt");
    std::fs::write(&f, b"through the proxy").unwrap();

    rs5cmd().args(["--proxy", &proxy, "mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["--proxy", &proxy, "cp", f.to_str().unwrap(), &s3("/p.txt")])
        .assert()
        .success();
    rs5cmd()
        .args(["--proxy", &proxy, "ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("p.txt"));

    let out = tmp.path().join("out.txt");
    rs5cmd()
        .args(["--proxy", &proxy, "cp", &s3("/p.txt"), out.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(std::fs::read(&out).unwrap(), b"through the proxy");

    rs5cmd().args(["--proxy", &proxy, "rm", &s3("/*")]).assert().success();
    rs5cmd().args(["--proxy", &proxy, "rb", &s3("")]).assert().success();
}

#[test]
fn ls_show_fullpath_and_start_after() {
    // --show-fullpath prints absolute s3:// paths only; --start-after resumes a
    // listing past a given key (exclusive). (s5cmd #599/#601, #850)
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("x");
    std::fs::write(&f, b"x").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    for k in ["a.txt", "b.txt", "c.txt"] {
        rs5cmd()
            .args(["cp", f.to_str().unwrap(), &s3(&format!("/{k}"))])
            .assert()
            .success();
    }

    rs5cmd()
        .args(["ls", "--show-fullpath", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains(s3("/a.txt")))
        .stdout(predicate::str::contains(s3("/c.txt")));

    rs5cmd()
        .args(["ls", "--start-after", "b.txt", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("c.txt"))
        .stdout(predicate::str::contains("a.txt").not())
        .stdout(predicate::str::contains("b.txt").not());

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn sync_exclude_from_file() {
    // --exclude-from reads globs from a file; matching objects are not copied.
    // (s5cmd #868)
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("keep.txt"), b"k").unwrap();
    std::fs::write(dir.join("drop.log"), b"d").unwrap();
    let patterns = tmp.path().join("excludes.txt");
    std::fs::write(&patterns, b"# logs\n\n*.log\n").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args([
            "sync",
            "--exclude-from",
            patterns.to_str().unwrap(),
            &format!("{}/", dir.to_str().unwrap()),
            &s3("/m/"),
        ])
        .assert()
        .success();

    rs5cmd()
        .args(["ls", &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("keep.txt"))
        .stdout(predicate::str::contains("drop.log").not());

    rs5cmd().args(["rm", &s3("/m/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn completion_script_generates() {
    // `completion <shell>` emits a script naming the binary; no endpoint needed.
    rs5cmd()
        .args(["completion", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rs5cmd"));
}

#[test]
fn addressing_style_path_works() {
    // --addressing-style path is accepted and path-style requests succeed
    // against MinIO (its default). (s5cmd #795)
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("a.txt");
    std::fs::write(&f, b"hi").unwrap();

    rs5cmd().args(["--addressing-style", "path", "mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["--addressing-style", "path", "cp", f.to_str().unwrap(), &s3("/a.txt")])
        .assert()
        .success();
    rs5cmd()
        .args(["--addressing-style", "path", "ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"));

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn cp_skips_non_regular_files() {
    // A directory containing a FIFO must upload the regular files and skip the
    // FIFO instead of hanging/erroring. (s5cmd PR#776)
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("real.txt"), b"data").unwrap();

    let fifo = dir.join("pipe.fifo");
    let ok = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: mkfifo unavailable");
        return;
    }

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", &format!("{}/", dir.to_str().unwrap()), &s3("/u/")])
        .assert()
        .success();

    rs5cmd()
        .args(["ls", &s3("/u/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("real.txt"))
        .stdout(predicate::str::contains("pipe.fifo").not());

    rs5cmd().args(["rm", &s3("/u/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn cp_preserve_timestamps_roundtrip() {
    // --preserve-timestamps stores the local mtime as object metadata on upload
    // and restores it on download. (s5cmd #534)
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("ts.txt");
    std::fs::write(&src, b"timestamped").unwrap();

    if !std::process::Command::new("touch")
        .args(["-d", "2020-06-15T12:00:00Z"])
        .arg(&src)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("skipping: touch -d unavailable");
        return;
    }
    let src_mtime = std::fs::metadata(&src).unwrap().modified().unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", "--preserve-timestamps", src.to_str().unwrap(), &s3("/ts.txt")])
        .assert()
        .success();

    let out = tmp.path().join("out.txt");
    rs5cmd()
        .args(["cp", "--preserve-timestamps", &s3("/ts.txt"), out.to_str().unwrap()])
        .assert()
        .success();

    let out_mtime = std::fs::metadata(&out).unwrap().modified().unwrap();
    let diff = src_mtime
        .duration_since(out_mtime)
        .or_else(|_| out_mtime.duration_since(src_mtime))
        .unwrap();
    assert!(
        diff.as_secs() <= 2,
        "preserved mtime should match source within 2s, diff={}s",
        diff.as_secs()
    );

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn cp_client_copy_remote_to_remote() {
    // --client-copy performs a remote→remote copy by streaming through the
    // client (download+upload) instead of server-side CopyObject. (s5cmd #671)
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("orig.bin");
    let data: Vec<u8> = (0..5000u32).map(|i| (i % 256) as u8).collect();
    std::fs::write(&src, &data).unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", src.to_str().unwrap(), &s3("/orig.bin")])
        .assert()
        .success();
    rs5cmd()
        .args(["cp", "--client-copy", &s3("/orig.bin"), &s3("/copy.bin")])
        .assert()
        .success();

    let out = tmp.path().join("out.bin");
    rs5cmd()
        .args(["cp", &s3("/copy.bin"), out.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(std::fs::read(&out).unwrap(), data);

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn sync_checksum_detects_same_size_content_change() {
    // --checksum compares content (MD5/ETag), so it re-copies a file whose
    // content changed but size did not — which --size-only would miss.
    // (s5cmd #799)
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"AAAA").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["sync", "--checksum", dir.to_str().unwrap(), &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("cp "));
    rs5cmd()
        .args(["sync", "--checksum", dir.to_str().unwrap(), &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("cp ").not());

    std::fs::write(dir.join("a.txt"), b"BBBB").unwrap();
    rs5cmd()
        .args(["sync", "--checksum", dir.to_str().unwrap(), &s3("/m/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("cp "));

    rs5cmd().args(["rm", &s3("/m/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

/// Regression test for upstream s5cmd #677: a key containing an XML-illegal
/// control character (here ESC, 0x1b) used to make the ListObjectsV2 XML
/// response fail to deserialize ("failed to decode REST XML response status
/// code: 200"). We now request `EncodingType=Url` on all list paths and
/// percent-decode the echoed keys, so such a key both lists and round-trips.
#[test]
fn list_key_with_xml_illegal_control_char() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // Build a local source file to upload.
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("src.txt");
    std::fs::write(&local, b"control-char key contents").unwrap();

    // Object key embeds a raw ESC (0x1b) control char, which is illegal in XML
    // 1.0 character data and previously broke list deserialization.
    let key = "prefix/ctrl\u{1b}name.txt";
    let dst = s3(&format!("/{key}"));
    rs5cmd()
        .args(["cp", local.to_str().unwrap(), &dst])
        .assert()
        .success();

    // The bug manifested as a list/deserialization failure; success here (and a
    // non-empty listing) proves the EncodingType=Url fix works.
    let out = rs5cmd()
        .args(["ls", &s3("/prefix/")])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        stdout.contains("ctrl") && stdout.contains("name.txt"),
        "listing did not contain the control-char key; got: {stdout:?}"
    );

    // Round-trip: remove the object by its decoded key to confirm the echoed key
    // is usable end to end.
    rs5cmd().args(["rm", &dst]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn dir_marker_object_is_a_file_not_a_dir() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let endpoint = std::env::var("AWS_ENDPOINT_URL")
        .or_else(|_| std::env::var("S3_ENDPOINT_URL"))
        .unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // Create a REAL object with key exactly "foo/" (console dir-marker). Use an
    // explicit s3api PutObject so the marker carries an ETag/size/last_modified
    // exactly like a console-created dir marker, bypassing rs5cmd's own
    // client-side trailing-slash guards (pipe rejects a "/"-suffixed dest; cp
    // would append the source basename).
    let tmp = tempfile::tempdir().unwrap();
    let body = tmp.path().join("body");
    std::fs::write(&body, b"marker-body").unwrap();
    let put = std::process::Command::new("aws")
        .args([
            "--endpoint-url",
            &endpoint,
            "s3api",
            "put-object",
            "--bucket",
            &bucket,
            "--key",
            "foo/",
            "--body",
            body.to_str().unwrap(),
        ])
        .status();
    match put {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("skipping: aws cli unavailable to create dir-marker");
            return;
        }
    }

    // Also create a normal object under a genuine prefix (CommonPrefix check).
    let real = tmp.path().join("real.txt");
    std::fs::write(&real, b"hello").unwrap();
    rs5cmd()
        .args(["cp", real.to_str().unwrap(), &s3("/baz/real.txt")])
        .assert()
        .success();

    // LOAD-BEARING: cat the exact marker key. cat routes through list() because
    // foo/ is a prefix; pre-fix the marker was typed Dir and skipped
    // ("no objects matched"); post-fix it is a File and its bytes are returned.
    rs5cmd()
        .args(["cat", &s3("/foo/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("marker-body"));

    // ls of the exact key shows it as a real object row (not a DIR row).
    rs5cmd()
        .args(["ls", &s3("/foo/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("foo/"))
        .stdout(predicate::str::contains("DIR").not());

    // cp the marker to a local file: only works if classified File.
    let out = tmp.path().join("marker.out");
    rs5cmd()
        .args(["cp", &s3("/foo/"), out.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(std::fs::read(&out).unwrap(), b"marker-body");

    // Regression guard: a genuine CommonPrefix still renders DIR under the
    // default delimiter, proving the common_prefixes path was not broken.
    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("baz/"));

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn ls_prefix_with_object_equal_to_prefix() {
    // ls of a prefix that ALSO exists as a real object key (key == prefix) must
    // print that object relativized to its basename, never the full
    // un-relativized key (upstream #755). With the default delimiter, `ls` of the
    // non-slash-terminated prefix "a/b" returns the exact-match object "a/b",
    // which must render as the relativized basename "b" (pre-fix it rendered as
    // the absolute key "a/b"). The sibling/children relativization is covered by
    // the url.rs unit test parse_non_batch_relativizes_key_equal_to_prefix.
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");

    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("x");
    std::fs::write(&f, b"x").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    // Object whose key EXACTLY equals the listing prefix "a/b" ...
    rs5cmd().args(["cp", f.to_str().unwrap(), &s3("/a/b")]).assert().success();
    // ... plus two siblings nested under that same prefix, so "a/b" is a real
    // prefix as well as a real object key.
    rs5cmd().args(["cp", f.to_str().unwrap(), &s3("/a/b/file1")]).assert().success();
    rs5cmd().args(["cp", f.to_str().unwrap(), &s3("/a/b/file2")]).assert().success();

    // ls the prefix "a/b" (no trailing slash — the reproducing case). The
    // exact-match object is relativized to its basename "b".
    let out = rs5cmd().args(["ls", &s3("/a/b")]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        stdout.lines().any(|l| l.ends_with("  b")),
        "exact-match object should list relativized as basename \"b\"; got: {stdout:?}"
    );
    // Regression guard for #755: the row must NOT carry the un-relativized key
    // "a/b", and nothing in this listing is an absolute s3:// path.
    assert!(
        !stdout.contains(" a/b\n") && !stdout.contains(" a/b "),
        "exact-match object must not list as the un-relativized \"a/b\"; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("s3://"),
        "no entry should be an absolute s3:// path; got: {stdout:?}"
    );

    // Cleanup: delete each key explicitly, then the bucket. (A non-recursive
    // root wildcard `rm /*` only removes the top-level "a/b" object and leaves
    // the nested children, which would make `rb` fail with BucketNotEmpty.)
    rs5cmd().args(["rm", &s3("/a/b/file1")]).assert().success();
    rs5cmd().args(["rm", &s3("/a/b/file2")]).assert().success();
    rs5cmd().args(["rm", &s3("/a/b")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn rb_force_deletes_nonempty_bucket() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("x.txt");
    std::fs::write(&f, b"data").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    for k in ["a.txt", "b.txt", "c.txt"] {
        rs5cmd()
            .args(["cp", f.to_str().unwrap(), &s3(&format!("/{k}"))])
            .assert()
            .success();
    }
    // Plain rb fails on a non-empty bucket (current behavior preserved).
    rs5cmd().args(["rb", &s3("")]).assert().failure();
    // Bucket + objects still present.
    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"));
    // rb --force empties then removes; prints per-object rm lines and the rb line.
    rs5cmd()
        .args(["rb", "--force", &s3("")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"))
        .stdout(predicate::str::contains("rb "));
    // Bucket is gone: ls now fails (NoSuchBucket).
    rs5cmd().args(["ls", &s3("/")]).assert().failure();
}

#[test]
fn rb_force_dry_run_deletes_nothing() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("x.txt");
    std::fs::write(&f, b"d").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();
    rs5cmd()
        .args(["cp", f.to_str().unwrap(), &s3("/a.txt")])
        .assert()
        .success();
    // Dry-run deletes nothing. rs5cmd's S3 listing path short-circuits under
    // --dry-run, so the force-empty loop sees no objects and emits no per-object
    // would-delete lines; remove_bucket is also a no-op under dry-run. The
    // command still succeeds and prints the final "rb <url>" line. The real
    // guarantee -- that nothing is deleted -- is verified by the follow-up ls.
    rs5cmd()
        .args(["--dry-run", "rb", "--force", &s3("")])
        .assert()
        .success()
        .stdout(predicate::str::contains("rb "));
    // Object still present and bucket still exists: dry-run deleted nothing.
    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("a.txt"));
    // Real cleanup.
    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

#[test]
fn cp_skips_broken_symlink_and_transfers_rest() {
    // A directory containing a dangling symlink must upload the good files and
    // skip (warn about, by name) the broken link instead of aborting (#749).
    if !endpoint_configured() {
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("src");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("good_a.txt"), b"aaa").unwrap();
    std::fs::write(dir.join("good_b.txt"), b"bbb").unwrap();

    // Dangling symlink: target does not exist => WalkDir(follow_links) errors.
    use std::os::unix::fs::symlink;
    symlink("/nonexistent/definitely/missing/target", dir.join("dangling")).unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // Recursive upload must SUCCEED: good files copy; broken symlink is a warning
    // whose message NAMES the offending path.
    rs5cmd()
        .args(["cp", &format!("{}/", dir.to_str().unwrap()), &s3("/u/")])
        .assert()
        .success()
        .stderr(predicate::str::contains("dangling"));

    // Both good files present in S3 -> proves the walk continued past the broken
    // symlink instead of aborting.
    rs5cmd()
        .args(["ls", &s3("/u/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("good_a.txt"))
        .stdout(predicate::str::contains("good_b.txt"));

    rs5cmd().args(["rm", &s3("/u/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}

/// Upstream #697: a `--dry-run` op must be visibly distinguishable from a real
/// one. The text success line is prefixed with `(dry-run) ` and the JSON line
/// gains `"dryRun":true`; a real op carries neither. All commands here are
/// short, non-streaming `cp`/`ls`/`mb`/`rb`/`rm` invocations driven through the
/// blocking `assert_cmd` helper used by every other e2e test (no signals, no
/// long-lived children) so the suite cannot hang.
#[test]
fn dry_run_output_is_marked() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("dry.txt");
    std::fs::write(&local, b"dry-run payload").unwrap();

    rs5cmd().args(["mb", &s3("")]).assert().success();

    // --dry-run (text): success line carries the marker.
    rs5cmd()
        .args(["--dry-run", "cp", local.to_str().unwrap(), &s3("/dry.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("(dry-run)"));

    // The dry-run must NOT have created the object.
    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry.txt").not());

    // --dry-run --json: success object gains "dryRun":true.
    rs5cmd()
        .args(["--json", "--dry-run", "cp", local.to_str().unwrap(), &s3("/dry.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"dryRun\":true"));

    // Real op (text): no marker.
    rs5cmd()
        .args(["cp", local.to_str().unwrap(), &s3("/dry.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("(dry-run)").not());

    // The real op DID create the object.
    rs5cmd()
        .args(["ls", &s3("/")])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry.txt"));

    // Real op (json): no dryRun field at all.
    rs5cmd()
        .args(["--json", "cp", local.to_str().unwrap(), &s3("/dry.txt")])
        .assert()
        .success()
        .stdout(predicate::str::contains("dryRun").not());

    rs5cmd().args(["rm", &s3("/*")]).assert().success();
    rs5cmd().args(["rb", &s3("")]).assert().success();
}
