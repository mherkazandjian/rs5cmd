# Testing and benchmarking rs5cmd

Checked against `docker-compose.yml`, `Dockerfile`, `tests/` and `bench/` at release v0.1.0.

## Compose services

| Service | Runs | Needs |
|---|---|---|
| `minio` | MinIO, the S3 endpoint for all tests. Ports 9000 (API) and 9001 (console) are published | nothing |
| `dev` | A shell (`bash`) in the build image, MinIO started first | nothing |
| `test` | `cargo test --all -- --nocapture`, default features | nothing |
| `test-fast` | `cargo test --features fast -- --nocapture` | `seccomp-iouring.json`, open-file limit 1048576 |
| `test-mount` | `cargo test --features mount -- --nocapture` | `/dev/fuse`, `SYS_ADMIN`, unconfined AppArmor |
| `test-proxy` | as `test`, with `RS5CMD_TEST_SOCKS5` pointing at the `socks5` service | the `socks5` container |
| `minio-tls` | MinIO over HTTPS with a self-signed certificate, host port 9002 | `bench/certs/` (see below) |
| `bench` | `bash bench/run.sh` | the seccomp profile, a sibling s5cmd checkout |

All Rust services share the image `rs5cmd-dev` (`rust:1-bookworm` plus `fuse3`, `clippy`,
`rustfmt`), mount the checkout at `/workspace`, keep the cargo registry in the named volume
`cargo-registry`, and write build output to `target/` in the checkout.

```bash
docker compose build dev
docker compose run --rm test
docker compose run --rm test-fast
docker compose run --rm test-mount
docker compose run --rm test-proxy
docker compose down            # add -v to drop the cargo registry volume too
```

Any service takes another command in place of its default:

```bash
docker compose run --rm test cargo test --test e2e json_output_mode -- --nocapture
docker compose run --rm test cargo test --lib filters
docker compose run --rm test-fast cargo test --features fast --test e2e_fast
docker compose run --rm --no-deps dev cargo clippy --all-targets
```

`--no-deps` skips starting MinIO; use it for pure compile or lint runs.

### Why `test-fast` has a seccomp profile

The fast path runs on io_uring. Docker's default seccomp profile blocks the io_uring system
calls. `seccomp-iouring.json` is the Docker default profile plus those calls, and `test-fast`
and `bench` use it through `security_opt`. Run fast-path tests in `test-fast`, not in `dev` or
`test`, which keep Docker's default profile.

### Optional: HTTPS

```bash
bash bench/gen-certs.sh                      # writes bench/certs/ (git-ignored), needs openssl
docker compose up -d minio-tls
docker compose run --rm -e RS5CMD_TLS_ENDPOINT=https://minio-tls:9000 test-fast
```

`RS5CMD_TLS_ENDPOINT` enables the HTTPS round-trip unit test in `src/fastpath/client.rs`, which
otherwise prints `skipping: no RS5CMD_TLS_ENDPOINT`. Never commit `bench/certs/`: it holds a
private key.

### Environment variables read by tests

| Variable | Effect |
|---|---|
| `AWS_ENDPOINT_URL` or `S3_ENDPOINT_URL` | Present: e2e tests run. Absent: they skip (`tests/common`) |
| `RS5CMD_E2E_REQUIRED=1` | A missing endpoint fails the e2e tests instead of skipping them. The compose `test` service sets it |
| `RS5CMD_TEST_SOCKS5` | Enables `proxy_socks5_transfer` in `tests/e2e.rs` |
| `RS5CMD_TLS_ENDPOINT` | Enables the HTTPS fast-path unit test |
| `RS5CMD_TEST_ENDPOINT` | Endpoint for `tests/e2e_region.rs`, before `AWS_ENDPOINT_URL` / `S3_ENDPOINT_URL` |
| `RS5CMD_TEST_TMP` | Scratch directory for `tests/e2e_region.rs` (default `/tmp`) |

## What the test suite is

- **Unit tests** in `#[cfg(test)]` modules next to the code. They need no endpoint, except the
  fast-path client round trips, which skip without one.
- **e2e tests** in `tests/*.rs`. They run the compiled `rs5cmd` binary as a child process
  (`assert_cmd`) against MinIO and check stdout, stderr, exit code and resulting files.
- There is **no CI test job**. `.github/workflows/release.yml` only builds releases. Running the
  suite before a merge is on you.
- Everything runs against MinIO. Nothing is tested against AWS S3.

## Reading a run

One `test result:` line is printed per test target: the library unit tests, the binary's unit
tests, each file in `tests/`, and the doc-tests.

```
test result: ok. <N> passed; 0 failed; 0 ignored; ...     <- library unit tests
test result: ok. <N> passed; 0 failed; ...                <- tests/e2e.rs
...
```

Check, in this order:

1. No `error: could not compile` or `error[E...]`. If the tests do not compile, there are no
   `test result:` lines, and a search for the word `failed` finds nothing. Absence of failures
   is not success.
2. A `test result: ok.` line for every binary you expect, including the feature-gated ones in
   `test-fast` and `test-mount`.
3. No unexpected `skipping:` lines. In the compose services the endpoint is set, so e2e tests
   must run (the `test` service sets `RS5CMD_E2E_REQUIRED=1`, so a missing endpoint fails
   there). On a bare host they all skip and the run is green without testing anything.

`cargo build` does not compile tests. To check that everything compiles without running it:

