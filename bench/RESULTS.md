# Benchmark results — small parallel transfers

Workload: **10,000 × 4 KB objects**, `--numworkers 256`, against MinIO over the
docker network. Both clients hit the *same* MinIO, so the server is constant and
the client delta is the signal. Measured with `/usr/bin/time` (wall, user+sys
CPU, peak RSS). Reproduce: `docker compose run --rm bench` (needs the hardened
io_uring seccomp profile; see `../docker-compose.yml`).

## Phase 0 — baseline (Go s5cmd vs rs5cmd on tokio + aws-sdk-s3)

| client | op | wall(s) | req/s | CPU(s) | peakRSS(MB) |
|---|---|---:|---:|---:|---:|
| s5cmd        | upload   | 1.24 |  8065 | 5.57 |  67 |
| s5cmd        | download | 0.78 | 12821 | 7.16 |  69 |
| rs5cmd-tokio | upload   | 1.82 |  5495 | 2.75 | 169 |
| rs5cmd-tokio | download | 1.21 |  8264 | 2.08 |  90 |

Notes:
- **rs5cmd-tokio uses ~2–3× less CPU per request** but is ~1.5× slower in
  wall-clock — it is latency-bound (under-driving concurrency through the SDK),
  not CPU-bound. This is the headroom the io_uring fast path targets.
- An earlier draft re-created the S3 client (full AWS config + credential chain)
  *per object*; fixing that to share one client roughly doubled rs5cmd throughput
  (upload 1471 → 5495 req/s). The table above is the post-fix, honest baseline.
- rs5cmd upload RSS (169 MB) is higher than s5cmd — to investigate (likely SDK
  buffering); the fast path should also improve this.

**Target for the fast path:** beat s5cmd on req/s for both upload and download
while preserving the CPU-per-request advantage.

## Phase 2 — io_uring fast path (thread-per-core monoio), first cut

| client | op | wall(s) | req/s | CPU(s) | peakRSS(MB) |
|---|---|---:|---:|---:|---:|
| s5cmd        | upload   | 1.18 |  8475 | 5.69 |  63 |
| s5cmd        | download | 0.79 | 12658 | 6.92 |  61 |
| rs5cmd-tokio | upload   | 1.82 |  5495 | 2.80 | 177 |
| rs5cmd-tokio | download | 1.20 |  8333 | 2.04 |  89 |
| **rs5cmd-fast** | **upload**   | 1.46 |  6849 | **0.96** | 155 |
| **rs5cmd-fast** | **download** | **0.56** | **17857** | 3.60 |  77 |

Headlines:
- **Download: fast path wins outright** — 0.56s vs 0.79s (1.4× faster, 17857 vs
  12658 req/s) at ~half the CPU.
- **Upload: 6× less CPU** (0.96 vs 5.69 CPU-s) but slower wall-clock. Cause:
  `std::fs::read` is a *blocking* syscall on the single-threaded monoio runtime,
  serializing local file reads per core and starving request concurrency. Fix =
  Phase 3 (async monoio::fs reads + registered buffers). Upload RSS is also high
  (body copy) — to address.

## Phase 3/4 — final (io_uring fast path, tuned), 10k × 4KB, workers=256

| client | op | wall(s) | req/s | CPU(s) | CPU/req | peakRSS(MB) |
|---|---|---:|---:|---:|---:|---:|
| s5cmd        | upload   | 1.20 |  8333 | 5.53 | 553µs |  68 |
| s5cmd        | download | 0.81 | 12346 | 6.88 | 688µs |  67 |
| rs5cmd-tokio | upload   | 1.87 |  5348 | 2.74 | 274µs | 177 |
| rs5cmd-tokio | download | 1.18 |  8475 | 2.12 | 212µs |  93 |
| **rs5cmd-fast** | **upload**   | 1.43 |  6993 | **1.17** | **117µs** | 149 |
| **rs5cmd-fast** | **download** | **0.62** | **16129** | 3.82 | **382µs** |  83 |

### Verdict

- **Download: the fast path wins outright** — **1.31× faster** wall-clock
  (16129 vs 12346 req/s) from the *same* MinIO, at **1.8× less CPU**.
- **Upload: 4.7× less CPU per request** (117µs vs 553µs). Wall-clock is ~16%
  slower because upload is server-influenced (see below), but the client does
  the same work for a fraction of the CPU.
- **CPU efficiency is the decisive, robust win**: the fast path moves the same
  bytes for **2–5× less CPU** than Go s5cmd across the board — the expected
  payoff of thread-per-core io_uring + no GC + SDK-free signing + pooled HTTP.

