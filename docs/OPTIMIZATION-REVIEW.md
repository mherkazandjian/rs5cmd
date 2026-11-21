# rs5cmd optimization review

Status: **Stage A complete; awaiting the maintainer's sign-off on the targets (§7) and the
ranking (§6)**. Stage A of the optimization plan (tracking issue #1): measure, profile and rank
before changing anything. Nothing in `src/` was changed. The experiments live on throwaway local
branches.

## 1. Summary

**Where rs5cmd stands.** As shipped (v0.1.0 = `main` 350fae9) against s5cmd v2.3.0, on a
client-bound S3 stub (ratios below 1 favour rs5cmd):

- **It wins** on:
  - listing (`ls` / `du`: 0.5× wall, 0.6× RSS);
  - cold sync (0.17–0.26×);
  - `cat` (0.28×, 0.06× RSS);
  - small-object downloads (0.75–0.80×);
  - 1 MiB uploads (0.57×).
- **It loses** on:
  - `run` (3.4–3.8×);
  - large downloads (1.3–2.4×, RSS up to 59×);
  - `rm` (1.8×);
  - small-object uploads (1.3–1.6×; 1.4 GB RSS for 100k files);
  - large uploads (1.2–1.7×);
  - warm and `--delete` sync (1.2–1.3×);
  - start-up (11–12 ms vs 5–8 ms; 3 s when no region is configured).
- **Its binary** is 1.53× s5cmd's (2.08× as tar.gz).

**Why.** Six causes explain nearly every loss. Each one is in a single place in the code:

1. a regex compiled for every URL, so per file and per deleted key: 46 % of `rm`'s CPU, 43 % of
   a warm sync's, 1.3 GB for 100k files;
2. the AWS SDK's per-request cost: ~148 µs of CPU against ~30 µs for a lean client, ~70 % of
   small-object CPU;
3. downloads that collect every 8 MiB part in memory before writing it;
4. uploads that read files 4 KiB at a time through tokio's blocking pool;
5. a fresh SDK config and HTTP client for every `run` line;
6. DeleteObjects batches sent one at a time.

**What fixing them buys.** Each of these was measured as a one-change experiment against s5cmd:

| Change | Before → after (× s5cmd wall) |
|---|---|
| no regex for plain URLs (E1) | `rm` 1.84 → **0.31**; uploads 1.2–1.34 → **0.73–0.88**; warm / changed / `--delete` sync 0.74–1.23 → **0.30–0.46**, with 0.3× the RSS |
| larger upload reads (E10) | large uploads 1.03–1.37 → **0.31–0.38** |
| streamed downloads (E2) | large downloads 1.38–2.29 → **1.01–1.65**, RSS ÷ 5–18 |
| one SDK config and HTTP client per process (E7b) | `run` 3.2–3.8 → **1.35–1.68**, at 0.6× s5cmd's CPU |
| concurrent DeleteObjects (E4) | `rm` → **0.93–1.00** |
| fat LTO + 1 codegen unit (E5) | −11 % CPU everywhere; binary 1.53× → **1.16×** (1.05× with `panic = "abort"`) |

E1 alone turns every sync loss and every small-object upload loss into a win.

**What is left after that:**

- Small-object CPU stays near the SDK's floor. The one large decision (§6, item 10) is whether
  to put a lean client on the default data path. `--fast` already shows what that gives:
  0.25–0.35× wall on small objects.
- Large downloads end near parity, because both tools spend most of their time copying in the
  kernel.

**Bugs found on the way** (not fixed, not filed):

- `cp --fast` on io_uring fails for every file of 4 GiB or more.
- `cp --fast` ignores SIGTERM / Ctrl-C until every transfer is done, then exits 0.
- A local `cat` reads the whole file into memory.
- Under 30 % `503 SlowDown`, rs5cmd fails every large transfer that s5cmd completes, because of
  F02 and F04.

**Asked of the maintainer:**

- the targets (§7);
- the ranking (§6);
- the lean-data-path decision;
- `panic = "abort"`;
- whether uploads should send a checksum (s5cmd sends Content-MD5; rs5cmd sends none).

## 2. Method

### Clients

| Client | What | How it was built |
|---|---|---|
| s5cmd | upstream release v2.3.0, the baseline | official release tarball (Go, stripped) |
| s5cmd-go1.22 / s5cmd-golatest | the same source, local builds | Go 1.22.12 and 1.27.4, `-ldflags "-s -w"`, release version stamps |
| rs5cmd | this repository at `origin/main` 350fae9 | exactly like `release.yml`: `cargo zigbuild --release --locked --target x86_64-unknown-linux-gnu.2.28 --features fast`, stripped |
| rs5cmd `--fast` | the io_uring data path of the same binary | measured both on io_uring and on its epoll fallback; each run records which one it used |
| rs5cmd-prof | the same source with `[profile.profiling]` (release + debug info) and frame pointers | profiling only |

The local rs5cmd build was cross-checked against the published v0.1.0 release asset (same size
within 0.2 %).

### Endpoints

- **s3stub over HTTP** (primary): an in-repo, stateless S3 stub (`src/bin/s3stub.rs`, feature
  `bench-stub`) on its own two physical cores. It serves synthetic, paged listings for millions
  of keys, streamed pattern bodies (byte `o` of every object is `o % 251`), multipart,
  DeleteObjects, and counts every request. It never touches a disk.
- **s3stub over TLS**: the same stub behind rustls, HTTP/1.1 only. The test CA is trusted via
  `SSL_CERT_FILE`, because `--no-verify-ssl` / `--ca-certs-file` would move rs5cmd's default
  path onto a different TLS stack; `--fast` (no custom-CA option) uses `--no-verify-ssl`.
- **MinIO** (RELEASE.2025-09-07, pinned by digest) with its data on a tmpfs, on the same two
  cores as the stub would use. Rows too large for its 9 GiB tmpfs are scaled down and marked.

### Host, pinning and caps

A shared x86-64 host (AMD Zen 4, Linux 5.14). The review was confined to 4 physical cores with
both hyperthreads (8 logical CPUs): the server (stub or MinIO) on 2 cores, the client on the other
2. Memory limits of everything running at once add up to 32 GiB, tmpfs included; block I/O is
capped with `io.max`; no run starts while the host's 1-minute load average is above 11.
Client files live on a tmpfs (primary) and on ext4 on NVMe (secondary `.disk` rows, small,
throttled to 200 MB/s by the cap).

### Measurement

- A static launcher pins the command, `exec`s it and reports wall (monotonic), user and system
  CPU and peak RSS from `wait4`, plus busy/softirq time per cpuset from `/proc/stat` (loopback
  network work is charged to no process).
- Rounds are interleaved across clients and the order rotates each round. One warm-up run per
  client (its stdout is checked, it is not counted), then 10 rounds for short rows and 5 for
  rows of 5 s or more.
- Every run is checked: stub request and byte counters, file counts and byte-exact contents,
  listing line counts, MinIO `du` totals. A failed check fails the run.
- Reported: best, median and min–max; RSS as median and max (never best); the median of
  per-round ratios against s5cmd. A difference is called a win or a loss only when the min–max
  ranges do not overlap.
- A run is rejected as neighbour noise when the CPUs outside the cpuset were busier than
  35 % **and** more than 20 points above the median of its own group (same row, endpoint,
  variant and client). The relative part was added after A3: the outside-busy figure rises with
  our own load, because a saturated cpuset pushes neighbours' unpinned tasks onto the other CPUs
  (up to ~60 % on rows that use all 8 CPUs; the busier client in an interleaved pair showed
  more of it, within seconds). The fixed 35 % rule alone rejected whole heavy groups and
  rejected the busier client more often; it moved no other median by more than 3 %. With the
  relative test, 28 of ~2300 runs were rejected.

### Pilot (A2)

- **Stub capacity**: moving the stub from 2 physical cores to 1 changes no client's median by
  more than 3 %, and the stub never exceeds 53 % of one core. No row is stub-bound.
- **Spread** (max − min) / median of wall time: median 1.9 %, worst 12 % (`--fast` W3 upload),
  over 32 groups of 5 rounds.
- **TIME_WAIT**: MinIO is not on loopback, so with the kernel default (`tcp_tw_reuse=2`) every
  closed connection pins an ephemeral port for 60 s. That made s5cmd's small-object downloads
  spend over 90 % of their CPU in the kernel's port search, and made `rs5cmd run` fail with
  `EADDRNOTAVAIL`. Measurement runs against MinIO therefore use `tcp_tw_reuse=1` in the client's
  network namespace; the default-setting behaviour is reported as a finding (§5).

### Workloads

| Row | What | Notes |
|---|---|---|
| W1 | `cp` of many 4 KiB objects, up and down: 1k, 10k, 100k; a deep tree (100k keys, 4 levels, long names); worker counts 16/64/256; `.disk` variant | small-object path, per-object overhead |
| W2 | 5000 × 1 MiB up and down | medium objects (MinIO: 2000 × 1 MiB) |
| W3 | 1 × 4 GiB, 4 × 1 GiB, 32 × 128 MiB up and down; `.eq` = rs5cmd with s5cmd's multipart settings (`--concurrency 5 --part-size 50`) | large-object path (MinIO: ≤ 2 GiB per run) |
| W4 | `ls` of 100k and 1M keys | listing, output formatting |
| W5 | `sync` cold / warm / changed (10 % differ in size) / `--delete`, up and down, 100k keys (MinIO: chains of 10k) | sync planning |
| W6 | `rm` of 100k keys (MinIO: 10k) | batch delete |
| W7 | `cat` of a 4 GiB object to `/dev/null` and through a pipe | streaming to stdout |
| W8 | `run` with 10k `cp` lines, up and down | command file |
| W9 | `--version`, `ls` of one key, `cp` of one key, with and without a region | start-up cost |
| W10 | `du` of 1M keys | listing without formatting |

## 3. Results: rs5cmd against s5cmd

The baseline matrix (A3): about 2300 measured runs plus 366 `--fast` runs on io_uring, every one
of them checked. Ratios are rs5cmd ÷ s5cmd (median of per-round ratios; below 1 is better) for
wall time, CPU time (user + sys) and peak RSS. **Bold** = a win or loss outside the spread
(min–max ranges do not overlap); plain = within the spread. The full per-row tables, with
absolute numbers, are in [`bench/review/results/scorecard-a3.md`](../bench/review/results/scorecard-a3.md).

#### s3stub over HTTP (client-bound; the primary endpoint)

| Workload | s5cmd wall | rs5cmd wall× / CPU× / RSS× | rs5cmd `--fast` (epoll) | Reading |
|---|---:|---|---|---|
| W1 download 10k / 100k × 4 KiB | 0.83 s / 7.8 s | **0.80 / 0.80** / **1.8**; **0.75 / 0.77 / 9.9** | **0.25–0.32 / 0.18–0.20** / **1.7–10** | faster; RSS grows with object count |
| W1 download 1k | 0.10 s | **1.47** / 1.04 / **1.27** | 0.89 / **0.31** / 0.96 | start-up dominates |
| W1 upload 10k / 100k | 0.74 s / 6.5 s | **1.28** / 0.98 / **5.6**; **1.37 / 1.05 / 27** | **0.79–0.81 / 0.34–0.36** / **5.4–28** | slower at equal CPU; 1.4 GB RSS at 100k |
| W1 deep tree 100k, up / down | 8.6 s / 8.4 s | **1.18 / 0.87 / 56**; **0.74 / 0.76 / 10.7** | **0.74 / 0.34**; **0.26 / 0.19** | RSS 1.8 GB on upload |
| W2 5000 × 1 MiB, up / down | 3.2 s / 1.4 s | **0.57 / 0.51 / 22**; **1.26 / 1.31 / 9.0** | **0.34 / 0.27**; **1.20 / 1.25** | downloads lose |
| W3 large downloads (1 × 4 GiB, 4 × 1 GiB, 32 × 128 MiB) | 0.79–1.45 s | **1.32–2.14 / 1.37–2.08 / 10–53** | **1.51–1.72** / 0.51–1.67 / **150–195** | the largest loss |
| W3 large uploads | 2.3–2.4 s | **1.19–1.68 / 1.16–1.35** / 0.90–1.37 | **0.37**–1.00 / **0.25–0.37** / **123–156** | default path loses, `--fast` wins (RSS aside) |
| W4 `ls` 100k / 1M | 1.1 s / 10.8 s | **0.51–0.52 / 0.48 / 0.62** | – | clear win |
| W10 `du` 100k / 1M | 0.89 s / 9.7 s | **0.54–0.55 / 0.49–0.50 / 0.62** | – | clear win |
| W5 `sync` cold, up / down (100k) | 35 s / 38 s | **0.26 / 0.18** / **1.33**; **0.17 / 0.16 / 6.9** | – | clear win |
| W5 `sync` warm / `--delete`, up and down | 3.7–4.1 s | **1.22–1.30 / 0.50–0.62** / ~1.0 | – | half the CPU, yet slower |
| W5 `sync` 10 % changed | 6.6–6.9 s | **0.74–0.80 / 0.29–0.31** / ~1.05 | – | win |
| W6 `rm` 10k / 100k | 0.14 s / 1.35 s | **1.83–1.84** / **0.87–0.93** / **0.70–0.73** | – | slower (batches one at a time) |
| W7 `cat` 4 GiB to `/dev/null` / through a pipe | 1.7 s / 2.3 s | **0.28 / 0.24 / 0.06**; **0.54 / 0.46 / 0.31** | – | clear win |
| W8 `run`, 10k `cp` lines, up / down | 2.1 s / 2.6 s | **3.44–3.79 / 1.40–1.51 / 2.5–3.4** | – | the worst loss |
| W9 `--version` | 4 ms | **0.78** / 0.77 / **0.53** | – | win |
| W9 `ls` / `cp` of one key | 5–8 ms | **1.5–2.3** | – | 11–12 ms per call |
| W9 the same without a region | 5–8 ms | **3.0 s** (×390–570) | – | instance-metadata probe |

#### Other endpoints and variants

- **TLS** (s3stub behind rustls): s5cmd re-handshakes on every reconnect. Its 100k-object runs
  take 17–18 s and 57 CPU-s, against 6.5–7.8 s and 23–27 CPU-s on plain HTTP. rs5cmd keeps
  its connections:
  - small objects: **0.35–0.59** wall, **0.38–0.46** CPU;
  - W2 upload: **0.55**;
  - W2 download: still **1.17**;
  - large downloads: still **1.53–2.10**;
  - large uploads: **1.06–1.37**.
- **MinIO** (on tmpfs, sharing the host) is server-bound for small objects:
  - W1 downloads: 0.85–0.95 wall, **0.74–0.76** CPU;
  - W1 uploads: **1.25–1.26** wall, **0.80–0.81** CPU;
  - W3 downloads: **1.97–2.35**;
  - `ls`/`du`: **0.74–0.76**;
  - cold sync: **0.26–0.43**;
  - warm and `--delete` sync: **1.27–1.34**;
  - `rm`: **2.43**;
  - `run`: **3.44–3.63**;
  - `cat`: 0.92 wall with **0.07** RSS.
- **Worker counts** (W1 100k at 16/64/256 workers) do not change the picture. rs5cmd downloads
  are **0.70–0.75** wall at every count, and uploads **1.25–1.39**.
- **Pinning**: with 1 thread per core (2 CPUs), or unpinned across all 8 CPUs, the ratios stay
  within about 0.1 of the pinned ones:
  - W1 download: 0.68–0.76;
  - W1 upload: 1.15–1.26;
  - W3 4 × 1 GiB download: 1.61–1.90;
  - `ls` 1M: 0.48–0.53.
- **stdout to a pipe** instead of `/dev/null`: same ratios (W4 `ls` **0.58**, `rm` **1.81**).
- **`--fast` on io_uring vs its epoll fallback**:
  - On the stub they are within a few percent of each other. The gain over the default path
    comes from `--fast`'s lean HTTP client and signer, not from io_uring.
  - Over TLS, io_uring is worse. W1.up.10k is bimodal (0.6 s or 1.47 s at the same CPU, an idle
    stall of ~0.85 s in half the runs), and W2 uses about 1.6× epoll's user CPU. The stall
    was not traced in Stage A: that needs another io_uring window. It belongs with §6 item 11.
  - Every ≥ 4 GiB transfer fails on io_uring (§5).
- **s5cmd's Go version**: the local Go 1.22 build matches the official binary within 1 %.
  Building with the latest Go makes s5cmd's `ls` of 1M keys 18 % faster and leaves the rest
  within 6 %, so the baseline will move.

### Binary size

| Binary | Stripped bytes | tar.gz bytes | vs s5cmd |
|---|---:|---:|---:|
| s5cmd v2.3.0 (official) | 17,658,008 | 5,037,198 | 1.00 |
| s5cmd, Go 1.22 local build | 17,637,528 | – | 1.00 |
| s5cmd, Go 1.27 local build | 18,604,192 | – | 1.05 |
| rs5cmd, release build (`--features fast`) | 26,985,088 | 10,466,883 | 1.53 (tar.gz 2.08) |
| rs5cmd v0.1.0 published | 26,943,120 | 10,488,807 | 1.53 |
| rs5cmd, default features | 25,637,640 | 9,945,581 | 1.45 |
| rs5cmd, `mount` | 27,445,776 | 10,536,339 | 1.55 |
| rs5cmd, `fast` + `mount` | 28,743,960 | 11,042,041 | 1.63 |

Release-profile knobs: the same source and recipe (`--features fast`, zigbuild, stripped), one
profile setting changed through `CARGO_PROFILE_RELEASE_*`. Their effect on speed is in §4.

| Profile | Stripped bytes | tar.gz bytes | vs s5cmd (tar.gz) |
|---|---:|---:|---:|
| as shipped (no LTO, 16 codegen units, unwind, `opt-level=3`) | 26,985,088 | 10,466,883 | 1.53 (2.08) |
| `lto = "thin"` | 27,855,640 | 10,708,781 | 1.58 (2.13) |
| `lto = "fat"` | 22,856,336 | 9,499,389 | 1.29 (1.89) |
| `codegen-units = 1` | 21,595,576 | 8,744,585 | 1.22 (1.74) |
| `panic = "abort"` | 21,498,464 | 8,345,332 | 1.22 (1.66) |
| fat + cgu 1 | 20,520,096 | 8,700,338 | 1.16 (1.73) |
| fat + cgu 1 + `opt-level=2` | 20,195,744 | 8,556,808 | 1.14 (1.70) |
| fat + cgu 1 + abort | 18,503,504 | 7,687,970 | 1.05 (1.53) |
| fat + cgu 1 + `opt-level="s"` | 15,848,376 | 6,858,995 | 0.90 (1.36) |
| fat + cgu 1 + `opt-level="z"` | 14,696,304 | 6,363,834 | 0.83 (1.26) |
| fat + cgu 1 + abort + `opt-level="s"` | 14,340,968 | 6,100,744 | 0.81 (1.21) |

What the bytes are:

- **Code size:** `.text` is 17.5 MB against s5cmd's 6.7 MB. Unwind tables (`.eh_frame` plus
  `.gcc_except_table`) add 3.5 MB, and PIE relocations (`.rela.dyn`) 1.6 MB.
