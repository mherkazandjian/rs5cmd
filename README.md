# rs5cmd

A Rust port of [s5cmd](https://github.com/peak/s5cmd) — a fast S3 and local
filesystem execution tool — with an optional **io_uring fast path** that beats
Go s5cmd on small-object throughput and CPU efficiency.

Development and testing are fully containerized (no Rust toolchain needed on the
host); the suite runs against a MinIO S3-compatible server via docker-compose.

- **16 commands**: `ls cp mv rm cat mb rb sync du pipe head presign select run bucket-version completion tree`
- **Two transfer engines**: a portable `tokio` + `aws-sdk-s3` path (default), and
  an opt-in `--fast` io_uring path (Linux, `fast` feature) for many small objects.
- **Tested end-to-end** against MinIO: ~119 unit + ~50 e2e tests on the default
  build (`cargo test`), and the fast path compiles/tests with `--features fast`.

## Features

- **Transfers** (`cp`/`mv`) in every direction — local↔S3 and S3↔S3 (server-side
  copy) — with wildcard, prefix, and recursive expansion over a concurrency-limited
  worker pool. Large objects use concurrent **multipart upload** and **ranged
  parallel download** (`--part-size <MiB>`, `--concurrency <N>`); small objects use
  a single PUT/GET. Server-side S3→S3 copies of objects over the 5 GiB
  `CopyObject` limit transparently fall back to **multipart `UploadPartCopy`**;
  `--client-copy` streams a remote→remote copy through the client instead.
  `--preserve-timestamps` carries the file mtime across as object metadata.
  Non-regular files (sockets/FIFOs/devices) are skipped rather than erroring; a
  broken/dangling symlink is reported and skipped without aborting the rest of
  the walk. `mv` deletes the source only after a successful transfer.
  Additional `cp`/`mv` controls: **multiple sources** in one invocation
  (`cp a b c dst/`), `--all-versions` (copy every version of a key),
  `--links` (round-trip a symlink as a placeholder object, Unix),
  `--if-none-match` (conditional write — skip if the object already exists), and
  `mv --remove-empty-dirs` (prune now-empty local source dirs after a move).
- **Bandwidth caps** — `--limit-upload`/`--limit-download` accept size strings
  (e.g. `10MB`) and throttle aggregate throughput across the worker pool via a
  shared token bucket.
- **`sync`** with size-only, size+modtime, and content-**`--checksum`** (MD5/
  ETag) strategies, `--delete`, `--include`/`--exclude` globs (plus
  `--include-from`/`--exclude-from` files), `--exit-on-error`, and a
  `--max-delete N` safety cap that aborts before touching anything if
  `--delete` would remove more than N objects (guards against a misconfigured
  source wiping the dest), and `--force-glacier-transfer` to include Glacier
  objects instead of skipping them.
- **Addressing style** is selectable with `--addressing-style path|virtual` for
  S3-compatible providers (defaults: path-style for custom endpoints,
  virtual-host for AWS).
- **Proxy** support via `--proxy`/`-x` (or the `ALL_PROXY`/`HTTPS_PROXY`/
  `HTTP_PROXY` env vars): SOCKS5 (`socks5://`, `socks5h://`) and HTTP `CONNECT`
  (`http://`, `https://`), with optional `user:pass@`. Applies to the default
  transport (not the `--fast` io_uring path).
- **Listing** (`ls`) with `-H/--humanize`, `--storage-class`, `--etag`,
  `--summarize` (totals footer), `--show-fullpath`, `--start-after`, JSON output,
  client-side `--newer-than`/`--older-than` time filters, `--include`/`--exclude`
  (plus `--include-from`/`--exclude-from`) globs, and `--local-time` (renders
  timestamps in the system local zone with an offset; default stays UTC).
  Console "directory marker" objects (keys ending in `/`) are treated as real
  objects, and listed keys render with consistent relative paths.
- **`tree`** prints objects under a prefix as a hierarchy with box-drawing
  connectors (`--depth`, `--limit`). **`du`** size/count summaries (`--exclude`,
  `--all-versions`, group-by-storage-class).
- **Deletion**: `rm` with `--include`/`--exclude` (+ `-from`) filters and `--raw`
  to delete a DIROBJ object whose key ends in `/`; `rb --force` empties a bucket
  (honoring `--dry-run`) before removing it.
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
- **`mount`** exposes an S3 bucket/prefix as a local **FUSE** filesystem
  (rclone-style; Linux/macOS, `mount` feature). Reads stream through a per-handle
  **chunked read-ahead** cache (`--vfs-read-chunk-size`, `--buffer-size`,
  concurrent prefetch); writes buffer to a **write-back cache file** uploaded
  (single PUT or multipart) on close. Supports `mkdir`/`rmdir`/`unlink`/`rename`/
  truncate, attribute & directory caches (`--attr-timeout`/`--dir-cache-time`),
  and `--read-only`.
- **Cross-cutting**: `--json` structured output (one object per result line),
  `--color auto|always|never` ANSI styling (honors `NO_COLOR`, suppressed under
  `--json`/non-TTY), `indicatif` progress bars (`cp`/`mv`/`sync`, auto-suppressed
  under `--json` / non-TTY), `--retry-count` with error-classified exponential
  backoff (transient/5xx retried, permanent 4xx fail fast), `--dry-run` (with a
  `(dry-run)` output marker), `--use-list-objects-v1` (for providers like GCS),
  `--no-sign-request`, and `--no-verify-ssl`. Exits with code **130** on
  SIGINT/SIGTERM. On Unix, the open-file limit (`RLIMIT_NOFILE`) is auto-raised
  toward the hard limit and a warning is printed if it's too low for the
  configured concurrency.
- **Endpoint/region**: `--use-dualstack-endpoint` (IPv6) and
  `--use-fips-endpoint`; per-side `--source-region`/`--destination-region` and
  `--source-endpoint-url`/`--destination-endpoint-url`, which route an S3→S3
  copy through a two-client download+upload when the sides differ. *(Note:
  dualstack and genuine cross-region/endpoint copies are wired but not exercised
  by the single-region MinIO test suite.)*

## Usage

```
rs5cmd [--endpoint-url URL] [--region R] [--profile P] [--json]
       [--no-sign-request] [--no-verify-ssl] [--use-list-objects-v1]
       [--addressing-style path|virtual] [--proxy URL | -x URL]
       [--retry-count N] [--dry-run] [--numworkers N] <command>

  ls   [s3://bucket[/prefix]] [--summarize] [--show-fullpath] [--start-after KEY]
                                   list buckets or objects
  cp   <src> <dst>                 copy (local↔s3, s3↔s3); wildcards, --fast,
                                   --preserve-timestamps, --client-copy
  mv   <src> <dst>                 move (copy then delete source)
  rm   <target>...                 remove objects (wildcard/prefix, --include/--exclude)
  cat  <s3://bucket/[key|*]>       stream object(s) to stdout (wildcard concatenates)
  mb   <s3://bucket>               make bucket
  rb   <s3://bucket>               remove bucket
  sync <src> <dst>                 sync (--delete, --size-only, --checksum,
                                   --include/--exclude, --include-from/--exclude-from,
                                   --exit-on-error, --max-delete N,
                                   --preserve-timestamps)
  du   [s3://bucket/prefix/*]      summarize size/count (--exclude, --all-versions)
  pipe <s3://bucket/key>           upload stdin (--sse, --sse-kms-key-id, multipart)
  head <s3://bucket[/key]>         print object metadata / check bucket
  presign [--expire D] [--put] <s3://bucket/key>   presigned GET (or PUT) URL
  select [-e SQL] <s3://bucket/[key|*]>            run an S3 Select query
  run  [file]                      run newline-delimited commands from file/stdin
  mount <s3://bucket[/prefix]> <dir>   mount as a local FUSE filesystem (mount feature)
  bucket-version [--set S] <s3://bucket>           get/set versioning (Enabled/Suspended)
  completion <bash|zsh|fish|powershell|elvish>     print a shell completion script
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

## Mounting (FUSE) (`mount`, Linux/macOS, `mount` feature)

`rs5cmd mount s3://bucket[/prefix] <dir>` exposes a bucket or prefix as a local
filesystem via FUSE — an rclone-style mount with buffering, concurrency, chunked
reads, and multipart writes. It is built on the async `fuse3` binding over the
existing `storage::s3` backend; the VFS core (inode table, attr/dir caches,
chunked reader, write-back cache) is kept independent of the FUSE binding.

```bash
# In the FUSE-capable compose service (provides /dev/fuse + CAP_SYS_ADMIN):
docker compose run --rm test-mount \
  bash -c 'cargo run --features mount -- mount s3://bucket /mnt/s3 & \
           sleep 2; ls -l /mnt/s3; fusermount3 -u /mnt/s3'
```

- **Reads**: a per-open-handle chunked reader fetches large chunks
  (`--vfs-read-chunk-size`, default 4 MiB), keeps a bounded LRU buffer
  (`--buffer-size`, default 16 MiB), fetches missing chunks concurrently
  (`--concurrency`), and prefetches ahead on sequential access.
- **Writes**: each write-opened file is backed by a local **write-back cache
  file**; data is uploaded via the existing single-PUT/multipart `upload` on
  flush/close. Random writes, append, and truncate are supported.
- **Namespace**: directories are synthesized from key prefixes; `mkdir`/`rmdir`
  use a zero-byte `prefix/` marker, and `rename` is copy+delete (a directory
  rename rewrites every key under it — O(n), non-atomic).
- **Caching**: attribute and directory caches (`--attr-timeout`, default 1s;
  `--dir-cache-time`, default 5m), invalidated on local mutations.
- Other flags: `--read-only`, `--cache-dir`, `--allow-other`, `--uid`/`--gid`.

Building needs the `mount` feature; mounting needs libfuse3's `fusermount3`
helper at runtime (the `fuse3` crate speaks the protocol itself, so no libfuse
dev headers are required to build). macOS uses macFUSE.

**Caveats** (S3 is not a POSIX filesystem):

- *Eventually consistent.* Attributes and directory listings are cached
  (`--attr-timeout`, `--dir-cache-time`) and the kernel caches too; changes made
  out-of-band become visible after the TTL. Mutations through this mount
  invalidate the relevant caches eagerly.
- *Namespace ops aren't atomic.* `rename`/`mkdir`/`rmdir` are emulated over S3
  and have inherent check-then-act races. A directory rename copies **then**
  deletes every key under it (O(n)); if it fails midway the source data is
  preserved (copy-before-delete) but the move may be left incomplete.
- *Durability.* Written data is uploaded on `flush`/`close`. Data not yet
  flushed when the mount is interrupted (Ctrl-C / SIGKILL) is lost. If the
  upload on close fails, the local cache file is **retained** (its path is
  logged) and `EIO` is returned, so the bytes aren't silently lost.
- *Limits.* Object keys containing `*`/`?` or non-UTF-8 bytes aren't accessible
  through the mount; POSIX permissions/ownership are synthesized (not stored);
  backend errors other than not-found/exists/not-empty surface as `EIO`.

## Develop & test (Docker)

```bash
docker compose build dev                       # build the dev/build image

docker compose run --rm test                   # full suite vs MinIO (cargo test --all)
docker compose run --rm test-fast              # suite WITH --features fast (io_uring;
                                               #   hardened seccomp profile)
docker compose run --rm test-mount             # suite WITH --features mount (FUSE;
                                               #   needs /dev/fuse + CAP_SYS_ADMIN)

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
  mount/                FUSE mount (Linux/macOS, `mount` feature)
    mod.rs fs.rs vfs.rs inode.rs reader.rs writer.rs
  bin/s3stub.rs         in-memory discard-S3 server for the client-ceiling benchmark
tests/
  e2e.rs                end-to-end tests vs MinIO (default features)
  e2e_fast.rs           fast-path e2e (cfg(feature = "fast"))
  e2e_mount.rs          FUSE mount e2e (cfg(feature = "mount"), Linux)
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
