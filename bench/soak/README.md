# Soak harness (`bench/soak/`)

Runs the soak scenarios S1–S14 against a real S3 endpoint for two or more clients and compares
them: rs5cmd (a build from the optimization review's `bins` volume) and the official s5cmd
release. A dry run does the same at small scale against a MinIO sidecar. Nothing is compiled
here, and everything runs inside hard resource limits, because the host is shared.

```sh
bench/soak/soak.sh dry                      # every scenario, small, against the MinIO sidecar
bench/soak/soak.sh dry --only S1,S4         # some scenarios
bench/soak/soak.sh aws                      # the real run (scale `full`), needs aws.env
bench/soak/soak.sh rgw                      # the same against a Ceph RGW, needs rgw.env
bench/soak/soak.sh aws cleanup RUN-ID       # delete what an interrupted run left behind
bench/soak/soak.sh down                     # remove containers, network and volumes
```

`soak.sh` is the only entry point. It runs on the host and needs `sudo` for docker, `nsenter`
and `tc`.

## Files

| File | Where it runs | What it is |
|---|---|---|
| `soak.sh` | host | Entry point: containers, limits check, binaries, rate limit with its removal trap, the host guard, then the runner |
| `dc` | host | `docker compose` for the project, with the io.max device found as `bench/review/dc` does |
| `compose.yml`, `compose.creds.yml` | host | The project `rs5cmd-soak`: `soak` (runner and clients) and, for the dry run, `minio` |
| `tc.sh` | host | The only script that writes a `tc` qdisc; see "Rate limit" |
| `dry-setup.sh` | host | Dry run: buckets and a policy-limited user in the MinIO sidecar |
| `soak.py` | container | The runner: targets, clients, guarded execution, verification, cleanup, report |
| `scenarios.py` | container | S0 (probe) and S1–S14, and the scale profiles `dry` and `full` |
| `s3check.py` | container | Independent checker: a SigV4 client on the standard library |
| `throttle_proxy.py` | container | Dry run: answers every Nth request with `503 SlowDown` (or `429`) |
| `iam/` | - | The IAM policy and lifecycle rule the soak expects on AWS (`BUCKET` is a placeholder) |
| `targets/dry.env` | - | The dry-run target (throw-away values of the sidecar) |
| `pack-results.sh`, `scan.py` | host | Before a commit: gzip the raw results, scan for credentials and private information |

## Limits, and what enforces them

| Limit | Enforced by |
|---|---|
| CPUs 3 and 11 only (one core, both hyperthreads), every container | `cpuset` in `compose.yml`; `soak.sh` checks the effective cpuset of every container before a run; `launch -c 3,11` |
| Memory limits of all containers together at most 2 GiB (tmpfs included) | `mem_limit` 1280m + 768m, swap off; `soak.sh` adds up the limits and stops above 2 GiB; `dc` refuses `run` (a one-off container would add its own limit) |
| Block I/O: 300 MB/s read, 200 MB/s write, 3000 / 2000 IOPS | `blkio_config` (io.max) on the device behind the docker root; checked by `soak.sh` |
| Scratch at most 25 GB, deleted afterwards | Files are sized by the scale profile (about 14 GB at the peak of S1); the runner stops at 24 GB; the cleanup deletes the run's scratch directory |
| Network 40 MB/s each way | `tc.sh` (tbf) on the soak container's two interfaces; the runner samples the container's counters every 2 s and stops if a window exceeds the cap |
| Host conntrack table | The host guard reads `nf_conntrack_count` every 10 s: warning above 10,000; above 30,000 it kills the soak's containers. The runner reads the same number from the guard's heartbeat and stops without one |
| Host load | Before every command the runner waits while load1 is above `LOAD_MAX` (11), as `matrix.py` does; waits are recorded |
| Volume per endpoint (about 80 GiB up, 55 GiB down) | Every scenario has a share (`shares` in `scenarios.py`); a scenario that goes beyond it stops the run |
| No compiles | The image is the review's toolchain image; `b-stage`, `s5cmd` and `launch` are copied from the review's `bins` volume, mounted read-only, and checked against its manifest |

## Rate limit

`tc.sh apply` shapes exactly two interfaces to 40 MB/s (320 Mbit/s) with tbf:

- uploads: `eth0` inside the soak container's network namespace (`nsenter -t <pid> -n tc ...`);
- downloads: the host-side veth of that `eth0`.

The veth is found, not guessed. The container's `eth0` iflink is read two independent ways (the
container's own `/sys`, and netlink inside the namespace) and both must agree. Exactly one host
interface must have that ifindex; it must be named `veth*`, be of kind veth, have no hardware
device, point back at the container's `eth0`, and be a port of the project's bridge
(`br-rs5cmd-soak`); the bridge's ports must be exactly the `eth0` peers of the project's running
containers. A failed assertion stops the script before anything is written. (`nsenter -n cat
/sys/class/net/eth0/iflink` cannot be used for the lookup: `/sys` still shows the host's
interfaces there.)

`soak.sh` removes the qdiscs through a trap on every way out and checks that no tbf qdisc is on
the host that was not there before. The guard verifies both qdiscs every 10 s and kills the soak
if one is missing. A scenario may ask the guard for a lower rate (S10 uses 1 MB/s); a request
above the cap is refused.

## Targets and credentials

A target is an env file. The dry run's is `targets/dry.env`. The real ones are written by hand on
the host, mode 600, and are never part of the repository:

| File | Keys |
|---|---|
| `~/.config/rs5cmd-soak/aws.env` | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `SOAK_BUCKET`; optional `AWS_ROLE_ARN` (S10), `SOAK_DENIED_BUCKET` (an existing bucket outside the policy, S8), `SOAK_PUBLIC_URL` and `SOAK_PUBLIC_REGION` (S12; default: an AWS Open Data object), `SOAK_STORAGE_CLASSES` |
| `~/.config/rs5cmd-soak/rgw.env` | the same, plus `SOAK_ENDPOINT_URL`; optional `SOAK_ADDRESSING` (`path` or `virtual`), `SOAK_CA_FILE` (a file in the same directory), `SOAK_RGW_VERSION` |

The directory is mounted read-only into the soak container (never through `environment` or
`env_file`, which `docker inspect` shows). The runner parses the file, hands the values to a
client only through its environment (never on a command line) and to the checker, and replaces
them, the bucket name and the endpoint host in everything it writes to `results/`. Profile files
for S10 are written to a tmpfs. After adding the files, start the run as usual: `dc` adds the
mount when the directory exists.

## Clients are configuration

```sh
bench/soak/soak.sh aws --client-bin rs5cmd-lean=/opt/bins/b-stage --client-flag rs5cmd-lean=--some-switch \
                       --client-env rs5cmd-lean=KEY=VALUE --clients rs5cmd,rs5cmd-lean,s5cmd
```

A name that starts with `s5cmd` speaks s5cmd's dialect, any other name rs5cmd's. The defaults are
`rs5cmd` = `/opt/bins/b-stage` and `s5cmd` = `/opt/bins/s5cmd` (v2.3.0). Each run records the
binaries' SHA-256, their versions and the `perf/stage-b` commit at the time of the copy;
`SOAK_BINS=keep` reuses the copies of the previous run, so that two endpoints get the same build.

## Scenarios

| Id | What runs | What is verified |
|---|---|---|
| S0 | One upload and one download of a probe file | Rates against the cap on the container's counters; ETag, SHA-256 |
| S1 | Sizes from 0 B to the largest object, up and down; `--part-size 5` around 5 MiB; `cat`, `pipe`; copies inside the bucket | Size and ETag of every object (MD5, or the MD5 of the part MD5s with the part size read from the object); SHA-256 of every download |
| S2 | Keys with space, `+ % = & ? # ~`, unicode, `//`, a leading and a trailing `/`, a 1024-byte key | Exact key names with the checker; content after download; `ls` names; `rm` leaves nothing |
| S3 | Many 1 KiB keys in directories; `ls` by prefix, wildcard, pattern, `--use-list-objects-v1`, `--json`; `du` | Counts, sizes, a sample of ETags, line counts, totals |
| S4 | `sync`: cold, warm, changed, `--delete`, `--size-only`, `--exclude`, `--include`, same-second, down | Remote key sets, sizes and ETags against the local tree; copy counts |
| S5 | `rm` by wildcard, by bare prefix, `sync --delete`, `rm` of everything | Key counts after each step |
| S6 | Content headers, storage classes, SSE, `--request-payer`, `--acl`, `select` | Headers on the stored object (HEAD); nothing readable without credentials |
| S7 | On the scratch bucket `<bucket>-mb-versions` only: `mb`, versioning, two versions, `ls --all-versions`, `cp --version-id`, `rm`, `rb` | Versions and delete markers with the checker; the bucket is gone |
| S8 | NoSuchKey, NoSuchBucket, AccessDenied, a bare `ls`, the wrong region | Non-zero exit, the error names the cause, no file written |
| S9 | Many workers of 1 KiB PUTs and GETs on one prefix (256 on AWS; 32 and stop at the first retry on an RGW) | Every object there and intact; retries and throttling counted from the clients' debug logs |
| S10 | Environment keys, a profile, `--credentials-file`, no credentials; a sync slowed to 1 MB/s (with the role, if there is one) | Uploads verified; no failed request during the slow sync |
| S11 | The endpoint: CRC32C header and aws-chunked trailer, right and wrong. The clients: a small and a multipart upload | Wrong checksums are refused; checksums left on the objects are right |
| S12 | `--no-sign-request` on a public object with no credentials in the environment | Content equals an unsigned GET by the checker. Expected to fail on rs5cmd until its issue #6 is fixed: recorded, not counted |
| S13 | A loop of uploads, downloads, small batches, syncs and metadata uploads paced to about 5 MB/s, two thirds up | Every step verified as above; failed steps and retries counted |
| S14 | W1 many 4 KiB objects up and down, W3 one large object up and down, W4 listing; rounds alternate the client order | Wall, user, sys and max RSS per command from `launch` |

S14 runs before S13, so that the performance rows do not depend on the long loop finishing.

Commands that the clients should handle alike run through `Run.each`, which diffs standard
output, standard error and exit codes after replacing run-specific parts (prefixes, times,
request ids, ETags). A difference is a finding, not a failure. Where a behaviour is checked
(`parity` in `scenarios.py`), s5cmd is the reference: a client that fails where s5cmd succeeds
gets a failed check; if every client fails, it is a note.

## Results

`results/<run-id>/`:

| File | Content |
|---|---|
| `summary.md` | Scenario table, resources per client, S14 rows, volume and cost, guards, cleanup, findings |
| `meta.json` | Target kind, scale, clients (binary, SHA-256, version, flags), limits, outcome |
| `steps.jsonl` | One record per command (`t: step`: argv, exit code, wall, CPU, RSS, bytes, load) and per check, scenario and cleanup |
| `outputs.jsonl` | Standard output and error of every command (capped) |
| `findings.jsonl` | `bug` (data or behaviour wrong), `gap` (a flag is missing), `diff` (clients differ), `endpoint`, `note` |
| `events.jsonl` | Load holds, rate changes, probe and throttle numbers, warnings |
| `state.json` | Scratch buckets and keys outside the run's prefix, for a later cleanup |

Every run works under `soak/<run-id>/<client>/` in the bucket and deletes it at the end, with any
scratch bucket, incomplete multipart upload and local scratch; what could not be removed is
listed in the summary.

Before committing: `bench/soak/pack-results.sh`, then
`env -u PYTHONSTARTUP python3 bench/soak/scan.py` (it reads the credential files, if present, to
search for their values and never prints them).

## What the dry run cannot show

- The assumed role of S10 (15-minute sessions): the dry run uses static keys for the slow sync.
- AWS behaviour: `BadDigest`, `AccessControlListNotSupported`, storage classes, KMS, bucket
  regions, virtual-hosted addressing. MinIO refuses keys with `//` and rewrites a leading `/`, so
  those two key shapes are first tried on the real endpoints.
- Real throttling: S9 in the dry run goes through `throttle_proxy.py`.
