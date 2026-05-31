use std::process::ExitCode;

use clap::Parser;

use rs5cmd::command::{self, Cli};

/// Exit code returned when the process is interrupted by a termination signal
/// (SIGINT/Ctrl-C or SIGTERM). 128 + SIGINT(2) = 130, matching the conventional
/// shell exit code and upstream s5cmd behavior (upstream #615 / PR #863).
fn interrupt_exit_code() -> u8 {
    130
}

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

    // Race the command against termination signals so that a Ctrl-C (SIGINT) or
    // SIGTERM cancels the in-flight work and exits with code 130 instead of
    // hanging or returning an arbitrary status.
    tokio::select! {
        biased;

        _ = wait_for_signal() => {
            eprintln!("interrupted");
            ExitCode::from(interrupt_exit_code())
        }

        res = command::run(cli) => match res {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        }
    }
}

/// Resolves when the process receives a termination signal. Handles Ctrl-C
/// (SIGINT) on all platforms and additionally SIGTERM on Unix.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // If installing the SIGTERM handler fails, fall back to Ctrl-C only.
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_exit_code_is_130() {
        // Pure mapping logic: interruption must map to the conventional 130
        // exit status (128 + SIGINT). No process spawning or signals involved.
        assert_eq!(interrupt_exit_code(), 130);
    }
}
