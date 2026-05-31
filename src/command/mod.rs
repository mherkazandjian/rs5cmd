//! CLI definition and command dispatch.

mod bucket;
mod bucket_version;
mod cat;
mod cp;
mod du;
mod filters;
mod head;
mod ls;
mod pipe;
mod presign;
mod rm;
mod run;
mod select;
mod sync;
mod tree;

use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::output::ColorChoice;
use crate::ratelimit::RateLimiter;
use crate::storage::Options;

/// A very fast S3 and local filesystem execution tool (Rust port of s5cmd).
#[derive(Parser, Debug)]
#[command(name = "rs5cmd", version, about)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone)]
pub struct GlobalOpts {
    /// Use the given endpoint URL (e.g. a MinIO/S3-compatible server).
    #[arg(long, global = true, env = "AWS_ENDPOINT_URL")]
    pub endpoint_url: Option<String>,

    /// Do not sign requests (anonymous access).
    #[arg(long, global = true)]
    pub no_sign_request: bool,

    /// Use ListObjects (V1) instead of ListObjectsV2 (for providers like GCS).
    #[arg(long, global = true)]
    pub use_list_objects_v1: bool,

    /// Skip TLS certificate verification (for self-signed endpoints).
    #[arg(long, global = true)]
    pub no_verify_ssl: bool,

    /// Print what would be done without actually doing it.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Emit results as one JSON object per line.
    #[arg(long, global = true)]
    pub json: bool,

    /// AWS region.
    #[arg(long, global = true, env = "AWS_REGION")]
    pub region: Option<String>,

    /// AWS region for the SOURCE side of a copy/move/sync. Overrides `--region`
    /// for the source client only; falls back to `--region` when unset. Lets a
    /// single copy span two regions (upstream #858/#816/#514/#702/#700/#671).
    #[arg(long, global = true)]
    pub source_region: Option<String>,

    /// AWS region for the DESTINATION side of a copy/move/sync. Overrides
    /// `--region` for the destination client only; falls back to `--region`.
    #[arg(long, global = true)]
    pub destination_region: Option<String>,

    /// Endpoint URL for the SOURCE side of a copy/move/sync (e.g. a different
    /// S3-compatible server than the destination). Overrides `--endpoint-url`
    /// for the source client only; falls back to `--endpoint-url` when unset.
    #[arg(long, global = true)]
    pub source_endpoint_url: Option<String>,

    /// Endpoint URL for the DESTINATION side of a copy/move/sync. Overrides
    /// `--endpoint-url` for the destination client only; falls back to
    /// `--endpoint-url` when unset.
    #[arg(long, global = true)]
    pub destination_endpoint_url: Option<String>,

    /// AWS named profile.
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// S3 addressing style: `path` (e.g. host/bucket/key) or `virtual`
    /// (bucket.host/key). Defaults to path-style for custom endpoints and
    /// virtual-host for real AWS.
    #[arg(long, global = true, value_parser = ["path", "virtual"])]
    pub addressing_style: Option<String>,

    /// Resolve S3 endpoints to their dual-stack (IPv4 + IPv6) variant so
    /// requests can travel over IPv6 (upstream #719). Has no effect against a
    /// custom `--endpoint-url` (e.g. MinIO), which is used verbatim.
    #[arg(long, global = true)]
    pub use_dualstack_endpoint: bool,

    /// Resolve S3 endpoints to their FIPS-compliant variant.
    #[arg(long, global = true)]
    pub use_fips_endpoint: bool,

    /// Route requests through a proxy: `socks5://`, `socks5h://`, `http://` or
    /// `https://[user:pass@]host:port`. Falls back to the ALL_PROXY/HTTPS_PROXY/
    /// HTTP_PROXY env vars. (Applies to the default path, not `--fast`.)
    #[arg(long, short = 'x', global = true)]
    pub proxy: Option<String>,

    /// Number of concurrent workers for batch operations.
    #[arg(long, global = true, default_value_t = 256)]
    pub numworkers: usize,

