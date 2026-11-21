# Lean data path (D4): plan for approval

Written at the close of Stage B, after the final scorecard
(bench/review/results/stage-b-final.md). Nothing here is implemented: it waits for approval.

## 1. Why

- **The SDK's per-request floor.** One small request through aws-sdk-s3 costs about 148 µs of
  CPU; a lean hyper 1 client with rs5cmd's own SigV4 signer costs about 30 µs (E8; signing is
  ~11 µs of it). On small objects that floor is ~70 % of rs5cmd's CPU.
- **What Stage B could not reach without it.** Of the D3 targets still missed, these are
  request-bound:
  - `run` CPU 0.56–0.72 × s5cmd (target 0.5);
  - single requests (W9) 1.33–2.02 × s5cmd's 5–9 ms. Only partly the data path: configuration
    and credential loading stay with the SDK.

  Small-object CPU (0.57–0.62) and listing CPU (0.29) already meet D3; for them the lean path is
  headroom. The other misses are not the per-request floor: large and 1 MiB downloads (per-byte
  costs of the body path: P1 measures whether a lean body path helps), peak RSS on 1 MiB objects
  (body sizing, independent of D4), and the MinIO rows (server-bound).
- **`--fast` already shows the gain** (small objects on the stub 0.22–0.36 wall and 0.16–0.23 CPU × s5cmd),
  but on monoio / io_uring: its futures are not `Send`, it falls back to epoll where io_uring is
  off, it has its own HTTP stack and a TLS stall, and it covers only part of `cp`. A lean path on
  tokio brings the gain to every default-path command.
- **Upload checksums (D6)** fit the budget only there. Through the SDK, CRC32C cost +26 % CPU on
  large uploads over TLS (the CRC itself ~5 %, the SDK's aws-chunked body copies the rest) and up
  to 2.4 × over plain HTTP, where the SDK signs every chunk (results/b-crc32c-profile.txt).

## 2. Shape

| Concern | Lean path | Stays with the SDK |
|---|---|---|
| HTTP and TLS | rs5cmd's `build_http_client` connector, already under the SDK today: HTTP/1.1, aws-lc-rs, platform trust, proxy, `--no-verify-ssl`, `--ca-certs-file`, connect timeout | – |
| Signing | SigV4 through `aws-sigv4` (the `--fast` signer, moved to tokio), UNSIGNED-PAYLOAD as today | SigV4a and S3 Express: those requests stay on the SDK |
| Credentials | asked of the SDK config's identity cache | every provider (env, profiles, SSO, process, web identity, ECS, IMDS) |
| Region, endpoint, addressing | the SDK's endpoint resolver called with the same parameters (region, path style, dual-stack, FIPS, custom endpoint), so URLs and auth schemes do not drift | configuration loading |
| Retries | the SDK's standard policy: attempts = `--retry-count` + 1, exponential backoff with jitter, the same retryable classes (5xx, throttling, timeouts, connection errors: `connector_error`), clock-skew correction | – |
| Errors | S3 error XML parsed into the error type printed today, so messages stay identical | – |
| Bodies | downloads stream into the file (`write_body`), uploads stream `upload_body` | – |
| Checksums (if approved, P2) | CRC32C computed over each chunk as `upload_body` reads it; a header for in-memory bodies, an aws-chunked trailer framed without copying (STREAMING-UNSIGNED-PAYLOAD-TRAILER) for streamed ones | – |

## 3. Phases

Each phase on its own branch with the Stage B process (tests, D1 window when `--fast` is touched,
before/after rows, `--no-ff` merge).

- **P0, the seam.** One switch per S3 handle, `RS5CMD_DATA_PATH=sdk|lean` (hidden; for A/B
  measurement and as a fallback). The default stays `sdk` until P4.
- **P1, GetObject (whole and ranged) and HeadObject:** downloads, `cat`, download sync. Rows W1.dn,
  W2.dn, W3.dn, W5.dn, W7, W8.run.dn, W9.cp1.
- **P2, PutObject and multipart (Create, UploadPart, Complete, Abort):** uploads, upload sync,
  `pipe`; CRC32C if approved (decision 6). Rows W1.up, W2.up, W3.up, W5.up, W8.run.up.
- **P3, ListObjectsV2 (and versions) and DeleteObjects:** `ls`, `du`, `rm`, the sync listing.
  XML through `aws-smithy-xml`, already compiled. Rows W4, W6, W10, W5, W9.ls1.
- **P4, the default switch**, after the full matrix on both paths and a soak (MinIO, and real S3
  if a test bucket is provided). The SDK path stays behind the switch for one release.

Out of scope, staying on the SDK: bucket operations, CopyObject / UploadPartCopy,
SelectObjectContent, versioning settings, tagging, presigning, and any request whose endpoint
resolves to a non-SigV4 auth scheme.

## 4. Tests

- The e2e suite on both paths (CI runs it twice, `RS5CMD_DATA_PATH=sdk` and `lean`).
- A recording in-test server (as tests/upload_checksum.rs) comparing both paths' requests: method,
  target, signed header set and body, apart from date, signature and user agent.
- Faults: the stub's 503 rate and dropped connections give the same attempt counts and backoff
  on both paths; RequestTimeTooSkewed is corrected.
- Signatures: aws-sigv4's test vectors, MinIO (which verifies them), the TLS tests.
- Error goldens: NoSuchKey, NoSuchBucket, AccessDenied, SlowDown, PreconditionFailed.

## 5. Expected effect

Projections from E8 and the `--fast` rows, not measurements; each phase measures its own.

| Rows | Now (× s5cmd) | Projected |
|---|---|---|
| small objects W1 10k / 100k, CPU | 0.57–0.62 | 0.2–0.35 (E8: request path 1.47–1.49 → 0.29–0.31 CPU-s per 10k) |
| small objects W1, wall (stub) | 0.55–0.63 | 0.3–0.4 (`--fast`: 0.22–0.36) |
| `run` W8, CPU | 0.56–0.72 | under 0.5 (the same floor, once per line) |
| peak RSS, 10k small GETs | 0.76 | ~0.4 (E8: 37 → 18–19 MB) |
| single requests W9 | 1.33–2.02 | little: start-up and configuration loading remain |
| large downloads W3.dn | 1.07–1.43 | unknown: P1 measures it |

Listing gains less (the XML parse remains). Binary size roughly unchanged: the SDK stays for
credentials and configuration, and `aws-sigv4` is already linked for `--fast`.

## 6. Risks

- **Behaviour drift** in what the SDK did implicitly (header defaults, retry quota, checksum
  rules). Mitigated by the narrow scope, the SDK fallback and the differential tests.
- **Maintenance:** S3 behaviour changes the SDK used to absorb become rs5cmd's to track.
- **Retry quota:** the SDK's standard mode also has a retry token bucket; the lean path matches it
  or documents the difference.
- **Real-S3 coverage:** the bench has only the stub and MinIO.

## 7. Decisions needed

1. The shape: hyper 1 + rs5cmd's signer, the SDK kept for credentials, configuration and
   endpoint resolution.
2. The scope (P1–P3) and the list that stays on the SDK.
3. The switch's name, and when to make `lean` the default (P4).
4. Whether `--fast` stays separate or becomes the lean path on io_uring, sharing its code.
5. A real-S3 soak before P4 (needs credentials and a bucket).
6. CRC32C upload checksums in P2 (D6 revisited), and whether streamed bodies may use aws-chunked
   trailers: some S3-compatible servers reject them; s5cmd sends Content-MD5.
