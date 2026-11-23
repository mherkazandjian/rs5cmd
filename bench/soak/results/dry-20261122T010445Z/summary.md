# Soak run dry-20261122T010445Z

Target: minio (dry), scale `dry`.

| Client | Binary | sha256 | Version | Flags | Env |
|---|---|---|---|---|---|
| rs5cmd | /opt/bins/b-stage | 055b993ae213e7fe | rs5cmd 0.1.0 | - | - |
| s5cmd | /opt/bins/s5cmd | 672299fea8941281 | v2.3.0-991c9fb | - | - |

rs5cmd source: perf/stage-b at 9693674 2026-11-22T00:14:58Z when the binary was copied.

## Scenarios

| Scenario | Ran | rs5cmd | s5cmd | Shared checks failed | MiB up | MiB down | Seconds |
|---|---|---|---|---|---|---|---|
| S0 | to the end | pass: 2 steps, 3 checks | -: 0 steps, 0 checks | 0 | 257.3 | 257.3 | 16.2 |
| S1 | to the end | pass: 27 steps, 45 checks | pass: 27 steps, 45 checks | 0 | 691.9 | 651.9 | 47.7 |
| S2 | to the end | FAIL (3): 29 steps, 39 checks; flags missing | pass: 29 steps, 38 checks | 0 | 0.1 | 0.1 | 6.0 |
| S3 | to the end | pass: 10 steps, 10 checks | pass: 10 steps, 10 checks | 0 | 11.3 | 10.7 | 8.4 |
| S4 | to the end | FAIL (3): 12 steps, 18 checks | pass: 12 steps, 18 checks | 0 | 143.1 | 29.8 | 20.1 |
| S5 | to the end | FAIL (1): 4 steps, 4 checks | pass: 4 steps, 4 checks | 0 | 0.5 | 3.9 | 1.5 |
| S6 | to the end | pass: 11 steps, 8 checks; flags missing | pass: 12 steps, 9 checks | 0 | 0.1 | 0.0 | 3.4 |
| S7 | to the end | pass: 12 steps, 9 checks; flags missing | pass: 10 steps, 9 checks | 0 | 0.0 | 0.0 | 2.3 |
| S8 | to the end | pass: 8 steps, 12 checks | pass: 8 steps, 12 checks | 0 | 0.0 | 0.0 | 1.6 |
| S9 | to the end | FAIL (1): 4 steps, 5 checks | pass: 4 steps, 5 checks | 0 | 13.0 | 11.7 | 20.0 |
| S10 | to the end | pass: 5 steps, 9 checks | pass: 5 steps, 9 checks | 0 | 115.7 | 1.6 | 131.5 |
| S11 | to the end | pass: 2 steps, 3 checks | pass: 2 steps, 3 checks | 0 | 24.1 | 0.1 | 1.1 |
| S12 | to the end | pass: 2 steps, 0 checks; expected failure (#6), not counted | pass: 2 steps, 1 checks | 0 | 0.0 | 0.0 | 0.4 |
| S14 | to the end | pass: 10 steps, 10 checks | pass: 10 steps, 10 checks | 0 | 280.2 | 280.5 | 23.0 |
| S13 | to the end | pass: 97 steps, 115 checks | pass: 99 steps, 118 checks | 0 | 699.8 | 351.5 | 210.0 |

## Resources per client

| Scenario | Client | Wall s | CPU s | Max RSS MiB | MiB up | MiB down | Retries seen |
|---|---|---|---|---|---|---|---|
| S0 | rs5cmd | 14.2 | 0.7 | 20 | 257.3 | 257.3 | 0 |
| S1 | rs5cmd | 20.6 | 1.0 | 21 | 346.0 | 326.0 | 0 |
| S1 | s5cmd | 20.9 | 1.6 | 25 | 345.9 | 325.9 | 0 |
| S2 | rs5cmd | 0.5 | 0.3 | 16 | 0.0 | 0.0 | 0 |
| S2 | s5cmd | 0.5 | 0.3 | 19 | 0.0 | 0.0 | 0 |
| S3 | rs5cmd | 2.4 | 0.9 | 41 | 5.6 | 6.6 | 0 |
| S3 | s5cmd | 2.5 | 1.6 | 56 | 5.7 | 2.7 | 0 |
| S4 | rs5cmd | 2.4 | 0.4 | 37 | 71.4 | 1.1 | 0 |
| S4 | s5cmd | 3.7 | 1.8 | 33 | 71.7 | 28.3 | 0 |
| S5 | rs5cmd | 0.4 | 0.1 | 26 | 0.2 | 1.0 | 0 |
| S5 | s5cmd | 0.5 | 0.3 | 50 | 0.2 | 0.1 | 0 |
| S6 | rs5cmd | 0.2 | 0.1 | 15 | 0.0 | 0.0 | 0 |
| S6 | s5cmd | 0.2 | 0.1 | 19 | 0.0 | 0.0 | 0 |
| S7 | rs5cmd | 0.2 | 0.1 | 15 | 0.0 | 0.0 | 0 |
| S7 | s5cmd | 0.1 | 0.1 | 19 | 0.0 | 0.0 | 0 |
| S8 | rs5cmd | 0.1 | 0.1 | 15 | 0.0 | 0.0 | 0 |
| S8 | s5cmd | 0.1 | 0.1 | 17 | 0.0 | 0.0 | 0 |
| S9 | rs5cmd | 6.5 | 1.3 | 23 | 6.8 | 5.3 | 105 |
| S9 | s5cmd | 10.5 | 2.2 | 34 | 6.2 | 5.1 | 125 |
| S10 | rs5cmd | 62.1 | 0.2 | 19 | 57.6 | 0.6 | 0 |
| S10 | s5cmd | 62.4 | 0.3 | 28 | 58.1 | 1.0 | 0 |
| S11 | rs5cmd | 0.4 | 0.0 | 17 | 12.0 | 0.0 | 0 |
| S11 | s5cmd | 0.4 | 0.0 | 19 | 12.0 | 0.0 | 0 |
| S12 | rs5cmd | 0.0 | 0.0 | 14 | 0.0 | 0.0 | 0 |
| S12 | s5cmd | 0.0 | 0.0 | 19 | 0.0 | 0.0 | 0 |
| S14 | rs5cmd | 9.6 | 1.6 | 48 | 140.0 | 140.0 | 0 |
| S14 | s5cmd | 10.2 | 2.3 | 53 | 139.9 | 139.5 | 0 |
| S13 | rs5cmd | 17.3 | 2.3 | 34 | 320.0 | 175.8 | 0 |
| S13 | s5cmd | 19.7 | 4.0 | 38 | 379.8 | 175.1 | 0 |

## S14 performance rows (median of the rounds)

| Row | Client | Rounds | Wall s | User s | Sys s | CPU s | Max RSS MiB |
|---|---|---|---|---|---|---|---|
| W1 down | rs5cmd | 2 | 0.45 | 0.16 | 0.17 | 0.33 | 48 |
| W1 down | s5cmd | 2 | 0.51 | 0.29 | 0.13 | 0.43 | 44 |
| W1 up | rs5cmd | 2 | 0.71 | 0.17 | 0.11 | 0.28 | 42 |
| W1 up | s5cmd | 2 | 0.93 | 0.30 | 0.10 | 0.40 | 53 |
| W3 down | rs5cmd | 2 | 1.75 | 0.01 | 0.09 | 0.10 | 16 |
| W3 down | s5cmd | 2 | 1.75 | 0.01 | 0.12 | 0.13 | 19 |
| W3 up | rs5cmd | 2 | 1.87 | 0.01 | 0.05 | 0.06 | 17 |
| W3 up | s5cmd | 2 | 1.90 | 0.11 | 0.04 | 0.15 | 18 |
| W4 ls | rs5cmd | 2 | 0.03 | 0.01 | 0.01 | 0.02 | 16 |
| W4 ls | s5cmd | 2 | 0.04 | 0.02 | 0.01 | 0.02 | 23 |

## Volume

- Sent 2.35 GB, received 1.68 GB (the soak container's interface counters, checker traffic included).
- Requests by the clients, estimated from the operation counts: about 20,245 of the PUT / COPY / POST / LIST class and 8,499 of the GET / HEAD class.
- At AWS list prices (us and eu regions) that is about $0.26: requests $0.10, egress $0.15. (This target is not AWS: nothing was billed.)

## Guards

- Host conntrack count: highest seen 126 (warn above 10,000, abort above 30,000).
- Load holds: 0, 0 s in all.
- Highest rate (any window of 4 s or more, or a whole command of 2 s or more): 38.4 MB/s up, 38.3 MB/s down (cap 40).

## Cleanup

- 6101 objects deleted; left behind: nothing.

## Failed steps and checks (8)

A client that fails where the reference (s5cmd) passes, wrong data, or an unexpected exit code. The command is the client's last one before the check.
1. **S2 rs5cmd**: down question: content
   - Detail: `exit 0, destination file a directory holding ['what?.txt']; `
   - Command: `/opt/bins/b-stage cp s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s2/k/what?.txt <scratch>/rs5cmd/s2/dl/question.bin`
2. **S2 rs5cmd**: ls all keys: the names
   - Detail: `missing ['with space.txt'], extra ['with+space.txt']`
   - Command: `/opt/bins/b-stage ls s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s2/k/*`
3. **S2 rs5cmd**: rm all keys: nothing left
   - Detail: `left: ['with space.txt']`
   - Command: `/opt/bins/b-stage rm s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s2/k/*`
4. **S4 rs5cmd**: sync right after a same-second sync copies nothing
   - Detail: `20 then 20 copies`
   - Command: `/opt/bins/b-stage sync <scratch>/rs5cmd/s4/tree/ s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s4/t/`
5. **S4 rs5cmd**: sync down (new directory): the tree arrives intact
   - Detail: `exit 1, 0 of 160 files; error: no match found for "<scratch>/rs5cmd/s4/down/*"`
   - Command: `/opt/bins/b-stage sync s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s4/t/* <scratch>/rs5cmd/s4/down/`
6. **S4 rs5cmd**: sync down again: copies nothing
   - Detail: `exit 1, 0 copies`
   - Command: `/opt/bins/b-stage sync s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s4/t/* <scratch>/rs5cmd/s4/down/`
7. **S5 rs5cmd**: rm of a bare prefix (no wildcard) deletes nothing
   - Detail: `500 of 500 keys under the prefix were deleted`
   - Command: `/opt/bins/b-stage rm s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s3/k/d01/`
8. **S9 rs5cmd**: 429 TooManyRequests is retried to success
   - Detail: `exit 1, 480 of 500 keys, 0 retries seen; ERROR cp <scratch>/src/s9small/d00/f0478 s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s9/k429/d00/f0478: service error: unhandled error (TooManyRequests): Error { code`
   - Command: `/opt/bins/b-stage --numworkers 32 cp <scratch>/src/s9small/ s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s9/k429/`

## Findings (45 recorded, grouped where they say the same about several steps)

1. **[endpoint] S2**: not tested here, this endpoint refuses or rewrites such keys: double-slash, lead
2. **[endpoint] S6 rs5cmd**: --acl public-read: exit 0 on this endpoint
3. **[endpoint] S6 s5cmd**: --acl public-read: exit 0 on this endpoint
4. **[endpoint] S11**: a wrong CRC32C header is refused with XAmzContentChecksumMismatch, not BadDigest
5. **[endpoint] S11**: a wrong CRC32C trailer is refused with XAmzContentChecksumMismatch, not BadDigest
6. **[gap] S2 rs5cmd**: `cp --raw` is not in rs5cmd's help (s5cmd has it)
7. **[gap] S6 rs5cmd**: `cp --cache-control` is not in rs5cmd's help (s5cmd has it)
8. **[gap] S6 rs5cmd**: `cp --expires` is not in rs5cmd's help (s5cmd has it)
9. **[gap] S6 rs5cmd**: `cp --content-encoding` is not in rs5cmd's help (s5cmd has it)
10. **[gap] S6 rs5cmd**: `cp --content-disposition` is not in rs5cmd's help (s5cmd has it)
11. **[gap] S6 rs5cmd**: `cp --sse` is not in rs5cmd's help (s5cmd has it) (and 1 more steps: `cp --sse` is not in rs5cmd's help (s5cmd has it))
12. **[gap] S7 rs5cmd**: `rm --all-versions` is not in rs5cmd's help (s5cmd has it)
13. **[diff] S2**: trailing-slash key: removed by rm: s5cmd does not, rs5cmd does
   - Detail: `{"rs5cmd": "ok", "s5cmd": "still there"}`
14. **[diff] S3**: ls --json: rs5cmd and s5cmd differ in stdout
   - Command: `/opt/bins/b-stage --json ls s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s3/k/d00/*`
   - Detail: `{"stdout": {"lines": {"rs5cmd": 500, "s5cmd": 500}, "only_rs5cmd": ["{\"etag\":\"<etag>\",\"key\":\"f0000\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0001\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0002\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0003\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\"STANDARD\",\"type\":\"file\"}", "{\"etag\":\"<etag>\",\"key\":\"f0004\",\"last_modified\":\"<ts>\",\"size\":1024,\"storage_class\":\`
15. **[diff] S3**: du --humanize: rs5cmd and s5cmd differ in stdout
   - Command: `/opt/bins/b-stage du --humanize s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s3/k/*`
   - Detail: `{"stdout": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["2.93 MiB bytes in 3000 objects: s3://<bucket>/<base>/s3/k/*"], "only_s5cmd": ["2.9M bytes in 3000 objects: s3://<bucket>/<base>/s3/k/*"]}}`
16. **[diff] S4**: sync cold: rs5cmd and s5cmd differ in stderr (and 5 more steps: sync --delete; sync --exclude; sync --include; sync after --size-only; sync changed)
   - Command: `/opt/bins/b-stage sync <scratch>/rs5cmd/s4/tree/ s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s4/t/`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 1, "s5cmd": 0}, "only_rs5cmd": ["# synced 140 objects, deleted 0"], "only_s5cmd": []}}`
17. **[diff] S4**: sync warm: rs5cmd and s5cmd differ in stderr (and 1 more steps: sync --size-only)
   - Command: `/opt/bins/b-stage sync <scratch>/rs5cmd/s4/tree/ s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s4/t/`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 1, "s5cmd": 0}, "only_rs5cmd": ["# nothing to sync; source and destination are already in sync"], "only_s5cmd": []}}`
18. **[diff] S4**: sync down (new directory): rs5cmd and s5cmd differ in exit, stdout, stderr
   - Command: `/opt/bins/b-stage sync s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s4/t/* <scratch>/rs5cmd/s4/down/`
   - Detail: `{"exit": {"rs5cmd": 1, "s5cmd": 0}, "stdout": {"lines": {"rs5cmd": 0, "s5cmd": 160}, "only_rs5cmd": [], "only_s5cmd": ["cp s3://<bucket>/<base>/s4/t/d0/sub0/f00000.log <work>/s4/down/d0/sub0/f00000.log", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00042.bin <work>/s4/down/d0/sub0/f00042.bin", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00063.txt <work>/s4/down/d0/sub0/f00063.txt", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00084.bin <work>/s4/down/d0/sub0/f00084.bin", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00105.txt <work>/s4/down/d0/sub0/f00105.txt", "cp s3://<bucket>/<base>/s4/t/d0/sub0/f00126.bin <work>/s4/down/d0/sub0/f00126.bin"]}, "stderr": {"lines": {"rs5cmd": 1, "s5cmd": 0}, "only_rs5cmd": ["error`
19. **[diff] S4**: sync down again: rs5cmd and s5cmd differ in exit, stderr
   - Command: `/opt/bins/b-stage sync s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s4/t/* <scratch>/rs5cmd/s4/down/`
   - Detail: `{"exit": {"rs5cmd": 1, "s5cmd": 0}, "stderr": {"lines": {"rs5cmd": 1, "s5cmd": 0}, "only_rs5cmd": ["error: no match found for \"<work>/s4/down/*\""], "only_s5cmd": []}}`
20. **[diff] S5**: rm prefix: rs5cmd and s5cmd differ in exit, stdout, stderr
   - Command: `/opt/bins/b-stage rm s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s3/k/d01/`
   - Detail: `{"exit": {"rs5cmd": 0, "s5cmd": 1}, "stdout": {"lines": {"rs5cmd": 500, "s5cmd": 0}, "only_rs5cmd": ["rm s3://<bucket>/<base>/s3/k/d01/f0000", "rm s3://<bucket>/<base>/s3/k/d01/f0001", "rm s3://<bucket>/<base>/s3/k/d01/f0002", "rm s3://<bucket>/<base>/s3/k/d01/f0003", "rm s3://<bucket>/<base>/s3/k/d01/f0004", "rm s3://<bucket>/<base>/s3/k/d01/f0005"], "only_s5cmd": []}, "stderr": {"lines": {"rs5cmd": 0, "s5cmd": 1}, "only_rs5cmd": [], "only_s5cmd": ["ERROR \"rm s3://<bucket>/<base>/s3/k/d01/\": s3 bucket/prefix cannot be used for delete operations (forgot wildcard character?)"]}}`
21. **[diff] S5**: sync --delete: rs5cmd and s5cmd differ in stdout, stderr
   - Command: `/opt/bins/b-stage sync --delete <scratch>/src/keep/ s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s3/k/`
   - Detail: `{"stdout": {"lines": {"rs5cmd": 1500, "s5cmd": 2000}, "only_rs5cmd": [], "only_s5cmd": ["rm s3://<bucket>/<base>/s3/k/d01/f0000", "rm s3://<bucket>/<base>/s3/k/d01/f0001", "rm s3://<bucket>/<base>/s3/k/d01/f0002", "rm s3://<bucket>/<base>/s3/k/d01/f0003", "rm s3://<bucket>/<base>/s3/k/d01/f0004", "rm s3://<bucket>/<base>/s3/k/d01/f0005"]}, "stderr": {"lines": {"rs5cmd": 1, "s5cmd": 0}, "only_rs5cmd": ["# synced 0 objects, deleted 1500"], "only_s5cmd": []}}`
22. **[diff] S6**: request-payer ls: rs5cmd and s5cmd differ in stdout
   - Command: `/opt/bins/b-stage --request-payer requester ls s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s6/payer`
   - Detail: `{"stdout": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["<ts>              1024  payer"], "only_s5cmd": ["<ts>              1024  <base>/s6/payer"]}}`
23. **[diff] S8**: cat a missing key: rs5cmd and s5cmd differ in stderr
   - Command: `/opt/bins/b-stage cat s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s8/no-such-key`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["error: service error: NoSuchKey: The specified key does not exist.: NoSuchKey: The specified key does not exist."], "only_s5cmd": ["ERROR \"cat s3://<bucket>/<base>/s8/no-such-key\": given object s3://<bucket>/<base>/s8/no-such-key not found"]}}`
24. **[diff] S8**: cp a missing key: rs5cmd and s5cmd differ in stderr
   - Command: `/opt/bins/b-stage cp s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s8/no-such-key <scratch>/rs5cmd/s8/none.dl`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 2, "s5cmd": 1}, "only_rs5cmd": ["ERROR cp s3://<bucket>/<base>/s8/no-such-key <work>/s8/none.dl: service error: NoSuchKey: The specified key does not exist.: NoSuchKey: The specified key does not exist.", "error: one or more cp operations failed"], "only_s5cmd": ["ERROR \"cp s3://<bucket>/<base>/s8/no-such-key <work>/s8/none.dl\": NoSuchKey: The specified key does not exist. status code: 404, request id: <id>, host id: <id>"]}}`
25. **[diff] S8**: head a missing key: rs5cmd and s5cmd differ in stderr
   - Command: `/opt/bins/b-stage head s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s8/no-such-key`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["error: given object s3://<bucket>/<base>/s8/no-such-key not found"], "only_s5cmd": ["ERROR \"head s3://<bucket>/<base>/s8/no-such-key\": given object s3://<bucket>/<base>/s8/no-such-key not found"]}}`
26. **[diff] S8**: ls a missing bucket: rs5cmd and s5cmd differ in stderr
   - Command: `/opt/bins/b-stage ls s3://<bucket>-mb-nosuch-43123737/`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["error: service error: NoSuchBucket: The specified bucket does not exist: NoSuchBucket: The specified bucket does not exist"], "only_s5cmd": ["ERROR \"ls s3://<bucket>-mb-nosuch-43123737/\": NoSuchBucket: The specified bucket does not exist status code: 404, request id: <id>, host id: <id>"]}}`
27. **[diff] S8**: cp into a missing bucket: rs5cmd and s5cmd differ in stderr
   - Command: `/opt/bins/b-stage cp <scratch>/src/s8/obj.bin s3://<bucket>-mb-nosuch-43123737/x`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 2, "s5cmd": 1}, "only_rs5cmd": ["ERROR cp <scratch>/src/s8/obj.bin s3://<bucket>-mb-nosuch-43123737/x: service error: unhandled error (NoSuchBucket): Error { code: \"NoSuchBucket\", message: \"The specified bucket does not exist\", aws_request_id: \"18EA388FC1ED6061\", s3_extended_request_id: \"dd9025bab4ad464b049177c95eb6ebf374d3b3fd1af9\n... [23 characters cut] ...\nERROR cp <scratch>/src/s8/obj.bin s3://<bucket>-mb-nosuch-43123737/x: service error: unhandled error (NoSuchBucket): Error { code: \"NoSuchBucket\", message: \"The specified bucket does not exist\", aws_request_id: \"18EA388FC1ED6061\", s3_extended_request_id: \"dd9025bab4ad464b049177c95eb6ebf374`
28. **[diff] S8**: ls a bucket outside the policy: rs5cmd and s5cmd differ in stderr
   - Command: `/opt/bins/b-stage ls s3://<denied-bucket>/`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["error: service error: unhandled error (AccessDenied): Error { code: \"AccessDenied\", message: \"Access Denied.\", s3_extended_request_id: \"dd9025bab4ad464b049177c95eb6ebf374d3b3fd1af9251148b658df7ac2e3e8\", aws_request_id: \"18EA388FCE824C40\" }"], "only_s5cmd": ["ERROR \"ls s3://<denied-bucket>/\": AccessDenied: Access Denied. status code: 403, request id: <id>, host id: <id>"]}}`
29. **[diff] S8**: ls with the wrong region: rs5cmd and s5cmd differ in stdout
   - Command: `/opt/bins/b-stage ls s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s8/region.bin`
   - Detail: `{"stdout": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["<ts>                 6  region.bin"], "only_s5cmd": ["<ts>                 6  <base>/s8/region.bin"]}}`
30. **[diff] S10**: no credentials: must fail: rs5cmd and s5cmd differ in stderr
   - Command: `/opt/bins/b-stage ls s3://<bucket>/soak/dry-20261122T010445Z/rs5cmd/s10/env.bin`
   - Detail: `{"stderr": {"lines": {"rs5cmd": 1, "s5cmd": 1}, "only_rs5cmd": ["error: dispatch failure: other: the credential provider was not enabled: no providers in chain provided credentials"], "only_s5cmd": ["ERROR \"ls s3://<bucket>/<base>/s10/env.bin\": NoCredentialProviders: no valid providers in chain. Deprecated. For verbose messaging see aws.Config.CredentialsChainVerboseErrors"]}}`
31. **[note] S6**: sse AES256: stored as asked: fails for every client
   - Detail: `{"rs5cmd": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "s5cmd": "exit 1: ERROR \"cp <scratch>/src/s6/obj.bin s3://<bucket>/soak/dry-20261122T010445Z/s5cmd/s6/sse-AES256\": NotImplemented: Server side encryption specified but KMS is not configur"}`
32. **[note] S6**: sse aws:kms: stored as asked: fails for every client
   - Detail: `{"rs5cmd": "exit 1: error: service error: unhandled error (NotImplemented): Error { code: \"NotImplemented\", message: \"Server side encryption specified but KMS is not configured (KMS not configured for a server side encry", "s5cmd": "exit 1: ERROR \"cp <scratch>/src/s6/obj.bin s3://<bucket>/soak/dry-20261122T010445Z/s5cmd/s6/sse-aws-kms\": NotImplemented: Server side encryption specified but KMS is not configu"}`
33. **[note] S8 rs5cmd**: wrong region (works): exit 0
   - Detail: `"2026/11/22 01:06:32                 6  region.bin"`
34. **[note] S8 s5cmd**: wrong region (works): exit 0
   - Detail: `"2026/11/22 01:06:32                 6  soak/dry-20261122T010445Z/s5cmd/s8/region.bin"`
35. **[note] S10**: no AWS_ROLE_ARN for this target: the slow sync ran with the profile's static keys, so credential refresh was not tested
36. **[note] S11 rs5cmd**: checksums left on uploaded objects: single PUT none, multipart none
37. **[note] S11 s5cmd**: checksums left on uploaded objects: single PUT none, multipart none
38. **[note] S12 rs5cmd**: --no-sign-request fails as expected (issue #6): exit 1
   - Command: `/opt/bins/b-stage --no-sign-request cp s3://<bucket>-mb-public/hello.txt <scratch>/rs5cmd/s12/pub.dl`
   - Detail: `"ERROR cp s3://<bucket>-mb-public/hello.txt <scratch>/rs5cmd/s12/pub.dl: dispatch failure: other: the credential provider was not enabled: no providers in chain provided credentials\nerror: one or more cp operations failed"`