- **Largest crates in `.text`** (cargo-bloat):
  - rs5cmd 1.9 MiB;
  - aws-sdk-s3 1.8 MiB;
  - aws-lc-sys 1.3 MiB;
  - the SSO/OIDC/STS credential clients (aws-sdk-ssooidc, -sts, -sso) 1.4 MiB together;
  - the SDK's HTTP client 0.9 MiB;
  - rustls 0.9 MiB;
  - the regex stack (regex-automata, regex-syntax, aho-corasick) 0.86 MiB;
  - h2 0.44 MiB;
  - clap 0.44 MiB.
- **Duplicated crates:** 23 crates are compiled twice, among them hyper, h2, http, rustls,
  rustls-webpki, tokio-rustls, hyper-rustls, webpki-roots and sha2.
  - rustls 0.21 comes in through the SDK's legacy hyper 0.14 client and through monoio-rustls
    (the `--fast` path).
  - rustls 0.23 runs on aws-lc, and ring is also linked.
  - That makes three TLS stacks in all (audit lead F75, confirmed).

## 4. Where the time, memory and bytes go

### Profiles (A4)

One profiled run per row and client, after the measured batches, using:

- `perf record` with frame pointers. rs5cmd is a release build with debug info and frame pointers;
  s5cmd is built with Go 1.22 and debug info.
