# Stage C (C4): the lean data path, phases 1 to 3, against D3

perf/lean-path = perf/stage-b a67d1de + perf/l-p1 (GetObject, HeadObject), perf/l-p2 (PutObject,
multipart; CRC32C header on in-memory bodies, `--checksum-trailers` for streamed ones) and
perf/l-p3 (ListObjectsV2, ListObjectVersions, DeleteObjects, DeleteObject), merged `--no-ff`.
Switch `--data-path lean|sdk` (env `RS5CMD_DATA_PATH`), default `sdk`; `--fast` is separate.

Each phase's gated binary ran as two clients that differ only by the switch, `rs5cmd-sdk` and
`rs5cmd-lean`, interleaved with s5cmd v2.3.0, on the phase's rows (c4-p1/p2/p3.jsonl.gz, .log.gz,
scorecard-c4-p*.md, d3-c4-p*.txt). P2 adds `rs5cmd-lean-tr` (lean + `--checksum-trailers`) on
MinIO's streamed-upload rows: MinIO decodes aws-chunked and checks the trailer. Ratios are
rs5cmd ÷ s5cmd (medians); sdk → lean. Stub over HTTP unless noted; TLS = the stub over HTTPS.

## Gates

| Phase | Commit | `cargo test --all --locked` | Integration tests with `RS5CMD_DATA_PATH=lean` (e2e on MinIO included) |
|---|---|---|---|
| P1 | 4039a51 | 235 / 0 (l-p1-tests.log) | 81 / 0 (l-p1-lean-tests.log) |
| P2 | a17b642 | 245 / 0 (l-p2-tests.log) | 86 / 0 (l-p2-lean-tests.log) |
| P3 | 3bc3317 | 250 / 0 (l-p3-tests.log) | 88 / 0 (l-p3-lean-tests.log) |

`cargo check --all-targets --features fast,mount` without warnings for each. Found at the gates
and fixed in the phases' commits: with `--checksum-trailers` the aws-chunked body went as one
chunk, which MinIO refuses above 16 MiB; it now goes in the SDK's 64 KiB chunks, byte for byte as
the SDK frames it (a 60 MiB multipart round trip on MinIO with trailers is byte-identical). The
SDK's DeleteObjects also sends `x-amz-sdk-checksum-algorithm: CRC32`; lean now does too.

## P1: downloads

| Rows | sdk → lean, wall | sdk → lean, CPU |
|---|---|---|
| W1.dn 10k / 100k, stub | 0.50–0.54 → 0.27–0.33 | 0.51–0.52 → 0.25–0.27 |
| W1.dn.10k, TLS | 0.30 → 0.20 | 0.29 → 0.16 |
| W1.dn.10k, MinIO | 0.84 → 0.77 | 0.65 → 0.40 |
| W1.dn.1k, stub | 1.34 → 0.86 | 0.75 → 0.39 |
| W1.dn.deep, stub | 0.49 → 0.28 | 0.50 → 0.26 |
| W8.run.dn, stub / MinIO | 0.38 → 0.32 / 0.40 → 0.34 | 0.39 → 0.33 / 0.41 → 0.34 |
| W5.dn (sync), stub | 0.13–0.33 → 0.08–0.33 | 0.12–0.16 → 0.06–0.16 |
| W2.dn, W3.dn (large) | unchanged within noise (W3.dn.32x128m stub 1.11 → 1.08) | unchanged |

## P2: uploads

