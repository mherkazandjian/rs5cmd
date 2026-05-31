//! CLI definition and command dispatch.

mod bucket;
mod bucket_version;
mod cat;
mod cp;
mod du;
mod filters;
mod head;
mod import_s3cfg;
mod ls;
#[cfg(feature = "mount")]
pub(crate) mod mount;
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
    pub command: Option<Command>,
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

    /// s3cmd-style single bandwidth cap: shorthand that limits BOTH upload and
    /// download to this rate, e.g. "1MB" (bytes/sec). An explicit
    /// --limit-upload/--limit-download takes precedence for that direction.
    #[arg(long, global = true, value_name = "RATE")]
    pub limitrate: Option<String>,

    /// Trust additional CA certificates from this PEM bundle (e.g. a private or
    /// self-signed CA) without disabling verification.
    #[arg(long, global = true)]
    pub ca_certs_file: Option<String>,

    /// Send `x-amz-request-payer` (value: `requester`) for requester-pays buckets.
    #[arg(long, global = true)]
    pub request_payer: Option<String>,

    /// Read AWS credentials from this file instead of `~/.aws/credentials`
    /// (sets AWS_SHARED_CREDENTIALS_FILE for the SDK credential chain).
    #[arg(long, global = true)]
    pub credentials_file: Option<String>,

    /// Translate an s3cmd `.s3cfg` and apply it for this run. Explicit flags win;
    /// see the `import-s3cfg` subcommand to persist it to `~/.rs5cmd`.
    #[arg(long, global = true)]
    pub s3cfg: Option<String>,

    /// Install shell completion into your shell rc (detected from `$SHELL`) and
    /// exit. Mirrors s5cmd's `--install-completion`.
    #[arg(long, global = true)]
    pub install_completion: bool,
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
            ca_certs_file: self.ca_certs_file.clone(),
            request_payer: self.request_payer.clone(),
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
        // `--limitrate` (s3cmd-style) caps BOTH directions; an explicit
        // --limit-upload/--limit-download overrides it for that direction.
        let up_src = self.limit_upload.as_ref().or(self.limitrate.as_ref());
        let down_src = self.limit_download.as_ref().or(self.limitrate.as_ref());
        let up = match up_src {
            Some(s) => Some(parse_limiter(s)?),
            None => None,
        };
        let down = match down_src {
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
    /// Mount a remote S3 path as a local filesystem (FUSE, rclone-style).
    #[cfg(feature = "mount")]
    Mount(mount::MountArgs),
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
    /// Translate an s3cmd `.s3cfg` into rs5cmd config (~/.rs5cmd + ~/.aws/credentials).
    ImportS3cfg(import_s3cfg::ImportS3cfgArgs),
}

#[derive(Args, Debug)]
pub struct CompletionArgs {
    /// Shell to generate the completion script for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// Runs the parsed CLI.
pub async fn run(mut cli: Cli) -> anyhow::Result<()> {
    // `--install-completion`: install into the shell rc and exit (works without
    // a subcommand; mirrors s5cmd).
    if cli.global.install_completion {
        return install_completion();
    }

    // `--credentials-file`: point the AWS SDK credential chain at a custom file.
    if let Some(path) = &cli.global.credentials_file {
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", path);
    }

    // `import-s3cfg` *produces* config; run it before any translation/dispatch.
    if let Some(Command::ImportS3cfg(args)) = &cli.command {
        return import_s3cfg::run(args.clone());
    }

    // Apply config files onto unset global flags: an explicit `--s3cfg` for this
    // run first, then the auto-loaded `~/.rs5cmd` dotfile. Explicit CLI flags
    // always win (see `apply_translated`).
    if let Some(path) = cli.global.s3cfg.clone() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading --s3cfg {path}: {e}"))?;
        let t = crate::s3cfg::translate_s3cfg(&crate::s3cfg::parse_ini(&text));
        for w in &t.warnings {
            tracing::warn!("s3cfg: {w}");
        }
        apply_translated(&mut cli.global, &t);
    }
    if let Some(t) = load_rs5cmd_dotfile() {
        apply_translated(&mut cli.global, &t);
    }

    crate::output::set_json(cli.global.json);
    // Resolve `--color` once, globally. Must run AFTER set_json so JSON output
    // stays clean (set_color force-disables color under JSON mode).
    crate::output::set_color(cli.global.color.into());
    // Honor `--dry-run` so result lines are visibly marked (set once, globally).
    crate::output::set_dry_run(cli.global.dry_run);
    // Fail fast on an invalid bandwidth limit (incl. a translated `--limitrate`)
    // before any storage work begins (#433).
    cli.global.validate_bandwidth_limits()?;

    let global = cli.global;
    let Some(command) = cli.command else {
        // No subcommand: show help (like `--help`) and exit success.
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    match command {
        Command::Ls(args) => ls::run(&global, args).await,
        Command::Cp(args) => cp::run(&global, args, false).await,
        Command::Mv(args) => cp::run(&global, args, true).await,
        Command::Rm(args) => rm::run(&global, args).await,
        Command::Cat(args) => cat::run(&global, args).await,
        Command::Mb(args) => bucket::run_mb(&global, args).await,
        Command::Rb(args) => bucket::run_rb(&global, args).await,
        Command::Sync(args) => sync::run(&global, args).await,
        Command::Du(args) => du::run(&global, args).await,
        Command::Tree(args) => tree::run(&global, args).await,
        Command::Pipe(args) => pipe::run(&global, args).await,
        Command::Head(args) => head::run(&global, args).await,
        #[cfg(feature = "mount")]
        Command::Mount(args) => mount::run(&global, args).await,
        Command::Presign(args) => presign::run(&global, args).await,
        Command::Select(args) => select::run(&global, args).await,
        Command::Run(args) => run::run(&global, args).await,
        Command::BucketVersion(args) => bucket_version::run(&global, args).await,
        Command::Completion(args) => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            clap_complete::generate(args.shell, &mut cmd, "rs5cmd", &mut std::io::stdout());
            Ok(())
        }
        // Handled before translation, above.
        Command::ImportS3cfg(_) => unreachable!("import-s3cfg handled before dispatch"),
    }
}

