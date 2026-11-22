# Adding a command or a flag, end to end

Checked against the source at release v0.1.0. Read the neighbouring code before copying a
snippet; `src/command/bucket_version.rs` is the smallest complete command and a good model.

## 1. A new command

### 1.1 The module: `src/command/<name>.rs`

```rust
//! `<name>` — one line that says what the command does.

use clap::Args;

use super::GlobalOpts;
use crate::storage::s3::S3;
use crate::storage::url::Url;

#[derive(Args, Debug)]
pub struct NameArgs {
    /// Positional argument. This doc comment is its help text.
    pub target: String,

    /// A flag. `--some-flag` is derived from the field name.
    #[arg(long)]
    pub some_flag: bool,
}

pub async fn run(global: &GlobalOpts, args: NameArgs) -> anyhow::Result<()> {
    let opts = global.storage_options();
    let url = Url::parse(&args.target).map_err(|e| anyhow::anyhow!(e))?;
    if !url.is_remote() {
        anyhow::bail!("expected an s3:// URL, got {:?}", args.target);
    }
    let s3 = S3::new(&url, &opts).await?;
    // ... do the work with `s3`, then report it (section 1.3).
    Ok(())
}
```

- Take every connection setting from `global.storage_options()`. It carries endpoint, region,
  profile, TLS, proxy, retries and the bandwidth limiters. Reading the environment or the flags
  again in a command bypasses the config-file layering done in `command::run`.
- Validate the URL shape first (`is_remote`, `is_bucket`, `is_prefix`, `is_wildcard`) and fail
  with `anyhow::bail!` and a message that says what was expected.
- For a command that works on local and remote paths, use `storage::new_client(&url, &opts)`
  and the `Storage` trait instead of `S3` directly.
- S3 calls that only this command needs can live in the same file in an `impl S3 { ... }`
  block, as `head.rs`, `pipe.rs`, `presign.rs` and `select.rs` do.

### 1.2 Wiring: `src/command/mod.rs`

Three edits:

```rust
mod name;                                   // with the other `mod` lines

pub enum Command {
    // ...
    /// One line shown in `rs5cmd --help` next to the command.
    Name(name::NameArgs),                   // `Name` becomes `rs5cmd name`; `FooBar` -> `foo-bar`
}

match command {
    // ...
    Command::Name(args) => name::run(&global, args).await,
}
```

A command behind a Cargo feature needs `#[cfg(feature = "...")]` on all three (see `mount`), and
the module must be listed in `src/lib.rs` under the same gate if it has a top-level module.

Nothing else is needed for `run` files: each line is parsed as a full `Cli`, so a new command is
usable there at once. Shell completion is generated from the clap definitions as well.

### 1.3 Output: `src/output.rs`

| Situation | Call |
|---|---|
| An operation on one object succeeded | `output::op_success("name", &src, Some(&dst))` or `None` when there is no destination |
| An operation failed, but the batch continues | `output::op_error("name", &src, dst, &err.to_string())` and remember the failure |
| A record (listing line, metadata, URL) | `if output::is_json() { output::json_line(serde_json::json!({...})) } else { println!(...) }` |

- `op_success` prints `name src dst` in text mode, adds the `(dry-run)` prefix when needed, and
  prints `{"operation","success","source","destination"}` in JSON mode.
- In JSON mode `op_error` writes to stdout. Do not add a second error line yourself.
- After a batch with failures, end with
  `anyhow::bail!("one or more <name> operations failed")`. `main` prints it as `error: ...` and
  exits with 1.
- Diagnostics go to stderr (`eprintln!` or `tracing::warn!`), never to stdout: stdout is the
  data channel that scripts parse.
- Honour `--dry-run`: any call that changes something must be skipped when `opts.dry_run` is
  set, and the operation must still be reported.

### 1.4 Concurrency

For work over many objects, copy the pool from `cp.rs`: one `tokio::spawn` per object into a
`JoinSet`, and call `join_next()` whenever `set.len() >= workers`, where
`workers = global.numworkers.max(1)`. Share the client with `Arc`. Build the client once, before
the loop: constructing it loads the whole AWS configuration and credential chain.

### 1.5 Tests

Unit-test pure logic in a `#[cfg(test)] mod tests` at the bottom of the module. Add an e2e test
to `tests/e2e.rs` using the helpers at its top:

```rust
#[test]
fn name_does_the_thing() {
    if !endpoint_configured() {
        eprintln!("skipping: no S3 endpoint configured");
        return;
    }
    let bucket = unique_bucket();
    let s3 = |suffix: &str| format!("s3://{bucket}{suffix}");
    rs5cmd().args(["mb", &s3("")]).assert().success();

    rs5cmd()
        .args(["name", &s3("/key")])
        .assert()
        .success()
        .stdout(predicate::str::contains("expected text"));

    rs5cmd()
        .args(["--json", "name", &s3("/key")])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"operation\":\"name\""));

    rs5cmd().args(["rb", "--force", &s3("")]).assert().success();
}
```