| Rows | sdk → lean, wall | sdk → lean, CPU |
|---|---|---|
| W1.up 10k / 100k, stub | 0.52–0.57 → 0.25–0.31 | 0.54 → 0.25–0.26 |
| W1.up.10k, TLS | 0.29 → 0.18 | 0.27 → 0.15 |
| W1.up.10k, MinIO | 0.99 → 0.96 | 0.59 → 0.34 |
| W1.up.1k, stub | 1.13 → 0.85 | 0.76 → 0.47 |
| W1.up.deep, stub | 0.44 → 0.21 | 0.46 → 0.22 |
| W2.up, stub / MinIO | 0.36 → 0.31 / 0.89–0.92 → 0.88–0.91 | 0.33 → 0.28 / 0.38–0.39 → 0.34 |
| W8.run.up, stub / MinIO | 0.44 → 0.38 / 0.50 → 0.49 | 0.45 → 0.37 / 0.50 → 0.44 |
| W5.up (sync), stub | 0.12–0.32 → 0.06–0.32 | 0.10–0.15 → 0.05–0.15 |
| W3.up (large, streamed) | unchanged within noise | unchanged |
| W7.pipe, MinIO | 0.88 → 0.88 | 0.56 → 0.61 |

Trailers (lean → lean + `--checksum-trailers`, MinIO): W2.up wall 0.91 → 0.96, W3.up 0.55–0.63
→ 0.63–0.69, client CPU unchanged (0.34–0.40 → 0.34–0.38): MinIO's decoding and checking costs a
little wall. `pipe` on lean sends a CRC32C header on every part (D6): the CRC over each 50 MiB part
is the CPU it adds. Its peak RSS (1.03 → 1.37) is two modes, about 330 or about 440 MB, that the sdk
client also reaches (2 of its 5 rounds at 439 MB): whether two freed parts are reused before new
ones are allocated, the mimalloc effect w4 recorded for `pipe` (1.18–1.36).

## P3: listings and deletes

| Rows | sdk → lean, wall | sdk → lean, CPU |
|---|---|---|
| W4.ls 100k / 1m, stub (also `--stdout pipe`) | 0.31–0.32 → 0.28–0.29 | 0.24–0.26 → 0.21–0.23 |
| W4.ls.100k, MinIO | 0.63 → 0.62 | 0.28 → 0.25 |
| W10.du, stub / MinIO | 0.35 → 0.31–0.32 / 0.67 → 0.65 | 0.27 → 0.25 / 0.30 → 0.27 |
| W6.rm, stub / MinIO | 0.22–0.31 → 0.20–0.29 / 0.93 → 0.91 | 0.12–0.16 → 0.11–0.15 / 0.21 → 0.20 |
| W5 (sync, listing + transfers), stub | 0.12–0.33 → 0.07–0.32 | 0.10–0.16 → 0.05–0.16 |

Listings stay parse-bound: lean saves the SDK's per-request work, not the XML. W10.du.1m's peak
RSS rises (0.65 → 0.84, below s5cmd's).

## Against D3, with the switch on

- Wall ≤ 0.8 on small objects: now also met on MinIO downloads (0.77); MinIO uploads stay
  MinIO-bound (0.96). The 1k rows come to 0.85–0.86 (sdk 1.13–1.34).
- CPU ≤ 0.8 on small objects: 0.15–0.47 everywhere (sdk 0.27–0.76).
- Unchanged by the switch: large transfers (W3.dn.32x128m stays 1.07–1.08 on the stub, the
  per-byte body path), single requests (W9, start-up), sync's cold-download RSS (W5.dn.cold
  5.2–5.3).
- Wall and CPU at or below sdk on every row within noise (the largest rise +0.02), but W7.pipe's
  CPU (the CRC32C header, intended). Peak RSS rises on W7.pipe (above), W10.du.1m and W8.run.up on
  MinIO (1.41 → 1.50).

Two failed records, both warm-ups (not in the medians): `W5.minio.up.warm`, one for rs5cmd-lean
(p2, 93 files re-uploaded) and one for rs5cmd-sdk (p3, 235 files): the known sync issue (listed
object times kept to the second; MinIO returns milliseconds), on both settings
(fail-c4-p2-*.out, fail-c4-p3-*.out).

## Not on the lean path yet

ListObjects v1 (`--use-list-objects-v1`), CopyObject / UploadPartCopy, bucket operations, the
region lookup, select and presign stay on the SDK with either setting; so does any bucket the
SDK's resolver gives a non-SigV4 scheme (S3 Express) and ARNs. Not mirrored: the SDK's upload stall
check (crate-private), so a lean upload to a server that stops reading waits for the timeout.