- DWARF unwinding for two rows;
- `perf stat` and `strace -c`;
- heaptrack (glibc builds only, so rs5cmd only);
- an RSS breakdown sampled from `/proc/<pid>/smaps` at the peak.

`bench/review/profile.sh` regenerates every profile; the flamegraph SVGs are not committed. Shares
are of the client's sampled CPU time. "Inclusive" counts a sample if the layer appears anywhere on
its stack.

**Small objects (W1, 10k × 4 KiB, s3stub over HTTP)**

- **The SDK is the cost.** On the download, 60 % of rs5cmd's CPU is inside aws-smithy / aws-sdk-s3
  (DWARF profile), with 28 % in hyper and http below it. SigV4 is 3 %, rs5cmd's own URL and key
  handling 5 %, file I/O 4 %. E8 measured the same floor from outside: ~148 µs of CPU per request,
  against ~30 µs for a lean client.
- **Thread churn.** rs5cmd does 10–14× s5cmd's context switches:
  - download: 40.7k vs 3.4k;
  - upload: 47.5k vs 6.2k;
  - 61–68k `futex` calls;
  - 3.5–7× the CPU migrations.

  tokio's work-stealing workers wake each other for every request. Instructions per cycle are
  0.6–0.8, against s5cmd's 1.2–1.3.
