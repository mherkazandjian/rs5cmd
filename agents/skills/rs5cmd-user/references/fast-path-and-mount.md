# The `--fast` path and the FUSE mount

Checked against release v0.1.0. Both are optional Cargo features of rs5cmd:

| Feature | Gives | Platforms | In release binaries and packages |
|---|---|---|---|
| `fast` | `cp --fast`, `mv --fast` | Linux | Linux: yes. macOS: no |
| `mount` | the `mount` command | Linux, macOS (macFUSE) | No. Build from source |

## `--fast`

A second transfer engine for `cp` and `mv`, made for **many small objects**. It runs one
io_uring event loop per CPU core and signs requests itself instead of going through the AWS SDK.
The project's benchmarks show its largest gains on small objects and on downloads, mostly as
lower CPU use per request.

```bash
export AWS_ENDPOINT_URL=http://storage.example.com:9000 AWS_REGION=us-east-1
rs5cmd cp --fast 'dir/*' s3://bucket/prefix/
rs5cmd cp --fast 's3://bucket/prefix/*' ./out/
rs5cmd mv --fast 's3://bucket/in/*' s3://bucket/done/
```

### Requirements

- **A build with the `fast` feature.** Without it, rs5cmd prints
  `warning: --fast ignored (built without the `fast` feature); using default path` and does the
  copy the normal way.
- **An explicit endpoint** from `--endpoint-url`, `AWS_ENDPOINT_URL`, `--s3cfg` or `~/.rs5cmd`.
  `S3_ENDPOINT_URL` is not enough. Otherwise:
  `error: --fast currently requires --endpoint-url (e.g. http://host:9000)`.
  For AWS, pass the regional endpoint, for example `https://s3.eu-west-1.amazonaws.com`.
- **Path-style requests** (`https://host/bucket/key`) are always used. The service must accept
  them for the bucket.
- **TLS:** `https://` endpoints are verified against the public CA set. `--no-verify-ssl` works.
  `--ca-certs-file` does not, so an endpoint with a private CA fails with `UnknownIssuer`.
- **io_uring in the kernel and allowed for the process.** Docker's default seccomp profile
  blocks the io_uring system calls; the repository ships `seccomp-iouring.json` (the Docker
  default plus those calls) for `docker run --security-opt seccomp=...`. On a host where io_uring
  is disabled by sysctl, v0.1.0 was seen to fall back to epoll and still complete, but treat
  that as luck, not a guarantee.
- Credentials come from the normal chain, with `--region` and `--profile` honoured.

### What it does

- Upload, download, and S3→S3 copy (server-side), also mixed in one command, and `mv`.
- If any pair in the command is local→local, the whole command uses the default engine.
- Honours `--dry-run`, `--json`, `--retry-count`, `--numworkers`, `--no-verify-ssl`,
  `--tls-post-quantum`, `--region`, `--profile`, `--part-size`, and the `--include` /
  `--exclude` filters. A file larger than the part (8 MiB unless `--part-size` is given) goes to
  the default engine; a download is fetched in ranges of the part.
- Retries network errors and the status codes 5xx, 408 and 429 with a growing delay. Other 4xx
  answers fail at once.
- A failed transfer prints `ERROR cp <src> <dst>: <reason>`; the command then ends with
  `error: one or more fast-path transfers failed` and exit code 1.

### Flags it hands to the default engine

With any of these the whole command runs on the default engine, so the flag takes effect, and
rs5cmd prints `warning: --fast does not support <flags>; using the default path`, naming them
in this order:

- Object properties: `--storage-class`, `--acl`, `--content-type`.
- Copy behaviour: `--preserve-timestamps`, `--if-none-match`, `--links`, `--version-id`,
  `--all-versions`, `--client-copy`, `--remove-empty-dirs`.
- Requests: `--concurrency` other than the default 5, `--no-sign-request` (the engine signs
  every request), `--proxy`, `--ca-certs-file`, `--request-payer`,
  `--addressing-style virtual`, `--limit-upload`, `--limit-download`, `--limitrate`,
  `--source-region` / `--destination-region`, `--source-endpoint-url` /
  `--destination-endpoint-url`.

