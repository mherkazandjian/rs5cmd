# rs5cmd

A Rust port of [s5cmd](https://github.com/peak/s5cmd) — a fast S3 and local
filesystem execution tool — with an optional **io_uring fast path** that beats
Go s5cmd on small-object throughput and CPU efficiency.

Development and testing are fully containerized (no Rust toolchain needed on the
host); the suite runs against a MinIO S3-compatible server via docker-compose.

- **14 commands**: `ls cp mv rm cat mb rb sync du pipe head presign select run bucket-version`
- **Two transfer engines**: a portable `tokio` + `aws-sdk-s3` path (default), and
  an opt-in `--fast` io_uring path (Linux, `fast` feature) for many small objects.
- **Tested end-to-end** against MinIO: 92 tests on the default build, 99 with the
  fast path (`cargo test --features fast`).

## Features

- **Transfers** (`cp`/`mv`) in every direction — local↔S3 and S3↔S3 (server-side
  copy) — with wildcard, prefix, and recursive expansion over a concurrency-limited
  worker pool. Large objects use concurrent **multipart upload** and **ranged
  parallel download** (`--part-size <MiB>`, `--concurrency <N>`); small objects use
  a single PUT/GET. `mv` deletes the source only after a successful transfer.
- **`sync`** with size-only and size+modtime strategies, `--delete`,
  `--include`/`--exclude` globs, `--exit-on-error`, and a `--max-delete N`
  safety cap that aborts before touching anything if `--delete` would remove
  more than N objects (guards against a misconfigured source wiping the dest).
- **Listing** (`ls`) with `-H/--humanize`, `--storage-class`, `--etag`,
  `--summarize` (totals footer), and JSON output. **`du`** size/count summaries
  (`--exclude`, `--all-versions`, group-by-storage-class).
- **Object versioning** — `ls --all-versions`/`--version-id` (delete markers shown
  distinctly), `cp`/`cat`/`head`/`rm --version-id`, and `bucket-version` to get/set
  a bucket's versioning status.
- **`pipe`** streams stdin to S3 via concurrent multipart for large inputs
  (`--part-size`/`--concurrency`, `--sse`/`--sse-kms-key-id`).
- **`select`** runs S3 Select over a single object or a wildcard/prefix set
  (sorted, sequential streaming; `--exclude`; Glacier objects skipped).
- **`cat`** streams one object or concatenates a wildcard set; **`head`** prints
  object metadata (full JSON) or checks a bucket; **`presign`** mints GET or
  `--put` URLs with a configurable `--expire`.
- **`run`** executes newline-delimited commands from a file or stdin, propagating
  global flags, with bounded concurrency.
- **Cross-cutting**: `--json` structured output (one object per result line),
  `indicatif` progress bars (`cp`/`mv`/`sync`, auto-suppressed under `--json` /
  non-TTY), `--retry-count` with error-classified exponential backoff
  (transient/5xx retried, permanent 4xx fail fast), `--dry-run`,
  `--use-list-objects-v1` (for providers like GCS), `--no-sign-request`, and
  `--no-verify-ssl`.

## Usage

```
rs5cmd [--endpoint-url URL] [--region R] [--profile P] [--json]
       [--no-sign-request] [--no-verify-ssl] [--use-list-objects-v1]
       [--retry-count N] [--dry-run] [--numworkers N] <command>

  ls   [s3://bucket[/prefix]] [--summarize]  list buckets or objects
  cp   <src> <dst>                 copy (local↔s3, s3↔s3); wildcards, --fast
  mv   <src> <dst>                 move (copy then delete source)
  rm   <target>...                 remove objects (wildcard/prefix, --include/--exclude)
  cat  <s3://bucket/[key|*]>       stream object(s) to stdout (wildcard concatenates)
  mb   <s3://bucket>               make bucket
  rb   <s3://bucket>               remove bucket
  sync <src> <dst>                 sync (--delete, --size-only, --include/--exclude,
                                   --exit-on-error, --max-delete N)
  du   [s3://bucket/prefix/*]      summarize size/count (--exclude, --all-versions)
  pipe <s3://bucket/key>           upload stdin (--sse, --sse-kms-key-id, multipart)
  head <s3://bucket[/key]>         print object metadata / check bucket
  presign [--expire D] [--put] <s3://bucket/key>   presigned GET (or PUT) URL
  select [-e SQL] <s3://bucket/[key|*]>            run an S3 Select query
  run  [file]                      run newline-delimited commands from file/stdin
  bucket-version [--set S] <s3://bucket>           get/set versioning (Enabled/Suspended)
```

Large-object knobs (`--part-size`, `--concurrency`) and version flags
(`--version-id`, `--all-versions`) apply where relevant; see `--help`.

## io_uring fast path (`--fast`, Linux-only, `fast` feature)

A high-throughput path for **many small objects**, built on `monoio`
(thread-per-core io_uring) + `monoio-transports` (pooled HTTP/1.1) + `aws-sigv4`
(SDK-free signing) — bypassing the tokio/aws-sdk-s3 control plane. Work is sharded
across cores, one io_uring ring per core, with async file I/O via `monoio::fs`.

```bash
# Direct use (needs --endpoint-url and AWS creds in the environment):
rs5cmd --endpoint-url http://host:9000 cp --fast "dir/*" s3://bucket/p/
```

It handles upload, download, S3→S3 server-side copy, and mixed-direction batches
(only local→local falls back to the default path), and honors `--json`,
`--dry-run`, `--retry-count`, `mv`, HTTP/HTTPS (rustls; webpki roots by default,
so it works against real AWS), the standard AWS credential provider chain
(env → profile → SSO → IMDS), and `--no-verify-ssl` for self-signed endpoints.

### Benchmark vs Go s5cmd

10k × 4 KB objects, best of 3 (full analysis in `bench/RESULTS.md`). Against an
in-memory discard-S3 stub (`STUB=1`) that removes the server bottleneck and
exposes the *true client ceiling*:

| op | Go s5cmd | rs5cmd `--fast` | |
|----|---------:|----------------:|---|
| upload   | 21,739 req/s | **24,390 req/s** | 1.18× faster, **5.6× less CPU** |
| download | 17,544 req/s | **28,571 req/s** | 1.69× faster, **2.9× less CPU** |

The fast path beats Go s5cmd on **both** operations in wall-clock *and* CPU.
(Against MinIO directly, throughput is server-capped, but the 2–6× CPU-per-request
efficiency win holds.)

```bash
# Reproduce the stub benchmark (needs the hardened io_uring seccomp profile):
docker compose run --rm -e STUB=1 -e RS5CMD_FEATURES=fast \
  -e VARIANTS="s5cmd rs5cmd rs5cmd-fast" bench
```

`--no-verify-ssl` is honored on **both** paths (skips TLS certificate verification
for self-signed HTTPS endpoints): the `--fast` path uses a no-verify rustls config;
the default (SDK) path supplies a custom hyper-rustls connector via the SDK's
`http_client` hook (its built-in TLS stack has no skip-verify option). Use only
for trusted self-signed dev endpoints — it also disables hostname checking.

## Develop & test (Docker)

```bash
docker compose build dev                       # build the dev/build image

docker compose run --rm test                   # full suite vs MinIO (cargo test --all)
docker compose run --rm test-fast              # suite WITH --features fast (io_uring;
                                               #   hardened seccomp profile)

docker compose run --rm dev bash               # interactive build shell
docker compose run --rm --no-deps dev cargo build           # default build
docker compose run --rm --no-deps dev cargo build --features fast

docker compose run --rm dev target/debug/rs5cmd mb s3://demo   # drive vs MinIO
```

The `dev`/`test` services preset `AWS_ENDPOINT_URL=http://minio:9000` and dummy
credentials, so S3 operations hit MinIO. e2e tests self-skip when no endpoint is
configured.

The `--fast` path needs io_uring, which Docker's default seccomp profile blocks;
the `test-fast` and `bench` services run with a hardened profile
(`seccomp-iouring.json` = Docker default + the three `io_uring_*` syscalls). The
HTTPS / `--no-verify-ssl` checks use a self-signed cert for the `minio-tls`
service — generate it once with `bench/gen-certs.sh` (the cert is git-ignored).

### Testing notes

All e2e tests run against **MinIO only** — there is no coverage against real AWS
S3. Behaviors that differ subtly on real S3 (multipart edge cases, versioning
semantics, server-side encryption) are exercised against MinIO and should be
validated on real S3 before relying on them in production. `--fast` e2e tests live
in `tests/e2e_fast.rs` (feature-gated; run via `test-fast`).

## Architecture

```
src/
  main.rs               binary entry (clap parse → dispatch)
  lib.rs                module wiring
  strutil.rs            wildcard→regex helpers
  error.rs              JobError, warnings, cancellation detection
  output.rs             text vs --json result emission
  progress.rs           indicatif progress-bar wrapper (no-op in --json/non-TTY)
  storage/
    url.rs              URL type, glob matching, prefix/filter (faithful to Go path/filepath)
    mod.rs              Storage trait; Object / Metadata / Bucket / Options types
    fs.rs               local filesystem backend
    s3.rs               S3 backend (aws-sdk-s3): list (V2/V1/versions), multipart,
                        ranged download, copy, delete, versioning, no-verify TLS
  command/              one module per command (clap args + run())
    mod.rs ls cp rm cat bucket bucket_version sync sync_strategy du pipe head
    presign select run
  fastpath/             io_uring fast path (Linux-only, `fast` feature)
    mod.rs client.rs sign.rs runtime.rs
  bin/s3stub.rs         in-memory discard-S3 server for the client-ceiling benchmark
tests/
  e2e.rs                end-to-end tests vs MinIO (default features)
  e2e_fast.rs           fast-path e2e (cfg(feature = "fast"))
bench/                  benchmark harness (Go s5cmd vs rs5cmd), Dockerfile, RESULTS.md
```

The default path keeps the `tokio` + `aws-sdk-s3` control plane (listing, bucket
ops, presign, large-object multipart, correctness reference). The fast path is a
self-contained alternative engine for the small-object hot loop; both share the
`storage::url` and `storage` types.

### Notes on fidelity

- `sync`'s default (size + modtime) strategy matches s5cmd exactly: a file is
  re-copied if its source mtime is after the destination's or sizes differ.
  Because S3 `LastModified` is truncated to whole seconds, a file modified in the
  same second it is uploaded can be re-copied on the next run (same as upstream
  s5cmd; use `--size-only` for deterministic comparison).
- `du`/`sync` on a bare prefix list non-recursively (S3 delimiter), matching
  s5cmd; pass a wildcard (`s3://bucket/prefix/*`) to recurse.

## Not in scope

Real-AWS test coverage, byte-level progress bars, bucket policy/tagging/lifecycle
commands, and `cp` arbitrary-metadata flags (`--metadata` map, `--expires`,
`--content-language`) are intentionally out of scope for this port.

## License

MIT (matching upstream s5cmd).
