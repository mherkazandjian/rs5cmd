//! io_uring fast path for high-rate small-object transfers (Linux-only,
//! `fast` feature). Built on monoio (thread-per-core io_uring) + monoio-transports
//! (pooled HTTP) + aws-sigv4 (runtime-agnostic signing), bypassing the
//! tokio/aws-sdk-s3 control plane used by the default commands.

pub mod client;
pub mod runtime;
pub mod sign;

pub use client::{Endpoint, FastClient, HttpStatusError};
pub use runtime::{run_transfers, FastConfig, Outcome, Transfer};
pub use sign::Signer;
