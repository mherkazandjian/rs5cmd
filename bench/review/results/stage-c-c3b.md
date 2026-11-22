# Stage C (C3b): the older bugs as one batch

fix/older-bugs = perf/stage-b a67d1de + one branch per bug, merged `--no-ff` in the order 4, 6,
8, 9, 10, 11, 12, 13 (#10 stacked on #6), + a closing skills commit (known issues and pitfalls
of the bugs fixed on perf/stage-b and in this batch removed, the rest renumbered).

| Issue | Fix | Gate (`cargo test --all --locked`, MinIO up) |
|---|---|---|
| #4 | sync: an empty or missing local destination is an empty destination | 227 / 0 (f-4-tests.log) |
| #6 | s3: `--no-sign-request` sends unsigned requests | 226 / 0 (f-6-tests.log) |
| #8 | output: pipe reports a JSON result with `--json`; tree refuses `--json` | 227 / 0 (f-8-tests.log) |
| #9 | fs: a wildcard source walks a symbolic link to a directory | 227 / 0 (f-9-tests.log) |
| #10 | fast: fall back to the default path for flags `--fast` ignores | 226 / 0 (f-10-tests.log) |
| #11 | run: every global flag given to `run` reaches its lines | 226 / 0 (f-11-tests.log) |
| #12 | du: `-H` prints the size as s5cmd does, with the unit once | 226 / 0 (f-12-tests.log) |
| #13 | e2e: e2e_region skips without an endpoint; `RS5CMD_E2E_REQUIRED=1` makes skips fail | 225 / 0 (f-13-tests.log) |

The merged tip: `cargo test --all --locked` 234 / 0, plus #10's two tests that need
`--features fast` (f-older-tests.log); `cargo check --all-targets --features fast,mount` without
warnings; the `--features fast` suite 248 / 0 inside an io_uring window (f-older-fast-tests.log,
iouring-window.log). #13 by hand: without an endpoint e2e_region's five tests skip, with
`RS5CMD_E2E_REQUIRED=1` they fail, against MinIO they run. No rustfmt or clippy in the build image
(nor in CI): new Rust lines were checked by hand for width (<= 100), tabs and trailing spaces.

## Benchmark rows

Only #9 changes a hot path (the local source expansion under every upload): f-9 against
perf/stage-b's binary (9693674, the w4 binary), interleaved with s5cmd (f-9.jsonl.gz, 333
records, all ok). Medians, f-9 ÷ perf/stage-b:

| Endpoint | Row | Rounds | Wall | CPU | Peak RSS | Wall vs s5cmd |
|---|---|---|---|---|---|---|
| stub | W1.up.1k | 10 | 1.008 | 1.010 | 0.976 | 1.174 |
| stub | W1.up.10k | 10 | 0.976 | 0.992 | 0.984 | 0.570 |
| stub | W1.up.100k | 5 | 0.991 | 0.994 | 0.978 | 0.521 |
| stub | W1.up.deep | 5 | 0.998 | 0.998 | 1.014 | 0.433 |
| stub | W5.up.warm | 10 | 1.001 | 0.994 | 0.972 | 0.329 |
| stub | W5.up.changed | 10 | 1.022 | 1.009 | 0.997 | 0.250 |
| MinIO | W1.up.10k | 10 | 0.985 | 0.987 | 0.995 | 0.986 |
| MinIO | W5.minio.up.cold | 10 | 1.021 | 0.996 | 1.009 | 0.344 |
| MinIO | W5.minio.up.warm | 10 | 1.022 | 1.019 | 1.029 | 0.588 |
| MinIO | W5.minio.up.changed | 10 | 0.945 | 1.014 | 1.043 | 0.592 |
| MinIO | W5.minio.up.delete | 10 | 0.988 | 0.978 | 1.004 | 0.608 |

Within noise everywhere (wall 0.95-1.02, CPU 0.98-1.02): the fix costs nothing measurable.
