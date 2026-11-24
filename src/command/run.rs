//! `run` — read newline-delimited s5cmd commands from a file (or stdin) and
//! execute them concurrently, dispatching each line back through the main CLI.

use clap::{Args, Parser};

use super::GlobalOpts;

#[derive(Args, Debug)]
pub struct RunArgs {
    /// File containing commands (one per line). Reads stdin if omitted.
    pub file: Option<String>,
}

/// The dispatcher reaches `run` again through each line (`Command::Run`). A named, boxed `Send`
/// future here breaks that cycle: an `async fn` would leave an opaque type the compiler cannot
/// prove `Send` while it is still computing it, and the lines are spawned as tasks.
pub fn run(
    global: &GlobalOpts,
    args: RunArgs,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
    Box::pin(run_lines(global, args))
}

async fn run_lines(global: &GlobalOpts, args: RunArgs) -> anyhow::Result<()> {
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

    // Lines run as tasks on the multi-threaded runtime, at most `numworkers` at a time. (Polling
    // them all inside this one task kept a 10k-line file on about one core.) Every line shares
    // the process's SDK config and HTTP client (S3::new), so connections are reused across lines.
    let global = std::sync::Arc::new(global.clone());
    let mut inflight = tokio::task::JoinSet::new();
    let mut iter = lines.into_iter();
    loop {
        while inflight.len() < workers {
            let Some((lineno, line)) = iter.next() else { break };
            inflight.spawn(run_one(std::sync::Arc::clone(&global), line, lineno));
        }
        let Some(done) = inflight.join_next().await else { break };
        match done {
            Ok((line, r)) => report(&line, r, &mut had_error),
            Err(e) => {
                eprintln!("ERROR run: a line's task failed: {e}");
                had_error = true;
            }
        }
    }

    if had_error {
        anyhow::bail!("one or more run commands failed");
    }
    Ok(())
}

/// Runs one line, returning it alongside its result for reporting.
async fn run_one(
    global: std::sync::Arc<GlobalOpts>,
    line: String,
    lineno: usize,
) -> (String, anyhow::Result<()>) {
    let r = run_line(&global, &line, lineno).await;
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
    if matches!(cli.command, Some(super::Command::Run(_))) {
        anyhow::bail!("nested run is not supported (line {lineno})");
    }

    // `run` (below `super::run`) returns a boxed future, so this one has a finite size.
    super::run(cli).await
}

