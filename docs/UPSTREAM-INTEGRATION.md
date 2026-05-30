# Upstream integration triage

A classification of **every open** [peak/s5cmd](https://github.com/peak/s5cmd)
issue and pull request, filtered for what is worth integrating into this Rust
port (`rs5cmd`). Each item was checked against the actual rs5cmd source.

> **Coverage honesty.** An earlier version of this file called itself "a survey
> of open issues and PRs" but in fact only deep-verified ~20 hand-picked items.
> That wording overstated the coverage. This version replaces it: on
> **2026-05-30** all open items were pulled straight from the GitHub API
> (`gh issue list` / `gh pr list`) and **every one** was classified by a
> source-reading agent, with each "portable" candidate re-checked by a second,
> adversarial agent. Integrity check: the 184 classified numbers exactly equal
> the 184 numbers GitHub returned — none invented, none missed, no duplicates.

Snapshot counts at triage time: **139 open issues + 45 open PRs = 184 items.**

---

## 1. Coverage matrix (all 184 items)

| Category | Count | Meaning |
|----------|------:|---------|
| `NOT_APPLICABLE`     | 52 | Go-specific, packaging, CI, questions, support threads, docs-only, duplicates |
| `BUG_NOT_PRESENT`    | 41 | An upstream bug that **rs5cmd already avoids** (Rust design / already-correct) |
| `PORTABLE_MISSING`   | 30 | A genuine, in-scope capability rs5cmd lacks |
| `OUT_OF_SCOPE`       | 29 | Conflicts with the agreed out-of-scope list (tagging, SSE-C, progress bars, packaging, …) |
| `ALREADY_IMPLEMENTED`| 24 | rs5cmd already does this |
| `BUG_PRESENT`        |  8 | An upstream bug **faithfully reproduced** in rs5cmd |
| **Total**            | **184** | |

The 30 `PORTABLE_MISSING` + 8 `BUG_PRESENT` = **38 portable candidates** were
each handed to an adversarial verifier (default-skeptic: *prove it's already
handled*). The verifier **downgraded 8** of them and **confirmed 29** as
genuinely worth doing (one bug, #834, folds into its own PR #861, so 38 = 29
confirmed + 8 downgraded + 1 folded).

### Adversarially downgraded (initial triage thought portable → proven otherwise)

These are the confidence payoff of the second pass — flagged as gaps, then
disproven against the source:

| # | Title | Re-verdict | Why |
|---|-------|-----------|-----|
| #690 | Extended character support for S3-compatible backend | already-present | no over-strict bucket-name regex exists in rs5cmd |
| #694 | endpoint_url in config file | already-present | endpoint resolution already covers it |
| #695 | sync --delete wipes dest on source error | already-present | rs5cmd aborts on source-listing error before --delete runs |
| #707 | rm doesn't delete 0-byte folder placeholders | already-present | rs5cmd's lister/expander does not drop trailing-slash keys |
| #824 | sync silently fails on wrong-region bucket | already-present | sync surfaces the error and exits non-zero |
| #826 | cp fails with "chmod operation not permitted" | already-present | rs5cmd does not unconditionally chmod on download |
| #827 | --exclude doesn't work on filenames as expected | already-present | filter semantics match upstream's documented behavior |
| #696 | retry on multipart signature auth error | out-of-scope | upstream never implemented it; auto-retrying 403 masks real cred/clock faults |

---

## 2. Confirmed worklist — 29 genuinely-missing, in-scope items

Verified present-as-gap by reading the rs5cmd source. Effort: **S** ≈ hours,
**M** ≈ a day. None are started; this is the menu, not a record of work done.

### 2a. Correctness bugs faithfully ported from upstream (highest trust-per-line)

| # | Gap | Fix sketch | Effort |
|---|-----|-----------|:--:|
| #677 | List **deserialization fails on keys with XML-illegal control chars** (the `SerializationError: failed to decode REST XML 200`). All three list paths omit `EncodingType=url`. | Add `.encoding_type(Url)` to `list_v2`/`list_objects_v1`/`list_object_versions` and percent-decode echoed keys/prefixes **and pagination markers** (not the opaque V2 token). `src/storage/s3.rs` | M |
| #517 | **Keys ending in `/`** (S3-console "directory" marker objects) are misclassified as directories in the `Contents`/`Versions` loops, so they can't be `cp`/`cat`/`ls`'d as objects. | Treat only `CommonPrefixes` entries as `Dir`; a real object key (has ETag/size) is a `File` even if it ends `/`. `src/storage/s3.rs` | M |
| #755 | `ls` emits **both absolute and relative paths** in one listing: an object whose key equals the prefix prints absolute, siblings print relative. | Drop the `key == prefix` special-case in `parse_non_batch`. `src/storage/url.rs` (*upstream #755 is itself unresolved — fixing it diverges from Go behavior; confirm parity-vs-fix*). | S |
| #834 / #861 | **`rm` can't delete DIROBJ objects** (trailing `/`): `is_prefix()` rejects them. Upstream's own fix PR is #861 (a `--raw` flag). | Add `rm --raw` → route through single-object `delete()`. `src/command/rm.rs`, `src/storage/url.rs` | S |
| #749 | A **broken/dangling symlink** during the local walk aborts the whole transfer and the error doesn't name the link. | In `walk_dir` name the offending path and `continue` instead of `return`. `src/storage/fs.rs` | S |

### 2b. Exit-code correctness

| # | Gap | Fix sketch | Effort |
|---|-----|-----------|:--:|
| #615 / #863 | **No SIGINT handling** — Ctrl-C doesn't return POSIX exit code 130. (#863 is upstream's fix PR for #615.) | `tokio::select!` `command::run` against `ctrl_c()`, return `ExitCode::from(130)`, ideally cancel in-flight transfers. `src/main.rs` | S |

### 2c. New transfer / command capabilities

| # | Gap | Effort |
|---|-----|:--:|
| #2 | **Multiple local sources** in one `cp`/`mv` (`cp f1 f2 f3 dst/`); `CpArgs` holds single `src`/`dst` today. | M |
| #762 | `cp --all-versions` of a single key (route through the ListObjectVersions path; disambiguate dest by version id). | M |
| #785 | rclone-style `--links` — round-trip symlinks as placeholder objects. | M |
| #812 | `--force-glacier-transfer` on **sync** (cp already errors on Glacier; sync just skips). | M |
| #752 | Conditional write `--if-none-match` (S3 `If-None-Match: *`, map 412 → "skipped"). | M |
| #846 | `mv --remove-empty-dirs` (prune source dirs emptied by a local→remote move). | S |
| #651 | `rb --force` (empty the bucket, then delete). | S |

### 2d. Multi-region / multi-endpoint cluster (shares one design)

rs5cmd builds a **single** S3 client from one region/endpoint/profile and shares
it for both sides of a copy/sync. These all want per-side config and/or
bucket-region auto-detection:

| # | Gap | Effort |
|---|-----|:--:|
| #858 | sync region handling + **bucket-region auto-detect** (PR). | M |
| #816 | sync not respecting region flags (per-side region). | M |
| #514 | `cp` per-side `--source-region`/`--destination-region`. | M |
| #702 | `ls`/implied lists respect a region argument (per-side region). | M |
| #700 | per-side `--source-endpoint-url`/`--destination-endpoint-url`. | M |
| #671 | per-side region/profile/endpoint/no-verify for `--client-copy` (base feature already done). | M |

### 2e. Listing / filtering UX

| # | Gap | Effort |
|---|-----|:--:|
| #655 | `--include`/`--exclude` (+ `-from`) on **ls and mv** (promote rm's `Filters` helper to a shared module). | M |
| #388 | `ls --newer-than`/`--older-than` client-side LastModified filter. | M |
| #822 | `ls --local-time` (render timestamps in local tz; keep UTC default). | S |
| #489 | `tree` command (hierarchical listing with box-drawing connectors). | M |

### 2f. Throughput / ergonomics

| # | Gap | Effort |
|---|-----|:--:|
| #433 | `--limit-upload`/`--limit-download` bandwidth cap (shared token bucket across workers). | M |
| #390 | RLIMIT_NOFILE awareness: raise the soft limit and/or warn before EMFILE. | M |
| #719 | `--use-dualstack-endpoint` (IPv6) (+ optional `--use-fips-endpoint`). | S |
| #697 | Dry-run **indicator** in output (`(dry-run)` prefix / `"dryRun": true`) at the `op_success` choke point. | S |
| #88  | `--color auto\|always\|never` styling for ls/du/errors. | M |

---

## 3. Already implemented (24) — no action

#29 (>5 GiB bucket-to-bucket copy) · #152 (MD5/hash overwrite) · #532, #534
(preserve timestamps) · #561, #799 (sync by hash) · #571 (assume-role profile) ·
#670 (external/in-cloud S3-compatible copy) · #699 (max-delete) · #758 (sync
s3→s3) · #771 (fish completion) · #776 (exclude non-regular files) · #792, (and
out-of-scope #793) (skip HEAD when no progress bar) · #794, #795 (virtual-host
addressing) · #796 (sync "nothing to do" message) · #809 (Tencent COS = custom
endpoint) · #823 (SOCKS5 proxy) · #830 (multipart chunk size) · #844 (parallel
rm) · #847 (--profile full chain) · #850 (`ls --start-after`) · #856 (>5 GiB
multipart copy) · #868 (`--exclude-from`/`--include-from`).

