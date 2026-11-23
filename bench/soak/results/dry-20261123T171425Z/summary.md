# Soak run dry-20261123T171425Z

Target: minio (dry), scale `dry`.

| Client | Binary | sha256 | Version | Flags | Env | Scenarios |
|---|---|---|---|---|---|---|
| rs5cmd-sdk | /opt/bins/l-p3 | 736ad3f50f60f126 | rs5cmd 0.1.0 | --data-path=sdk | - | all |
| rs5cmd-lean | /opt/bins/l-p3 | 736ad3f50f60f126 | rs5cmd 0.1.0 | --data-path=lean | - | all |
| rs5cmd-lean-tr | /opt/bins/l-p3 | 736ad3f50f60f126 | rs5cmd 0.1.0 | --data-path=lean --checksum-trailers | - | S1, S11, S14 (uploads only) |
| s5cmd | /opt/bins/s5cmd | 672299fea8941281 | v2.3.0-991c9fb | - | - | all |

rs5cmd binary: `l-p3`; its worktree was at perf/l-p3 3bc3317 2026-11-22T03:44:05Z when the binary was copied.

## Scenarios

| Scenario | Ran | rs5cmd-sdk | rs5cmd-lean | rs5cmd-lean-tr | s5cmd | Shared checks failed | MiB up | MiB down | Seconds |
|---|---|---|---|---|---|---|---|---|---|
| S0 | to the end | pass: 2 steps, 3 checks | -: 0 steps, 0 checks | -: 0 steps, 0 checks | -: 0 steps, 0 checks | 0 | 257.3 | 257.2 | 16.1 |
| S1 | to the end | pass: 29 steps, 64 checks | pass: 29 steps, 69 checks | pass: 15 steps, 55 checks | pass: 29 steps, 52 checks | 0 | 1607.6 | 978.5 | 113.8 |
| S2 | to the end | FAIL (3): 29 steps, 39 checks; flags missing | FAIL (3): 29 steps, 39 checks; flags missing | -: 0 steps, 0 checks | pass: 29 steps, 38 checks | 0 | 0.2 | 0.2 | 8.9 |
| S3 | to the end | pass: 10 steps, 10 checks | pass: 10 steps, 10 checks | -: 0 steps, 0 checks | pass: 10 steps, 10 checks | 0 | 16.3 | 18.3 | 27.7 |
| S4 | to the end | FAIL (3): 12 steps, 20 checks | FAIL (3): 12 steps, 20 checks | -: 0 steps, 0 checks | pass: 12 steps, 20 checks | 0 | 214.3 | 31.3 | 34.3 |
| S5 | to the end | FAIL (1): 4 steps, 4 checks | FAIL (1): 4 steps, 4 checks | -: 0 steps, 0 checks | pass: 4 steps, 4 checks | 0 | 0.7 | 6.3 | 2.2 |
| S6 | to the end | pass: 19 steps, 14 checks; flags missing | pass: 19 steps, 14 checks; flags missing | -: 0 steps, 0 checks | pass: 21 steps, 16 checks | 0 | 114.4 | 0.3 | 12.8 |
| S7 | to the end | pass: 12 steps, 9 checks; flags missing | pass: 12 steps, 9 checks; flags missing | -: 0 steps, 0 checks | pass: 10 steps, 9 checks | 0 | 0.1 | 0.1 | 3.5 |
| S8 | to the end | pass: 8 steps, 12 checks | pass: 8 steps, 12 checks | -: 0 steps, 0 checks | pass: 8 steps, 12 checks | 0 | 0.0 | 0.0 | 2.5 |
| S9 | to the end | FAIL (1): 4 steps, 5 checks | FAIL (1): 4 steps, 5 checks | -: 0 steps, 0 checks | pass: 4 steps, 5 checks | 0 | 19.0 | 17.9 | 35.2 |
| S10 | to the end | pass: 5 steps, 9 checks | pass: 5 steps, 9 checks | -: 0 steps, 0 checks | pass: 5 steps, 9 checks | 0 | 173.4 | 2.3 | 193.4 |
| S11 | to the end | pass: 6 steps, 24 checks | pass: 6 steps, 27 checks | pass: 6 steps, 29 checks | pass: 6 steps, 18 checks | 0 | 180.4 | 0.3 | 9.4 |
| S12 | to the end | pass: 2 steps, 0 checks; expected failure (#6), not counted | pass: 2 steps, 0 checks; expected failure (#6), not counted | -: 0 steps, 0 checks | pass: 2 steps, 3 checks | 0 | 0.0 | 0.0 | 3.6 |
| S14 | to the end | pass: 30 steps, 26 checks | pass: 30 steps, 40 checks | pass: 4 steps, 6 checks | pass: 30 steps, 26 checks | 0 | 1071.7 | 862.7 | 78.3 |
| S13 | to the end | pass: 16 steps, 23 checks | pass: 17 steps, 24 checks | -: 0 steps, 0 checks | pass: 17 steps, 24 checks | 0 | 694.3 | 310.0 | 210.0 |

## Resources per client

| Scenario | Client | Wall s | CPU s | Max RSS MiB | MiB up | MiB down | Retries seen |
|---|---|---|---|---|---|---|---|
| S0 | rs5cmd-sdk | 14.1 | 0.6 | 21 | 257.3 | 257.2 | 0 |
| S1 | rs5cmd-sdk | 22.4 | 1.1 | 77 | 402.1 | 326.1 | 0 |
| S1 | rs5cmd-lean | 22.4 | 1.1 | 77 | 402.0 | 326.0 | 0 |
| S1 | rs5cmd-lean-tr | 12.1 | 0.5 | 76 | 401.6 | 0.5 | 0 |
| S1 | s5cmd | 22.6 | 1.7 | 78 | 401.9 | 325.9 | 0 |
| S2 | rs5cmd-sdk | 0.5 | 0.3 | 16 | 0.0 | 0.0 | 0 |
| S2 | rs5cmd-lean | 0.5 | 0.3 | 16 | 0.0 | 0.0 | 0 |
| S2 | s5cmd | 0.5 | 0.3 | 17 | 0.0 | 0.0 | 0 |
| S3 | rs5cmd-sdk | 2.2 | 0.9 | 44 | 5.6 | 6.6 | 0 |
| S3 | rs5cmd-lean | 2.2 | 0.6 | 37 | 5.0 | 6.8 | 0 |
| S3 | s5cmd | 2.7 | 1.6 | 55 | 5.7 | 2.7 | 0 |
| S4 | rs5cmd-sdk | 2.4 | 0.4 | 35 | 71.4 | 1.1 | 0 |
| S4 | rs5cmd-lean | 2.3 | 0.3 | 33 | 71.3 | 1.1 | 0 |
| S4 | s5cmd | 3.7 | 1.7 | 32 | 71.6 | 28.3 | 0 |
| S5 | rs5cmd-sdk | 0.4 | 0.1 | 27 | 0.2 | 1.1 | 0 |
| S5 | rs5cmd-lean | 0.4 | 0.1 | 27 | 0.2 | 1.1 | 0 |
| S5 | s5cmd | 0.5 | 0.3 | 46 | 0.2 | 0.1 | 0 |
| S6 | rs5cmd-sdk | 1.6 | 0.3 | 23 | 36.1 | 0.1 | 0 |
| S6 | rs5cmd-lean | 1.5 | 0.3 | 22 | 36.1 | 0.1 | 0 |
| S6 | s5cmd | 1.7 | 0.3 | 19 | 42.1 | 0.1 | 0 |
| S7 | rs5cmd-sdk | 0.2 | 0.1 | 15 | 0.0 | 0.0 | 0 |
| S7 | rs5cmd-lean | 0.2 | 0.1 | 15 | 0.0 | 0.0 | 0 |
| S7 | s5cmd | 0.1 | 0.1 | 19 | 0.0 | 0.0 | 0 |
| S8 | rs5cmd-sdk | 0.1 | 0.1 | 15 | 0.0 | 0.0 | 0 |
| S8 | rs5cmd-lean | 0.1 | 0.1 | 15 | 0.0 | 0.0 | 0 |
| S8 | s5cmd | 0.1 | 0.1 | 19 | 0.0 | 0.0 | 0 |
| S9 | rs5cmd-sdk | 9.0 | 1.3 | 23 | 6.9 | 5.4 | 105 |
| S9 | rs5cmd-lean | 9.2 | 0.8 | 23 | 5.9 | 5.5 | 0 |
| S9 | s5cmd | 10.6 | 2.0 | 34 | 6.1 | 5.1 | 125 |
| S10 | rs5cmd-sdk | 62.1 | 0.2 | 20 | 57.6 | 0.6 | 0 |
| S10 | rs5cmd-lean | 62.4 | 0.1 | 19 | 57.8 | 0.7 | 0 |
| S10 | s5cmd | 62.4 | 0.3 | 25 | 58.0 | 1.0 | 0 |
| S11 | rs5cmd-sdk | 1.5 | 0.1 | 28 | 45.1 | 0.1 | 0 |
| S11 | rs5cmd-lean | 1.5 | 0.1 | 28 | 45.1 | 0.1 | 0 |
| S11 | rs5cmd-lean-tr | 1.5 | 0.1 | 28 | 45.1 | 0.1 | 0 |
| S11 | s5cmd | 1.5 | 0.2 | 31 | 45.1 | 0.1 | 0 |
| S12 | rs5cmd-sdk | 0.0 | 0.0 | 14 | 0.0 | 0.0 | 0 |
| S12 | rs5cmd-lean | 0.0 | 0.0 | 14 | 0.0 | 0.0 | 0 |
| S12 | s5cmd | 0.0 | 0.0 | 18 | 0.0 | 0.0 | 0 |
| S14 | rs5cmd-sdk | 20.7 | 2.6 | 48 | 287.9 | 287.6 | 0 |
| S14 | rs5cmd-lean | 20.1 | 2.2 | 48 | 286.9 | 287.7 | 0 |
| S14 | rs5cmd-lean-tr | 6.0 | 0.2 | 25 | 209.1 | 1.0 | 0 |
| S14 | s5cmd | 20.7 | 3.8 | 54 | 287.6 | 285.6 | 0 |
| S13 | rs5cmd-sdk | 9.8 | 0.6 | 34 | 231.5 | 92.6 | 0 |
| S13 | rs5cmd-lean | 10.6 | 0.6 | 28 | 231.4 | 108.6 | 0 |
| S13 | s5cmd | 10.5 | 1.1 | 32 | 231.4 | 108.5 | 0 |

## S14 performance rows (median of the rounds)

Wall is bound by the rate limit; CPU is what compares. The criterion for the lean path is CPU at or below the sdk path on every row except `W7 pipe` (10 % and 0.05 s allowed for noise).

| Row | Client | Rounds | Wall s | User s | Sys s | CPU s | CPU / sdk | CPU / s5cmd | Max RSS MiB |
|---|---|---|---|---|---|---|---|---|---|
| W1 down | rs5cmd-lean | 2 | 0.40 | 0.09 | 0.14 | 0.23 | 0.74 | 0.57 | 38 |
| W1 down | rs5cmd-sdk | 2 | 0.44 | 0.15 | 0.16 | 0.31 | - | 0.78 | 42 |
| W1 down | s5cmd | 2 | 0.49 | 0.26 | 0.14 | 0.40 | 1.29 | - | 53 |
| W1 up | rs5cmd-lean | 2 | 0.60 | 0.09 | 0.09 | 0.19 | 0.66 | 0.49 | 36 |
| W1 up | rs5cmd-sdk | 2 | 0.59 | 0.17 | 0.12 | 0.28 | - | 0.74 | 44 |
| W1 up | s5cmd | 2 | 0.70 | 0.29 | 0.10 | 0.39 | 1.36 | - | 51 |
| W10 du | rs5cmd-lean | 2 | 0.03 | 0.01 | 0.01 | 0.01 | 0.96 | 0.61 | 16 |
| W10 du | rs5cmd-sdk | 2 | 0.03 | 0.01 | 0.01 | 0.02 | - | 0.64 | 17 |
| W10 du | s5cmd | 2 | 0.04 | 0.02 | 0.00 | 0.02 | 1.57 | - | 23 |
| W2 down | rs5cmd-lean | 2 | 1.11 | 0.03 | 0.10 | 0.13 | 0.98 | 0.82 | 22 |
| W2 down | rs5cmd-sdk | 2 | 1.13 | 0.03 | 0.10 | 0.13 | - | 0.84 | 24 |
| W2 down | s5cmd | 2 | 1.12 | 0.03 | 0.12 | 0.15 | 1.19 | - | 24 |
| W2 up | rs5cmd-lean | 2 | 1.14 | 0.01 | 0.04 | 0.05 | 0.95 | 0.46 | 27 |
| W2 up | rs5cmd-lean-tr | 2 | 1.14 | 0.02 | 0.04 | 0.06 | 0.98 | 0.48 | 25 |
| W2 up | rs5cmd-sdk | 2 | 1.59 | 0.02 | 0.04 | 0.06 | - | 0.49 | 27 |
| W2 up | s5cmd | 2 | 1.12 | 0.08 | 0.03 | 0.12 | 2.06 | - | 24 |
| W3 down | rs5cmd-lean | 2 | 1.76 | 0.01 | 0.09 | 0.10 | 1.01 | 0.85 | 16 |
| W3 down | rs5cmd-sdk | 2 | 1.77 | 0.01 | 0.09 | 0.10 | - | 0.84 | 16 |
| W3 down | s5cmd | 2 | 1.76 | 0.02 | 0.11 | 0.12 | 1.19 | - | 19 |
| W3 up | rs5cmd-lean | 2 | 2.14 | 0.01 | 0.04 | 0.05 | 0.98 | 0.36 | 17 |
| W3 up | rs5cmd-lean-tr | 2 | 1.87 | 0.02 | 0.04 | 0.06 | 1.10 | 0.40 | 17 |
| W3 up | rs5cmd-sdk | 2 | 1.93 | 0.01 | 0.04 | 0.05 | - | 0.36 | 18 |
| W3 up | s5cmd | 2 | 2.12 | 0.10 | 0.04 | 0.14 | 2.75 | - | 21 |
| W4 ls | rs5cmd-lean | 2 | 0.34 | 0.01 | 0.01 | 0.01 | 1.03 | 0.64 | 16 |
| W4 ls | rs5cmd-sdk | 2 | 0.35 | 0.01 | 0.01 | 0.01 | - | 0.62 | 16 |
| W4 ls | s5cmd | 2 | 0.35 | 0.02 | 0.01 | 0.02 | 1.62 | - | 23 |
| W5 sync warm | rs5cmd-lean | 2 | 0.04 | 0.01 | 0.02 | 0.03 | 0.98 | 0.20 | 21 |
| W5 sync warm | rs5cmd-sdk | 2 | 0.04 | 0.01 | 0.02 | 0.03 | - | 0.21 | 21 |
| W5 sync warm | s5cmd | 2 | 0.10 | 0.12 | 0.01 | 0.13 | 4.76 | - | 54 |
| W6 rm | rs5cmd-lean | 2 | 0.39 | 0.01 | 0.01 | 0.02 | 0.98 | 0.34 | 19 |
| W6 rm | rs5cmd-sdk | 2 | 0.41 | 0.01 | 0.01 | 0.02 | - | 0.34 | 20 |
| W6 rm | s5cmd | 2 | 0.40 | 0.05 | 0.01 | 0.05 | 2.92 | - | 26 |
| W7 cat | rs5cmd-lean | 2 | 0.89 | 0.01 | 0.07 | 0.08 | 0.99 | 1.26 | 15 |
| W7 cat | rs5cmd-sdk | 2 | 0.89 | 0.01 | 0.07 | 0.08 | - | 1.27 | 15 |
| W7 cat | s5cmd | 2 | 0.88 | 0.01 | 0.06 | 0.07 | 0.79 | - | 18 |
| W7 pipe | rs5cmd-lean | 2 | 1.02 | 0.02 | 0.03 | 0.05 | 1.10 | 0.69 | 48 |
| W7 pipe | rs5cmd-sdk | 2 | 0.98 | 0.02 | 0.03 | 0.05 | - | 0.62 | 48 |
| W7 pipe | s5cmd | 2 | 1.05 | 0.06 | 0.02 | 0.08 | 1.61 | - | 51 |
| W8 run up | rs5cmd-lean | 2 | 0.17 | 0.07 | 0.05 | 0.12 | 0.90 | 0.60 | 39 |
| W8 run up | rs5cmd-sdk | 2 | 0.18 | 0.09 | 0.04 | 0.13 | - | 0.67 | 42 |
| W8 run up | s5cmd | 2 | 0.21 | 0.17 | 0.02 | 0.20 | 1.49 | - | 38 |
| W9 cp1 | rs5cmd-lean | 2 | 0.02 | 0.00 | 0.01 | 0.01 | 0.98 | 1.15 | 15 |
| W9 cp1 | rs5cmd-sdk | 2 | 0.02 | 0.00 | 0.01 | 0.01 | - | 1.18 | 15 |
| W9 cp1 | s5cmd | 2 | 0.02 | 0.01 | 0.00 | 0.01 | 0.85 | - | 19 |
| W9 ls1 | rs5cmd-lean | 2 | 0.01 | 0.00 | 0.01 | 0.01 | 0.89 | 1.91 | 14 |
| W9 ls1 | rs5cmd-sdk | 2 | 0.01 | 0.00 | 0.01 | 0.01 | - | 2.16 | 14 |
| W9 ls1 | s5cmd | 2 | 0.01 | 0.00 | 0.00 | 0.01 | 0.46 | - | 18 |

## Volume

- Sent 4.56 GB, received 2.61 GB (the soak container's interface counters, checker traffic included).
- Requests by the clients, estimated from the operation counts: about 27,216 of the PUT / COPY / POST / LIST class and 13,133 of the GET / HEAD class.
- At AWS list prices (us and eu regions) that is about $0.38: requests $0.14, egress $0.23. (This target is not AWS: nothing was billed.)

## Guards

- Host conntrack count: highest seen 101 (warn above 10,000, abort above 30,000).
- Load holds: 2, 25 s in all.
- Highest rate (any window of 4 s or more, or a whole command of 2 s or more): 38.4 MB/s up, 38.4 MB/s down (cap 40).

## Cleanup

- 9194 objects deleted; left behind: nothing.

## Failed steps and checks (16)

A client that fails where the reference (s5cmd) passes, wrong data, or an unexpected exit code. The command is the client's last one before the check.
1. **S2 rs5cmd-sdk**: down question: content (known: #34, no cp --raw)
   - Detail: `exit 0, destination file a directory holding ['what?.txt']; `
   - Command: `/opt/bins/l-p3 --data-path=sdk cp s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s2/k/what?.txt <scratch>/rs5cmd-sdk/s2/dl/question.bin`
2. **S2 rs5cmd-sdk**: ls all keys: the names (known: #31)
   - Detail: `missing ['with space.txt'], extra ['with+space.txt']`
   - Command: `/opt/bins/l-p3 --data-path=sdk ls s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s2/k/*`
3. **S2 rs5cmd-sdk**: rm all keys: nothing left (known: #31)
   - Detail: `left: ['with space.txt']`
   - Command: `/opt/bins/l-p3 --data-path=sdk rm s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s2/k/*`
4. **S2 rs5cmd-lean**: down question: content (known: #34, no cp --raw)
   - Detail: `exit 0, destination file a directory holding ['what?.txt']; `
   - Command: `/opt/bins/l-p3 --data-path=lean cp s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s2/k/what?.txt <scratch>/rs5cmd-lean/s2/dl/question.bin`
5. **S2 rs5cmd-lean**: ls all keys: the names (known: #31)
   - Detail: `missing ['with space.txt'], extra ['with+space.txt']`
   - Command: `/opt/bins/l-p3 --data-path=lean ls s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s2/k/*`
6. **S2 rs5cmd-lean**: rm all keys: nothing left (known: #31)
   - Detail: `left: ['with space.txt']`
   - Command: `/opt/bins/l-p3 --data-path=lean rm s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s2/k/*`
7. **S4 rs5cmd-sdk**: sync right after a same-second sync copies nothing (known: #33)
   - Detail: `20 then 20 copies`
   - Command: `/opt/bins/l-p3 --data-path=sdk sync <scratch>/rs5cmd-sdk/s4/tree/ s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s4/t/`
8. **S4 rs5cmd-lean**: sync right after a same-second sync copies nothing (known: #33)
   - Detail: `20 then 20 copies`
   - Command: `/opt/bins/l-p3 --data-path=lean sync <scratch>/rs5cmd-lean/s4/tree/ s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s4/t/`
9. **S4 rs5cmd-sdk**: sync down (new directory): the tree arrives intact (known: #4)
   - Detail: `exit 1, 0 of 160 files; error: no match found for "<scratch>/rs5cmd-sdk/s4/down/*"`
   - Command: `/opt/bins/l-p3 --data-path=sdk sync s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s4/t/* <scratch>/rs5cmd-sdk/s4/down/`
10. **S4 rs5cmd-lean**: sync down (new directory): the tree arrives intact (known: #4)
   - Detail: `exit 1, 0 of 160 files; error: no match found for "<scratch>/rs5cmd-lean/s4/down/*"`
   - Command: `/opt/bins/l-p3 --data-path=lean sync s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s4/t/* <scratch>/rs5cmd-lean/s4/down/`
11. **S4 rs5cmd-sdk**: sync down again: copies nothing (known: #4)
   - Detail: `exit 1, 0 copies`
   - Command: `/opt/bins/l-p3 --data-path=sdk sync s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s4/t/* <scratch>/rs5cmd-sdk/s4/down/`
12. **S4 rs5cmd-lean**: sync down again: copies nothing (known: #4)
   - Detail: `exit 1, 0 copies`
   - Command: `/opt/bins/l-p3 --data-path=lean sync s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s4/t/* <scratch>/rs5cmd-lean/s4/down/`
13. **S5 rs5cmd-sdk**: rm of a bare prefix (no wildcard) deletes nothing (known: #30)
   - Detail: `500 of 500 keys under the prefix were deleted`
   - Command: `/opt/bins/l-p3 --data-path=sdk rm s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s3/k/d01/`
14. **S5 rs5cmd-lean**: rm of a bare prefix (no wildcard) deletes nothing (known: #30)
   - Detail: `500 of 500 keys under the prefix were deleted`
   - Command: `/opt/bins/l-p3 --data-path=lean rm s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s3/k/d01/`
15. **S9 rs5cmd-sdk**: 429 TooManyRequests is retried to success (known: #32)
   - Detail: `exit 1, 480 of 500 keys, 0 retries seen; ERROR cp <scratch>/src/s9small/d00/f0478 s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s9/k429/d00/f0478: service error: unhandled error (TooManyRequests): Error { `
   - Command: `/opt/bins/l-p3 --data-path=sdk --numworkers 32 cp <scratch>/src/s9small/ s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s9/k429/`
16. **S9 rs5cmd-lean**: 429 TooManyRequests is retried to success (known: #32)
   - Detail: `exit 1, 480 of 500 keys, 0 retries seen; ERROR cp <scratch>/src/s9small/d00/f0457 s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s9/k429/d00/f0457: service error: unhandled error (TooManyRequests): Error {`
   - Command: `/opt/bins/l-p3 --data-path=lean --numworkers 32 cp <scratch>/src/s9small/ s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-lean/s9/k429/`

## Data paths: lean against sdk

- `path` findings: 0. Each is one command where the same binary behaved differently between its data paths (exit code or output), or where a lean command took far longer than its sdk twin. They are listed under Findings; there should be none.
- Intended or known differences, checked or left out as described:
  1. Lean uploads of bodies held in memory (files up to 256 KiB, every part of `pipe`) carry a CRC32C header, and a multipart `pipe` ends with a composite checksum (`-N`). The SDK path sends no upload checksum. S1 and S11 check exactly this per client on the stored objects; none of it shows in rs5cmd's output.
  2. With `--checksum-trailers` (client rs5cmd-lean-tr) streamed bodies carry the CRC32C as an aws-chunked trailer. An endpoint that refuses trailers is recorded with its error text and version, and the client sits out.
  3. `pipe` on the lean path computes a CRC32C over every part, so its CPU is above the SDK path's: S14's CPU criterion leaves the `W7 pipe` row out.
  4. #33: a file written in the same second as its upload is uploaded again by the next sync (listed times are kept to the second), on both data paths, and how many depends on timing. S4 writes its files more than a second before each sync and judges the same-second step by bucket state and copy counts, not by comparing output.
  5. On the SDK with both settings, so controls and not lean tests: `--use-list-objects-v1`, copies inside the bucket (CopyObject, UploadPartCopy), `mb` / `rb` / `bucket-version`, `select`, the region lookup.
  6. #34: rs5cmd's `cp` has no `--cache-control`, `--content-encoding`, `--content-disposition` or `--sse` (S6 sends them through `pipe`, as a single PUT and as a multipart upload), `--expires` exists nowhere, `cp` has no `--raw`, and `rm` has no `--all-versions` (S7 removes the versions one by one with `rm --version-id`).
  7. The versions listing sends no `x-amz-request-payer` even with `--request-payer`. That cannot show on a bucket without requester-pays, so it is recorded here and not tested.
  8. The lean path has no upload stall check (the SDK fails an attempt after 5 s without progress). Every command is timed, and a lean command that takes more than twice its sdk twin plus 5 s is reported as a `path` finding.
  9. A retry on the lean path leaves no log line, so the harness cannot count its 503s (the SDK path's come from the SDK's retry log, s5cmd's from its debug log). In the dry run the injector's count per client stands in. Where the first 503 must stop a client (a shared cluster), the lean clients run with `--retry-count 0`, so that the 503 comes out as an error and stops the scenario.
  10. #6: `--no-sign-request` fails on rs5cmd builds without the fix: S12 records that and does not count it. With the fix, S12 also checks on a plain-HTTP endpoint that no request carries a signature.

## Findings (61 recorded, grouped where they say the same about several steps)

1. **[endpoint] S2**: not tested here, this endpoint refuses or rewrites such keys: double-slash, lead
2. **[endpoint] S6 rs5cmd-sdk**: --acl public-read: exit 0 on this endpoint (and 1 more steps: --acl public-read)
3. **[endpoint] S6 rs5cmd-lean**: --acl public-read: exit 0 on this endpoint (and 1 more steps: --acl public-read)
4. **[endpoint] S6 s5cmd**: --acl public-read: exit 0 on this endpoint (and 1 more steps: --acl public-read)
5. **[endpoint] S11**: a wrong CRC32C header is refused with XAmzContentChecksumMismatch, not BadDigest
6. **[endpoint] S11**: a wrong CRC32C trailer is refused with XAmzContentChecksumMismatch, not BadDigest
7. **[gap] S2 rs5cmd-sdk**: `cp --raw` is not in rs5cmd-sdk's help (s5cmd has it), #34
8. **[gap] S2 rs5cmd-lean**: `cp --raw` is not in rs5cmd-lean's help (s5cmd has it), #34
9. **[gap] S6 rs5cmd-sdk**: `cp --cache-control` is not in rs5cmd-sdk's help (s5cmd has it), #34
10. **[gap] S6 rs5cmd-lean**: `cp --cache-control` is not in rs5cmd-lean's help (s5cmd has it), #34
11. **[gap] S6 rs5cmd-sdk**: `cp --expires` is not in rs5cmd-sdk's help (s5cmd has it), #34
12. **[gap] S6 rs5cmd-lean**: `cp --expires` is not in rs5cmd-lean's help (s5cmd has it), #34
13. **[gap] S6 rs5cmd-sdk**: `cp --content-encoding` is not in rs5cmd-sdk's help (s5cmd has it), #34
14. **[gap] S6 rs5cmd-lean**: `cp --content-encoding` is not in rs5cmd-lean's help (s5cmd has it), #34
15. **[gap] S6 rs5cmd-sdk**: `cp --content-disposition` is not in rs5cmd-sdk's help (s5cmd has it), #34
16. **[gap] S6 rs5cmd-lean**: `cp --content-disposition` is not in rs5cmd-lean's help (s5cmd has it), #34
17. **[gap] S6 rs5cmd-sdk**: `cp --sse` is not in rs5cmd-sdk's help (s5cmd has it), #34
18. **[gap] S6 rs5cmd-lean**: `cp --sse` is not in rs5cmd-lean's help (s5cmd has it), #34
19. **[gap] S7 rs5cmd-sdk**: `rm --all-versions` is not in rs5cmd-sdk's help (s5cmd has it), #34
20. **[gap] S7 rs5cmd-lean**: `rm --all-versions` is not in rs5cmd-lean's help (s5cmd has it), #34
21. **[diff] S2**: trailing-slash key: removed by rm: s5cmd does not, rs5cmd-sdk, rs5cmd-lean does
   - Detail: `{"rs5cmd-sdk": "ok", "rs5cmd-lean": "ok", "s5cmd": "still there"}`
22. **[diff] S3**: ls --json: rs5cmd-sdk and s5cmd differ in stdout
   - Command: `/opt/bins/l-p3 --data-path=sdk --json ls s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s3/k/d00/*`
   - Detail: `{"stdout": {"lines": {"rs5cmd-sdk": 500, "s5cmd": 500}, "only_rs5cmd-sdk": ["{\"etag\":\"<etag>\",\"key\":\"f0000\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0001\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0002\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0003\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0004\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_c`
23. **[diff] S3**: du --humanize: rs5cmd-sdk and s5cmd differ in stdout
   - Command: `/opt/bins/l-p3 --data-path=sdk du --humanize s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s3/k/*`
   - Detail: `{"stdout": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["2.93 MiB bytes in 3000 objects: s3://<bucket>/<base>/s3/k/*"], "only_s5cmd": ["2.9M bytes in 3000 objects: s3://<bucket>/<base>/s3/k/*"]}}`
24. **[diff] S4**: sync cold: rs5cmd-sdk and s5cmd differ in stderr (and 5 more steps: sync --delete; sync --exclude; sync --include; sync after --size-only; sync changed)
   - Command: `/opt/bins/l-p3 --data-path=sdk sync <scratch>/rs5cmd-sdk/s4/tree/ s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s4/t/`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 0}, "only_rs5cmd-sdk": ["# synced 140 objects, deleted 0"], "only_s5cmd": []}}`
25. **[diff] S4**: sync warm: rs5cmd-sdk and s5cmd differ in stderr (and 1 more steps: sync --size-only)
   - Command: `/opt/bins/l-p3 --data-path=sdk sync <scratch>/rs5cmd-sdk/s4/tree/ s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s4/t/`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 0}, "only_rs5cmd-sdk": ["# nothing to sync; source and destination are already in sync"], "only_s5cmd": []}}`
26. **[diff] S4**: sync down (new directory): rs5cmd-sdk and s5cmd differ in exit, stdout, stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk sync s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s4/t/* <scratch>/rs5cmd-sdk/s4/down/`
   - Detail: `{"exit": {"rs5cmd-sdk": 1, "s5cmd": 0}, "stdout": {"lines": {"rs5cmd-sdk": 0, "s5cmd": 160}, "only_rs5cmd-sdk": [], "only_s5cmd": ["cp s3://<bucket>/<base>/s4/t/d0/sub0/f00000.log <work>/s4/down/d0/sub0/f00000.log", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00042.bin <work>/s4/down/d0/sub0/f00042.bin", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00063.txt <work>/s4/down/d0/sub0/f00063.txt", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00084.bin <work>/s4/down/d0/sub0/f00084.bin", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00105.txt <work>/s4/down/d0/sub0/f00105.txt", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00126.bin <work>/s4/down/d0/sub0/f00126.bin"]}, "stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 0}, "only_`
27. **[diff] S4**: sync down again: rs5cmd-sdk and s5cmd differ in exit, stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk sync s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s4/t/* <scratch>/rs5cmd-sdk/s4/down/`
   - Detail: `{"exit": {"rs5cmd-sdk": 1, "s5cmd": 0}, "stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 0}, "only_rs5cmd-sdk": ["error: no match found for \"<work>/s4/down/*\""], "only_s5cmd": []}}`
28. **[diff] S5**: rm prefix: rs5cmd-sdk and s5cmd differ in exit, stdout, stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk rm s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s3/k/d01/`
   - Detail: `{"exit": {"rs5cmd-sdk": 0, "s5cmd": 1}, "stdout": {"lines": {"rs5cmd-sdk": 500, "s5cmd": 0}, "only_rs5cmd-sdk": ["rm s3://<bucket>/<base>/s3/k/d01/f0000", "rm s3://<bucket>/<base>/s3/k/d01/f0001", "rm s3://<bucket>/<base>/s3/k/d01/f0002", "rm s3://<bucket>/<base>/s3/k/d01/f0003", "rm s3://<bucket>/<base>/s3/k/d01/f0004", "rm s3://<bucket>/<base>/s3/k/d01/f0005"], "only_s5cmd": []}, "stderr": {"lines": {"rs5cmd-sdk": 0, "s5cmd": 1}, "only_rs5cmd-sdk": [], "only_s5cmd": ["ERROR \"rm s3://<bucket>/<base>/s3/k/d01/\": s3 bucket/prefix cannot be used for delete operations (forgot wildcard character?)"]}}`
29. **[diff] S5**: sync --delete: rs5cmd-sdk and s5cmd differ in stdout, stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk sync --delete <scratch>/src/keep/ s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s3/k/`
   - Detail: `{"stdout": {"lines": {"rs5cmd-sdk": 1500, "s5cmd": 2000}, "only_rs5cmd-sdk": [], "only_s5cmd": ["rm s3://<bucket>/<base>/s3/k/d01/f0000", "rm s3://<bucket>/<base>/s3/k/d01/f0001", "rm s3://<bucket>/<base>/s3/k/d01/f0002", "rm s3://<bucket>/<base>/s3/k/d01/f0003", "rm s3://<bucket>/<base>/s3/k/d01/f0004", "rm s3://<bucket>/<base>/s3/k/d01/f0005"]}, "stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 0}, "only_rs5cmd-sdk": ["# synced 0 objects, deleted 1500"], "only_s5cmd": []}}`
30. **[diff] S6**: request-payer ls: rs5cmd-sdk and s5cmd differ in stdout
   - Command: `/opt/bins/l-p3 --data-path=sdk --request-payer requester ls s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s6/payer`
   - Detail: `{"stdout": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["<ts>              1024  payer"], "only_s5cmd": ["<ts>              1024  <base>/s6/payer"]}}`
31. **[diff] S8**: cat a missing key: rs5cmd-sdk and s5cmd differ in stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk cat s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s8/no-such-key`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["error: service error: NoSuchKey: The specified key does not exist.: NoSuchKey: The specified key does not exist."], "only_s5cmd": ["ERROR \"cat s3://<bucket>/<base>/s8/no-such-key\": given object s3://<bucket>/<base>/s8/no-such-key not found"]}}`
32. **[diff] S8**: cp a missing key: rs5cmd-sdk and s5cmd differ in stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk cp s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s8/no-such-key <scratch>/rs5cmd-sdk/s8/none.dl`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 2, "s5cmd": 1}, "only_rs5cmd-sdk": ["ERROR cp s3://<bucket>/<base>/s8/no-such-key <work>/s8/none.dl: service error: NoSuchKey: The specified key does not exist.: NoSuchKey: The specified key does not exist.", "error: one or more cp operations failed"], "only_s5cmd": ["ERROR \"cp s3://<bucket>/<base>/s8/no-such-key <work>/s8/none.dl\": NoSuchKey: The specified key does not exist. status code: 404, request id: <id>, host id: <id>"]}}`
33. **[diff] S8**: head a missing key: rs5cmd-sdk and s5cmd differ in stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk head s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s8/no-such-key`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["error: given object s3://<bucket>/<base>/s8/no-such-key not found"], "only_s5cmd": ["ERROR \"head s3://<bucket>/<base>/s8/no-such-key\": given object s3://<bucket>/<base>/s8/no-such-key not found"]}}`
34. **[diff] S8**: ls a missing bucket: rs5cmd-sdk and s5cmd differ in stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk ls s3://<bucket>-mb-nosuch-46864113/`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["error: service error: NoSuchBucket: The specified bucket does not exist: NoSuchBucket: The specified bucket does not exist"], "only_s5cmd": ["ERROR \"ls s3://<bucket>-mb-nosuch-46864113/\": NoSuchBucket: The specified bucket does not exist status code: 404, request id: <id>, host id: <id>"]}}`
35. **[diff] S8**: cp into a missing bucket: rs5cmd-sdk and s5cmd differ in stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk cp <scratch>/src/s8/obj.bin s3://<bucket>-mb-nosuch-46864113/x`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 2, "s5cmd": 1}, "only_rs5cmd-sdk": ["ERROR cp <scratch>/src/s8/obj.bin s3://<bucket>-mb-nosuch-46864113/x: service error: unhandled error (NoSuchBucket): Error { code: \"NoSuchBucket\", message: \"The specified bucket does not exist\" }", "error: one or more cp operations failed"], "only_s5cmd": ["ERROR \"cp <scratch>/src/s8/obj.bin s3://<bucket>-mb-nosuch-46864113/x\": NoSuchBucket: The specified bucket does not exist status code: 404, request id: <id>, host id: <id>"]}}`
36. **[diff] S8**: ls a bucket outside the policy: rs5cmd-sdk and s5cmd differ in stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk ls s3://<denied-bucket>/`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["error: service error: unhandled error (AccessDenied): Error { code: \"AccessDenied\", message: \"Access Denied.\" }"], "only_s5cmd": ["ERROR \"ls s3://<denied-bucket>/\": AccessDenied: Access Denied. status code: 403, request id: <id>, host id: <id>"]}}`
37. **[diff] S8**: ls with the wrong region: rs5cmd-sdk and s5cmd differ in stdout
   - Command: `/opt/bins/l-p3 --data-path=sdk ls s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s8/region.bin`
   - Detail: `{"stdout": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["<ts>                 6  region.bin"], "only_s5cmd": ["<ts>                 6  <base>/s8/region.bin"]}}`
38. **[diff] S10**: no credentials: must fail: rs5cmd-sdk and s5cmd differ in stderr
   - Command: `/opt/bins/l-p3 --data-path=sdk ls s3://<bucket>/soak/dry-20261123T171425Z/rs5cmd-sdk/s10/env.bin`
   - Detail: `{"stderr": {"lines": {"rs5cmd-sdk": 1, "s5cmd": 1}, "only_rs5cmd-sdk": ["error: dispatch failure: other: the credential provider was not enabled: no providers in chain provided credentials"], "only_s5cmd": ["ERROR \"ls s3://<bucket>/<base>/s10/env.bin\": NoCredentialProviders: no valid providers in chain. Deprecated. For verbose messaging see aws.Config.CredentialsChainVerboseErrors"]}}`
39. **[note] S6**: sse AES256 single: stored as asked: fails for every client
   - Detail: `{"rs5cmd-sdk": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "rs5cmd-lean": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "s5cmd": "exit 1: ERROR \"cp <scratch>/src/s6/obj.bin s3://<bucket>/soak/dry-20261123T171425Z/s5cmd/s6/sse-AES256-single\": NotImplemented: Server side encryption specified but KMS is not c"}`
40. **[note] S6**: sse AES256 multipart: stored as asked: fails for every client
   - Detail: `{"rs5cmd-sdk": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "rs5cmd-lean": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "s5cmd": "exit 1: ERROR \"cp <scratch>/src/s6/big.bin s3://<bucket>/soak/dry-20261123T171425Z/s5cmd/s6/sse-AES256-multipart\": NotImplemented: Server side encryption specified but KMS is no"}`
41. **[note] S6**: sse aws:kms single: stored as asked: fails for every client
   - Detail: `{"rs5cmd-sdk": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "rs5cmd-lean": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "s5cmd": "exit 1: ERROR \"cp <scratch>/src/s6/obj.bin s3://<bucket>/soak/dry-20261123T171425Z/s5cmd/s6/sse-aws-kms-single\": NotImplemented: Server side encryption specified but KMS is not "}`
42. **[note] S6**: sse aws:kms multipart: stored as asked: fails for every client
   - Detail: `{"rs5cmd-sdk": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "rs5cmd-lean": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "s5cmd": "exit 1: ERROR \"cp <scratch>/src/s6/big.bin s3://<bucket>/soak/dry-20261123T171425Z/s5cmd/s6/sse-aws-kms-multipart\": NotImplemented: Server side encryption specified but KMS is n"}`
43. **[note] S8 rs5cmd-sdk**: wrong region (works): exit 0
   - Detail: `"2026/11/23 17:18:07                 6  region.bin"`
44. **[note] S8 rs5cmd-lean**: wrong region (works): exit 0
   - Detail: `"2026/11/23 17:18:07                 6  region.bin"`
45. **[note] S8 s5cmd**: wrong region (works): exit 0
   - Detail: `"2026/11/23 17:18:07                 6  soak/dry-20261123T171425Z/s5cmd/s8/region.bin"`
46. **[note] S10**: no AWS_ROLE_ARN for this target: the slow sync ran with the profile's static keys, so credential refresh was not tested
47. **[note] S11 rs5cmd-sdk**: CRC32C left on uploaded objects: small none, mid none, big none, multi none, piped none, piped-multi none
48. **[note] S11 rs5cmd-lean**: CRC32C left on uploaded objects: small Wdv/Ag==, mid none, big none, multi none, piped Wdv/Ag==, piped-multi KYCxlg==-3
49. **[note] S11 rs5cmd-lean-tr**: CRC32C left on uploaded objects: small Wdv/Ag==, mid QsFY2g==, big MvGp6g==, multi KYCxlg==-3, piped Wdv/Ag==, piped-multi KYCxlg==-3
50. **[note] S11 s5cmd**: CRC32C left on uploaded objects: small none, mid none, big none, multi none, piped none, piped-multi none
51. **[note] S12 rs5cmd-sdk**: --no-sign-request fails as expected without the fix for #6: exit 1; 0 of 0 requests carried a signature
   - Command: `/opt/bins/l-p3 --data-path=sdk --no-sign-request cp s3://<bucket>-mb-public/hello.txt <scratch>/rs5cmd-sdk/s12/pub.dl`
   - Detail: `"ERROR cp s3://<bucket>-mb-public/hello.txt <scratch>/rs5cmd-sdk/s12/pub.dl: dispatch failure: other: the credential provider was not enabled: no providers in chain provided credentials\nerror: one or more cp operations failed"`
52. **[note] S12 rs5cmd-lean**: --no-sign-request fails as expected without the fix for #6: exit 1; 0 of 0 requests carried a signature
   - Command: `/opt/bins/l-p3 --data-path=lean --no-sign-request cp s3://<bucket>-mb-public/hello.txt <scratch>/rs5cmd-lean/s12/pub.dl`
   - Detail: `"ERROR cp s3://<bucket>-mb-public/hello.txt <scratch>/rs5cmd-lean/s12/pub.dl: dispatch failure: other: the credential provider was not enabled: no providers in chain provided credentials\nerror: one or more cp operations failed"`