- **`--fast` on the same download** uses 0.57 CPU-s against 2.2, more than half of it in the
  kernel. SigV4 is 9.5 % of what remains.
- **Uploads compile a regex per local file.** `Url::new` → `Url::set_prefix_and_filter` builds and
  keeps a regex for every file found by the local walk. That is 10 % of the default path's upload
  CPU and 25 % of `--fast`'s.

**Listing, deleting, syncing**

- **`rm` 10k:** 46 % of the CPU compiles regexes, because `multi_delete` parses a `Url` per deleted
  key. E1 removes this.
- **Warm `sync` of 100k local files:** 43 % of the CPU is regex compilation and 46 % is URL
  handling: one `Url`, with its compiled regex, per local file. This is why rs5cmd's warm sync
  runs at CPU ≈ wall. s5cmd has the same design and spends 31 % in `regexp`.
- **`ls` 100k:** XML parsing 26 %, output formatting 11 % (E3), SDK 34 %.
- **`run` 10k lines:** SDK 38 % inclusive. 145k context switches and 22k migrations, against
  s5cmd's 12k and 551.

**Large objects**

- **W3 download, 4 × 1 GiB:** 83 % of rs5cmd's CPU is kernel time:
  - copying out of the socket (17 %);
  - copying into the tmpfs page cache (13 %);
  - spinning on the file's inode lock while parts write at once (6 %).

  Another 14 % is libc `memcpy` assembling each 8 MiB part. That totals 28 G cycles against
  s5cmd's 15.5 G, and 121k page faults against 1.3k (every part gets a fresh buffer). E2 removes
  both the assembling copy and the fresh buffers.
- **W3 upload, 4 × 1 GiB:** s5cmd spends 69 % of its CPU on MD5 (Content-MD5) and SHA-256 (the
  signed payload). rs5cmd hashes nothing and still uses 1.38× the CPU (11.8 s vs 8.6 s). The SDK
  reads the file in 4 KiB chunks, and each chunk is a `tokio::fs` read through the blocking thread
  pool:
  - 1.05 M `read` calls;
  - 2.1 M `futex` calls;
  - 1.56 M context switches;
  - 230k `sched_yield` calls;
  - instructions per cycle 0.47 against 1.40.

  Experiment E10 tests this.
- **`--fast` large transfers:** ~900k page faults per run, because whole objects are held in
  memory (F43).

**Memory (heaptrack, and smaps at the peak)**

The peak is live heap data, not fragmentation or allocator caching. E6 agrees: no allocator moves
the peak.

| Row | rs5cmd peak heap | s5cmd peak RSS | What holds it |
|---|---:|---:|---|
| W1 upload 100k | 1.38 GB | 51 MB | `Url::set_prefix_and_filter` from `Url::new` in the local walk (a compiled regex per file): 865 + 397 MB |
| W5 warm sync 100k | 1.33 GB | 1.29 GB | the same call: 917 + 296 MB. s5cmd's sync also holds both listings |
| W1 download 100k | 525 MB | 57 MB | `Url` clones and joins for every (source, destination) pair, all collected before the first transfer (F13). Peak at 0.9 s of a 6 s run |
| W3 download 32 × 128 MiB | 1.07 GB | 28 MB | collected part bodies (E2) |