    /// Max retry attempts for transient errors.
    #[arg(long, global = true, default_value_t = 10)]
    pub retry_count: u32,

    /// Colorize output: `auto` (color only when stdout is a TTY and NO_COLOR is
    /// unset), `always`, or `never`. Color is always suppressed under `--json`.
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,

    /// Cap aggregate upload bandwidth, e.g. "100MB", "1GB", "512KB" (bytes/sec).
    /// The cap is shared across all concurrent workers, not per-object (#433).
    #[arg(long, global = true, value_name = "RATE")]
    pub limit_upload: Option<String>,

    /// Cap aggregate download bandwidth, e.g. "100MB", "1GB", "512KB"
    /// (bytes/sec). Shared across all concurrent workers, not per-object (#433).
    #[arg(long, global = true, value_name = "RATE")]
    pub limit_download: Option<String>,
}

/// User-facing `--color` choice. Mirrors [`ColorChoice`] but lives on the CLI
/// surface so `output` stays free of clap.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

// clap's `default_value_t` requires `Display`; render the lowercase value names.
impl std::fmt::Display for ColorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ColorMode::Auto => "auto",
            ColorMode::Always => "always",
            ColorMode::Never => "never",
        };
        f.write_str(s)
    }
}

impl From<ColorMode> for ColorChoice {
    fn from(m: ColorMode) -> Self {
        match m {
            ColorMode::Auto => ColorChoice::Auto,
            ColorMode::Always => ColorChoice::Always,
            ColorMode::Never => ColorChoice::Never,
        }
    }
}

impl GlobalOpts {
    pub fn storage_options(&self) -> Options {
        // The `--limit-upload`/`--limit-download` strings are validated once at
        // startup (see `validate_bandwidth_limits`, called from `run`), so by the
        // time this runs they parse cleanly; build the shared limiters here.
        let (upload_limiter, download_limiter) = self
            .bandwidth_limiters()
            .unwrap_or_else(|_| (None, None));
        Options {
            endpoint: self.endpoint_url.clone(),
            dry_run: self.dry_run,
            no_sign_request: self.no_sign_request,
            no_verify_ssl: self.no_verify_ssl,
            use_list_objects_v1: self.use_list_objects_v1,
            region: self.region.clone(),
            source_region: self.source_region.clone(),
            destination_region: self.destination_region.clone(),
            source_endpoint: self.source_endpoint_url.clone(),
            destination_endpoint: self.destination_endpoint_url.clone(),
            profile: self.profile.clone(),
            proxy: self.proxy.clone(),
            addressing_style: self.addressing_style.clone(),
            use_dualstack_endpoint: self.use_dualstack_endpoint,
            use_fips_endpoint: self.use_fips_endpoint,
            max_retries: self.retry_count,
            upload_limiter,
            download_limiter,
            ..Default::default()
        }
    }

    /// Parses `--limit-upload`/`--limit-download` into shared [`RateLimiter`]s,
    /// returning `(upload, download)`. Either is `None` when the flag is unset.
    /// Returns an error if a size string is invalid or zero.
    fn bandwidth_limiters(
        &self,
    ) -> anyhow::Result<(Option<Arc<RateLimiter>>, Option<Arc<RateLimiter>>)> {
        let up = match &self.limit_upload {
            Some(s) => Some(parse_limiter(s)?),
            None => None,
        };
        let down = match &self.limit_download {
            Some(s) => Some(parse_limiter(s)?),
            None => None,
        };
        Ok((up, down))
    }

    /// Validates the bandwidth-limit flags up front so an invalid value produces
    /// a clear error before any work begins. Called from [`run`].
    pub fn validate_bandwidth_limits(&self) -> anyhow::Result<()> {
        self.bandwidth_limiters().map(|_| ())
    }
}