/// Applies translated config-file overrides onto `global`, filling only fields
/// the user left unset so explicit CLI flags always win. Credentials are
/// exported as `AWS_*` env vars only when not already set (real env wins,
/// matching the SDK credential chain).
fn apply_translated(global: &mut GlobalOpts, t: &crate::s3cfg::Translated) {
    fn set_env_if_unset(key: &str, val: &Option<String>) {
        if let Some(v) = val {
            if std::env::var_os(key).is_none() {
                std::env::set_var(key, v);
            }
        }
    }
    set_env_if_unset("AWS_ACCESS_KEY_ID", &t.access_key);
    set_env_if_unset("AWS_SECRET_ACCESS_KEY", &t.secret_key);
    set_env_if_unset("AWS_SESSION_TOKEN", &t.session_token);

    // `endpoint_url`/`region` carry `env =` attributes, so a set env var already
    // populated them and `is_none()` is false — env is thus treated as CLI-set.
    if global.endpoint_url.is_none() {
        global.endpoint_url = t.endpoint_url.clone();
    }
    if global.region.is_none() {
        global.region = t.region.clone();
    }
    if global.addressing_style.is_none() {
        global.addressing_style = t.addressing_style.clone();
    }
    if t.no_verify_ssl {
        global.no_verify_ssl = true;
    }
    if global.ca_certs_file.is_none() {
        global.ca_certs_file = t.ca_certs_file.clone();
    }
    if global.proxy.is_none() {
        global.proxy = t.proxy.clone();
    }
    if global.request_payer.is_none() {
        global.request_payer = t.request_payer.clone();
    }
    // `--limitrate` is an s3cmd-style single cap; main's limiter parses a size
    // string, so render the translated byte/sec value back to a string.
    if global.limitrate.is_none() {
        if let Some(rate) = t.limitrate {
            global.limitrate = Some(rate.to_string());
        }
    }
    // `retry_count` has a non-Option default (10); only override if untouched.
    if global.retry_count == 10 {
        if let Some(rc) = t.retry_count {
            global.retry_count = rc;
        }
    }
}

/// Loads `~/.rs5cmd` (rs5cmd-native INI written by `import-s3cfg`) if present,
/// so saved config applies automatically. `None` when absent or `HOME` is unset.
fn load_rs5cmd_dotfile() -> Option<crate::s3cfg::Translated> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home).join(".rs5cmd");
    let text = std::fs::read_to_string(path).ok()?;
    Some(crate::s3cfg::from_rs5cmd(&crate::s3cfg::parse_ini(&text)))
}

/// Installs shell completion into the user's shell rc (detected from `$SHELL`),
/// mirroring s5cmd's `--install-completion`. Idempotent: the `source` line is
/// added only once. fish completions go to its standard completions directory
/// (no rc edit needed).
fn install_completion() -> anyhow::Result<()> {
    use clap::CommandFactory;
    use std::io::Write;

    let shell_path = std::env::var("SHELL").unwrap_or_default();
    let shell_name = std::path::Path::new(&shell_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("bash");
    let shell = match shell_name {
        "zsh" => clap_complete::Shell::Zsh,
        "fish" => clap_complete::Shell::Fish,
        _ => clap_complete::Shell::Bash,
    };

    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;

    // Generate the completion script for the detected shell.
    let mut buf = Vec::new();
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "rs5cmd", &mut buf);
    let script = String::from_utf8(buf).expect("completion script is valid UTF-8");

    let (rc_path, comp_path) = match shell {
        clap_complete::Shell::Zsh => (
            Some(home.join(".zshrc")),
            home.join(".config").join("rs5cmd").join("completion.zsh"),
        ),
        clap_complete::Shell::Fish => (
            None,
            home.join(".config")
                .join("fish")
                .join("completions")
                .join("rs5cmd.fish"),
        ),
        _ => (
            Some(home.join(".bashrc")),
            home.join(".config").join("rs5cmd").join("completion.bash"),
        ),
    };

    if let Some(parent) = comp_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&comp_path, script.as_bytes())?;
    println!("wrote completion script to {}", comp_path.display());

    if let Some(rc) = rc_path {
        let source_line = format!("source \"{}\"", comp_path.display());
        let existing = std::fs::read_to_string(&rc).unwrap_or_default();
        if existing.contains(&source_line) {
            println!("{} already sources it; nothing to do", rc.display());
        } else {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&rc)?;
            writeln!(f, "\n# rs5cmd shell completion\n{source_line}")?;
            println!(
                "added completion to {} (restart your shell or `source` it)",
                rc.display()
            );
        }
    } else {
        println!("fish will auto-load it on next shell start");
    }
    Ok(())
}