At the peak rs5cmd runs 63–68 threads on W1 (s5cmd 15–34), and glibc spreads its heap over 18
arenas.

**TLS (s3stub behind rustls)**

rustls / aws-lc takes 12–13 % of rs5cmd's CPU on the W1 and W3 downloads. s5cmd's `crypto/tls`
takes 40–47 %, because it re-handshakes on every reconnect. TLS is not why rs5cmd loses on large
downloads: E2 brings them to 1.01–1.40.

### Quick experiments (A5)

Each experiment changes one thing on a throwaway branch (`perf/exp-*`, local only). The full test
suite passes on every branch. Each experiment ran interleaved with s5cmd and the unchanged rs5cmd
on the rows its change targets. Ratios are against s5cmd. "before → after" is rs5cmd → the
experiment, from the same rounds.

| Id | Change | Rows | wall× before → after | Other effects |
|---|---|---|---|---|
| E1 | non-wildcard URLs test a prefix instead of compiling a match-all regex | `rm` 100k | **1.84 → 0.31** | CPU 0.86 → 0.22. `multi_delete` parses a `Url`, and so compiles a regex, for every deleted key |
| | | `ls` 100k / 1M | 0.52–0.54 → 0.48–0.49 | |
| | | W1 upload 100k | **1.34 → 0.86** | RSS **27× → 3.0×** s5cmd (1.33 GB → 145 MB); CPU 1.02 → 0.85. Every local file's `Url` compiled and kept a regex |
| | | W1 upload 10k / deep tree 100k | **1.20–1.30 → 0.73–0.88** | CPU 0.72–0.83; deep-tree RSS 56× → 5.7× |
| | | `sync` warm / changed / `--delete`, up and down, 100k | **0.74–1.23 → 0.30–0.46** | CPU **0.16–0.30**, RSS **0.28–0.31** |
| | | `sync` cold upload / download | 0.26 → **0.16**; 0.17 → 0.17 | RSS 1.26 → **0.16**; 7.2 → 5.5 |
| E2 | stream ranged-GET bodies to the file instead of collecting each part | W3 downloads (stub, TLS, MinIO) | **1.38–2.29 → 1.01–1.65** | RSS ÷ 5 to ÷ 18 (32 × 128 MiB: 59× s5cmd → 6.4×) |
| | | W2 download | 1.23–1.32 → **1.03–1.15** | RSS ÷ 2 |
| E3 | one buffered stdout writer instead of a locked, line-flushed print per line | `ls` 100k / 1M, to `/dev/null` and a pipe | 0.52–0.58 → **0.47–0.48** | CPU −16 to −28 % |
| | | `rm`, `cp` 100k | no change | |
| E4 | up to 10 DeleteObjects batches in flight | `rm` 10k / 100k (stub, MinIO) | **1.71–2.41 → 0.93–1.00** | |
| E7 | load the SDK config once per (region, profile) | `run` 10k lines (stub, MinIO) | **3.19–3.76 → 2.32–2.51** | CPU −20 to −28 %; still one new connection per line (10,001) |
| E7b | E7 plus one HTTP client (connection pool) for the process | `run` 10k lines (stub, MinIO) | **→ 1.35–1.68** | CPU **0.56–0.64** of s5cmd; 3–4 connections in all |
| E10 | read upload bodies from files in 256 KiB chunks (the SDK default is 4 KiB, each a blocking-pool round trip) | W3 uploads (stub, TLS) | **1.03–1.37 → 0.31–0.38** | CPU **0.28–0.33**; RSS 0.93–1.38 → 1.9–5.9 (53–199 MB: 256 KiB buffers × parts in flight). W1 / W2 uploads unchanged (bodies ≤ 1 MiB are read whole) |
| E9 | SDK default checksums (drop the WhenRequired overrides) | W1, W2, W3 up and down | unchanged, except W3 upload **1.18 → 1.51** | the current setting is right; CRC32 costs 28 % on large uploads |
| E6 | jemalloc / mimalloc (LD_PRELOAD), `MALLOC_ARENA_MAX=2`, glibc THP tunable | W1 100k, W3 down, W5 warm, `ls` 1M | jemalloc / mimalloc: **0.75 → 0.68** (W1 down), 1.21 → 1.05–1.07 (warm sync), 0.52 → 0.46–0.47 (`ls`) | jemalloc worse on large downloads (1.83 → 2.03); no allocator lowers peak RSS; arena cap worse; THP no effect (host THP is `always`) |
| E5 | release-profile knobs | W1 10k up / down, `ls` 1M, W3 4 × 1 GiB over TLS | fat LTO + 1 CGU: **0.79 → 0.70** (W1 down), **1.29 → 1.19** (W1 up), `ls` 0.52 → 0.50 | CPU −11 %, binary −24 %. `opt-level="s"` / `"z"` make small uploads 1.6× slower (W1 up **2.05–2.15**) |

E1 and E4 address different costs in `rm` (per-key work and request concurrency), so they should
stack. E7b's remaining gap has a visible cause. `run` polls every line's future on one task, with no
`spawn`, because the fast path's futures are not `Send`. So 10k lines use about 1.4 cores and 3–4
connections, where s5cmd uses up to 280.

#### E8: the SDK floor

The cost of one small request through aws-sdk-s3, against a lean hyper 1 client that signs with
rs5cmd's own SigV4 code (the `--fast` path's signer, here on tokio). 10,000 × 4 KiB, 256 in flight,
s3stub over HTTP, 10 interleaved rounds:

| Client | GET wall / CPU | PUT wall / CPU | CPU per request | Peak RSS |
|---|---|---|---:|---:|
| aws-sdk-s3 (as rs5cmd configures it) | 0.45 s / 1.49 s | 0.45 s / 1.47 s | ~148 µs | 37 MB |
| lean hyper + rs5cmd SigV4 | 0.14 s / 0.29 s | 0.13 s / 0.31 s | ~30 µs | 18–19 MB |
| lean hyper, unsigned | 0.11 s / 0.18 s | 0.10 s / 0.18 s | ~18 µs | 17–19 MB |

