---
name: rs5cmd-developer
description: >-
  Use when changing, building, testing, benchmarking or releasing rs5cmd itself, the Rust port of
  s5cmd: any edit under src/, tests/, bench/, packaging/ or .github/workflows/ of the rs5cmd
  repository. Covers the containerized workflow (docker compose build dev, run --rm test,
  test-fast with the io_uring seccomp profile, test-mount, bench), the MinIO-backed e2e tests,
  the bench harness and the s3stub discard server, a code map, how to add a command or flag end
  to end (clap args, dispatch, run.rs argv propagation, --json output, e2e test), conventions and
  verified pitfalls, the release workflow (cargo-zigbuild, glibc 2.28, nfpm packages) and commit
  rules. Triggers: "add a flag or command to rs5cmd", "fix a bug in rs5cmd", "run the rs5cmd
  tests", "cargo test printed no results", "benchmark rs5cmd against s5cmd", "cut a release". For
  running rs5cmd as a tool, use the rs5cmd-user skill instead.
---

# rs5cmd — developer skill

rs5cmd is a Rust port of [s5cmd](https://github.com/peak/s5cmd) (clap 4, tokio, `aws-sdk-s3`).
It has three parts that share the `storage` types:

| Part | Cargo feature | What |
|---|---|---|
| Default engine | none | Every command, on tokio and the AWS SDK |
| Fast path | `fast` (Linux) | `cp --fast` / `mv --fast` on monoio io_uring, one runtime per core, own SigV4 signing |
| Mount | `mount` (Linux, macOS) | `rs5cmd mount` on `fuse3` |

A fourth feature, `bench-stub`, builds the `s3stub` benchmark server. `mimalloc` is on by
default (mimalloc as the global allocator, in `src/main.rs`, which also turns transparent huge
pages off for the process). `sso` is off by default (credentials from IAM Identity Center
profiles; it enables aws-config's `sso`), so release builds have no SSO.

The code is the authority. This file was checked against the source at release v0.1.0; when it
disagrees with the code, trust the code and fix this file in the same change.

## Ground rules

1. **Build and test in the containers.** The workflow assumes Docker Compose and no Rust
   toolchain on the host.
2. **A change is done when the suite ran and you read the result lines**, not when it compiles.
   See [Reading test output](#reading-test-output).
3. **Match behaviour to upstream s5cmd** unless the difference is deliberate. Upstream issue
   numbers appear in comments and commit subjects as `(#123)`.
4. **Keep the diff small.** There is no formatter or lint gate, and the tree is not uniformly
   `rustfmt`-formatted. Do not run `cargo fmt` over files you are not changing.
5. **Update `agents/skills/`** when a command, flag, default, service or convention changes.
6. **Commits carry no AI attribution.** See [Commits](#commits).

## Build and test

```bash
docker compose build dev                      # build the image (rust:1-bookworm + fuse3)

docker compose run --rm test                  # cargo test --all, default features, vs MinIO
docker compose run --rm test-fast             # cargo test --features fast (io_uring seccomp profile)
docker compose run --rm test-mount            # cargo test --features mount (/dev/fuse, SYS_ADMIN)
docker compose run --rm test-proxy            # as `test`, plus the SOCKS5 proxy test

docker compose run --rm dev bash                                  # shell with MinIO up
docker compose run --rm --no-deps dev cargo build --tests         # compile check, no MinIO
docker compose run --rm --no-deps dev cargo build --features "fast mount"
docker compose run --rm dev cargo test --test e2e sync_           # one test binary, name filter
docker compose run --rm dev target/debug/rs5cmd ls                # after a build: drive it vs MinIO

docker compose down                           # stop MinIO and remove the containers
```

- The repository is mounted at `/workspace`; build output goes to `target/` in the checkout.
  The image runs as root, so with a rootful Docker daemon `target/` is owned by root on the
  host.
- `dev` and the `test*` services set `AWS_ENDPOINT_URL` and `S3_ENDPOINT_URL` to
  `http://minio:9000`, dummy credentials and `AWS_REGION=us-east-1`. Tests reach MinIO over the
  compose network. The published host ports (9000, 9001, and 9002 for `minio-tls`) are a
  convenience and will collide with anything already listening there.
- Compose derives the project name from the directory. A second checkout or a git worktree
  starts a second MinIO that fails on the same host ports. Reuse one stack with
  `docker compose -p <name> ...`.
- Each feature set is its own test run. `test` does not compile `fastpath/`, `mount/` or their
  tests. A change that touches shared types needs `test`, `test-fast` and `test-mount`.
- Use `bash -c '...'` for compound commands in the container, not `bash -lc`: a login shell
  resets `PATH` and loses `cargo`.

Services, optional TLS and proxy set-ups, and how the e2e tests are written:
[testing and bench](references/testing-and-bench.md).

## Reading test output

- `cargo build` and `cargo check` do **not** compile test code. A field added to a struct can
  build cleanly and still break `#[cfg(test)]` modules and `tests/*.rs`. Use
  `cargo build --tests` (add `--features ...` for the gated ones) or run the suite.
- A successful run prints one `test result: ok. N passed; 0 failed` line **per test binary**
  (the library unit tests, then each file in `tests/`). If you filter the output for `failed`
  and find nothing, that proves nothing: when the tests do not compile there are no
  `test result:` lines at all. Look for the lines, and for `error: could not compile`.
- e2e tests **skip themselves** and pass when no endpoint is configured (they print
  `skipping: ...`). Green on a bare host means "compiled", not "exercised". Exception:
  `tests/e2e_region.rs` has no skip guard and always targets `http://minio:9000`
  (`RS5CMD_TEST_ENDPOINT` overrides it), so it fails outside the compose network.
- The services pass `--nocapture`, so test output and `skipping:` notes are interleaved with
  cargo's own lines.

## Code map

| Path | Holds |
|---|---|
| `src/main.rs` | Entry: logging to stderr (`RUST_LOG`), open-file limit, SIGINT/SIGTERM → exit 130, errors → exit 1 |
| `src/command/mod.rs` | `Cli`, `GlobalOpts` (every global flag), `Command` enum, `run()` dispatch, config-file layering |
| `src/command/<name>.rs` | One module per command: a clap `Args` struct and `pub async fn run(global, args)` |
| `src/command/run.rs` | `run` batch command; **rebuilds argv per line** in `build_argv` |
| `src/command/filters.rs`, `sync_strategy.rs` | `--include` / `--exclude` matching; sync comparison |
| `src/storage/mod.rs` | `Storage` trait, `Object`, `Metadata`, `Options` (+ manual `Default`), per-side options |
| `src/storage/s3.rs` | S3 backend: client construction (`S3::new`), list, upload, download, copy, delete |
| `src/storage/fs.rs`, `url.rs` | Local backend; `Url` parsing, wildcards, relative paths |
| `src/fastpath/` | `fast` feature: `client.rs`, `sign.rs`, `runtime.rs` |
| `src/mount/` | `mount` feature: `fs.rs` (fuse3 adapter), `vfs.rs`, `inode.rs`, `reader.rs`, `writer.rs` |
| `src/output.rs` | Text or `--json` result lines, dry-run prefix, colour |
| `src/progress.rs`, `ratelimit.rs`, `rlimit.rs` | Progress bar; token-bucket bandwidth limiter; `RLIMIT_NOFILE` handling |
| `src/s3cfg.rs` | `.s3cfg` parsing and translation; `~/.rs5cmd` |
| `src/bin/s3stub.rs` | `bench-stub` feature: discard-everything S3 stub |
| `tests/` | e2e tests that run the built binary against MinIO |
| `bench/`, `packaging/`, `.github/workflows/release.yml` | Benchmark harness; nfpm packages; release pipeline |

Module by module, with the main functions: [code map](references/code-map.md).

## Adding a command or flag

Checklist. The full walk-through with code is in
[adding a command](references/adding-a-command.md).

**New command**

1. `src/command/<name>.rs`: `#[derive(Args)] pub struct <Name>Args` and
   `pub async fn run(global: &GlobalOpts, args: <Name>Args) -> anyhow::Result<()>`.
2. `src/command/mod.rs`: `mod <name>;`, a `Command` variant (its doc comment is the help text),
   and an arm in the `match command` dispatch. A feature-gated command needs the same
   `#[cfg(feature = "...")]` on all three.
3. Get the client settings from `global.storage_options()`. Do not read flags or the
   environment again.
4. Print through `crate::output` so `--json` and `--dry-run` work.
5. Add an e2e test in `tests/e2e.rs`, and unit tests next to pure logic.
6. Document it: `README.md` and `agents/skills/rs5cmd-user/references/commands.md`.

**New per-command flag**: add the field to that command's `Args` struct, use it in `run`, test
it. If `cp` gets it, `mv` has it too (both use `CpArgs`); decide whether `sync` needs it and
whether the fast path honours or ignores it.

**New global flag**: this touches five places, and missing one gives a flag that parses but does
nothing somewhere.

1. Field in `GlobalOpts` with `#[arg(long, global = true)]`.
2. `GlobalOpts::storage_options()` → a field in `storage::Options` **and** in its manual
   `impl Default`.
3. Consume it where clients are built (`S3::new`) or where the command runs.
4. `build_argv` in `src/command/run.rs`, or the flag is lost for every line of a `run` file.
5. The explicit `GlobalOpts { .. }` literal in the `run.rs` test module, or the tests stop
   compiling.

Then decide: should `--s3cfg` / `~/.rs5cmd` set it (`apply_translated`), and should the fast
path (`try_fast_path` in `cp.rs`) honour it? If not, list it in `fast_unsupported`.

## Conventions and pitfalls

Each of these was checked in the code.

- **Struct fields and explicit literals.** `Object` derives `Default`, but several sites build
  it with every field spelled out: in `src/storage/s3.rs` (the list paths), in
  `src/command/head.rs`, and in a `head.rs` test. Adding a field breaks them. The same holds for
  `GlobalOpts` in the `run.rs` tests and for `Options` in its `Default` impl. After adding a
  field, grep for `<Type> {` and build with `--tests` for every feature set.
- **Worker pools for per-object work use `tokio::spawn` with a bounded `JoinSet`** (`cp.rs`,
  `sync.rs`): spawn until `set.len() >= workers`, then `join_next()`. Building and signing a
  request costs CPU. `buffer_unordered` polls all futures inside one task on one thread, so it
  serializes that CPU. It is still used, on purpose, for I/O fan-out inside one object
  (multipart parts and ranged reads in `s3.rs`, the mount reader).
- **`run.rs` uses `FuturesUnordered` without spawning**, because a dispatched command future is
  not `Send` (the fast path holds thread-local state). Do not "fix" it into `tokio::spawn`.
- **Fast-path futures are `!Send` and live on one thread.** Nothing from tokio may be awaited
  inside `src/fastpath/`.
- **Tests must never be able to hang `cargo test`.** No test sends a signal to a child it then
  waits on without a deadline; the SIGINT exit code is covered by a unit test of the mapping
  only. If you write such a test: spawn without blocking, signal after a short sleep, poll
  `try_wait` under a hard timeout, and kill and fail on timeout. The same goes for anything that
  mounts FUSE or starts a server.
- **Output goes through `output.rs`.** Use `op_success` / `op_error` for an operation and
  `is_json()` + `json_line` for records. A bare `println!` ignores `--json`. `pipe` and `tree`
  still do this.
- **Dry-run is implemented where the mutation happens**: `Options::dry_run` is checked inside
  the `S3` and local backends and in a few commands, and `output::is_dry_run()` adds the
  `(dry-run)` prefix. A new mutating call needs its own guard, and must still report the
  operation it skipped.
- **Errors**: return `anyhow::Result`. A batch command reports each failure, keeps going, and
  ends with `anyhow::bail!("one or more ... failed")` so the exit code is 1.
- **Dependencies**: `Cargo.lock` is committed and releases build with `--locked`. Some helpers
  are hand-rolled to avoid new crates (libc FFI in `rlimit.rs`, ANSI colour in `output.rs`, the
  token bucket in `ratelimit.rs`). A new dependency needs a reason, and must build for the
  glibc 2.28 release targets and for macOS.
- **Tests cover MinIO only.** Say so in the change description when behaviour may differ on AWS.

Known defects in v0.1.0, with where to look, are listed at the end of
[code map](references/code-map.md).

## Benchmarks

```bash
docker compose run --rm bench                                    # Go s5cmd vs rs5cmd, vs MinIO
docker compose run --rm -e STUB=1 -e RS5CMD_FEATURES=fast \
  -e VARIANTS="s5cmd rs5cmd rs5cmd-fast" bench                   # vs the in-process s3stub
```

The `bench` image compiles upstream s5cmd from source. Its build context is the parent
directory: the checkout must be in a directory named `rs5cmd` next to a clone of s5cmd named
`s5cmd` (with its `vendor/` tree). Knobs (`N`, `SIZE`, `WORKERS`, `ITERS`, `QUICK`), `s3stub`
and how to read the table: [testing and bench](references/testing-and-bench.md). Results belong
in `bench/RESULTS.md` together with the workload parameters. Measure every variant in the same
run; never compare against numbers from another session or machine.

## Release and packaging

- Pushing a tag `v*` runs `.github/workflows/release.yml`: Linux x86_64 and aarch64 with
  `cargo zigbuild --release --locked --target <triple>.2.28 --features fast`, macOS x86_64 and
  aarch64 with plain `cargo build --release --locked` (no features). Each target yields a
  stripped binary in a `tar.gz` plus a `.sha256`.
- A second job repackages the Linux binaries with nfpm (`packaging/nfpm.yaml`,
  `packaging/build-packages.sh`) into `.deb`, `.rpm` and Arch `.pkg.tar.zst`.
- glibc 2.28 is the floor because the fast-path runtime needs `statx`. There is no musl build.
- Release binaries do not include the `mount` or `sso` features.

Steps, version bumps and local package builds: [release](references/release.md).

## Commits

- Subject in conventional style, as in the history: `feat:`, `fix:`, `perf:`, `docs:`, `ci:`,
  `chore:`, with an optional scope and upstream reference, for example
  `fix(sync): accept a missing local destination (#123)`.
- **No AI attribution of any kind**: no `Co-Authored-By` trailer naming an AI or assistant, no
  "Generated with" line, no mention of an AI tool as author or helper in the subject or body.
  The rule holds even where an older commit in the history looks different. Author and
  committer stay the configured git user.
- Do not commit generated or local files: `target/`, `bench/certs/`, editor and agent state.
- Commit or push only when the person you work for asks.

## Performance work

The current optimization plan (CPU, memory, binary size and wall-clock against s5cmd) is tracked
in <https://github.com/mherkazandjian/rs5cmd/issues/1>. Read it before starting performance
work, and measure with the bench harness before and after each change.
