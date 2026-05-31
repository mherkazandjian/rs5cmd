# TODO — deferred / partial / unverified work

Status as of branch `implement-upstream-portable` (HEAD around `483ddd6`).

All **29 confirmed-portable** upstream items from `UPSTREAM-INTEGRATION.md` were
implemented and committed green (119 unit + 50 e2e, 0 failed; default and
`--features fast` both compile). This file records what is **not** fully done:
deliberate scope cuts, things the MinIO-only test environment cannot verify, and
follow-ups worth doing before trusting these features against real AWS.

---

## 1. Implemented but NOT exercised by the test suite (MinIO limitations)

These compile and are wired, but the single-region, single-endpoint MinIO test
environment cannot actually drive the behavior — tests only confirm flag parsing,
plumbing, and same-endpoint regression. **Validate against real AWS before relying
on them.**

- **`--use-dualstack-endpoint` / `--use-fips-endpoint` (#719)** — sets the SDK
  builder flags, but no IPv6 dual-stack or FIPS endpoint is ever resolved in tests.
  TODO: verify against a real `s3.dualstack.<region>.amazonaws.com` endpoint.
- **Per-side region / endpoint cross-region copy (#514, #700, #816, #702, #671)** —
  `--source-region`/`--destination-region` and `--source-endpoint-url`/
  `--destination-endpoint-url` build two clients and route S3→S3 through a
  download+upload fallback when the sides differ. Tests only run both sides against
  the *same* MinIO. TODO: verify a genuine cross-region and cross-endpoint S3→S3
  copy (correctness + that the server-side fast path is correctly bypassed).

## 2. Deliberately skipped (in-scope but cut)

- **Bucket-region auto-detection (#858)** — NOT implemented. The rest of the
  multi-region cluster (explicit per-side flags + two-client copy) landed, but
  automatic region discovery (HeadBucket `x-amz-bucket-region` header, or
  `GetBucketLocation`, with a per-bucket cache and a no-op fallback for custom
  endpoints/MinIO) was judged too risky to keep green and was left out.
  TODO: implement auto-detect so cross-region "just works" without explicit flags;
  must no-op against `--endpoint-url`/MinIO where `GetBucketLocation` is unreliable.

## 3. Reduced test coverage (feature works, test is weaker than ideal)

- **SIGINT/SIGTERM exit 130 (#615/#863)** — covered by a **unit test** of the
  exit-code mapping only. There is intentionally **no e2e test** that signals a
  running transfer (an earlier e2e that did so hung `cargo test` and stacked
  orphaned containers). TODO: if a hang-proof harness is built (spawn non-blocking,
  signal after a short sleep, `try_wait` with a hard timeout, kill+fail on
  timeout), add a real e2e. Also confirm/define whether SIGINT should **gracefully
  cancel in-flight transfers** or just exit — current behavior returns 130 but does
  not coordinate cancellation of outstanding worker tasks.
- **Glacier transfer (#812)** — `--force-glacier-transfer` guard is **unit-tested**
  against a synthetic GLACIER-storage-class object; MinIO has no Glacier tier, so
  there is no real-restore e2e.

## 4. Scope to verify / possibly finish

- **Bandwidth limits `--limit-upload` / `--limit-download` (#433)** — a shared
  token-bucket limiter (`src/ratelimit.rs`, hand-rolled on tokio, no new crate) is
  wired through the storage layer. **TODO: confirm the DOWNLOAD path is actually
  throttled, not just upload.** The original implementing agent stalled and the WIP
  was salvaged/merged; the upload path is the confident case. Read the limiter call
  sites in `src/storage/s3.rs` (upload / multipart `UploadPart` vs ranged/multipart
  GET write paths) and add a download-side timing test if it's missing. Also confirm
  the cap is genuinely *aggregate* across the concurrent worker pool, not per-task.
- **`ls --local-time` (#822)** — uses libc `localtime_r`/`tm_gmtoff` for the offset
  (the `time` crate's `local-offset` feature wasn't in the offline build cache).
  TODO: if the dependency cache is ever refreshed, consider switching to the
  `time` crate's native local-offset for portability; verify DST correctness.

## 5. Cross-cutting follow-ups

- **No-new-crate workarounds, by necessity** — the Docker build registry is
  offline-cached, so several features avoid otherwise-natural deps: rlimit (#390)
  uses inline libc FFI instead of the `rlimit` crate; `--color` (#88) uses raw ANSI
  escapes instead of `anstream`; bandwidth (#433) hand-rolls the token bucket
  instead of `governor`. TODO: if/when the registry is online, evaluate replacing
  these with the maintained crates.
- **Fast path (`--fast`, io_uring) parity** — these round-2 features target the
  default tokio/SDK transport. The io_uring fast path still compiles, but does NOT
  pick up: `--proxy`, `--limit-upload/--limit-download`, per-side region/endpoint,
  dualstack, or `--color`-aware output. TODO: decide which (if any) the fast path
  should honor.
- **Real-AWS validation pass** — the entire suite is MinIO-only by design. Before
  these land in a release, run a smoke pass against real S3 for: cross-region copy,
  dualstack, Glacier `--force-glacier-transfer`, and SSO/assume-role profiles.
- **`main` merge** — branch `implement-upstream-portable` is complete and green but
  has NOT been merged to `main` (awaiting approval).

---

## Not in this list (already settled — see UPSTREAM-INTEGRATION.md)

The 184-item triage classified everything else as already-implemented (24),
upstream-bug-not-present-in-the-port (41), out-of-scope (29 — tagging, SSE-C,
progress-bar variants, packaging, HTTP/3, etc.), or not-applicable (52 — questions,
Go-internals, CI/docs). Those require no action and are not repeated here.
