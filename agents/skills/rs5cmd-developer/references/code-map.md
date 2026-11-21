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
| `cp.rs` | `cp` and `mv` (`run(global, args, is_move)`). `expand_sources` turns each source into `(src, dst)` pairs; `copy_one` does one transfer; `try_fast_path` hands the pairs to `fastpath`; `expand_all_versions`, `upload_symlink` / `download_symlink` (`--links`), `prune_empty_dirs` |
| `sync.rs` | `collect_source_objects` and `collect_dest_objects` build maps keyed by relative path; the partition into copy / delete sets; `--max-delete` check before any work; copy phase, then delete phase |
| `sync_strategy.rs` | `SyncStrategy::{SizeOnly, SizeAndModification, Checksum}` and `should_sync` |
| `filters.rs` | `Filters::should_skip`, `patterns_with_files`; globs become anchored regexes via `strutil` |
| `ls.rs`, `du.rs`, `tree.rs`, `head.rs`, `cat.rs` | Read-only commands. `ls.rs` also holds the time-filter parsing and `list_buckets` |
| `rm.rs` | Expands targets, then deletes through `S3::multi_delete` (batches of 1000) or the local backend |
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
| `s3.rs` | `S3::new` builds the SDK client: credentials chain, region, endpoint (`S3_ENDPOINT_URL` fallback is here), addressing style, proxy connector, TLS (`load_ca_bundle`, no-verify), retry count, limiters. Then `upload` (`upload_single`, `upload_multipart`), `download` (`download_single`, `download_remaining_parts`), `copy` (`copy_object_single`, `copy_multipart`, `client_copy`, `client_copy_to`), listing (`list_v2`, `list_objects_v1`, `list_object_versions`), `multi_delete` / `delete_chunk`, bucket calls |
| `upload_body.rs` | Upload bodies over 256 KiB (`file_range`): 128 KiB chunks read on the blocking pool under one 4 MiB budget for the whole process. `buffer` / `bytes` (also used for `upload_single`'s whole-file bodies) leave a 64-byte `LEAD` before the data, which keeps TLS uploads fast with mimalloc: read the comment on `LEAD` before changing it |

`list` returns an `mpsc::Receiver<Object>`; an entry with `err` set is an error in the stream,
not a panic. Consumers must check `obj.err`.

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
| `output.rs` | Process-global atomics `JSON`, `DRY_RUN`, `COLOR`. `op_success(op, src, dst)`, `op_error(op, src, dst, err)`, `json_line(value)`, `is_json()`, `is_dry_run()`, `paint_*`. In JSON mode `op_error` prints to stdout |
| `progress.rs` | `Progress::new(total, op)`, `inc`, `finish`. Counts objects, not bytes. A no-op under `--json` or when stderr is not a terminal |
| `ratelimit.rs` | `RateLimiter::new(bytes_per_sec)` and `acquire(n)`: token bucket shared through `Arc` |
| `rlimit.rs` | `setup_nofile_limits`, `nofile_warning`, `EMFILE_HINT` |
| `s3cfg.rs` | `parse_ini`, `translate_s3cfg` → `Translated` (values plus warnings), `parse_byte_rate` |
| `error.rs` | `JobError`, `is_cancellation`, `is_emfile`, `format_error` |
| `strutil.rs` | `wildcard_to_regexp` and friends, ported from s5cmd |
| `bin/s3stub.rs` | Stateless S3 stub for benchmarks (feature `bench-stub`) |

## Tests

| File | Runs when | Covers |
|---|---|---|
| unit tests in `src/**` | always | Pure logic: URL arithmetic, filters, sync strategy, argv rebuild, parsers |
| `tests/e2e.rs` | endpoint set (else skips) | Most commands against MinIO, through the built binary |
| `tests/e2e_s3cmd_compat.rs`, `tests/e2e_cp_all_versions.rs` | endpoint set (else skips) | `.s3cfg` handling; `cp --all-versions` |
| `tests/e2e_region.rs` | **always** | Per-side region and endpoint flags. No skip guard |
| `tests/e2e_fast.rs` | feature `fast`, endpoint set | `cp --fast` round trips |
| `tests/e2e_mount.rs` | feature `mount`, Linux, endpoint and `/dev/fuse` | FUSE mount |

## Known defects in v0.1.0, and where they live

Reproduced with the release binary. The user-facing description of each is in the rs5cmd-user
skill (`agents/skills/rs5cmd-user/references/known-issues.md` in the repository). Remove an
entry from both files when it is fixed, and add a regression test with the fix.

| Symptom | Where to look |
|---|---|
| `cp data dst/` (bare relative directory) maps files onto themselves or to keys with `../` | `Url::set_relative` in `storage/url.rs`: for the base `data`, the parent directory is `.`, and the relative path of `data/a.txt` against it comes out as `../data/a.txt`. The existing unit tests cover a wildcard base and an absolute directory base only |
| `sync` to a missing or empty local directory fails with `no match found for "dir/*"` | `collect_dest_objects` in `command/sync.rs` skips only errors containing `no object found` or `not found`; the local backend's message is `no match found for ...` (`storage/fs.rs`) |
| `--dry-run rm 's3://b/p/*'` prints nothing | Dry-run branch of `S3::delete_chunk` in `storage/s3.rs`: `tx.send(...)` is not awaited, so nothing reaches the receiver |
| `--no-sign-request` has no effect | `Options::no_sign_request` is set and propagated but never read in `S3::new` |
| Exit 101 when stdout is closed early | `println!` panics on a broken pipe; nothing handles `EPIPE` |
| `pipe` and `tree` ignore `--json` | They print with `println!` instead of `output.rs` |
| `du -H` prints `18 B bytes` | `size_message` in `command/du.rs` appends `bytes` to an already humanized size |
| `--follow-symlinks` cannot be switched off | Declared as a plain flag with `default_value_t = true` in `CpArgs` and `SyncArgs` |
| A wildcard source fails on a symlink to a directory | Source expansion in `storage/fs.rs` (`expand_glob`) returns the link as one entry; a directory source walks it |
| Slow start without a region | `S3::new` uses the SDK region chain, which queries instance metadata before the `us-east-1` fallback |
| Some global flags are lost inside `run` | `build_argv` in `command/run.rs` forwards a fixed list |
| `tests/e2e_region.rs` fails outside compose | No endpoint guard at the top of its tests |
