# Stage C: scorecard after C2 against the D3 targets

perf/stage-b 9693674 (Stage B plus C2: c-mem, c-multipart, c-mimalloc, c-sso-off, c-tls-pq)
against s5cmd v2.3.0, from the full matrix `w4` (results/w4.jsonl, w4-uring.jsonl,
scorecard-w4.md): 2680 default-path and 426 io_uring records, the same batches as `w3`; the
`--features fast` suite 237 / 0 in its io_uring window. Ratios are rs5cmd ÷ s5cmd (medians), the
end of Stage B (w3, stage-b-final.md) → now. Stub over HTTP unless noted; TLS = the stub over
HTTPS; MinIO over HTTP.

One record failed, a warm-up (not counted in the medians): `W5.minio.up.warm`, rs5cmd, round 0,
where sync re-uploaded 173 of 10,000 unchanged files (fail-w4-W5.minio.up.warm-rs5cmd-r0.out),
the ones the previous client's chain had just rewritten. Not a Stage C change, and documented
(known issue "`sync` may copy a just-uploaded file once more"): rs5cmd keeps a listed object's
LastModified to the whole second, so a file written in the same second as its upload looks
newer at the next sync. On AWS S3, whose times are whole seconds, s5cmd behaves the same; MinIO
returns milliseconds, which s5cmd keeps and rs5cmd drops. Keeping them would match s5cmd on such
servers too (noted for an issue).

## Targets

| D3 target | Rows | w3 → w4 | Met |
|---|---|---|---|
| wall ≤ 0.8, small objects | W1 10k / 100k | stub 0.55–0.63 → 0.49–0.56; TLS 0.24–0.34 → 0.20–0.32; MinIO 0.78–0.97 → 0.78–0.97 | stub, TLS yes; MinIO no (both clients MinIO-bound) |
| wall ≤ 0.8, listing | W4 | 0.33–0.36 → 0.29–0.31 (MinIO 0.64 → 0.63) | yes |
| wall ≤ 0.8, sync | W5 | 0.13–0.39 → 0.12–0.34 (MinIO 0.25–0.64 → 0.25–0.62) | yes |
| wall ≤ 0.8, rm | W6 | 0.22–0.37, 0.75 at 20 ms latency (MinIO 0.91 → 0.93) | stub yes; MinIO no |
| wall ≤ 0.8, run | W8 | 0.56–0.73 → 0.35–0.43 (MinIO 0.56–0.68 → 0.39–0.47) | yes |
| wall ≤ 0.8, cat | W7 | cat 0.26, pipe 0.51 (MinIO cat 0.96, pipe 0.80) | stub yes; MinIO cat no |
| wall ≤ 0.8, large uploads | W3.up | 0.21–0.46 → 0.19–0.47 (TLS 0.30–0.35 → 0.26–0.31, MinIO 0.60–0.65 → 0.55–0.63) | yes |
| wall ≤ 1.0, large downloads | W3.dn | 0.95–1.43 → 0.81–1.10 (TLS 0.96–1.25 → 0.89–1.05, MinIO 1.29–1.54 → 0.94–1.10) | 4 × 1 GiB yes everywhere; 32 × 128 MiB no (1.07 stub, 1.10 MinIO, TLS 1.00); 1 × 4 GiB TLS 1.05 |
| wall never worse | every row | misses: W3.dn above; W2.dn.disk 1.28 → 1.21; W9 single requests 1.33–2.02 → 1.33–2.29; W1 1k 1.14–1.23 (within noise) | no |
| CPU ≤ 0.5, listing / sync / large uploads | W4, W5, W3.up | 0.22–0.24; 0.10–0.16 (MinIO 0.14–0.19); 0.18–0.40 | yes |
| CPU ≤ 0.5, run | W8 | 0.56–0.72 → 0.38–0.48 | yes (new) |
| CPU ≤ 0.8, small objects | W1 | 0.57–0.62 → 0.51–0.53 (1k 0.79 / 0.82 → 0.75 / 0.76; MinIO 0.59–0.65) | yes (1k new) |
| peak RSS ≤ 1.0, every row | every row | misses: W2 1.15–9.17 → 1.26–2.21 (TLS up 1.25); W3.dn 1.09–6.27 → 1.07–1.87; W5.dn.cold 5.42 (MinIO 1.58); W1.up 1.12–1.58 (w3: deep 1.38, others ≤ 1.13); W3.up 1.05–1.36; W7.pipe 1.18–1.36 | no |
| binary ≤ 1.0 (stretch ≤ 0.85) | stripped, `--features fast` | 0.974 → 0.960 (16.94 MB) | firm yes; stretch no |
| no new failures | all | one failed warm-up record, the known sync issue above (present before Stage C); every gate green | no regression |

Rows with a loss: wall 20 → 13, CPU 17 → 8. Peak RSS 26 → 37 rows, but the large misses are
gone; most of the new ones come with mimalloc (its merge commit measured, against glibc,
W1.up.10k 1.12 → 1.42, W2.dn 1.44 → 1.86, W3.dn.4x1g MinIO 1.02 → 1.80), W5.dn.cold predates it.

`--fast` on the `*.c64` rows: the fast engine never received `--concurrency` (#10), so its c64
rows here and in w1–w3 measured it at its own concurrency with 32 MiB parts. The harness now gives
it `--part-size 32` only, which keeps those rows measuring the same thing once #10 sends a
`--fast` run with `--concurrency` to the default path.

## What is left, and why

- **128 MiB downloads** (W3.dn.32x128m): 1.06–1.11 × s5cmd on wall and CPU on the stub and MinIO,
  1.00 over TLS: the per-byte body path; one 4 GiB object over TLS at 1.05.
- **Single requests** (W9): process start-up and SDK set-up, 1.3–2.3 × s5cmd's 4–9 ms; mimalloc
  adds 8–11 % here (accepted in c-mimalloc). The lean data path (C4) cuts the per-request floor,
  not start-up.
- **Peak RSS**: mimalloc where many buffers are in flight (W1 uploads, W2, W3, `pipe`); sync's
  cold download (W5.dn.cold 5.4) is unchanged since Stage B.
- **MinIO rows**: MinIO bounds both clients.

## Decisions taken in Stage C (C2)

- c-mem: uploads over 256 KiB streamed from the file, downloads read against a budget (peak RSS
  with mimalloc on top: W2 up to 9.2 × → 2.2 ×, W3.dn up to 6.3 × → 1.9 ×).
- c-multipart (#17): s5cmd's defaults, 50 MiB parts and 5 parts in flight per file.
- c-mimalloc: mimalloc v3 as the global allocator, transparent huge pages off; W8 `run` CPU now
  under 0.5.
- c-sso-off: the `sso` feature off by default (binary 0.960 × s5cmd).
- c-tls-pq (#25): classic X25519 by default, `--tls-post-quantum` opts in (+22 % CPU per
  handshake for rs5cmd, +45–50 % for Go).