- Every test starts with the `endpoint_configured()` guard and uses its own `unique_bucket()`,
  because tests run in parallel against one MinIO.
- Local files go in `tempfile::tempdir()`.
- Cover the text output, the `--json` output, `--dry-run` (assert that nothing changed), and one
  failure case with its exit code (`.failure()` or `.code(1)`).
- Tests for a feature-gated command go in their own file with `#![cfg(feature = "...")]` at the
  top, like `tests/e2e_fast.rs`.

### 1.6 Documentation

`README.md` (the usage block and the feature list) and
`agents/skills/rs5cmd-user/references/commands.md`. Mention the command in
`agents/skills/rs5cmd-user/SKILL.md` if users will reach for it often.

## 2. A new flag on an existing command

1. Add the field to the command's `Args` struct with a doc comment. Use `Option<T>` for a value
   without a default, `bool` for a switch, `Vec<String>` for a repeatable value, and
   `default_value_t` for a default.
2. Use it in `run`. If the flag changes how objects are written, it usually travels in
   `storage::Metadata` (see `CpArgs::metadata`) or in a per-command field of `storage::Options`
   set after `storage_options()` (see how `cp.rs` sets `part_size` and `concurrency`).
3. Remember the commands that share code:
   - `mv` uses `CpArgs`, so a `cp` flag is an `mv` flag.
   - `sync` has its own `SyncArgs` and its own copy step. A flag on `cp` is not on `sync` until
     you add it there.
   - The fast path (`try_fast_path` in `cp.rs`) does not see `CpArgs`. Either plumb the flag
     into `fastpath::FastConfig` / `Transfer`, or add it to `fast_unsupported` in `cp.rs`, which
     sends the set to the default path with a warning (#10). Never let `--fast` drop a flag.
4. Test it, and add it to the user skill's command reference.

A boolean that defaults to true cannot be switched off as a plain flag (that is the
`--follow-symlinks` defect). For an on-by-default behaviour, add the negative flag
(`--no-<name>`) instead.

## 3. A new global flag

Trace an existing one first: `grep -rn request_payer src` shows every stop that
`--request-payer` makes. A new global flag makes the same stops.

| Step | Where | What |
|---|---|---|
| 1 | `GlobalOpts` in `src/command/mod.rs` | The field, with `#[arg(long, global = true)]` and a doc comment. Add `env = "..."` only if an environment variable should set it |
| 2 | `GlobalOpts::storage_options` | Copy it into `storage::Options` |
| 3 | `storage::Options` in `src/storage/mod.rs` | The field **and** its line in the manual `impl Default for Options` |
| 4 | `S3::new` in `src/storage/s3.rs` (or the command) | Use it. A flag that only reaches `Options` does nothing: `--no-sign-request` did nothing until it was read there (#6) |
| 5 | `build_argv` in `src/command/run.rs` | Forward it, or every line of a `run` file loses it |
| 6 | `fn opts()` in the `run.rs` test module | Add the field to the explicit `GlobalOpts { .. }` literal, or the unit tests stop compiling. Extend `argv_reconstructs_flags` to cover the new flag |
| 7 | `apply_translated` in `src/command/mod.rs`, `Translated` in `src/s3cfg.rs` | Only if `.s3cfg` or `~/.rs5cmd` should be able to set it. Fill the field only when the user left it unset |
| 8 | `fast_unsupported` / `try_fast_path` in `src/command/cp.rs` | Decide whether `--fast` honours it. If not, add it to `fast_unsupported` and to the user skill's list |
| 9 | Per-side behaviour | If it is a connection setting, decide whether `Options::for_side` needs a source and a destination variant |

Then compile the tests for every feature set:

```bash
docker compose run --rm --no-deps dev cargo build --tests
docker compose run --rm --no-deps dev cargo build --tests --features "fast mount"
```

A flag that changes process-wide behaviour instead of a client setting (like `--json` or
`--color`) is applied in `command::run` before the dispatch. Because `run` re-enters
`command::run` for every line, such a switch is set again per line; keep it idempotent.

## 4. Before you call it done

- [ ] `cargo build --tests` passes for the default features and for `fast` and `mount`.
- [ ] `docker compose run --rm test` shows a `test result: ok.` line for every test binary, and
      your new test is among the ones that ran (not skipped).
- [ ] `test-fast` and `test-mount` pass if shared types or `cp.rs` changed.
- [ ] `rs5cmd <command> --help` reads well: clap builds it from your doc comments.
- [ ] The flag works inside a `run` file and with `--json` and `--dry-run`.
- [ ] `README.md` and the user skill are updated.