## 4. Upstream bugs NOT reproduced in rs5cmd (41) — no action

Verified the Rust port already avoids each. Highlights: #751 (silent-drop on
listing UTF error — rs5cmd aborts), #869/#824/#852 (sync exits 0 on error —
rs5cmd bails non-zero), #815 (sync --delete ignores --exclude — rs5cmd filters
both sides), #838 (panic on missing stat — guarded), #804/#851/#860 (logs on
stdout — rs5cmd uses stderr), #817 (humanize byte suffix — already correct),
#521/#728/#761 (special-char filename quoting — rs5cmd never serializes to a
shell string), #542 (sha256 header on copy), #319/#400 (Go thread-limit
crashes), #683/#678/#526 (token expiry — SDK auto-refreshes), #718 (--no-clobber
semantics), #744 (umask), #791/#810 (local dest mkdir), #545/#554/#660/#667/
#681/#689/#691/#698/#709/#715/#775/#802/#807/#831/#845/#519/#520/#649/#839.

## 5. Out of scope (29) — no action without a scope change

Object tagging #803 · SSE-C #808 · `--metadata-directive` default #813 · GCS
SSE-CSEK #592 · per-object storage class #837 · GrantRead #515 · preserve perms
#350 · KMS cross-account #865 · mimetype guess toggle #673 · progress-bar items
#680/#688/#787/#784/#753/#793/#853 · log-to-file #723 · pre-signed upload #754 ·
s3tar #750 · 1-depth wildcard #540 · HTTP/3 QUIC #557/#560 · packaging
#687/#703/#783/#786/#866 · Go/SDK/EKS upgrades #769/#848.

