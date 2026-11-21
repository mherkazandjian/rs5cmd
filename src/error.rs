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

/// On Unix, `EMFILE` ("Too many open files") is errno 24. Reports whether any
/// error in the chain is a raw `EMFILE` I/O error.
#[cfg(unix)]
pub fn is_emfile(err: &anyhow::Error) -> bool {
    const EMFILE: i32 = 24;
    err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .and_then(|io| io.raw_os_error())
            == Some(EMFILE)
    })
}

#[cfg(not(unix))]
pub fn is_emfile(_err: &anyhow::Error) -> bool {
    false
}

/// The next step for a profile that needs SSO in a build without the `sso` feature: aws-config's
/// error names only the cargo feature.
pub const SSO_HINT: &str = "this build of rs5cmd has no SSO support: export the profile's \
     credentials first, with \
     eval \"$(aws configure export-credentials --profile NAME --format env)\", \
     or build rs5cmd with `--features sso`";

/// Whether aws-config refused a profile because the build lacks the `sso` feature.
fn needs_sso_feature(msg: &str) -> bool {
    msg.contains("cargo feature(s) enabled: sso")
}

/// Formats an error for display (`{e:#}`), appending the RLIMIT_NOFILE hint when
/// the cause is a raw `EMFILE` so the user gets an actionable next step (#390), and
/// `SSO_HINT` when the profile needs SSO and the build has none.
pub fn format_error(err: &anyhow::Error) -> String {
    let base = format!("{err:#}");
    if is_emfile(err) {
        format!("{base} ({})", crate::rlimit::EMFILE_HINT)
    } else if needs_sso_feature(&base) {
        format!("{} ({SSO_HINT})", base.trim_end())
    } else {
        base
    }
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
    fn an_sso_profile_without_the_feature_gets_the_next_step() {
        let e = anyhow::anyhow!(
            "the credentials provider was not properly configured: ProfileFile provider could \
             not be built: This behavior requires following cargo feature(s) enabled: sso. "
        );
        let msg = format_error(&e);
        assert!(msg.ends_with(&format!("enabled: sso. ({SSO_HINT})")), "{msg}");
        assert!(SSO_HINT.contains("aws configure export-credentials"));
        assert!(SSO_HINT.contains("--features sso"));
        assert_eq!(format_error(&anyhow::anyhow!("boom")), "boom");
    }

    #[test]
    fn warning_messages() {
        assert_eq!(Warning::ObjectExists.to_string(), "object already exists");
    }
}
