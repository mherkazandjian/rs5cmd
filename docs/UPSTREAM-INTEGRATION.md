# Upstream integration triage

A survey of [peak/s5cmd](https://github.com/peak/s5cmd) **open issues**, **open
PRs**, and **recently-merged work**, filtered for things worth integrating into
this Rust port. Compiled on branch `integrate-upstream` (2026-05-30).

Each item is filtered against what rs5cmd **already implements** (the 14
commands, all-direction transfers, multipart, ranged download, sync strategies,
versioning, `--json`, retries, `--no-verify-ssl`, the io_uring fast path, …) and
against the previously agreed **out-of-scope** list (bucket policy/tagging/
lifecycle, byte-level progress bars, real-AWS test coverage, and `cp`
arbitrary-metadata flags).

Effort: **S** = a few hours, **M** = a day-ish, **L** = multi-day. Numbers are
upstream issue (`#`) / PR (`PR#`) references.

---

## Round 2 (2026-05-30): open-PR feature batch

A second pass implemented a batch of good ideas from upstream **open PRs**.
Implemented (each with MinIO e2e tests):

| PR | Feature | Where |
|----|---------|-------|
| #795 ✅ | `--addressing-style path\|virtual` | `command/mod.rs`, `storage/s3.rs` |
| #776 ✅ | skip non-regular files (sockets/FIFOs/devices) in the local walk | `storage/fs.rs` |
| #534 ✅ | `--preserve-timestamps` (mtime ↔ object metadata) | `cp.rs`, `sync.rs`, `storage/s3.rs` |
| #671 ✅ | `--client-copy` (remote→remote via download+upload) | `cp.rs`, `storage/s3.rs` |
| #799 ✅ | `sync --checksum` (compare MD5/ETag) | `sync_strategy.rs`, `sync.rs` |
| #823 ✅ | `--proxy`/`-x` SOCKS5 + HTTP-CONNECT proxy (env fallback) | `storage/s3.rs`, `command/mod.rs` |

The proxy work adds a custom `tower` connector (SOCKS5 via `tokio-socks`,
HTTP `CONNECT` hand-rolled) wrapped by hyper-rustls and adapted to the SDK's
`http_client` hook, generalizing the former no-verify-only client builder. A
`socks5` + `test-proxy` docker-compose pair routes the `proxy_socks5_transfer`
e2e test through a real SOCKS5 proxy (`docker compose run --rm test-proxy`); it
self-skips in the plain `test` service. **Limitation:** proxy applies only to
the default transport, not the io_uring `--fast` path (monoio-transports has its
own connector).

Checked and found **already handled or not applicable** (no code change):

| PR | Finding |
|----|---------|
| #847 — `--profile` full chain | **Already handled.** `S3::new` (and the fast path's `resolve_credentials`) call `aws_config::defaults(...).profile_name(p)`, which uses the full shared-config provider chain — SSO, assume-role, web-identity — not just `~/.aws/credentials`. The Go bug was specific to s5cmd's own loader. |
| #683 — renew session token on expiry | **Handled on the SDK path:** the default credentials cache (`BehaviorVersion::latest`) refreshes expired temporary credentials automatically. *Limitation:* the io_uring fast path resolves credentials once at startup and signs with the static keys, so a very long fast-path run with short-lived creds would not refresh — acceptable for its small-object-burst use case; noted here for the record. |
| #761 — shell-quoting of special-char filenames | **Not applicable.** rs5cmd's `sync` never serializes filenames into shell command strings — it calls the copy/delete paths in-process with `Url`/`PathBuf`. `run` parses input with `shell_words` (correct shell parsing). The Go `%q` quoting bug has no analogue here. |
| #567 — server time for List | **Not worth a dedicated change.** The clock-skew re-copy concern it targets is now better addressed by the new `sync --checksum` (#799, content-based) and the existing `--size-only` (deterministic); the same-second-truncation caveat is already documented. |
| #843 — explicit up/download buffers | **Already covered** by `--part-size` (the multipart/ranged chunk size) and `--concurrency` (parallel parts), which are the tunables the PR asks for; a separate buffer flag would be redundant. |

---

## Audit (2026-05-30): Tier 1 / easy bugs were checked and are NOT ported

Before integrating anything, every "obvious and easy" Tier-1 bug (plus the easy
formatting/stream items) was verified against the actual rs5cmd source. **None
of them are reproduced in this port** — the Rust implementation already avoids
each one, so no fix was warranted:

| Upstream bug | rs5cmd status | Evidence |
|--------------|---------------|----------|
| #751 / #698 — silent drop on listing error → data loss | already safe | `collect_source_objects` returns `Err` on any listing error (`sync.rs:421-428`); `collect_dest_objects` propagates non-"not found" errors (`sync.rs:475-488`). No silent `continue`. |
| #869 / #824 / #852 — sync exits 0 despite errors | already correct | `sync.rs:281-283`: `if had_error { bail!(…) }`, unconditional (not gated on `--exit-on-error`, which only controls early abort). |
| #815 — `sync --delete` ignores `--exclude` | already correct | include/exclude filters applied to *both* source and dest listings (`sync.rs:437`, `:494`); the delete set is built from the already-filtered `dest_objects` (`sync.rs:153-159`). |
| #838 — panic on missing source stat | already safe | `sync_strategy.rs:62-69` treats unknown timestamps as `UNIX_EPOCH` via `.unwrap_or(…)`; no `unwrap()` to panic. |
| #804 / #860 — errors/logs on stdout | already correct | `op_error` → `eprintln!` (`output.rs:58-61`), run summary → stderr (`sync.rs:278`), tracing → stderr (`main.rs:14`). Payload/JSON only on stdout. |
| #817 — `--humanize` missing byte suffix | already correct | `ls.rs:232-235` uses `humansize::BINARY` (renders `B`/`KiB`). |

The remaining work below is therefore genuine new behavior (Tier 2/3) rather
than ported-bug fixes.

---

## Tier 1 — Correctness & data-safety

The most valuable cluster. These are *bugs in the Go tool* that a fresh Rust
port can get right and lock in with MinIO regression tests. Cheap, high-trust.

| Item | Refs | What's wrong upstream | What rs5cmd should do |
|------|------|------------------------|------------------------|
| **Silent drop on listing error** | #751 | A UTF-8/listing/pagination error truncates the object list; `cp`/`sync` skip files yet exit 0. Reported data loss on a 50 TB transfer. | Lister must surface decode/pagination errors loudly (abort or log+continue), never silently shorten the result set. |
| **sync exits 0 on error** | #869, #824, #852 | Bad path/region prints `ERROR` but the process returns 0, breaking CI/scripts. | Any per-job error propagates to a non-zero exit; audit `--exit-on-error` semantics. |
| **`sync --delete` ignores `--exclude`** | #815 | Excluded destination files are deleted anyway (diverges from `aws s3 sync`). | The delete pass must apply the same include/exclude filters as the copy pass. |
| **`sync --delete` wipes dest when source list fails** | PR#698 | A swallowed listing/network error yields an empty source set, which `--delete` treats as "delete everything." | Never treat a *failed* listing as an authoritative empty source; abort the delete phase on listing error. |
| **stdout/stderr hygiene** | #804, PR#860 | Errors/usage/logs go to stdout, corrupting `cat`/`pipe` pipelines and `--json`. | All diagnostics → stderr; only payload/`--json` → stdout. |
| **Panic on override stat failure** | PR#838 | With size/modtime override, a swallowed not-found on the *source* stat causes a nil deref. | Guard the size+modtime compare against a missing source stat (likely an `unwrap()`/`None` in `sync_strategy`). |
| **`--max-delete N` safety cap** ✅ DONE | PR#699 | No guard against a runaway `--delete`. | rsync-style cap on number of deletions; natural companion to the `--delete` fixes. **Implemented** in `sync.rs`: aborts before any copy/delete if the delete set exceeds N; e2e test `sync_max_delete_aborts_without_touching_anything`. |

**Recommended first PR.** Bundle these as one "sync/listing correctness +
exit-code/stream hygiene" change with targeted MinIO tests for each.

---

## Tier 2 — Genuine capability gaps

Real missing behavior (not just polish).

| Item | Refs | Gap | Effort |
|------|------|-----|--------|
| **Multipart server-side copy > 5 GiB** ✅ DONE | PR#856 | Plain `CopyObject` fails on sources > 5 GiB; rs5cmd's S3→S3 `cp`/`mv` had this exact gap. **Implemented** in `s3.rs`: a single `CopyObject` is tried first, and on the 5 GiB `EntityTooLarge` error it falls back to multipart `UploadPartCopy` (part size auto-grown to stay within the 10k-part limit, source content-type carried over). e2e test `s3_to_s3_multipart_copy_for_large_source` forces the path via `RS5CMD_MULTIPART_COPY_THRESHOLD` and byte-verifies the copy. | M |
| **Skip per-object HEAD when no progress bar** | PR#793 | Remote→local `cp` issues an extra HEAD per object just to size the bar; wasteful when no bar is shown. Clean perf/cost win that complements the io_uring fast path. | S |
| **Full `--profile` credential chain** | PR#847 | `--profile` should use the shared-config chain (SSO, assume-role), not just `~/.aws/credentials`. | S |
| **Session-token renewal on expiry** | PR#683 | Long-running jobs with web-identity creds fail on `ExpiredToken`; re-read the projected-token file. Relevant only for very long transfers. | M |
| **Glacier transfer controls** | v2.0.0 | rs5cmd hard-errors on Glacier objects with no override; add `--force-glacier-transfer` / `--ignore-glacier-warnings`. | M |

---

## Tier 3 — Low-effort parity features

Small, well-scoped, mostly additive.

| Item | Refs | Notes | Effort |
|------|------|-------|--------|
| `ls --show-fullpath` ✅ DONE | PR#599/#601 | Prints absolute s3:// path, suppresses columns; script/`xargs`-friendly. Implemented in `ls.rs`. | S |
| `ls --start-after` ✅ DONE | PR#850 | `ListObjectsV2 StartAfter` (V1 `Marker`) plumbed through `Url`/`UrlOptions` into both list paths. | S |
| `sync` "nothing to sync" message ✅ DONE | #796 | sync already printed a run summary on stderr; now says "nothing to sync" explicitly when no copies/deletes occurred. (Byte-level progress is out of scope; op-count progress bars already exist.) | S |
| `--exclude-from` / `--include-from` ✅ DONE | #868 | `sync` and `rm` read extra filter globs from files (one per line; blank lines and `#` comments ignored), combined with inline `--include`/`--exclude`. | S |
| Exit code 130 on SIGINT | PR#863 | POSIX-correct Ctrl-C exit code. Not yet done. | S |
| Shell completion ✅ DONE | v2.1.0 | `completion <shell>` subcommand via `clap_complete` (bash/zsh/fish/powershell/elvish). | S |
| `ls` local-timezone timestamps | #822, #845 | Optional flag to show local tz like aws-cli. | S |
| `--humanize` byte suffix | #817 | Print `B` suffix for sub-KiB sizes. | S |
| Addressing-style toggle | #794, PR#795 | `--addressing-style path|virtual` for S3-compatible providers. | S/M |
| Proxy support | PR#823 | `--proxy`/`-x` SOCKS5/HTTP(S); mostly HTTP-client config in Rust. | M |
| Skip non-regular files | PR#776 | Don't error on sockets/pipes/devices during cp/sync walk. | S |
| Global `--stat` summary | v1.2.0 | Print op-count/error totals at the end. | M |
| Download integrity verification | #829 | Optional checksum (ETag/CRC) validation after download — a credible rs5cmd differentiator. | M |

---

## Deferred — conflicts with current scope

These overlap the agreed out-of-scope cp-metadata work; listed for completeness
only, **not** recommended without a scope change:

- **`--metadata-directive` (COPY/REPLACE) + content-type propagation on S3→S3
  copy** (PR#668, PR#739). Note the storage layer already references
  `metadata_directive`; only `cp.rs` lacks the flag. The *content-type drop on
  server-side copy* (#739) is arguably a correctness bug rather than a feature,
  if you want to reclassify it into Tier 1.
- Plumbing `--content-encoding` / `--content-disposition` / `--cache-control` /
  `--expires` into `cp` (already wired for `pipe`).
- Object tagging (#803), per-object storage class (#837), SSE-C (#808),
  timestamp-preservation metadata (PR#534).

---

## Excluded (already done, packaging, or non-portable)

Already in rs5cmd: glacier-skip in sync (#712), relative-key sync matching
(#676), S3→S3 multipart copy basics, versioning, dry-run, retries, multiple
wildcard sources (#2). Non-portable / out of scope: AWS SDK v2 upgrade (#832),
Go-toolchain CVE rebuilds (#805/#820/#835), Ubuntu PPA / pip wheels
(#786/#703), Go-only channel-direction fix (PR#864), HTTP/3 QUIC (#560).

---

## Suggested sequencing

1. **Tier 1 correctness bundle** — one change, one test per bug. Highest
   trust-per-line; nothing here is large.
2. **#856 (>5 GiB copy)** and **PR#793 (skip HEAD)** — the two Tier-2 items that
   are both small and clearly worth it.
3. **PR#847 (`--profile` chain)** — foundational auth correctness.
4. Cherry-pick Tier 3 by demand (`ls --show-fullpath`, sync progress, and shell
   completion are the most-requested).
