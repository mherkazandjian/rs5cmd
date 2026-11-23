# Stage B: final scorecard against the D3 targets

perf/stage-b 293af6c (waves 0–3) against s5cmd v2.3.0, from the full matrix `w3`
(results/w3.jsonl, w3-uring.jsonl, scorecard-w3.md): 2680 default-path and 426 io_uring records,
no failures; the `--features fast` suite 228 / 0 in its io_uring window. Ratios are rs5cmd ÷ s5cmd
(medians), the Stage A baseline (a3) → now. Stub over HTTP unless noted; TLS = the stub over
HTTPS; MinIO over HTTP.

Rows that moved between the wave 2 and wave 3 matrices (W9 single requests, W2 disk rows) were
re-run interleaved, the wave 2 build against this one (results/w3-check.jsonl): 0.95–1.01 on
wall and CPU, so the moves were noise between batches, not item 12.

## Targets

| D3 target | Rows | a3 → w3 | Met |
|---|---|---|---|
| wall ≤ 0.8, small objects | W1 10k / 100k | stub 0.75–1.37 → 0.55–0.63; TLS 0.35–0.59 → 0.24–0.34; MinIO 0.85–1.26 → 0.78–0.97 | stub, TLS yes; MinIO no (both clients MinIO-bound) |
| wall ≤ 0.8, listing | W4 | 0.51–0.52 → 0.35 (MinIO 0.74 → 0.64) | yes |
| wall ≤ 0.8, sync | W5 | 0.17–1.30 → 0.13–0.39 (MinIO 0.26–1.34 → 0.25–0.64) | yes |
| wall ≤ 0.8, rm | W6 | 1.83–1.84 → 0.25–0.35 (MinIO 2.43 → 0.91) | stub yes; MinIO no |
| wall ≤ 0.8, run | W8 | 3.44–3.79 → 0.56–0.73 | yes |
| wall ≤ 0.8, cat | W7 | cat 0.28 → 0.26, pipe 0.54 → 0.52 (MinIO cat 0.92 → 0.99) | stub yes; MinIO cat no |
| wall ≤ 0.8, large uploads | W3.up | 1.19–1.68 → 0.23–0.32 (TLS 1.06–1.37 → 0.30–0.35, MinIO 0.90–0.97 → 0.60–0.65) | yes |
| wall ≤ 1.0, large downloads | W3.dn | 1.32–2.14 → 1.07–1.43 (TLS 1.53–2.10 → 0.96–1.25, MinIO 1.97–2.35 → 1.29–1.54) | no (TLS 4x1g only) |
| wall never worse | every row | misses: W3.dn above; W2.dn 1.26 → 1.02 (MinIO 1.03, disk 1.28); W1 1k 1.10–1.24; W9 single requests 1.33–2.02 (no region: 348–570 → 1.44–2.02) | no |
| CPU ≤ 0.5, listing / sync / large uploads | W4, W5, W3.up | 0.29; 0.12–0.19; 0.22–0.28 (MinIO 0.36–0.38) | yes |
| CPU ≤ 0.5, run | W8 | 1.40–1.53 → 0.56–0.72 | no |
| CPU ≤ 0.8, small objects | W1 | 0.77–1.05 → 0.57–0.62 (1k 0.79 / 0.82) | 10k / 100k yes; 1k up no (0.82) |
| peak RSS ≤ 1.0, every row | every row | misses: W2.up 5.3–9.2 (was 6.9–21.6); W2.dn 2.9–4.6 (TLS 1.15); W3.dn 1.1–6.3 (was 8.5–110); W5.dn.cold 5.4 (MinIO 1.43); W1.up.deep 1.38, 10k 1.13 (variants 1.12–1.17), 1k 1.10, 100k unpinned 1.03; W3.up.32x128m 1.09–1.21, 4x256m.disk 1.03; W8.run.up 1.07 | no |
| binary ≤ 1.0 (stretch ≤ 0.85) | stripped, `--features fast` | 1.53 (Stage A) → 0.974 (17.20 MB; 0.944 without `sso`); tar.gz 2.08 → 1.41 | firm yes; stretch no |
| no new failures | all | 0 failed records; every gate green | yes |

## What is left, and why

- **Large and 1 MiB downloads** (W3.dn, W2.dn): CPU 1.05–1.41 × s5cmd on 4 × 1 GiB, 32 × 128 MiB
  and 1 MiB objects, peak RSS from the parts in flight: per-byte costs of the body path, not the
  SDK's per-request floor (512 requests for 4 GiB). One 4 GiB object (1.07 wall at 0.52 CPU) is
  bound by its 8 parts in flight (#17's defaults).
- **Single requests** (W9): process start-up and SDK set-up, 1.3–2.0 × s5cmd's 5–9 ms.
- **`run` CPU** (0.56–0.72): the SDK's per-request floor (~148 µs against ~30 µs for a lean client,
  E8). mimalloc (parked) measured W8 0.64 → 0.39.
- **Peak RSS on 1 MiB objects** (W2): whole files up to 1 MiB in memory × 256 workers.
- **MinIO rows**: MinIO bounds both clients.

## Decisions taken in Stage B

- D5 `panic = "abort"`: adopted (wave 1).
- Item 13: glibc kept, upload buffers allocated on the runtime's workers; mimalloc parked.
- Item 11 (`--fast`): ranged downloads (bounded memory, 4 GiB works) at 12–25 % more wall on large
  downloads; shards pinned only with RS5CMD_FAST_PIN=1 (+15 % CPU on small uploads).
- Item 12: aws-lc-rs as the one TLS provider (post-quantum key exchange kept); small `--fast`
  uploads over TLS +10 % wall, +6 % CPU.
- D6 (CRC32C upload checksums): not adopted. Large uploads +26 % CPU over TLS and up to 2.4 × over
  plain HTTP through the SDK (results/b-crc32c-profile.txt); the CRC itself is ~5 %.
