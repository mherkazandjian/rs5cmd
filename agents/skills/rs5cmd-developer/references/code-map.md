# rs5cmd code map

Checked against the source at release v0.1.0. Function names are given instead of line numbers
so the map survives edits; `grep -n 'fn <name>' src -r` finds them.

## Request flow

```
main.rs
  parse Cli (clap)                       raise RLIMIT_NOFILE
  select! { signal -> exit 130 ; command::run(cli) -> exit 0 or 1 }

command::run(cli)                        src/command/mod.rs
  --install-completion                   -> install_completion(), return
  --credentials-file                     -> sets AWS_SHARED_CREDENTIALS_FILE
  import-s3cfg                           -> import_s3cfg::run(), return
  --s3cfg FILE                           -> s3cfg::translate_s3cfg + apply_translated
  ~/.rs5cmd                              -> load_rs5cmd_dotfile + apply_translated
  output::set_json / set_color / set_dry_run      (process-global)
  validate_bandwidth_limits
  match command { Ls => ls::run(&global, args), ... }

<command>::run(global, args)
  global.storage_options()               -> storage::Options
  Url::new / Url::parse                  -> storage::url::Url
  S3::new(&url, &opts)  or  storage::new_client(&url, &opts)
  work                                   -> output::op_success / op_error / json_line
```

`apply_translated` fills only fields the user left unset, so explicit flags win. Credentials
from a config file are exported as `AWS_*` variables only when those are not set.

## `src/command/`

| File | Notes |
|---|---|
| `mod.rs` | `Cli`, `GlobalOpts`, `Command`, `run`. `GlobalOpts::storage_options` is the only bridge from flags to `storage::Options`. `bandwidth_limiters` builds the shared limiters per command. The `completion` command is handled inline in the dispatch |
| `cp.rs` | `cp` and `mv` (`run(global, args, is_move)`). `expand_sources` turns each source into `(src, dst)` pairs; `copy_one` does one transfer; `try_fast_path` hands the pairs to `fastpath`; `expand_all_versions`, `upload_symlink` / `download_symlink` (`--links`), `prune_empty_dirs`. `CpArgs::metadata` turns the object-property flags into a `Metadata`; `Listing` carries `--flatten` and `--no-follow-symlinks` into the expansion; `Overwrite` decides `-n` / `-s` / `-u` per pair (a stat of each side) before `copy_one`, and a kept destination is the silent `Skipped` error |
| `sync.rs` | `collect_source_objects` and `collect_dest_objects` build maps keyed by relative path; the partition into copy / delete sets; `--max-delete` check before any work; copy phase, then delete phase |
| `sync_strategy.rs` | `SyncStrategy::{SizeOnly, SizeAndModification, Checksum}` and `should_sync` |
| `filters.rs` | `Filters::should_skip`, `patterns_with_files`; globs become anchored regexes via `strutil` |
| `ls.rs`, `du.rs`, `tree.rs`, `head.rs`, `cat.rs` | Read-only commands. `ls.rs` also holds the time-filter parsing and `list_buckets` |
| `rm.rs` | Refuses a bucket or a bare prefix (`refused`), expands targets (wildcards; `--all-versions` through the versions listing, cut to the one key when there is no wildcard), then deletes through `S3::multi_delete` (batches of 1000, with version ids) or the local backend |
| `bucket.rs`, `bucket_version.rs` | `mb`, `rb`, `bucket-version` |
| `pipe.rs`, `presign.rs`, `select.rs`, `head.rs` | Each adds methods to `S3` in its own `impl S3` block (`put_multipart_reader`, `presign`, `select`, `head_object`, ...) |
| `run.rs` | Reads lines, splits them with `shell_words`, rebuilds argv in `build_argv`, parses a fresh `Cli` and calls `command::run` again for each line |
| `import_s3cfg.rs` | Writes `~/.rs5cmd` and one profile of `~/.aws/credentials` |
| `mount.rs` | Only `MountArgs`; the implementation is `src/mount/` |

Because each `run` line goes through `command::run` again, the config layering and the
process-global output switches are applied once per line. A line that carries its own `--json`
or `--dry-run` therefore changes the output mode for lines running at the same time.

