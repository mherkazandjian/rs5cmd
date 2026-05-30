//! CLI definition and command dispatch.

mod bucket;
mod bucket_version;
mod cat;
mod cp;
mod du;
mod head;
mod ls;
mod pipe;
mod presign;
mod rm;
mod run;
mod select;
mod sync;

use clap::{Args, Parser, Subcommand};

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

    /// AWS named profile.
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// S3 addressing style: `path` (e.g. host/bucket/key) or `virtual`
    /// (bucket.host/key). Defaults to path-style for custom endpoints and
    /// virtual-host for real AWS.
    #[arg(long, global = true, value_parser = ["path", "virtual"])]
    pub addressing_style: Option<String>,

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
}

impl GlobalOpts {
    pub fn storage_options(&self) -> Options {
        Options {
            endpoint: self.endpoint_url.clone(),
            dry_run: self.dry_run,
            no_sign_request: self.no_sign_request,
            no_verify_ssl: self.no_verify_ssl,
            use_list_objects_v1: self.use_list_objects_v1,
            region: self.region.clone(),
            profile: self.profile.clone(),
            proxy: self.proxy.clone(),
            addressing_style: self.addressing_style.clone(),
            max_retries: self.retry_count,
            ..Default::default()
        }
    }
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
