//! A closed stdout pipe ends rs5cmd quietly, killed by SIGPIPE like s5cmd (#7). Local only: no
//! S3 endpoint needed.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};

use assert_cmd::prelude::*;

const SIGPIPE: i32 = 13;

fn rs5cmd() -> Command {
    Command::cargo_bin("rs5cmd").unwrap()
}

fn spawn(args: &[&str]) -> Child {
    rs5cmd()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn assert_quiet_sigpipe(child: Child) {
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.signal(), Some(SIGPIPE), "status {:?}, stderr {stderr}", out.status);
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn cat_into_closed_pipe_ends_by_sigpipe() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("big");
    std::fs::write(&f, vec![b'x'; 8 << 20]).unwrap();
    let mut child = spawn(&["cat", f.to_str().unwrap()]);
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0u8; 16];
    stdout.read_exact(&mut first).unwrap();
    drop(stdout);
    assert_quiet_sigpipe(child);
}

#[test]
fn result_lines_into_closed_pipe_end_by_sigpipe() {
    // Like `rs5cmd ls 's3://bucket/many/*' | head -1`, with local copies: 3000 result lines
    // are more than a pipe holds.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir(&src).unwrap();
    for i in 0..3000 {
        std::fs::write(src.join(format!("file-with-a-longish-name-{i:05}.txt")), b"x").unwrap();
    }
    let pattern = format!("{}/*", src.to_str().unwrap());
    let dst = format!("{}/", tmp.path().join("dst").to_str().unwrap());
    let mut child = spawn(&["cp", &pattern, &dst]);
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert!(first.starts_with("cp "), "{first:?}");
    drop(stdout);
    assert_quiet_sigpipe(child);
}

#[test]
fn cat_local_file_streams_exact_bytes() {
    // #18: a local file is streamed, not read whole; the bytes must still come out unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("data");
    let data: Vec<u8> = (0..(3 << 20) + 17).map(|i: u32| (i % 251) as u8).collect();
    std::fs::write(&f, &data).unwrap();
    let out = rs5cmd().args(["cat", f.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout == data, "got {} bytes, want {}", out.stdout.len(), data.len());
}