```bash
docker compose run --rm --no-deps dev cargo build --tests
docker compose run --rm --no-deps dev cargo build --tests --features "fast mount"
```

## Writing tests

- Start every e2e test with the endpoint guard (`mod common;`, `common::endpoint_configured()`),
  and print `skipping: ...` before returning.
- Use a fresh bucket per test (`unique_bucket()`), create it with `mb`, and remove it at the
  end. Tests run in parallel against the same MinIO.
- Use `tempfile::tempdir()` for local files. Do not write into the checkout.
- Assert on behaviour a user sees: output text, JSON fields, exit code, bytes on disk. For
  `--dry-run`, assert that nothing changed.
- **No test may be able to block forever.** `cargo test` waits for every test, and a hung test
  in a container leaves the container behind. Therefore:
  - do not send a signal to a child and then `wait()` on it without a deadline;
  - do not start long-lived children (servers, mounts) without a bounded wait and a kill on
    timeout;
  - for a signal test, spawn without blocking, signal after a short sleep, poll `try_wait`
    under a hard timeout, and on timeout kill the child and fail the test.
  The interrupt exit code (130) is covered by a unit test of the mapping in `src/main.rs` for
  this reason.
- Mount tests must unmount in every path, including failure, or the next run finds a busy
  mount point.
- A test for a bug fix should fail before the fix. Run it once against the old code.

## Benchmarks

### What the harness does

`bench/run.sh` builds rs5cmd in release mode, generates `N` files of `SIZE` bytes, and for each
client in `VARIANTS` creates a bucket, uploads the corpus, downloads it again, and cleans up.
Upload and download are each run `ITERS` times; the best wall time is kept. It prints a
Markdown table with wall seconds, requests per second, client CPU seconds (user + system) and
peak RSS.

```bash
docker compose run --rm bench
docker compose run --rm -e QUICK=1 bench
docker compose run --rm -e RS5CMD_FEATURES=fast -e VARIANTS="s5cmd rs5cmd rs5cmd-fast" bench
docker compose run --rm -e N=50000 -e SIZE=1048576 -e ITERS=2 bench
```

| Variable | Default | Meaning |
|---|---|---|
| `N` | 10000 | Number of objects (`QUICK=1` sets 1000) |
| `SIZE` | 4096 | Object size in bytes |
| `WORKERS` | 256 | `--numworkers` for every client |
| `VARIANTS` | `s5cmd rs5cmd` | Clients to run: `s5cmd`, `rs5cmd`, `rs5cmd-fast` |
| `ITERS` | 3 | Runs per operation; the fastest counts |
| `RS5CMD_FEATURES` | empty | Cargo features for the rs5cmd build. `rs5cmd-fast` needs `fast` |
| `STUB` | 0 | `1`: benchmark against `s3stub` instead of MinIO |

### The sibling checkout

The `bench` image compiles upstream s5cmd from source so both clients run in the same
container. Its build context is the **parent directory** of this repository, and its Dockerfile
path is `rs5cmd/bench/Dockerfile`. So the layout must be:

```
<some dir>/
  rs5cmd/        this repository, under exactly this name
  s5cmd/         a clone of github.com/peak/s5cmd, with its vendor/ directory
```

Without that layout the image build fails. The other services do not need it.

### `s3stub`: measuring the client alone

MinIO is slower than the clients on small objects, so against MinIO the server caps the
throughput. `s3stub` (`src/bin/s3stub.rs`, feature `bench-stub`) is a stateless stand-in: it
accepts and discards uploads, serves synthetic objects and listings, ignores authentication,
and keeps nothing.

```bash
docker compose run --rm -e STUB=1 -e RS5CMD_FEATURES=fast \
  -e VARIANTS="s5cmd rs5cmd rs5cmd-fast" bench

# by hand
cargo run --release --features bench-stub --bin s3stub
```

| Variable | Default | Meaning |
|---|---|---|
| `STUB_PORT` | 9100 | Listen port |
| `STUB_OBJECT_SIZE` | 4096 | Size of every object it serves |
| `STUB_LIST_COUNT` | 10000 | Number of synthetic keys a listing returns |
| `STUB_STATS` | unset | Set to print connection statistics |

With `STUB=1`, `bench/run.sh` builds and starts the stub inside the bench container, passes
`SIZE` and `N` to it, and points every client at it. The stub implements only what the
transfer benchmark needs (create bucket, put, get with ranges, head, list v2, delete). Because
it stores nothing, the download check counts files but cannot verify content. Do not use it for
correctness tests.

### Rules for numbers you can trust

- Measure all variants in one invocation, on an otherwise idle machine. Numbers from different
  sessions or machines are not comparable.
- Report the workload with the numbers: `N`, `SIZE`, `WORKERS`, `ITERS`, MinIO or stub.
- Look at CPU seconds and peak RSS, not only wall time. Against MinIO the wall time is often
  set by the server.
- Check the `downloaded N/N files` line. A fast run that dropped objects is a bug, not a win.
- A change that claims a speed-up needs the before and after table from the same run in the
  change description, and `bench/RESULTS.md` updated if it changes the documented picture.
- The fast path helps most on many small objects and on downloads. At megabyte sizes the
  default engine can be as fast or faster on upload. Test both sizes before generalizing.

The current optimization plan is tracked in
<https://github.com/mherkazandjian/rs5cmd/issues/1>.
