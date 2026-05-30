//! Error types ported from s5cmd's `error` package.

use std::fmt;

use crate::storage::url::Url;

/// A job/command error carrying the operation and its operands, mirroring the
/// Go `Error` struct used for structured logging.
#[derive(Debug)]
pub struct JobError {
    /// The operation being performed (copy, move, etc.).
    pub op: String,
    pub src: Option<Url>,
    pub dst: Option<Url>,
    pub err: anyhow::Error,
}

impl JobError {
    pub fn full_command(&self) -> String {
        let src = self.src.as_ref().map(|u| u.to_string()).unwrap_or_default();
        let dst = self.dst.as_ref().map(|u| u.to_string()).unwrap_or_default();
        format!("{} {} {}", self.op, src, dst).trim().to_string()
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.err)
    }
}

impl std::error::Error for JobError {}

/// Warnings that are not fatal — the sync/cp strategies short-circuit on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Warning {
    #[error("object already exists")]
    ObjectExists,
    #[error("object is newer or same age")]
    ObjectIsNewer,
    #[error("object size matches")]
    ObjectSizesMatch,
    #[error("object is newer or same age and object size matches")]
    ObjectIsNewerAndSizesMatch,
    #[error("object is in Glacier storage class")]
    ObjectIsGlacier,
}

/// Reports whether the given error chain represents a cancellation.
pub fn is_cancellation(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.to_string().to_lowercase().contains("cancel"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::url::Url;

    #[test]
    fn full_command_formats_operands() {
        let e = JobError {
            op: "cp".to_string(),
            src: Some(Url::parse("s3://b/k").unwrap()),
            dst: Some(Url::parse("/tmp/k").unwrap()),
            err: anyhow::anyhow!("boom"),
        };
        assert_eq!(e.full_command(), "cp s3://b/k /tmp/k");
    }

    #[test]
    fn warning_messages() {
        assert_eq!(Warning::ObjectExists.to_string(), "object already exists");
    }
}
