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