## `src/storage/`

| File | Notes |
|---|---|
| `mod.rs` | `Storage` trait (`stat`, `list`, `delete`, `copy`), `new_client` (picks S3 or local from the URL), `Object`, `ObjectType`, `StorageClass`, `Metadata`, `Options`, `Side`, `Options::for_side` / `sides_differ` |
| `url.rs` | `Url::new` / `parse`, `is_remote` / `is_bucket` / `is_prefix` / `is_wildcard`, `absolute`, `relative`, `set_relative`, `join`, `matches`. Ported from the Go URL package, including its path arithmetic |
| `fs.rs` | Local backend: `stat`, `list` (`expand_glob`, `walk_dir`), `delete`, `copy`. Local wildcards go through the `glob` crate (shell rules: `*` does not cross `/`), while S3 wildcards are matched by regex in `Url::matches` (`*` crosses `/`) |
| `s3.rs` | `S3::new` builds the SDK client: credentials chain, region, endpoint (`S3_ENDPOINT_URL` fallback is here), addressing style, proxy connector, TLS (`load_ca_bundle`, no-verify), retry count and the `TooManyRequests` classifier (429 is throttling, as `SlowDown`; the SDK's own classifiers skip it), limiters. Then `upload` (`upload_single`, `upload_multipart`), `download` (`download_single`, `download_remaining_parts`), `copy` (`copy_object_single`, `copy_multipart`, `client_copy`, `client_copy_to`), listing (`list_v2`, `list_objects_v1`, `list_object_versions`), `multi_delete` / `delete_chunk`, bucket calls. `object_properties!` sets the object-property headers on a PUT, a multipart Create and a CopyObject alike; `expires_time` parses `--expires` |
| `lean.rs` | The lean data path (`--data-path lean`, D4; P1: GetObject and HeadObject; P2: PutObject and multipart uploads, with CRC32C as a header for in-memory bodies or, with `--checksum-trailers`, an aws-chunked trailer, `AwsChunked`; P3: ListObjectsV2, ListObjectVersions, DeleteObjects, DeleteObject, parsed into the SDK's output types). `Lean` sends the request itself on the SDK's pooled connector (`HttpClients::connectors` in `s3.rs`), signs with `aws-sigv4`, resolves endpoints with the SDK's resolver (`None`, so the SDK answers, for non-SigV4 buckets), retries as the SDK's standard mode (`RETRY_QUOTA`, `classify_*`; 429 as throttling, as rs5cmd's own SDK classifier makes it, #32; each retry logs one debug line worded as the SDK's, `attempt #N failed with <class>; retrying after <delay>`, with method, status and code fields, `log_retry`) and returns the SDK's own error values (`into_error`). `S3::get` / `S3::head` in `s3.rs` are the download seams; uploads, deletes and the versions listing branch on `S3::lean_for` (s3.rs, pipe.rs), with `object_headers` sending what `object_properties!` sets on the SDK request (pipe adds the KMS key id without `--sse`, as its SDK requests do) and `delete_objects` the version ids of `rm --all-versions`; `list_v2` reads `V2Pages` (the SDK paginator or lean pages under its rules) |
| `upload_body.rs` | Upload bodies over 256 KiB (`file_range`): 128 KiB chunks read on the blocking pool under one 4 MiB budget for the whole process. `buffer` / `bytes` (also used for `upload_single`'s whole-file bodies) leave a 64-byte `LEAD` before the data, which keeps TLS uploads fast with mimalloc: read the comment on `LEAD` before changing it |

`list` returns an `mpsc::Receiver<Object>`; an entry with `err` set is an error in the stream,
not a panic. Consumers must check `obj.err`.

S3 listings are requested with `encoding-type=url` (keys with XML-illegal characters), so every
echoed key, prefix and key marker goes through `percent_decode`: form encoding, a space comes as
`+` and a real `+` as `%2B`. A version id marker is not encoded and is used as it comes.

## `src/fastpath/` (feature `fast`)

| File | Notes |
|---|---|
| `runtime.rs` | `run_transfers(items, cfg)`: shards `Transfer` items over pinned monoio threads (`FusionDriver`), each with many transfers in flight; returns `Outcome`s. Retry policy lives here |
| `client.rs` | `FastClient`: pooled HTTP/1.1 and rustls HTTPS per core, `Endpoint`, `HttpStatusError`. Path-style only |
| `sign.rs` | `Signer`: SigV4 with `aws-sigv4` |

The fast path is entered only from `cp.rs::try_fast_path`. That function receives the global
options and the expanded pairs, but not the per-command `CpArgs`, which is why storage class,
ACL, content type and the other copy flags are ignored there.

## `src/mount/` (feature `mount`)

| File | Notes |
|---|---|
| `mod.rs` | `run`: validates the mount point, builds `S3`, the `Vfs` and the fuse3 session, waits for unmount or Ctrl-C, removes the per-mount cache directory |
| `fs.rs` | `S3Fuse`: thin fuse3 adapter that maps kernel calls to `Vfs` and errors to errno |
| `vfs.rs` | The core, independent of the FUSE binding: inode table, attribute and directory caches, open handles, read and write paths |
| `inode.rs`, `reader.rs`, `writer.rs` | Inode ↔ key table; chunked read-ahead reader with an LRU; write-back cache file uploaded on flush or close |

## Top-level modules

| File | Notes |
|---|---|
| `output.rs` | Process-global atomics `JSON`, `DRY_RUN`, `COLOR`. `op_success(op, src, dst)`, `op_success_version(op, src, version_id)` (s5cmd's versioned `rm` line), `op_error(op, src, dst, err)`, `json_line(value)`, `is_json()`, `is_dry_run()`, `paint_*`. In JSON mode `op_error` prints to stdout |
| `progress.rs` | `Progress::new(total, op)`, `inc`, `finish`. Counts objects, not bytes. A no-op under `--json` or when stderr is not a terminal |
| `ratelimit.rs` | `RateLimiter::new(bytes_per_sec)` and `acquire(n)`: token bucket shared through `Arc` |
| `rlimit.rs` | `setup_nofile_limits`, `nofile_warning`, `EMFILE_HINT` |
| `s3cfg.rs` | `parse_ini`, `translate_s3cfg` → `Translated` (values plus warnings), `parse_byte_rate` |
| `error.rs` | `JobError`, `is_cancellation`, `is_emfile`, `format_error` |
| `strutil.rs` | `wildcard_to_regexp` and friends, ported from s5cmd |
| `tls.rs` | `provider()`: the rustls provider (aws-lc-rs) both paths' TLS clients take; `install(post_quantum)` from `command::run` sets its key exchange groups (`--tls-post-quantum`) |
| `bin/s3stub.rs` | Stateless S3 stub for benchmarks (feature `bench-stub`) |

## Tests

| File | Runs when | Covers |
|---|---|---|
| unit tests in `src/**` | always | Pure logic: URL arithmetic, filters, sync strategy, argv rebuild, parsers |
| `tests/e2e.rs` | endpoint set (else skips) | Most commands against MinIO, through the built binary |
| `tests/e2e_s3cmd_compat.rs`, `tests/e2e_cp_all_versions.rs`, `tests/e2e_rm_all_versions.rs` | endpoint set (else skips) | `.s3cfg` handling; `cp --all-versions`; `rm --all-versions` |
| `tests/cp_object_properties.rs` | always (in-test HTTP server) | The headers and metadata directive `cp` sends for the object-property flags |
| `tests/e2e_region.rs` | endpoint set (else skips) | Per-side region and endpoint flags |
| `tests/e2e_fast.rs` | feature `fast`, endpoint set | `cp --fast` round trips |
| `tests/e2e_mount.rs` | feature `mount`, Linux, endpoint and `/dev/fuse` | FUSE mount |

## Known defects in v0.1.0, and where they live

Reproduced with the release binary. The user-facing description of each is in the rs5cmd-user
skill (`agents/skills/rs5cmd-user/references/known-issues.md` in the repository). Remove an
entry from both files when it is fixed, and add a regression test with the fix.

| Symptom | Where to look |
|---|---|
| `sync` cannot switch off `--follow-symlinks` | `SyncArgs` declares it as a plain flag with `default_value_t = true` and has no `--no-follow-symlinks`; `CpArgs` shows the fix |