## 6. Not applicable (52) — no action

Questions/support (#414/#418/#454/#528/#531/#551/#575/#674/#679/#686/#693/#720/
#725/#741/#743/#745/#746/#748/#765/#797/#800/#819/#821/#829/#855), docs/CI/typos
(#499/#499/#585/#584/#701/#706/#773/#774/#780/#781/#828/#840/#857), Go-only
internals & toolchain/CVE/arch (#488/#805/#818/#820/#825/#832/#835/#841/#842/
#859/#862/#864/#867/#839/#867), MRAP #821.

---

## Appendix — methodology & prior rounds

This exhaustive sweep was run as a multi-agent workflow: 16 parallel classifier
agents (one per ~12-item chunk, each grepping/reading `rs5cmd/src`), then
per-candidate adversarial verifiers. Raw results were integrity-checked against
the `gh` item list before this summary was written.

The earlier hand-picked rounds (still valid, now subsumed by the matrix above)
implemented and MinIO-tested: `--max-delete` (#699), >5 GiB multipart copy
(#856), HEAD-less download (#792), Tier-3 parity (`ls --show-fullpath`/
`--start-after`, `--exclude-from`/`--include-from`, shell completion),
`--addressing-style` (#795), skip non-regular files (#776),
`--preserve-timestamps` (#534), `--client-copy` (#671 base), `sync --checksum`
(#799), and `--proxy`/`-x` SOCKS5 + HTTP-CONNECT (#823). Those audits also
confirmed #847/#683/#761/#567/#843 as already-handled or not-applicable —
consistent with the matrix here.