The SDK's per-request floor is about 5× the lean client's. Signing is about 11 µs of the lean
client's 30 µs. For comparison, rs5cmd's W1 download of 10k objects uses 2.1–2.2 CPU-s and s5cmd's
2.6–2.7. About 1.5 CPU-s of rs5cmd's figure (~70 %) is the SDK floor alone.

## 5. Findings

Every finding below is backed by the matrix (§3), a profile (§4) or an experiment (A5). The
ranking is in §6.

### Performance

1. **A regex per URL.** `Url::new` / `Url::parse` call `set_prefix_and_filter`, which compiles
   a regex for every URL, wildcard or not, and keeps it inside the `Url`. Three paths create one
   `Url` per object:
   - the local walk (`cp` / `sync` uploads): one per file;
   - `multi_delete`: one per deleted key;
   - the destination joins.

   Cost:
   - 46 % of the CPU of `rm` and 43 % of a warm `sync`;
   - 10 % of default-path uploads, 25 % of `--fast` uploads;
   - ~1.3 GB of heap for 100k local files.

   Removing it for non-wildcard URLs (E1):
   - `rm` 100k: 1.84 → **0.31**;
   - 100k-file upload: 1.34 → **0.86**, with 145 MB instead of 1.33 GB;
   - warm `sync`: 1.19 → **0.38**, with 0.18× the CPU.


   The same change helps every `sync` row in both directions:
   - warm, 10 %-changed and `--delete` syncs: 0.74–1.23 → **0.30–0.46** wall, **0.16–0.30**
     CPU, **0.28–0.31** RSS;
   - cold upload sync: 0.26 → **0.16**, RSS 1.26 → **0.16**;
   - small-object uploads: 1.20–1.34 → **0.73–0.88**.

   s5cmd builds its URLs the same way and spends 31 % of its warm-sync CPU in `regexp`.
2. **The SDK's per-request cost.** About 148 µs of CPU per small request, against ~30 µs for a
   lean hyper client with rs5cmd's own signer (E8). That is ~70 % of rs5cmd's CPU on small
   downloads, plus 10–14× s5cmd's context switches. `--fast` shows what a lean path does:
   0.25–0.35 wall and 0.18–0.36 CPU on small objects.
3. **Downloads collect every part before writing it.** Large downloads run at 1.3–2.4× s5cmd
   wall time, with RSS up to 59× s5cmd. The cost is an extra `memcpy` plus a fresh 8 MiB buffer
   per part. Streaming each part to the file (E2) brings them to 1.0–1.65, RSS ÷ 5–18.
4. **Uploads read files 4 KiB at a time.** `ByteStream::from_path` / `read_from` use the SDK's
   4 KiB default, and each chunk is a `tokio::fs` round trip through the blocking pool:
   - 1.05 M `read` and 2.1 M `futex` calls for 4 GiB;
   - 1.18–1.37× s5cmd wall, although s5cmd spends 69 % of its CPU hashing.

   With 256 KiB chunks (E10): **0.31–0.38** wall and 0.28–0.33 CPU, at 53–199 MB RSS.
5. **A fresh SDK config and client for every `run` line (F09).** `run` is 3.2–3.8× slower than
   s5cmd and opens 10,001 connections for 10k lines. With one config and one HTTP client per
   process (E7b) it is 1.35–1.68× at 0.56–0.64× s5cmd's CPU. What remains is one task polling
   every line.
6. **`rm` sends its DeleteObjects batches one at a time.** 1.7–2.4× s5cmd. Ten in flight (E4)
   bring it to 0.93–1.00. E1 and E4 should stack. `sync --delete` deletes one key per request
   (F26).
7. **`cp` / `mv` collect every (source, destination) pair first (F13).** The 100k download
   peaks at 525 MB before the first transfer; the deep-tree upload at 1.8 GB.
8. **No configured region costs ~3 s per invocation.** aws-config's region chain probes the EC2
   instance metadata service until it times out; the `us-east-1` fallback applies only
   afterwards. A single-key `ls` / `cp` takes 3.0 s against s5cmd's 5–8 ms. Even with a region,
   one-key commands take 11–12 ms against 5–8 ms.
9. **stdout is flushed line by line.** One buffered writer (E3) takes `ls` from 0.52 to 0.47 and
   cuts its CPU by 16–28 %.
10. **The binary is 1.53× s5cmd (2.08× as tar.gz).** There is no release profile (F80); three
    TLS stacks and 23 duplicated crates (F75); the SSO / OIDC / STS clients take 1.4 MiB. Fat
    LTO with one codegen unit gives −24 % size and −11 % CPU (faster on every row tried).
    `opt-level = "s"` is the only knob that gets under 0.9×, and it makes small uploads 1.6×
    slower.
11. **`--fast`:**
    - it holds whole objects in memory (F43: ~4.1 GB RSS for 1 GiB objects);
    - every run pins its threads to the first CPUs of its mask (F47);
    - on io_uring it is no faster than its epoll fallback, and slower over TLS.
12. **The allocator is a smaller lever.** mimalloc is 7–13 % faster on small objects, sync and
    `ls`, and no worse on large downloads. jemalloc is worse on large downloads. Neither lowers
    peak RSS.
13. **Retries (F02, F04).** `--retry-count` never reaches the SDK, and multipart loops keep
    sending parts after one has failed for good. With 30 % `503 SlowDown` on the stub, s5cmd
    completes every 4 GiB transfer, while rs5cmd fails every one after 549–728 requests.

### Correctness and behaviour (found while measuring; not fixed, not filed)

- **`cp --fast` on io_uring fails for any file or object of 4 GiB or more.** Every run of the W3
  1 × 4 GiB rows failed on io_uring, and all passed on the epoll fallback. monoio 0.2.4 passes
  the buffer length to the io_uring read/write entry as a 32-bit value, so a 4 GiB length
  becomes 0, and `read_exact_at` / `write_all_at` fail ("failed to fill / write whole buffer").
  Any size ≥ 4 GiB eventually leaves a remainder that is a multiple of 4 GiB. Bounded-chunk I/O,
  which F43 needs anyway, removes it.
