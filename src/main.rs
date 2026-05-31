use std::process::ExitCode;

use clap::Parser;

use rs5cmd::command::{self, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Best-effort: raise the open-file soft limit and warn if it is likely too
    // low for the configured parallelism (upstream issue #390). `--concurrency`
    // is a per-`cp`/`sync` flag (default 8); use that default for the estimate.
    rs5cmd::rlimit::setup_nofile_limits(cli.global.numworkers, 8);

    match command::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
