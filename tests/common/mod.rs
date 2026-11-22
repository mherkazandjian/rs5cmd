//! Shared by the e2e test files: the S3 endpoint they run against, if any.
#![allow(dead_code)]

/// The S3 endpoint for the e2e tests: `AWS_ENDPOINT_URL`, else `S3_ENDPOINT_URL`. Without either
/// the tests skip themselves, so `cargo test` passes on a host without MinIO, unless
/// `RS5CMD_E2E_REQUIRED=1` asks for them (the compose `test` service sets it): then a missing
/// endpoint fails the test, and a green run cannot be a vacuous one.
pub fn endpoint() -> Option<String> {
    let e = std::env::var("AWS_ENDPOINT_URL")
        .or_else(|_| std::env::var("S3_ENDPOINT_URL"))
        .ok();
    if e.is_none() && required() {
        panic!("RS5CMD_E2E_REQUIRED=1, but neither AWS_ENDPOINT_URL nor S3_ENDPOINT_URL is set");
    }
    e
}

pub fn endpoint_configured() -> bool {
    endpoint().is_some()
}

fn required() -> bool {
    std::env::var("RS5CMD_E2E_REQUIRED").is_ok_and(|v| v == "1")
}