- **`cp --fast` ignores SIGTERM and Ctrl-C until every transfer has finished, then exits 0.**
  `main` races the command against the signal in one `tokio::select!`. The fast path runs
  `run_transfers` inside `block_in_place` on that same task, so nothing polls the signal until
  it returns. Because a handler is installed, the default action does not apply either.

  Measured with 64 uploads at 1 s stub latency, SIGTERM after 1 s:
  - s5cmd exits at once (1);
  - the default path exits at once (130);
  - `--fast` keeps running for 15 s and exits **0**.
- **Local `cat` reads the whole file into memory (C5):** 1036 MB peak RSS for a 1 GiB file.
- **Upload integrity differs.** s5cmd sends Content-MD5 and signs the payload with SHA-256, which
  is 69 % of its CPU on large uploads. rs5cmd sends neither (checksums `WhenRequired`, unsigned
  payload). The SDK's default CRC32 would cost 28 % on large uploads (E9). A hardware CRC32C is
  the cheap way to send a checksum, if one is wanted.
- **Future timestamps.** s5cmd v2.3.0 leaves objects whose `LastModified` is in the future out of
  listings; rs5cmd lists them.
- **TIME_WAIT.** Against a non-loopback endpoint with kernel defaults, `run` exhausted the
  ephemeral ports (`EADDRNOTAVAIL`) because it opens a connection per line (item 5 above).

### Audit leads

The performance leads of the 2026-11-03 audit (`docs/IMPROVEMENT-PLAN.md` on the
`docs/improvement-plan` branch), each checked here, not taken as given.

