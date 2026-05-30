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
| **`--max-delete N` safety cap** | PR#699 | No guard against a runaway `--delete`. | rsync-style cap on number of deletions; natural companion to the `--delete` fixes. |

**Recommended first PR.** Bundle these as one "sync/listing correctness +
exit-code/stream hygiene" change with targeted MinIO tests for each.

---

## Tier 2 — Genuine capability gaps

Real missing behavior (not just polish).

| Item | Refs | Gap | Effort |
|------|------|-----|--------|
| **Multipart server-side copy > 5 GiB** | PR#856 | Plain `CopyObject` fails on sources > 5 GiB; rs5cmd's S3→S3 `cp`/`mv` has this exact gap. Fall back to `UploadPartCopy` on `EntityTooLarge`. | M |
| **Skip per-object HEAD when no progress bar** | PR#793 | Remote→local `cp` issues an extra HEAD per object just to size the bar; wasteful when no bar is shown. Clean perf/cost win that complements the io_uring fast path. | S |
| **Full `--profile` credential chain** | PR#847 | `--profile` should use the shared-config chain (SSO, assume-role), not just `~/.aws/credentials`. | S |
| **Session-token renewal on expiry** | PR#683 | Long-running jobs with web-identity creds fail on `ExpiredToken`; re-read the projected-token file. Relevant only for very long transfers. | M |
| **Glacier transfer controls** | v2.0.0 | rs5cmd hard-errors on Glacier objects with no override; add `--force-glacier-transfer` / `--ignore-glacier-warnings`. | M |

---

## Tier 3 — Low-effort parity features

Small, well-scoped, mostly additive.

| Item | Refs | Notes | Effort |
|------|------|-------|--------|
| `ls --show-fullpath` | PR#599/#601 | Prints absolute key, suppresses columns; script/`xargs`-friendly. | S |
| `ls --start-after` | PR#850 | Maps directly to `ListObjectsV2 StartAfter` for resume/pagination. | S |
| `sync` progress + "nothing to sync" message | #787, #796, #753 | Reuse the existing cp progress renderer at op-count granularity (not byte-level); emit a line on no-op. | S/M |
| `--exclude-from` / `--include-from` | #868 | Read filter patterns from a file (with `#` comments). | S |
| Exit code 130 on SIGINT | PR#863 | POSIX-correct Ctrl-C exit code. | S |
| Shell completion | v2.1.0 | `clap_complete` makes bash/zsh/fish/pwsh near-trivial. | S |
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