/// Reconstructs argv for one line: program name, the run invocation's global
/// flags (so each sub-command inherits them, matching s5cmd), then the tokens.
/// Every `GlobalOpts` field is forwarded except `--s3cfg`, whose values are already in the
/// others, and `--install-completion`, which exits before `run` (#11); the test
/// `argv_round_trips_every_global_flag` checks it.
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
    if let Some(style) = &global.addressing_style {
        argv.push("--addressing-style".to_string());
        argv.push(style.clone());
    }
    if let Some(proxy) = &global.proxy {
        argv.push("--proxy".to_string());
        argv.push(proxy.clone());
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
    // Compat flags that affect per-command behavior must propagate too.
    // (`--limitrate` builds a fresh per-command limiter; the aggregate cap is
    // therefore per sub-command, matching how main treats limit-upload/download.)
    if let Some(ca) = &global.ca_certs_file {
        argv.push("--ca-certs-file".to_string());
        argv.push(ca.clone());
    }
    if let Some(rp) = &global.request_payer {
        argv.push("--request-payer".to_string());
        argv.push(rp.clone());
    }
    if let Some(rate) = &global.limitrate {
        argv.push("--limitrate".to_string());
        argv.push(rate.clone());
    }
    if global.checksum_trailers {
        argv.push("--checksum-trailers".to_string());
    }
    if global.data_path != super::DataPath::Sdk {
        argv.push("--data-path".to_string());
        argv.push(global.data_path.to_string());
    }
    // These were lost for every line (#11).
    let valued = [
        ("--limit-upload", &global.limit_upload),
        ("--limit-download", &global.limit_download),
        ("--source-region", &global.source_region),
        ("--destination-region", &global.destination_region),
        ("--source-endpoint-url", &global.source_endpoint_url),
        ("--destination-endpoint-url", &global.destination_endpoint_url),
        ("--credentials-file", &global.credentials_file),
    ];
    for (flag, value) in valued {
        if let Some(v) = value {
            argv.push(flag.to_string());
            argv.push(v.clone());
        }
    }
    let switches = [
        ("--use-dualstack-endpoint", global.use_dualstack_endpoint),
        ("--use-fips-endpoint", global.use_fips_endpoint),
        ("--tls-post-quantum", global.tls_post_quantum),
    ];
    for (flag, on) in switches {
        if on {
            argv.push(flag.to_string());
        }
    }
    if global.color != super::ColorMode::Auto {
        argv.push("--color".to_string());
        argv.push(global.color.to_string());
    }

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
            source_region: None,
            destination_region: None,
            source_endpoint_url: None,
            destination_endpoint_url: None,
            profile: None,
            addressing_style: None,
            use_dualstack_endpoint: false,
            use_fips_endpoint: false,
            proxy: None,
            numworkers: 256,
            retry_count: 10,
            color: crate::command::ColorMode::Auto,
            limit_upload: None,
            limit_download: None,
            limitrate: None,
            ca_certs_file: None,
            tls_post_quantum: false,
            data_path: crate::command::DataPath::Sdk,
            checksum_trailers: false,
            request_payer: None,
            credentials_file: None,
            s3cfg: None,
            install_completion: false,
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

    /// #11: every global flag given to `run` reaches its lines. The literal names every field
    /// (a new one does not compile until it is added here); give each a value that is not its
    /// default, so a field `build_argv` does not forward fails the comparison.
    #[test]
    fn argv_round_trips_every_global_flag() {
        let s = |v: &str| Some(v.to_string());
        let g = GlobalOpts {
            endpoint_url: s("http://h:9000"),
            no_sign_request: true,
            no_verify_ssl: true,
            use_list_objects_v1: true,
            dry_run: true,
            json: true,
            region: s("eu-west-1"),
            source_region: s("eu-west-2"),
            destination_region: s("eu-west-3"),
            source_endpoint_url: s("http://src:9000"),
            destination_endpoint_url: s("http://dst:9000"),
            profile: s("p"),
            addressing_style: s("virtual"),
            use_dualstack_endpoint: true,
            use_fips_endpoint: true,
            proxy: s("http://proxy:3128"),
            numworkers: 7,
            retry_count: 3,
            color: crate::command::ColorMode::Never,
            limit_upload: s("1MB"),
            limit_download: s("2MB"),
            limitrate: s("3MB"),
            ca_certs_file: s("/ca.pem"),
            tls_post_quantum: true,
            request_payer: s("requester"),
            credentials_file: s("/creds"),
            data_path: crate::command::DataPath::Lean,
            checksum_trailers: true,
            // Not forwarded: applied to the fields above before the lines run.
            s3cfg: None,
            // Not forwarded: exits before `run`.
            install_completion: false,
        };
        let argv = build_argv(&g, &["ls".to_string(), "s3://b/".to_string()]);
        let cli = <crate::command::Cli as clap::Parser>::try_parse_from(&argv).unwrap();
        assert_eq!(format!("{:?}", cli.global), format!("{g:?}"));
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
        assert!(!argv.iter().any(|a| a == "--data-path"));
        g.data_path = crate::command::DataPath::Lean;
        let argv = build_argv(&g, &["ls".to_string()]);
        let i = argv.iter().position(|a| a == "--data-path").unwrap();
        assert_eq!(argv[i + 1], "lean");
        assert!(!argv.iter().any(|a| a == "--checksum-trailers"));
        g.checksum_trailers = true;
        let argv = build_argv(&g, &["ls".to_string()]);
        assert!(argv.iter().any(|a| a == "--checksum-trailers"));
    }
}
