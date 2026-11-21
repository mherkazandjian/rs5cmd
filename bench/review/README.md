# Optimization review harness

Measures rs5cmd against upstream s5cmd on wall-clock, CPU time, peak RSS and binary size, as
input to `docs/OPTIMIZATION-REVIEW.md`. Self-contained: `bench/run.sh` and the repository's
`docker-compose.yml` are untouched.

| File | Purpose |
|---|---|
| `Dockerfile` | Toolchain image: Rust, Go 1.22 and the latest Go, zig 0.13, perf, heaptrack, strace, jemalloc, mimalloc. Installs only, nothing is compiled in a `RUN` step |
| `compose.yml` | Compose project `rs5cmd-optreview`: `bench` (toolchain, runner, s3stub, clients), `minio` (pinned by digest), `test` (the e2e gate). No published ports |
| `dc` | Host wrapper: `sudo docker compose -f bench/review/compose.yml` with the caller's uid/gid passed in |
| `build.sh` | Builds every measured binary into the `bins` volume and records sizes in `manifest.jsonl` |
| `launch.c` | Static launcher: pins the command, `exec`, `wait4`; prints wall, user, sys, max RSS, faults, context switches and per-cpuset busy/softirq time as one JSON line |
| `iouring-probe.c` | Can this process create an io_uring? Used inside and outside the container |
| `matrix.py`, `run-matrix.sh` | Workload registry (W1–W10) and the interleaved round runner; appends to `results/<tag>.jsonl` |
| `pack-results.sh` | Gzips finished raw results before they are committed (`results/*.jsonl` → `.jsonl.gz`, logs over 256 KiB → `.log.gz`); the readers take either name |
| `aggregate.py` | JSONL (plain or gzipped) → markdown tables (best, median, min–max, per-round ratios, verdicts) |
| `fast-window.sh` | Host side: opens the io_uring sysctl window around a batch of `--fast` runs and always restores it |
| `profile.sh` | perf (frame pointers + DWARF), flamegraphs, perf stat, strace, heaptrack, smaps at peak; output in `prof/` (git-ignored) |
| `size.sh` | Section sizes, cargo-bloat, duplicate dependencies, Go package sizes, release-profile knob matrix |
| `report.py` | Compact scorecards (baseline absolute, then wall× CPU× RSS× per client with W/L/=), as in `results/scorecard-*.md` |
| `a3.sh`, `a4.sh`, `a5.sh` | The Stage A batches: baseline matrix, profiles (+ one fault-injection run), quick experiments |
| `checks.py` | Small one-off checks: local `cat` RSS (C5), `--fast` CPU pinning (F47), SIGTERM handling |
| `e8.py` | E8, the SDK floor: aws-sdk-s3 against a lean hyper client (`src/bin/sdkfloor.rs`) |

## Resource layout

The harness was written for a shared host and stays inside fixed caps:

- Every container is pinned to one cpuset of 8 logical CPUs (4 physical cores with both
  hyperthreads). Inside it, the server (s3stub, or MinIO in its own container) gets 2 physical
  cores and the client gets the other 2. Override with `CPUSET_ALL`, `CPUSET_SERVER`,
  `CPUSET_CLIENT` and `CPUSET_OTHER` (the CPUs outside the set, watched for neighbour load).
- Memory limits add up to 32 GiB including both tmpfs mounts: bench 18 (10 GiB tmpfs for
  client files), minio 14 (its data lives on a 9 GiB tmpfs, so MinIO does no disk I/O). The
  `test` service (the A1 e2e gate) sits behind the `gate` profile and runs only with `bench`
  stopped; later test suites run inside `bench`.
- Compiles run in the pinned `bench` container with `CARGO_BUILD_JOBS=8` / `GOMAXPROCS=8`;
  image builds only install packages (BuildKit ignores the runtime cpuset).
- Hard block-I/O limits (cgroup `io.max`) on the device behind the docker root, for every
  container: 300 MB/s read, 200 MB/s write, 3000 / 2000 IOPS. `dc` finds the device. (An I/O
  weight alone does nothing on NVMe or md devices.) Rows that write to disk (`.disk`) are
  small: at most 1 GiB per run and 3–5 rounds.
- No run starts while the host's 1-minute load average is above `LOAD_MAX` (default 11); the
  wait is recorded per run (`load_wait_s`).
- Rows too large for these caps are scaled down per endpoint and carry a `scaled` note in the
  results (MinIO: W2 2000 x 1 MiB, W3 at most 2 GiB per run, W7 1 GiB; W4/W10 1M keys and W6
  100k keys on the stub only).

## Sequence

```bash
D=bench/review/dc
$D build bench test
$D up -d minio bench
$D exec -T bench bash bench/review/build.sh                 # all binaries
$D run --rm -T test                                         # e2e gate: cargo test --all vs MinIO
$D exec -T bench bash bench/review/run-matrix.sh list       # the rows
$D exec -T bench bash bench/review/run-matrix.sh run --rows 'W1.*' --endpoint stub \
    --clients s5cmd,rs5cmd,rs5cmd-fast --tag baseline
bench/review/fast-window.sh $D exec -T bench bash bench/review/run-matrix.sh run \
    --rows 'W1.*' --endpoint stub --clients rs5cmd-fast --tag baseline   # io_uring rows
$D exec -T bench python3 bench/review/aggregate.py bench/review/results/baseline.jsonl
bash bench/review/pack-results.sh                            # gzip finished results before a commit
bash bench/review/a3.sh; bash bench/review/a4.sh; bash bench/review/a5.sh   # Stage A batches
$D down -v                                                  # remove containers and volumes
```

## Method

- **Clients**: the official s5cmd v2.3.0 release binary; rs5cmd built exactly like
  `.github/workflows/release.yml` (cargo-zigbuild, glibc 2.28, `--features fast`, stripped);
  `rs5cmd --fast` on io_uring and on its epoll fallback. Each `--fast` run records whether
  io_uring was permitted, and the warm-up run counts io_uring vs epoll syscalls with strace.
- **Endpoints**: s3stub over HTTP (stateless, synthetic datasets, pattern bodies), s3stub over
  TLS (HTTP/1.1 only; the test CA is trusted through `SSL_CERT_FILE`, because
  `--no-verify-ssl` / `--ca-certs-file` switch rs5cmd's default path to another TLS stack;
  `--fast` uses `--no-verify-ssl` since it has no custom-CA option), and MinIO over HTTP.
- **Rounds**: one warm-up (recorded, flagged, its stdout checked), then 10 rounds for short rows
  and 5 for rows of 5 s or more, interleaved across clients with the client order rotated each
  round. stdout goes to `/dev/null` (primary) or a pipe (`--stdout pipe`).
- **Checks** on every run: stub request/byte counters, file counts and byte-exact contents
  (every object's byte `o` is `o % 251`), listing line counts, MinIO `du` totals.
- **Metrics**: wall (monotonic), user and sys CPU, max RSS (floor: ~1 MiB, the static launcher's
  pre-exec image), softirq time of the client and server cpusets (loopback network work is not
  charged to any process), stub CPU, and CPU busy outside the cpuset (neighbour noise).
- **Reporting**: best, median and min–max; RSS as median and max; the median of per-round
  ratios against s5cmd; a difference counts as a win or loss only when the min–max ranges do
  not overlap.

## Limits

Loopback only (no real network, no real AWS); the stub answers instantly unless latency is
injected; MinIO shares the host with the client and is server-bound for small objects. Scaled-
down rows are marked in the results.