/// Parses a human size string like "100MB"/"512KB"/"1GB" into a bytes/second
/// [`RateLimiter`], reusing the `bytesize` crate already used elsewhere. A zero
/// or unparseable rate is an error.
fn parse_limiter(s: &str) -> anyhow::Result<Arc<RateLimiter>> {
    let bytes = s
        .trim()
        .parse::<bytesize::ByteSize>()
        .map(|b| b.as_u64())
        .map_err(|e| anyhow::anyhow!("invalid bandwidth limit '{}': {}", s, e))?;
    if bytes == 0 {
        anyhow::bail!("bandwidth limit must be greater than zero: '{}'", s);
    }
    Ok(RateLimiter::new(bytes))
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List buckets and objects.
    Ls(ls::LsArgs),
    /// Copy objects.
    Cp(cp::CpArgs),
    /// Move objects (copy then delete source).
    Mv(cp::CpArgs),
    /// Remove objects.
    Rm(rm::RmArgs),
    /// Print object contents to stdout.
    Cat(cat::CatArgs),
    /// Make bucket.
    Mb(bucket::MbArgs),
    /// Remove bucket.
    Rb(bucket::RbArgs),
    /// Synchronize source to destination.
    Sync(sync::SyncArgs),
    /// Show object size usage.
    Du(du::DuArgs),
    /// List objects under a prefix as a hierarchical tree.
    Tree(tree::TreeArgs),
    /// Stream stdin to a remote object.
    Pipe(pipe::PipeArgs),
    /// Print remote object metadata (or check a bucket exists).
    Head(head::HeadArgs),
    /// Print a presigned URL for a remote object.
    Presign(presign::PresignArgs),
    /// Run SQL queries on objects (S3 Select).
    Select(select::SelectArgs),
    /// Run commands from a file or stdin.
    Run(run::RunArgs),
    /// Get or set a bucket's versioning status.
    BucketVersion(bucket_version::BucketVersionArgs),
    /// Generate a shell completion script (bash, zsh, fish, powershell, elvish).
    Completion(CompletionArgs),
}

#[derive(Args, Debug)]
pub struct CompletionArgs {
    /// Shell to generate the completion script for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// Runs the parsed CLI.
pub async fn run(cli: Cli) -> anyhow::Result<()> {
    crate::output::set_json(cli.global.json);
    // Resolve `--color` once, globally. Must run AFTER set_json so JSON output
    // stays clean (set_color force-disables color under JSON mode).
    crate::output::set_color(cli.global.color.into());
    // Honor `--dry-run` so result lines are visibly marked (set once, globally).
    crate::output::set_dry_run(cli.global.dry_run);
    // Fail fast on an invalid `--limit-upload`/`--limit-download` value before
    // any storage work begins (#433).
    cli.global.validate_bandwidth_limits()?;
    match cli.command {
        Command::Ls(args) => ls::run(&cli.global, args).await,
        Command::Cp(args) => cp::run(&cli.global, args, false).await,
        Command::Mv(args) => cp::run(&cli.global, args, true).await,
        Command::Rm(args) => rm::run(&cli.global, args).await,
        Command::Cat(args) => cat::run(&cli.global, args).await,
        Command::Mb(args) => bucket::run_mb(&cli.global, args).await,
        Command::Rb(args) => bucket::run_rb(&cli.global, args).await,
        Command::Sync(args) => sync::run(&cli.global, args).await,
        Command::Du(args) => du::run(&cli.global, args).await,
        Command::Tree(args) => tree::run(&cli.global, args).await,
        Command::Pipe(args) => pipe::run(&cli.global, args).await,
        Command::Head(args) => head::run(&cli.global, args).await,
        Command::Presign(args) => presign::run(&cli.global, args).await,
        Command::Select(args) => select::run(&cli.global, args).await,
        Command::Run(args) => run::run(&cli.global, args).await,
        Command::BucketVersion(args) => bucket_version::run(&cli.global, args).await,
        Command::Completion(args) => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            clap_complete::generate(args.shell, &mut cmd, "rs5cmd", &mut std::io::stdout());
            Ok(())
        }
    }
}