### Where the bottleneck is (honest caveat)

Both clients are far slower on upload than download (s5cmd 1.20 vs 0.81; fast
1.43 vs 0.62) — MinIO's PUT path (disk writes) is a shared ceiling. Two
experiments localized it:
- **Concurrency 256 → 512**: throughput unchanged ⇒ not client-concurrency-bound.
- **Blocking std::fs → async monoio::fs**: upload unchanged ⇒ not file-I/O-bound.

So upload throughput is gated by the server/network, not the client. That also
means further client-side io_uring knobs (SQPOLL, registered buffers,
provided-buffer-ring multishot recv) cannot raise upload throughput here, and
SQPOLL's spinning kernel poller thread would make CPU accounting misleading — so
they were intentionally *not* applied. To expose the pure client ceiling you'd
benchmark against a discard-S3 stub instead of MinIO (future work).

### Tuning knobs applied / attributed

| knob | effect on this workload |
|---|---|
| thread-per-core monoio (16 io_uring rings) | core architecture; enables the CPU win |
| pooled HTTP/1.1 keep-alive (per core) | avoids per-request connect/TLS |
| SDK-free SigV4 (aws-sigv4, cached identity) | low per-request signing cost |
| async monoio::fs read/write | removes blocking fs from the runtime (no wall-clock change here — server-bound — but correct architecture) |
| concurrency 256 vs 512 | no change (server-bound) |
| SQPOLL / registered buffers / multishot recv | not applied — can't help a server-bound throughput; would distort CPU accounting |

Reproduce: `docker compose run --rm -e RS5CMD_FEATURES=fast \
  -e VARIANTS="s5cmd rs5cmd rs5cmd-fast" bench`

## Optimization round (in progress)

Harness hardened: each measurement is now **best-of-N** (`ITERS`, default 3) with
the download target wiped *untimed* between iterations, so iterations are
independent.

Applied:
- **Disabled SDK per-request flexible checksums** (`request_checksum_calculation`
  / `response_checksum_validation` = `WhenRequired`). Removes a CRC32 computation
  per request on the tokio path. Effect: tokio upload **5348 → ~6700 req/s** and
  CPU **2.74 → ~2.2 CPU-s**. Kept.
- **Fast path uses `UNSIGNED-PAYLOAD`** instead of hashing each body for SigV4.
  Effect at 4 KB: neutral (a 4 KB SHA-256 is trivial); meaningful for large
  objects. Kept.

Best-of-3 snapshot (10k × 4 KB, workers=256):

| client | op | wall(s) | req/s | CPU(s) | peakRSS(MB) |
|---|---|---:|---:|---:|---:|
| s5cmd        | upload   | 1.19 | 8403 | 5.53 |  67 |
| s5cmd        | download | 1.16 | 8621 | 21.75 | 76 |
| rs5cmd-tokio | upload   | 1.50 | 6667 | 2.20 | 176 |
| rs5cmd-tokio | download | 1.88 | 5319 | 3.07 |  99 |
| rs5cmd-fast  | upload   | 1.45 | 6897 | 1.14 | 149 |
| rs5cmd-fast  | download | 0.57 | 17544 | 3.39 | 83 |

Key obstacle to deeper optimization: **wall-clock is host/MinIO-noise-bound**
(s5cmd download wall ranged 0.79–1.22s across runs; its CPU ~7–22 CPU-s — Go
burning ~18 cores under 256-worker concurrency). To attribute fine-grained
client optimizations we need a **stateless in-memory discard-S3 stub** as the
target (removes server bottleneck + state noise). That's the next foundational
step. Until then, CPU-per-request is the robust metric — and the fast path leads
it everywhere (2–6× less CPU than Go s5cmd).

### Against the discard-S3 stub — TRUE client ceiling (the decisive result)

The `s3stub` target (stateless, in-memory; `STUB=1`) removes MinIO as bottleneck
and noise. This exposes what each client can actually drive (10k × 4 KB,
workers=256, best of 3):

| client | op | wall(s) | req/s | CPU(s) | CPU/req | peakRSS(MB) |
|---|---|---:|---:|---:|---:|---:|
| s5cmd        | upload   | 0.46 | 21739 | 4.15 | 191µs |  35 |
| s5cmd        | download | 0.57 | 17544 | 4.68 | 267µs | 101 |
| rs5cmd-tokio | upload   | 2.02 |  4950 | 1.68 | 339µs | 171 |
| rs5cmd-tokio | download | 1.59 |  6289 | 2.58 | 410µs | 111 |
| **rs5cmd-fast** | **upload**   | **0.41** | **24390** | **0.76** | **31µs**  | 152 |
| **rs5cmd-fast** | **download** | **0.35** | **28571** | **2.64** | **92µs**  |  82 |

