# Stage C (C3c): the second bug batch

fix/older-bugs (93e7cfd, PR #28) + one branch per issue, merged `--no-ff` in the order 30, 31,
32, 33, 34 (#34 stacked on #30: rm.rs builds on #30's checks). Every merge conflict was a
keep-both: tests added at the same place in e2e.rs (#31, #33, #34), two unit tests in s3.rs
(#31, #33), one code-map row (#32, #34).

| Issue | Fix | Gate (`cargo test --all --locked`, MinIO up) |
|---|---|---|
| #30 | rm: refuse a bucket or a bare prefix, as s5cmd does (dry runs too) | 236 / 0 (f-30-tests.log) |
| #31 | s3: listed keys decode "+" as a space; copy sources encode a space as %20 | 237 / 0 (f-31-tests.log) |
| #32 | s3: retry 429 Too Many Requests as a throttling error | 237 / 0 (f-32-tests.log) |
| #33 | s3: keep the sub-second part of object times (sync no longer re-copies) | 236 / 0 (f-33-tests.log) |
| #34 | cp: s5cmd's 16 missing flags; rm --all-versions | 253 / 0 (f-34-tests.log) |

The merged tip 3fe8f03: `cargo test --all --locked` 261 / 0 (f-older-tests.log); the
`--features fast` suite 275 / 0 inside an io_uring window (f-older-fast-tests.log,
iouring-window.log). No rustfmt or clippy in the build image (nor in CI): new Rust lines were
checked by hand for width (<= 100), tabs and trailing spaces.

The lean data path (perf/lean-path, PR #29) gets #32 too: ae0817f classifies a 429 as throttling
in the lean retry loop (gate 251 / 0). 609c45a then logs one debug line per lean retry, worded as
the SDK's own (`attempt #N failed with <class>; retrying after <delay>`), for the soak to count
retries (gate 252 / 0, lean-path-tests.log). #31 needs nothing there: lean's list pages go
through the shared decoding, and copies stay on the SDK.

## #32 on MinIO, 429 injected

The soak harness's throttle proxy (bench/soak/throttle_proxy.py) in front of the review MinIO
answers every third request with `429 TooManyRequests`. Each client uploads 60 files of 4 KiB,
lists them and downloads them through it (c3c-throttle.txt):

| Client | Stored | Downloaded | ERROR lines | Wall |
|---|---|---|---|---|
| fix/older-bugs before the batch (93e7cfd) | 40 / 60 | 26 / 60 | 34 | 0.1 s |
| fix/older-bugs after (3fe8f03) | 60 / 60 | 60 / 60 | 0 | 10.1 s |
| the same with `--fast` (epoll) | 60 / 60 | 60 / 60 | 0 | 2.0 s |
| perf/lean-path before (9dd3a84), `--data-path lean` | 40 / 60 | 26 / 60 | 34 | 0.1 s |
| perf/lean-path after (ae0817f), `--data-path lean` | 60 / 60 | 60 / 60 | 0 | 12.6 s |
| perf/lean-path after, default (SDK) path | 40 / 60 | 26 / 60 | 34 | 0.1 s |

The last row is perf/lean-path on its own: the SDK path's 429 classifier is on fix/older-bugs,
so it arrives when the two branches are merged. The walls are the SDK's backoff (full jitter
from 1 s); `--fast` already retried 429.

## Benchmark rows

Rows only where a fix touches a hot path, each against the fix/older-bugs binary the batch
started from (93e7cfd), interleaved with s5cmd. Medians of per-round ratios, fix ÷ before.

#31, the listed-key decode (runs per key): W4 / W10 on the stub (also with stdout to a pipe) and
MinIO; f-31.jsonl.gz, 195 records, 0 failed.

| Endpoint | Row | Rounds | Wall | CPU | Peak RSS | Wall vs s5cmd |
|---|---|---|---|---|---|---|
| Endpoint | Row | Rounds | Wall | CPU | Peak RSS | Wall vs s5cmd |
|---|---|---|---|---|---|---|
| MinIO | W10.du.100k | 10 | 0.991 | 0.992 | 0.945 | 0.670 |
| MinIO | W4.ls.100k | 10 | 0.995 | 0.996 | 1.010 | 0.625 |
| stub | W10.du.100k | 10 | 0.994 | 0.998 | 1.000 | 0.350 |
| stub | W10.du.1m | 5 | 0.984 | 0.985 | 0.925 | 0.335 |
| stub | W4.ls.100k | 10 | 0.975 | 0.977 | 1.000 | 0.308 |
| stub | W4.ls.100k (stdout pipe) | 10 | 0.977 | 0.976 | 1.000 | 0.317 |
| stub | W4.ls.1m | 5 | 0.973 | 0.980 | 1.007 | 0.308 |
| stub | W4.ls.1m (stdout pipe) | 5 | 0.984 | 0.979 | 0.984 | 0.312 |

#33, the object-time conversion (per listed key, and what sync compares): the same rows plus W5
sync, up and down, cold / warm / changed / delete, on the stub and MinIO; f-33.jsonl.gz, 675
records, 0 failed.

| Endpoint | Row | Rounds | Wall | CPU | Peak RSS | Wall vs s5cmd |
|---|---|---|---|---|---|---|
| Endpoint | Row | Rounds | Wall | CPU | Peak RSS | Wall vs s5cmd |
|---|---|---|---|---|---|---|
| MinIO | W10.du.100k | 10 | 1.006 | 0.987 | 0.968 | 0.671 |
| MinIO | W4.ls.100k | 10 | 0.987 | 0.989 | 1.016 | 0.635 |
| MinIO | W5.minio.dn.changed | 10 | 0.996 | 0.997 | 1.006 | 0.520 |
| MinIO | W5.minio.dn.cold | 10 | 1.008 | 1.006 | 1.009 | 0.254 |
| MinIO | W5.minio.dn.delete | 10 | 0.995 | 0.989 | 1.008 | 0.607 |
| MinIO | W5.minio.dn.warm | 10 | 0.981 | 0.955 | 1.013 | 0.600 |
| MinIO | W5.minio.up.changed | 10 | 1.007 | 1.025 | 1.007 | 0.587 |
| MinIO | W5.minio.up.cold | 10 | 0.972 | 0.986 | 1.000 | 0.331 |
| MinIO | W5.minio.up.delete | 10 | 0.991 | 0.999 | 1.003 | 0.611 |
| MinIO | W5.minio.up.warm | 10 | 0.977 | 0.926 | 0.960 | 0.581 |
| stub | W10.du.100k | 10 | 0.981 | 0.983 | 1.012 | 0.353 |
| stub | W10.du.1m | 5 | 0.987 | 0.995 | 1.012 | 0.340 |
| stub | W4.ls.100k | 10 | 0.983 | 0.983 | 0.997 | 0.316 |
| stub | W4.ls.100k (stdout pipe) | 10 | 0.992 | 0.977 | 1.031 | 0.320 |
| stub | W4.ls.1m | 5 | 0.982 | 0.989 | 0.910 | 0.306 |
| stub | W4.ls.1m (stdout pipe) | 5 | 0.980 | 0.967 | 0.998 | 0.310 |
| stub | W5.dn.changed | 10 | 0.979 | 0.984 | 0.992 | 0.255 |
| stub | W5.dn.cold | 10 | 1.005 | 1.007 | 1.000 | 0.133 |
| stub | W5.dn.delete | 10 | 0.990 | 0.989 | 1.013 | 0.330 |
| stub | W5.dn.warm | 10 | 0.996 | 0.995 | 0.997 | 0.328 |
| stub | W5.up.changed | 10 | 1.012 | 1.006 | 1.007 | 0.244 |
| stub | W5.up.cold | 10 | 0.996 | 0.991 | 0.996 | 0.117 |
| stub | W5.up.delete | 10 | 0.993 | 0.997 | 1.004 | 0.307 |
| stub | W5.up.warm | 10 | 1.002 | 1.004 | 1.016 | 0.315 |

Within noise everywhere. The lowest CPU, W5.minio.up.warm (0.93), is the row the fix changes:
before it, a warm sync on MinIO re-uploaded the files written in the same second as their upload
(reproduced: 20 of 20 per round, 0 after).

#34, cp's per-pair overwrite check and object properties, rm's result line: the merged tip
3fe8f03 (all five fixes) against 93e7cfd; f-older.jsonl.gz, 210 records, 0 failed.

| Endpoint | Row | Rounds | Wall | CPU | Peak RSS | Wall vs s5cmd |
|---|---|---|---|---|---|---|
| stub | W1.up.1k | 10 | 1.000 | 0.971 | 0.960 | 1.093 |
| stub | W1.up.10k | 10 | 1.017 | 0.983 | 1.006 | 0.561 |
| stub | W1.dn.1k (first run) | 10 | 1.073 | 1.044 | 1.040 | 1.106 |
| stub | W1.dn.1k (rerun) | 10 | 0.918 | 0.944 | 0.955 | 1.118 |
| stub | W1.dn.10k | 10 | 1.010 | 0.993 | 0.954 | 0.547 |
| stub | W6.rm.10k | 10 | 0.980 | 1.007 | 1.010 | 0.308 |
| MinIO | W1.up.10k | 10 | 1.004 | 0.985 | 0.999 | 0.997 |

W1.dn.1k's first run (interquartile range of the wall ratio 0.95-1.18) was run again with
W1.dn.10k: the rerun went the other way, so it is the noise of a short row. Without its flags
the overwrite check returns before any request, and the object properties are a few empty
setters per request.
