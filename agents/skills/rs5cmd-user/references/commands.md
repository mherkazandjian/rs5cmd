# rs5cmd command reference

Checked against release v0.1.0. `rs5cmd <command> --help` is the authority.

Conventions used below:

- A remote path is `s3://bucket`, `s3://bucket/prefix/` or `s3://bucket/key`. Anything else is a
  local path.
- `*` and `?` are wildcards. Quote them. In an S3 path `*` also matches `/`, so
  `'s3://b/p/*.log'` matches at any depth. In a local path they follow shell rules: `*` stays
  inside one directory, `**` crosses directories, and a matched directory is taken whole.
- Filter flags `--include GLOB`, `--exclude GLOB`, `--include-from FILE`, `--exclude-from FILE`
  can be repeated. A pattern file has one glob per line; blank lines and `#` lines are skipped.
  Globs match the whole path relative to the source root (`*.log` matches `sub/x.log` too).
  Exclude wins over include. Filters apply only when a source expands (wildcard, prefix,
  directory), never to a single named object or file.

## Contents

- [Global flags](#global-flags)
- [ls](#ls), [du](#du), [tree](#tree), [head](#head)
- [cp and mv](#cp-and-mv), [sync](#sync), [rm](#rm)
- [cat](#cat), [pipe](#pipe), [presign](#presign), [select](#select)
- [mb, rb, bucket-version](#mb-rb-bucket-version)
- [run](#run)
- [mount](#mount), [import-s3cfg](#import-s3cfg), [completion](#completion)

## Global flags

Accepted before or after the command name.

| Flag | Meaning |
|---|---|
| `--endpoint-url URL` | S3-compatible endpoint. Env: `AWS_ENDPOINT_URL` |
| `--region NAME` | Region. Env: `AWS_REGION` |
| `--profile NAME` | Named profile from the shared AWS config and credentials files |
| `--credentials-file PATH` | Use this file instead of `~/.aws/credentials` |
| `--s3cfg PATH` | Apply an s3cmd config for this run; explicit flags win |
| `--addressing-style path\|virtual` | Default: path for a custom endpoint, virtual for AWS |
| `--source-region`, `--destination-region` | Region for one side of `cp` / `mv` / `sync` |
| `--source-endpoint-url`, `--destination-endpoint-url` | Endpoint for one side |
| `--use-dualstack-endpoint`, `--use-fips-endpoint` | AWS endpoint variants; no effect with a custom endpoint |
| `--no-verify-ssl` | Skip TLS certificate and hostname checks |
| `--tls-post-quantum` | Offer the post-quantum hybrid X25519MLKEM768 first in the TLS key exchange (default: X25519, P-256, P-384), on both engines. Each new TLS connection then costs a little more; a notice on stderr says so (not with `--json`) |
| `--ca-certs-file PEM` | Trust extra CA certificates; ignored with `--no-verify-ssl` |
| `-x`, `--proxy URL` | `socks5://`, `socks5h://`, `http://` or `https://[user:pass@]host:port` |
| `--request-payer requester` | For requester-pays buckets |
| `--no-sign-request` | Accepted, but has no effect in v0.1.0 (requests are still signed) |
| `--numworkers N` | Parallel operations. Default 256 |
| `--retry-count N` | Retries for transient errors. Default 10 |
| `--limit-upload RATE`, `--limit-download RATE` | Bandwidth cap in bytes per second: `512KB`, `100MB`, `1GB`. One budget shared by all workers of a command |
| `--limitrate RATE` | Caps both directions; the two flags above override it per direction |
| `--use-list-objects-v1` | Use ListObjects v1 (for providers without v2) |
| `--dry-run` | Print what would be done; change nothing |
| `--json` | One JSON object per line on stdout |
| `--color auto\|always\|never` | Default `auto`: colour only on a terminal, off when `NO_COLOR` is set or with `--json` |
| `--install-completion` | Install shell completion for `$SHELL` and exit (edits the shell rc file) |
| `-h`, `--help`, `-V`, `--version` | Help and version |

Environment variables read besides the standard AWS ones: `S3_ENDPOINT_URL` (endpoint, lowest
priority), `ALL_PROXY` / `HTTPS_PROXY` / `HTTP_PROXY` (used when `--proxy` is absent),
`NO_COLOR`, `RUST_LOG` (log level on stderr, default `warn`), `RS5CMD_FAST_CORES` (`--fast`
threads), `RS5CMD_MULTIPART_COPY_THRESHOLD` (bytes; server-side copies above it use multipart
copy, default 5 GiB).

## ls

`rs5cmd ls [OPTIONS] [TARGET]` lists buckets when `TARGET` is omitted, otherwise objects.

| Flag | Meaning |
|---|---|
| `-H`, `--humanize` | Sizes in binary units (`B`, `KiB`, `MiB`, ...) |
| `-e`, `--etag` | Show the ETag |
| `--storage-class` | Show the storage class |
| `--show-fullpath` | Print only the full `s3://bucket/key`, one per line |
| `--summarize` | Add `Total objects:` and `Total size:` lines |
| `--all-versions` | List every version (versioned bucket) |
| `--version-id ID` | List only this version of the object |
| `--start-after KEY` | Start after this key (S3 `StartAfter`) |
| `--newer-than T`, `--older-than T` | `T` is an age (`45s`, `30m`, `24h`, `7d`) or an RFC 3339 time (`2026-01-02T15:04:05Z`) |
| `--local-time` | Times in the local zone instead of UTC |
| filter flags | see top |

```bash
rs5cmd ls                                   # buckets
rs5cmd ls s3://bucket/logs/                 # one level; sub-prefixes print as DIR
rs5cmd ls 's3://bucket/logs/*'              # recursive
rs5cmd ls -H --summarize 's3://bucket/*.parquet'
rs5cmd ls --newer-than 24h --show-fullpath 's3://bucket/in/*'
```

Text output: `2026/01/02 03:04:05                 6  a.txt`, and `DIR  sub/` for a prefix.
JSON record: `{"key","size","type","etag","storage_class","last_modified"}`; buckets print
`{"name":"s3://bucket","created_at"}`; `--summarize` adds
`{"summary":true,"total_objects","total_size"}`.

## du

`rs5cmd du [OPTIONS] [TARGET]` prints the total size and count.

Flags: `-H/--humanize`, `-g/--group` (one line per storage class), `--exclude GLOB`,
`--all-versions`, `--version-id ID`.

```bash
rs5cmd du 's3://bucket/data/*'      # 16 bytes in 3 objects: s3://bucket/data/*
rs5cmd du -g 's3://bucket/*'
```

A prefix ending in `/` counts only that level; add `*` for the whole subtree. JSON:
`{"source","count","size"}` plus `"storage_class"` with `--group`.

## tree

`rs5cmd tree [OPTIONS] <TARGET>` draws the hierarchy under an S3 prefix or a local directory.

Flags: `--depth N` (levels to descend), `--limit N` (stop after N keys), `--size` (append each
object's size). Output is text only: with `--json`, `tree` fails (use `ls --json`).

## head

`rs5cmd head [OPTIONS] <s3://bucket[/key]>` prints object metadata, or `s3://bucket  exists`.
It fails with exit code 1 when the object or bucket is missing, which makes it an existence test.
A prefix (trailing `/`) or a wildcard is rejected.

Flags: `--version-id ID`, `--raw` (no wildcard interpretation, for keys containing `*` or `?`).

```bash
rs5cmd head s3://bucket/key
rs5cmd --json head s3://bucket/key      # {"key","size","etag","content_type","metadata",...}
```

## cp and mv

`rs5cmd cp [OPTIONS] <SOURCE>... <DESTINATION>`. `mv` takes the same flags and removes each
source after its copy succeeded. With more than one source, or a source that expands, the
destination must be a bucket, a prefix ending in `/`, or a local directory.

| Flag | Meaning |
|---|---|
| `--part-size MiB` | Multipart part size. Default 50 (s5cmd's), minimum 5. Files up to this size use one PUT. `--fast` uses 8 unless it is given |
| `--concurrency N` | Parts in flight per object. Default 5 (s5cmd's) |
| `--storage-class CLASS` | For example `STANDARD_IA`, `GLACIER` |
| `--acl ACL` | Canned ACL, for example `bucket-owner-full-control` |
| `--content-type TYPE` | Default for uploads is `application/octet-stream` |
| `--if-none-match` | Write only when the destination does not exist; an existing one is skipped |
| `--preserve-timestamps` | Store the local mtime as object metadata on upload; restore it on download |
| `--links` | Store local symlinks as small `.s5cmdlink` placeholder objects, and recreate them on download (Unix) |
| `--client-copy` | S3→S3 through the client (download then upload) instead of a server-side copy |
| `--version-id ID` | Copy one version of one remote object |
| `--all-versions` | Copy every version of one remote key; each lands at `<destination>_<versionid>` |
| `--remove-empty-dirs` | `mv` local→S3 only: remove source directories that became empty |
| `--fast` | io_uring engine for small objects; see `fast-path-and-mount.md` |
| `--follow-symlinks` | Always on; it cannot be turned off |
| `--force-glacier-transfer` | Accepted for compatibility; `cp` copies every storage class anyway |
| filter flags | see top |

```bash
rs5cmd cp file.txt s3://bucket/dir/                 # -> dir/file.txt
rs5cmd cp file.txt s3://bucket/dir/renamed.txt
rs5cmd cp a.txt b.txt 'logs/*.gz' s3://bucket/in/   # several sources
rs5cmd cp 'data/*' s3://bucket/data/                # recursive upload
rs5cmd cp 's3://bucket/data/*' ./data/              # recursive download
rs5cmd cp 's3://a/x/*' s3://b/x/                    # server-side copy
rs5cmd cp --if-none-match 'data/*' s3://bucket/data/
rs5cmd --source-endpoint-url https://old.example.com \
       --destination-endpoint-url https://new.example.com \
       cp 's3://bucket/*' s3://bucket/              # between two endpoints, via the client
rs5cmd mv 'outbox/*' s3://bucket/sent/
```

Never give a bare relative directory (`data`, `./data`) as the source. See `known-issues.md`.

## sync

`rs5cmd sync [OPTIONS] <SOURCE> <DESTINATION>` copies objects that are missing at the
destination or differ from it. By default an object is copied when the source is newer or the
sizes differ.

| Flag | Meaning |
|---|---|
| `--delete` | Remove destination objects that are not in the source |
| `--max-delete N` | Abort before doing anything if `--delete` would remove more than `N` objects |
| `--size-only` | Compare sizes only |
| `--checksum` | Compare MD5 / ETag. Multipart-uploaded objects are always copied again |
| `--exit-on-error` | Stop at the first failure; a failed copy phase skips the delete phase |
| `--storage-class`, `--acl`, `--content-type` | As for `cp` |
| `--preserve-timestamps` | As for `cp` |
| `--force-glacier-transfer` | Also copy GLACIER / DEEP_ARCHIVE objects (skipped by default) |
| `--exact-timestamps` | Accepted, no effect |
| filter flags | see top; they apply to both sides |

`sync` has no `--fast`, `--part-size` or `--concurrency`.

```bash
rs5cmd --dry-run sync --delete --max-delete 50 'site/*' s3://bucket/site/
rs5cmd sync --delete --max-delete 50 'site/*' s3://bucket/site/
rs5cmd sync --size-only 's3://bucket/site/*' ./site/
rs5cmd sync --exclude '*.tmp' 's3://a/data/*' s3://b/data/
```

The summary (`# synced N objects, deleted M`) goes to stderr and is omitted with `--json`.
Use a wildcard source. The local destination must already hold at least one file. Both points
are explained in `known-issues.md`.

## rm

`rs5cmd rm [OPTIONS] <TARGET>...` deletes objects or local files. Remote wildcards and prefixes
are deleted in batches of 1000 keys.

Flags: `--version-id ID` (one version), `--raw` (treat each target as a literal key: no wildcard
or trailing-slash expansion, so a key ending in `/` or containing `*` can be removed), filter
flags.

```bash
rs5cmd rm s3://bucket/a.txt s3://bucket/b.txt
rs5cmd rm 's3://bucket/tmp/*'
rs5cmd rm --exclude '*.keep' 's3://bucket/tmp/*'
rs5cmd rm --raw 's3://bucket/weird*name'
```

`--dry-run rm` prints nothing for a remote wildcard in v0.1.0. Preview with `ls` and the same
pattern and filters.

## cat

`rs5cmd cat [OPTIONS] <SOURCE>` writes an object to stdout. A remote wildcard, prefix or bucket
is concatenated in key order. A local source must be a single file. Flag: `--version-id ID`
(single object only).

## pipe

`rs5cmd pipe [OPTIONS] <s3://bucket/key>` uploads stdin, as a multipart upload when the input
fills at least one part. Without `--content-type`, the type is guessed from the key's extension.
It prints `pipe s3://bucket/key`, or with `--json`
`{"destination":"s3://bucket/key","operation":"pipe","success":true}`.

Flags: `-p/--part-size MiB` (default 50), `-c/--concurrency N` (default 5), `--storage-class`,
`--acl`, `--content-type`, `--content-encoding`, `--content-disposition`, `--cache-control`,
`--sse AES256|aws:kms`, `--sse-kms-key-id ID`.

```bash
pg_dump mydb | gzip | rs5cmd pipe --content-encoding gzip s3://bucket/db/mydb.sql.gz
```

Input shorter than one part is sent as a single PUT. The part size is fixed for the whole
stream, so on AWS S3 (at most 10,000 parts per upload) the default 50 MiB parts cap a stream at
about 488 GiB. Raise `--part-size` for longer streams. Memory holds the `--concurrency` parts in
flight and the one being read: about 300 MiB by default.

## presign

`rs5cmd presign [OPTIONS] <s3://bucket/key>` prints a presigned URL.

Flags: `--expire DURATION` (default `3h`; `90s`, `15m`, `2h30m`, or a bare number of seconds;
days are not accepted), `--put` (URL for an upload instead of a download), `--version-id ID`
(download URLs only).

```bash
rs5cmd presign --expire 15m s3://bucket/report.pdf
curl -T upload.bin "$(rs5cmd presign --put --expire 1h s3://bucket/upload.bin)"
```

No request is sent to the storage service; the URL is computed locally from the credentials,
region and endpoint. The object does not have to exist.

## select

`rs5cmd select [OPTIONS] <SOURCE>` runs S3 Select on one object or on every object a wildcard
matches. The storage service must support S3 Select.

| Flag | Meaning |
|---|---|
| `-e`, `--query SQL` | Default `SELECT * FROM s3object s` |
| `--input-format json\|csv\|parquet` | Default `json` |
| `--json-type lines\|document` | For JSON input. Default `lines` |
| `--compression none\|gzip\|bzip2` | Compression of the input |
| `--delimiter CHAR` | For CSV input. Default `,` |
| `--use-header NONE\|USE\|IGNORE` | For CSV input. Default `NONE` |
| `--output-format json\|csv` | Format of the result |
| `--exclude GLOB`, `--all-versions`, `--version-id ID` | Source selection (`--version-id` does not change which version is queried) |

```bash
rs5cmd select -e "SELECT s.id FROM s3object s WHERE s.level = 'error'" \
       --compression gzip 's3://bucket/logs/*.json.gz'
rs5cmd select --input-format csv --use-header USE \
       -e 'SELECT s.name FROM s3object s' s3://bucket/people.csv
```

## mb, rb, bucket-version

```bash
rs5cmd mb s3://new-bucket
rs5cmd rb s3://old-bucket               # must be empty
rs5cmd rb --force s3://old-bucket       # deletes the objects first
rs5cmd bucket-version s3://bucket                  # Enabled, Suspended or Unset
rs5cmd bucket-version --set Enabled s3://bucket    # or Suspended
```

`rb --force` does not remove old versions or delete markers, so it fails on a bucket that has
them.

## run

`rs5cmd run [FILE]` reads one rs5cmd command per line from `FILE` or stdin and runs up to
`--numworkers` lines at a time.

- Lines are split like a shell command line: quote arguments that contain spaces. No variables,
  pipes or redirection. Blank lines and lines starting with `#` are skipped.
- Lines do not start with `rs5cmd`. A line cannot be another `run`.
- Order is not kept. Do not rely on one line finishing before the next starts.
- A failing line prints `ERROR <line>: <reason>` to stderr. The others still run, and the exit
  code is 1.
- These outer flags are passed to every line: `--endpoint-url`, `--region`, `--profile`,
  `--addressing-style`, `--proxy`, `--no-verify-ssl`, `--no-sign-request`, `--ca-certs-file`,
  `--request-payer`, `--use-list-objects-v1`, `--numworkers`, `--retry-count`, `--limitrate`,
  `--dry-run`, `--json`.
- These are **not** passed on: `--limit-upload`, `--limit-download`, `--source-region`,
  `--destination-region`, `--source-endpoint-url`, `--destination-endpoint-url`,
  `--use-dualstack-endpoint`, `--use-fips-endpoint`, `--color`. Write them in each line that
  needs them. A line may carry any global flag of its own.
- A bandwidth limit applies to each line separately, not to the whole batch.

```bash
rs5cmd --dry-run run jobs.txt
rs5cmd --numworkers 16 --json run jobs.txt > results.jsonl
```

For many copies with the same shape, one `cp` with a wildcard or several sources is faster than
a `run` file, because it shares one client and one worker pool.

## mount

`rs5cmd mount [OPTIONS] <s3://bucket[/prefix]> <MOUNTPOINT>` exists only in a binary built with
`--features mount`. See `fast-path-and-mount.md`.

## import-s3cfg

`rs5cmd import-s3cfg [OPTIONS] <PATH>` converts an s3cmd config file once, so later runs need no
flags.

Flags: `--out FILE` (default `~/.rs5cmd`), `--profile NAME` (profile to write in
`~/.aws/credentials`, default `default`), `--no-credentials` (write the settings file only).
See `configuration.md` for what is written and how keys are mapped.

## completion

`rs5cmd completion bash|zsh|fish|powershell|elvish` prints a completion script to stdout.

```bash
rs5cmd completion bash > ~/.local/share/bash-completion/completions/rs5cmd
```

`rs5cmd --install-completion` does it in one step for bash, zsh and fish, but it also appends a
`source` line to `~/.bashrc` or `~/.zshrc`. Ask before running it for a user.