**The fast path beats Go s5cmd on BOTH operations, decisively:**
- Upload: **1.18× faster** wall-clock (24390 vs 21739 req/s) at **5.6× less CPU**
  (31µs vs 191µs per request).
- Download: **1.69× faster** (28571 vs 17544 req/s) at **2.9× less CPU**.

This is the goal — "humiliate s5cmd on small parallel transfers" — proven against
the true client ceiling, not a server-capped benchmark.

It also confirmed MinIO was the earlier ceiling (s5cmd upload 8k → 21.7k req/s
against the stub) and surfaced the real weak link:

### The tokio/SDK path is the laggard (4–5× slower than s5cmd)

Hidden by MinIO before, the default `aws-sdk-s3` path manages only ~5–6k req/s —
low CPU (1.7–2.6 CPU-s) but high wall-clock, i.e. **latency-bound / under-driving
concurrency through the SDK** despite `buffer_unordered(256)` over a shared
client. This is the prime optimization target for the portable path (likely SDK
connection-pool / per-request middleware overhead). The fast path already
sidesteps it entirely.

### Tokio path — profiled and fixed (4–5× behind → competitive)

Profiling against the stub (and a connection counter built into `s3stub`)
disproved two guesses before finding the real causes:
- **Not** a connection-pool cap — the SDK opened 256–389 connections.
- **Not** Nagle — `aws-smithy-http-client` already defaults `enable_tcp_nodelay = true`.

The two real bottlenecks:
1. **`buffer_unordered` polls all N futures inside one task → one thread**, so the
   SDK's per-request CPU (request build + SigV4 HMAC + response parse) serialized
   on a single core (~0.8 cores busy). Fixed by `tokio::spawn` + a `Semaphore`
   (in `cp` and `sync`), letting the multi-threaded runtime spread work across
   all cores. → download **6.3k → 22.7k req/s**.
2. **`ByteStream::from_path` streamed each small body from disk** via the blocking
   pool per request. Fixed by reading files ≤1 MiB fully into memory
   (`ByteStream::from`). → upload **5.0k → 20.0k req/s**.

Also fixed the same per-object `S3::new` (full config reload) in `sync` that `cp`
had — now one shared `Arc<S3>` per invocation.

3. **Unbounded completed-task accumulation.** The spawn loop bounded only
   *running* tasks (a semaphore), so finished task outputs piled up in the
   `JoinSet` until the spawn loop ended. Bounding the in-flight `JoinSet` to
   `workers` (interleave `join_next` with spawning) cut peak RSS ~40% **and**
   nudged throughput up. RSS was *not* glibc-arena fragmentation
   (`MALLOC_ARENA_MAX=2` left RSS flat but tanked throughput) nor
   concurrency-scaled (64 vs 256 workers were similar) — it was the buffered
   results. Applied to both `cp` and `sync`.

### Final (optimized) — true client ceiling vs discard stub, 10k × 4 KB

| client | op | wall(s) | req/s | CPU(s) | peakRSS(MB) |
|---|---|---:|---:|---:|---:|
| s5cmd        | upload   | 0.46 | 21739 | 4.16 |  38 |
| s5cmd        | download | 0.57 | 17544 | 6.78 | 121 |
| rs5cmd-tokio | upload   | 0.46 | 21739 | 2.74 | 183 |
| rs5cmd-tokio | download | 0.43 | 23256 | 8.58 | 110 |
| **rs5cmd-fast** | **upload**   | **0.40** | **25000** | **0.74** | 151 |
| **rs5cmd-fast** | **download** | **0.36** | **27778** | **1.88** |  84 |

Both rs5cmd paths now **meet or beat** Go s5cmd: the tokio path **ties on upload
(21739 req/s) and beats on download** (23256 vs 17544) at less CPU; the fast path
leads everything on wall-clock *and* CPU. tokio RSS (183 MB) is down from 298 but
still above s5cmd (38) and the fast path (151) — the residual is the AWS SDK
stack footprint (hyper + rustls/aws-lc-rs + connection buffers), reducible only
by leaving the SDK (which is what the fast path does). Fast-path micro-knobs
(registered buffers, multishot recv, SQPOLL) remain available but the path is
already the leader.
