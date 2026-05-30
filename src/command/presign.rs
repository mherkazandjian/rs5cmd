//! `presign` — print a presigned URL for a remote object (GET, or PUT with `--put`).

use std::time::Duration;

use clap::Args;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;

impl S3 {
    /// Generates a presigned URL for a GET request on the given object,
    /// valid for `expire`. Mirrors the Go `S3.Presign`.
    pub async fn presign(&self, src: &Url, expire: Duration) -> anyhow::Result<String> {
        let mut req = self
            .client
            .get_object()
            .bucket(&src.bucket)
            .key(&src.path)
            .set_request_payer(self.request_payer());
        if !src.version_id.is_empty() {
            req = req.version_id(&src.version_id);
        }
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expire)?;
        let presigned = req.presigned(config).await?;
        Ok(presigned.uri().to_string())
    }

    /// Generates a presigned URL for a PUT (upload) request, valid for `expire`.
    /// Versioning does not apply to uploads.
    pub async fn presign_put(&self, dst: &Url, expire: Duration) -> anyhow::Result<String> {
        let req = self
            .client
            .put_object()
            .bucket(&dst.bucket)
            .key(&dst.path)
            .set_request_payer(self.request_payer());
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expire)?;
        let presigned = req.presigned(config).await?;
        Ok(presigned.uri().to_string())
    }
}

#[derive(Args, Debug)]
pub struct PresignArgs {
    /// Source object (s3://bucket/key).
    pub src: String,

    /// URL valid duration (Go-style duration, e.g. "1h", "30m", "2h30m").
    #[arg(long, default_value = "3h")]
    pub expire: String,

    /// Use the specified version of an object (GET only; ignored with --put).
    #[arg(long = "version-id")]
    pub version_id: Option<String>,

    /// Presign a PUT (upload) URL instead of a GET (download) URL.
    #[arg(long)]
    pub put: bool,
}

pub async fn run(global: &GlobalOpts, args: PresignArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();

    let url = Url::new(
        &args.src,
        crate::storage::url::UrlOptions {
            version_id: args.version_id.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;

    if !url.is_remote() {
        anyhow::bail!("source must be a remote object");
    }
    if url.is_bucket() || url.is_prefix() {
        anyhow::bail!("remote source must be an object");
    }
    if url.is_wildcard() {
        anyhow::bail!("remote source {url:?} can not contain glob characters");
    }

    let expire = parse_duration(&args.expire)?;

    let s3 = S3::new(&url, &opts).await?;
    let presigned = if args.put {
        s3.presign_put(&url, expire).await?
    } else {
        s3.presign(&url, expire).await?
    };
    if crate::output::is_json() {
        crate::output::json_line(serde_json::json!({ "url": presigned }));
    } else {
        println!("{presigned}");
    }
    Ok(())
}

/// Parses a Go-style duration string composed of `h`/`m`/`s` units, e.g.
/// "1h", "30m", "15s", "2h30m". A bare number is interpreted as seconds.
fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration");
    }

    // Allow a plain integer/float to mean seconds.
    if let Ok(secs) = s.parse::<f64>() {
        if secs < 0.0 {
            anyhow::bail!("duration must not be negative: {s:?}");
        }
        return Ok(Duration::from_secs_f64(secs));
    }

    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut saw_unit = false;

    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        if num.is_empty() {
            anyhow::bail!("invalid duration {s:?}: missing number before unit {c:?}");
        }
        let value: f64 = num
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid number {num:?} in duration {s:?}"))?;
        let unit_secs = match c {
            'h' => 3600.0,
            'm' => 60.0,
            's' => 1.0,
            other => anyhow::bail!("invalid duration unit {other:?} in {s:?}"),
        };
        total += Duration::from_secs_f64(value * unit_secs);
        num.clear();
        saw_unit = true;
    }

    if !num.is_empty() {
        anyhow::bail!("invalid duration {s:?}: trailing number {num:?} without unit");
    }
    if !saw_unit {
        anyhow::bail!("invalid duration {s:?}");
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("15s").unwrap(), Duration::from_secs(15));
    }

    #[test]
    fn parses_combined() {
        assert_eq!(
            parse_duration("2h30m").unwrap(),
            Duration::from_secs(2 * 3600 + 30 * 60)
        );
        assert_eq!(parse_duration("1h1m1s").unwrap(), Duration::from_secs(3661));
    }

    #[test]
    fn parses_bare_seconds() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn rejects_invalid() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("1d").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("1h30").is_err());
    }
}