The proxy environment variables (`ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`) are not used by the
engine: with one of them needed, give `--proxy` or drop `--fast`. `--use-dualstack-endpoint` and
`--use-fips-endpoint` do not apply with an explicit endpoint, which `--fast` needs, on either
engine.

Expanding a remote wildcard into a list of keys still uses the default engine, so listing does
follow the proxy and TLS settings even when the transfers do not.

### Limits and tuning

- Memory is roughly (objects in flight) × (object size). Keep `--fast` for objects of a few
  megabytes at most, and lower `--numworkers` if memory is tight.
- `--numworkers` is divided among the cores. The number of cores used defaults to the number
  available, capped at 16; set `RS5CMD_FAST_CORES=N` to change it.
- `sync` has no `--fast`.

## `mount`

Presents a bucket or a prefix as a local directory through FUSE.

```bash
mkdir -p /mnt/s3
rs5cmd mount s3://bucket/prefix /mnt/s3 &      # runs until unmounted
ls -l /mnt/s3
fusermount3 -u /mnt/s3                         # unmount (Linux)
```

### Requirements

- A binary built with the feature: `cargo build --release --locked --features mount`, or
  `--features "fast mount"` on Linux. The release binaries answer
  `error: unrecognized subcommand 'mount'`.
- At run time on Linux: FUSE 3 with the `fusermount3` helper and access to `/dev/fuse`. In a
  container that also means passing the device and the `SYS_ADMIN` capability. On macOS:
  macFUSE.
- The mount point must be an existing, empty directory.
- The command stays in the foreground. Run it in the background or under a service manager, and
  wait until the directory answers before using it.

### Flags

| Flag | Default | Meaning |
|---|---|---|
| `--read-only` | off | Reject all writes |
| `--cache-dir DIR` | `$XDG_CACHE_HOME/rs5cmd/mount` or `~/.cache/rs5cmd/mount` | Where files being written are staged |
| `--vfs-read-chunk-size BYTES` | 4194304 (4 MiB) | Size of each ranged read |
| `--buffer-size BYTES` | 16777216 (16 MiB) | Read-ahead memory per open file |
| `--dir-cache-time T` | `5m` | How long a directory listing is cached (`500ms`, `30s`, `5m`, `1h`) |
| `--attr-timeout T` | `1s` | How long file attributes are cached |
| `--part-size MiB` | 8 | Multipart part size for uploads (minimum 5) |
| `--concurrency N` | 8 | Parallel parts per file, for reads and uploads |
| `--allow-other` | off | Let other users in; unprivileged mounts need `user_allow_other` in `/etc/fuse.conf` |
| `--uid N`, `--gid N` | the caller's | Owner reported for every entry |

The global connection flags (`--endpoint-url`, `--region`, `--profile`, TLS, proxy) apply.

### How it behaves

- **Reads** fetch the object in chunks, keep recent chunks in memory, and read ahead on
  sequential access.
- **Writes** go to a local staging file. The whole file is uploaded when it is flushed or
  closed. Random writes, appends and truncation work, at the cost of local disk space equal to
  the file size.
- **Directories** are derived from key prefixes. `mkdir` creates an empty `name/` marker object.
- **Rename** is copy then delete. Renaming a directory rewrites every object below it.
- Ownership and permission bits are made up locally; they are not stored in S3.

### Caveats

- **Stale views.** Listings and attributes are cached. A change made by another client appears
  only after `--dir-cache-time` or `--attr-timeout` has passed. Lower them when other writers
  exist.
- **Not atomic.** `rename`, `mkdir` and `rmdir` are emulated and can race with other clients. A
  directory rename that fails midway leaves the source intact but the move incomplete.
- **Data loss window.** Bytes written but not yet flushed are lost if the mount is interrupted
  or killed. Close files, and check the exit status of the program that wrote them, before
  unmounting.
- **Failed upload on close** returns `EIO`. The staging file is kept and its path is logged, so
  the data can be recovered by hand. Other backend errors also surface as `EIO`.
- **Unreachable keys.** Objects whose keys contain `*` or `?`, or bytes that are not UTF-8,
  cannot be accessed through the mount.
- **Not POSIX.** Hard links, symbolic links, file locks and extended attributes are not
  implemented. Do not put databases or build trees on it.

For bulk transfers use `cp` or `sync`. They are faster and report per-object errors.