| Lead | Audit claim (short) | Verdict | Evidence |
|---|---|---|---|
| F02 | `--retry-count` never reaches the SDK | **confirmed** (correctness; listed because it decides F04's cost) | fault run below: rs5cmd gives up after the SDK's 3 attempts, while s5cmd retries up to 10 times |
| F04 | multipart loops keep sending parts after one has failed for good | **confirmed in effect** | stub with 30 % `503 SlowDown`, 4 × 1 GiB, 2 rounds. s5cmd completes every run (113–123 requests). rs5cmd fails every run after 549–728 requests. Three of the four runs (716–728) match sending every one of the 512 parts of 8 MiB with up to 3 attempts each (512 × 1.39 ≈ 712). The stub's counters cannot show how many requests came after the first permanent failure |
| F05 | the download limiter paces disk writes, not the network | not measured | no rate-limited workload in the matrix; E2 moves the same lines |
| F09 | a fresh SDK config and client per target / line | **confirmed** | E7 / E7b: `run` 3.2–3.8× → 1.35–1.68× s5cmd; 10,001 connections → 3–4 |
| F13 | `cp` / `mv` collect every (source, destination) pair first | **confirmed** | W1 download 100k: peak heap 525 MB at 0.9 s of a 6 s run (`Url` clones and joins); deep-tree upload 1.8 GB |
| F17 | blocking file-system calls on tokio workers | not measured as such | the profiles show the opposite cost on uploads: every 4 KiB read goes through the blocking pool (E10) |
| F26 | `sync --delete` sends one DeleteObject per key | **confirmed** | the harness's request counts (one DELETE per key; s5cmd batches up to 1000) |
| F30 | `run` fans out numworkers × numworkers | not measured | W8 lines are single objects, so the fan-out never happens there; W8's remaining limit is the single task polling all lines |
| F43 | `--fast` buffers whole objects; no multipart | **confirmed** | `--fast` RSS ~4.1 GB for objects ≥ 128 MiB (150–195× s5cmd); ~900k page faults per W3 run |
| F47 | `--fast` pins shards to the first CPUs | **confirmed** | `strace`: shard *i* pins itself to the *i*-th CPU of the process mask (6, 7, 14, 15 here). Two concurrent runs pin to the same CPUs. On a host with more than 16 CPUs, every `--fast` run takes the first 16 of its mask |
| F53 | mount: every close / fsync re-uploads the whole file | **confirmed by reading** (s5cmd has no mount) | `Writer::flush` (`src/mount/writer.rs`) runs on every `flush` (each `close(2)`) and `fsync` of a dirty handle: `sync_all()` on the cache file, then a full `S3::upload` |
| F75 | three TLS stacks compiled in | **confirmed** | `cargo tree -d`: rustls 0.21 (SDK legacy hyper 0.14 client, monoio-rustls) and 0.23 (aws-lc), ring also linked; 23 duplicated crates |
| F80 | Cargo.toml hygiene, no release profile | **confirmed** | `tokio-stream` unused (cargo warns on every build); fat LTO + 1 codegen unit: −24 % binary, −11 % CPU (E5) |
| C5 | local `cat` reads the whole file into memory | **confirmed** | `cat` of a local 1 GiB file: peak RSS 1036 MB (remote `cat` of 4 GiB: ~20 MB, W7) |

## 6. Ranked recommendations

Ranked by measured effect × breadth, then by size and risk. Each is a Stage B wave or part of
one. Every change gets its own local branch, tests, and before/after rows (plan, "Process for
every change"). Correctness bugs on the same lines are fixed with the change and called out.

| # | Change | Measured effect (× s5cmd, before → after) | Size / risk | Bundled correctness |
|---|---|---|---|---|
| 1 | **Release profile:** fat LTO, 1 codegen unit, strip; drop `tokio-stream` (F80). `panic = "abort"` only if accepted (a closed stdout pipe then aborts) | −11 % CPU on every row tried; binary 1.53× → 1.16× (abort: 1.05×) | tiny / low; moves every baseline, so re-baseline after it | – |
| 2 | **No regex per URL:** test a plain prefix for non-wildcard URLs; compile a wildcard's regex once per pattern, not per object (E1) | `rm` 1.84 → 0.31; uploads 1.20–1.34 → 0.73–0.88 (100k: RSS 1.33 GB → 145 MB); every non-cold `sync` 0.74–1.23 → 0.30–0.46 at 0.3× s5cmd's RSS; ~25 % of `--fast` upload CPU | small / matching semantics must stay identical (tests on wildcard, prefix, `--exclude`) | – |
| 3 | **Stream ranged-GET bodies to the file (E2)** | W3 downloads 1.3–2.4 → 1.0–1.65; RSS ÷ 5–18; W2 download 1.23 → 1.03 | medium / write ordering and partial files | F03 (atomic downloads), F05 (limiter placement), F04 (stop after a failed part) |
| 4 | **Read upload bodies in larger chunks (E10)**, sized so parts in flight × buffer stay bounded (64–256 KiB) | W3 uploads 1.03–1.37 → 0.31–0.38, CPU 0.28–0.33 | tiny / RSS grows with the buffer | F04 |
| 5 | **One SDK config and one HTTP client per process (F09, E7b)**, then spawn `run`'s lines instead of polling them on one task | `run` 3.2–3.8 → 1.35–1.68 (CPU 0.56–0.64); the spawn step is not measured | medium / `--fast` futures are not `Send` (keep them on their own path) | F02 (`--retry-count` into the shared config) |
| 6 | **Concurrent DeleteObjects (E4); `sync --delete` through `multi_delete` (F26)** | `rm` 1.7–2.4 → 0.93–1.00, before E1's gain | small / needs F01 first (audit) | F01 |
| 7 | **No instance-metadata probe when no region is set:** fall back to `us-east-1` (or the endpoint's region) without the IMDS step, as s5cmd does | one-key commands without a region: 3.0 s; expected ~11 ms like with a region (not measured) | small / region semantics on EC2 | – |
| 8 | **Buffered stdout (E3)** | `ls` 0.52 → 0.47, CPU −16 to −28 % | small / flush on exit and on errors | – |
| 9 | **Stream (source, destination) pairs into the workers (F13)** | not measured; the 100k download's peak is 525 MB of collected pairs | medium / `--fast` still needs a slice | – |
| 10 | **Decide the data path (E8):** a lean client for GET / PUT / UploadPart / List on the default path (hyper 1 + rs5cmd's signer, the `--fast` code on tokio), keeping the SDK for credentials, configuration and rare calls | E8: ~5× less CPU per request (148 → 30 µs), RSS 37 → 18 MB at 256 in flight; `--fast` today: 0.25–0.35 wall on small objects | large / it moves maintenance from the SDK to rs5cmd | – |
| 11 | **`--fast` fixes:** bounded-chunk / multipart I/O (F43), no pinning or pid-offset pinning (F47), signal handling | `--fast` RSS 150–195× → bounded; ≥ 4 GiB on io_uring works | medium | io_uring ≥ 4 GiB, SIGTERM |
| 12 | **Binary trimming:** one TLS stack (F75), no legacy hyper 0.14 client, credential providers behind features | about 1–3 MB (estimate from cargo-bloat, not built) | medium / feature matrix | – |
| 13 | **mimalloc as the global allocator** | 7–13 % on small objects, sync and `ls` (E6); re-measure after 2–5 | tiny / binary size | – |

Items 1–8 are each small to medium and measured. Together they remove every loss in §3 except
small-object uploads on the default path and part of the large-download gap. Item 10 is the one
decision that changes the plan's shape. Without it, small-object CPU stays near the SDK floor
(~0.8× s5cmd); with it, `--fast`'s numbers become the default path's.

## 7. Proposed targets

Proposed for the maintainer to confirm, revised from the plan's provisional set using the data
above. All ratios are rs5cmd ÷ s5cmd v2.3.0 on the s3stub over HTTP, pinned as in §2, median of
interleaved rounds. "Never worse" means no loss outside the spread.

| Metric | Provisional (plan) | Proposed | Why |
|---|---|---|---|
| Wall time | never worse; ≤ 0.8 where the client is the bottleneck | **never worse on any row; ≤ 0.8 on small objects, listing, sync, `rm`, `run`, `cat` and large uploads; ≤ 1.0 on large downloads** | large downloads are kernel-copy bound on both sides (s5cmd: 95 % of its CPU in the kernel); E2 reaches 1.0–1.06 on single large files |
| CPU time | ≤ 0.5 | **≤ 0.5 on listing, sync, large uploads and `run`; ≤ 0.8 on small objects without item 10, ≤ 0.5 with it** | the SDK floor is ~70 % of rs5cmd's small-object CPU (E8) |
| Peak RSS | ≤ 0.67, independent of object size and count | **bounded: independent of object count and size; ≤ 1.0 on every row, ≤ 0.67 where the lean path is used** | s5cmd itself uses 23–60 MB on most rows; the SDK path alone needs ~37 MB at 256 in flight (E8) |
| Binary (stripped) | ≤ 0.7 | **≤ 1.0 firm, ≤ 0.85 stretch; tar.gz ≤ 1.5** | 0.7× needs `opt-level = "s"`, which makes small uploads 1.6× slower. Fat LTO + 1 CGU + abort is 1.05×, and items 12 and 10 remove more |
| Correctness | – | **no new failures:** the full test suite and the harness's checks on every row; the faults run completes like s5cmd | F02 / F04 today |

## 8. Limits

Loopback and a local bridge only: no real network, no real AWS. The stub answers instantly
unless latency is injected. MinIO shares the host with the client and is server-bound for small
objects. The host is shared, so rows that would need more than 8 logical CPUs, 32 GiB or heavy
disk I/O were scaled down and are marked in the raw results (`scaled`).

## 9. Reproducing

Everything runs in the `bench/review` compose project. Setup and the row registry are described
in `bench/review/README.md`.

| Stage | Command | Output |
|---|---|---|
| builds (A1) | `bench/review/build.sh <target>` inside bench; `size.sh` for sizes and profile knobs | `/opt/bins`, `results/size/` |
| matrix (A3) | `bench/review/a3.sh`; the `--fast` io_uring batch inside `fast-window.sh` | `results/a3*.jsonl.gz`, `scorecard-a3.md` |
| profiles (A4) | `bench/review/a4.sh [fp dwarf stat strace heap smaps fault]`; `checks.py` (C5, F47, SIGTERM) | `bench/review/prof/` (git-ignored), `results/a4-*` |
| experiments (A5) | `bench/review/a5.sh [batch]` (branches `perf/exp-*`, built by `build.sh exp-<name>`); `e8.py` | `results/a5*.jsonl.gz`, `scorecard-a5.md` |
| tables | `aggregate.py` (per-group statistics, rejection rule), `report.py` (scorecards) | – |

Raw results are one JSON line per run, with the command, the checks and the launcher's
measurements.
