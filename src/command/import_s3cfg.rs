//! `import-s3cfg` — translate an s3cmd `.s3cfg` into rs5cmd configuration.
//!
//! Writes the non-secret connection settings to `~/.rs5cmd` (auto-loaded on
//! every run) and, unless `--no-credentials`, writes the access/secret keys to
//! the standard `~/.aws/credentials` (mode 0600). After importing, rs5cmd works
//! against the same endpoint without needing `--s3cfg`.

use std::io::Write;
use std::path::PathBuf;

use clap::Args;

use crate::s3cfg;

#[derive(Args, Debug, Clone)]
pub struct ImportS3cfgArgs {
    /// Path to the s3cmd config file (e.g. ~/.s3cfg).
    pub path: String,

    /// Write the rs5cmd config here instead of `~/.rs5cmd`.
    #[arg(long)]
    pub out: Option<PathBuf>,

    /// AWS profile to write credentials under (in `~/.aws/credentials`).
    #[arg(long, default_value = "default")]
    pub profile: String,

    /// Do not write credentials; persist connection settings only.
    #[arg(long)]
    pub no_credentials: bool,
}

pub fn run(args: ImportS3cfgArgs) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(&args.path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", args.path))?;
    let t = s3cfg::translate_s3cfg(&s3cfg::parse_ini(&text));

    for w in &t.warnings {
        eprintln!("warning: {w}");
    }

    // 1) Connection settings -> ~/.rs5cmd (or --out). No secrets are written.
    let out = match &args.out {
        Some(p) => p.clone(),
        None => home_dir()?.join(".rs5cmd"),
    };
    std::fs::write(&out, t.to_rs5cmd_ini())
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", out.display()))?;
    println!("wrote rs5cmd config to {}", out.display());

    // 2) Credentials -> ~/.aws/credentials (mode 0600), unless suppressed.
    if !args.no_credentials {
        match (&t.access_key, &t.secret_key) {
            (Some(ak), Some(sk)) => {
                let path =
                    write_aws_credentials(&args.profile, ak, sk, t.session_token.as_deref())?;
                println!(
                    "wrote AWS credentials to {} [profile {}] (mode 0600)",
                    path.display(),
                    args.profile
                );
            }
            _ => eprintln!(
                "note: no access_key/secret_key in {}; set credentials via env or ~/.aws/credentials",
                args.path
            ),
        }
    }

    println!("done. rs5cmd now works against this endpoint without --s3cfg.");
    Ok(())
}

fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

/// Writes (or replaces) a `[profile]` section in `~/.aws/credentials`, creating
/// the file at mode 0600. Other profiles in the file are preserved verbatim.
fn write_aws_credentials(
    profile: &str,
    access_key: &str,
    secret_key: &str,
    session_token: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let dir = home_dir()?.join(".aws");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("credentials");

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut section = format!(
        "[{profile}]\naws_access_key_id = {access_key}\naws_secret_access_key = {secret_key}\n"
    );
    if let Some(tok) = session_token {
        section.push_str(&format!("aws_session_token = {tok}\n"));
    }
    let merged = replace_ini_section(&existing, profile, &section);

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    f.write_all(merged.as_bytes())?;
    // Re-assert 0600 even if the file pre-existed with looser permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Replaces the `[name]` section in INI `text` with `replacement` (which must
/// include its own header line), or appends it if absent. Other sections are
/// kept as-is.
fn replace_ini_section(text: &str, name: &str, replacement: &str) -> String {
    let header = format!("[{name}]");
    let mut out = String::new();
    let mut skipping = false;
    let mut replaced = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if trimmed == header {
                out.push_str(replacement);
                if !replacement.ends_with('\n') {
                    out.push('\n');
                }
                skipping = true;
                replaced = true;
                continue;
            }
            skipping = false;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(replacement);
        if !replacement.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_section_preserving_others() {
        let existing = "[other]\nk = v\n\n[default]\naws_access_key_id = OLD\n";
        let new = "[default]\naws_access_key_id = NEW\naws_secret_access_key = S\n";
        let merged = replace_ini_section(existing, "default", new);
        assert!(merged.contains("[other]\nk = v"));
        assert!(merged.contains("aws_access_key_id = NEW"));
        assert!(!merged.contains("OLD"));
    }

    #[test]
    fn appends_section_when_absent() {
        let merged = replace_ini_section("[other]\nk = v\n", "default", "[default]\nx = 1\n");
        assert!(merged.contains("[other]"));
        assert!(merged.trim_end().ends_with("[default]\nx = 1"));
    }

    #[test]
    fn writes_into_empty_file() {
        let merged = replace_ini_section("", "default", "[default]\nx = 1\n");
        assert_eq!(merged, "[default]\nx = 1\n");
    }
}
