//! `run` — read newline-delimited s5cmd commands from a file (or stdin) and
//! execute them concurrently, dispatching each line back through the main CLI.

use clap::{Args, Parser};
use futures::stream::{FuturesUnordered, StreamExt};

use super::GlobalOpts;

#[derive(Args, Debug)]
pub struct RunArgs {
    /// File containing commands (one per line). Reads stdin if omitted.
    pub file: Option<String>,
}

pub async fn run(global: &GlobalOpts, args: RunArgs) -> anyhow::Result<()> {
    let content = match &args.file {
        Some(path) => {
            std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("opening {path}: {e}"))?
        }
        None => std::io::read_to_string(std::io::stdin())
            .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?,
    };

    // (line-number, command) pairs, skipping blank and `#` comment lines.
    let lines: Vec<(usize, String)> = content
        .lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                None
            } else {
                Some((i + 1, line.to_string()))
            }
        })
        .collect();

    let workers = global.numworkers.max(1);
    let mut had_error = false;

    // Drive lines concurrently with a bounded FuturesUnordered. We do NOT use
    // tokio::spawn: the dispatched command future is not `Send` (the fast path
    // holds thread-local io_uring state), and spawning isn't needed — each
    // dispatched command parallelizes its own work across cores internally.
    // FuturesUnordered drives many lines on this task with no `Send` bound, and
    // borrowing `global` by reference avoids per-line clones.
    let mut inflight = FuturesUnordered::new();
    let mut iter = lines.into_iter();
    for _ in 0..workers {
        match iter.next() {
            Some((lineno, line)) => inflight.push(run_one(global, line, lineno)),
            None => break,
        }
    }
    while let Some((line, r)) = inflight.next().await {
        report(&line, r, &mut had_error);
        if let Some((lineno, line)) = iter.next() {
            inflight.push(run_one(global, line, lineno));
        }
    }

    if had_error {
        anyhow::bail!("one or more run commands failed");
    }
    Ok(())
}

/// Runs one line, returning it alongside its result for reporting.
async fn run_one(global: &GlobalOpts, line: String, lineno: usize) -> (String, anyhow::Result<()>) {
    let r = run_line(global, &line, lineno).await;
    (line, r)
}

/// Parses one command line into a full CLI invocation and dispatches it through
/// the main dispatcher, propagating the run invocation's global flags.
async fn run_line(global: &GlobalOpts, line: &str, lineno: usize) -> anyhow::Result<()> {
    let tokens =
        shell_words::split(line).map_err(|e| anyhow::anyhow!("parsing line {lineno}: {e}"))?;
    if tokens.is_empty() {
        return Ok(());
    }

    let argv = build_argv(global, &tokens);
    let cli =
        super::Cli::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("line {lineno}: {e}"))?;

    // s5cmd disallows `run` nested inside a run file.
    if matches!(cli.command, super::Command::Run(_)) {
        anyhow::bail!("nested run is not supported (line {lineno})");
    }

    // `super::run` is async and we are inside it; box the recursive future to
    // break the otherwise infinitely-sized future type.
    Box::pin(super::run(cli)).await
}

/// Reconstructs argv for one line: program name, the run invocation's global
/// flags (so each sub-command inherits them, matching s5cmd), then the tokens.
fn build_argv(global: &GlobalOpts, tokens: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(tokens.len() + 16);
    argv.push("rs5cmd".to_string());

    if let Some(url) = &global.endpoint_url {
        argv.push("--endpoint-url".to_string());
        argv.push(url.clone());
    }
    if let Some(region) = &global.region {
        argv.push("--region".to_string());
        argv.push(region.clone());
    }
    if let Some(profile) = &global.profile {
        argv.push("--profile".to_string());
        argv.push(profile.clone());
    }
    if global.no_sign_request {
        argv.push("--no-sign-request".to_string());
    }
    if global.no_verify_ssl {
        argv.push("--no-verify-ssl".to_string());
    }
    if global.dry_run {
        argv.push("--dry-run".to_string());
    }
    if global.json {
        argv.push("--json".to_string());
    }
    if global.use_list_objects_v1 {
        argv.push("--use-list-objects-v1".to_string());
    }
    argv.push("--numworkers".to_string());
    argv.push(global.numworkers.to_string());
    argv.push("--retry-count".to_string());
    argv.push(global.retry_count.to_string());

    argv.extend(tokens.iter().cloned());
    argv
}

fn report(line: &str, r: anyhow::Result<()>, had_error: &mut bool) {
    if let Err(e) = r {
        *had_error = true;
        eprintln!("ERROR {line}: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> GlobalOpts {
        GlobalOpts {
            endpoint_url: None,
            no_sign_request: false,
            no_verify_ssl: false,
            use_list_objects_v1: false,
            dry_run: false,
            json: false,
            region: None,
            profile: None,
            addressing_style: None,
            numworkers: 256,
            retry_count: 10,
        }
    }

    #[test]
    fn argv_prefixes_program_and_numworkers() {
        let argv = build_argv(&opts(), &["rm".to_string(), "s3://b/y".to_string()]);
        assert_eq!(argv[0], "rs5cmd");
        let nw = argv.iter().position(|a| a == "--numworkers").unwrap();
        assert_eq!(argv[nw + 1], "256");
        assert_eq!(&argv[argv.len() - 2..], &["rm".to_string(), "s3://b/y".to_string()]);
    }

    #[test]
    fn argv_reconstructs_flags() {
        let mut g = opts();
        g.endpoint_url = Some("http://h:9000".to_string());
        g.dry_run = true;
        let argv = build_argv(&g, &["ls".to_string()]);
        let i = argv.iter().position(|a| a == "--endpoint-url").unwrap();
        assert_eq!(argv[i + 1], "http://h:9000");
        assert!(argv.iter().any(|a| a == "--dry-run"));
        assert!(!argv.iter().any(|a| a == "--no-sign-request"));
    }
}
