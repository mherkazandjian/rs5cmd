---
name: rs5cmd-user
description: >-
  Use when running or scripting rs5cmd, the Rust port of the s5cmd S3 and local-filesystem
  command-line tool. Covers installing it (release tarball, deb, rpm, Arch package); connecting to
  AWS S3 or an S3-compatible endpoint such as MinIO (credentials, profiles, --endpoint-url,
  AWS_ENDPOINT_URL, S3_ENDPOINT_URL, region, TLS, proxy, .s3cfg import); every command (ls, cp,
  mv, rm, cat, mb, rb, sync, du, tree, pipe, head, presign, select, run, bucket-version, mount,
  import-s3cfg, completion) and the global flags; the io_uring --fast path; the FUSE mount; and
  known pitfalls. Triggers: "rs5cmd", bulk or parallel S3 copy, sync with delete, batch files for
  run, presigned URLs, piping to or from S3, --json output for scripts or agents, migrating from
  s5cmd or s3cmd, or an rs5cmd error or surprising result. For changing rs5cmd's own source code,
  use the rs5cmd-developer skill instead.
---

# rs5cmd — user skill

rs5cmd is a Rust port of [s5cmd](https://github.com/peak/s5cmd): one binary that lists, copies,
moves, deletes and syncs objects between the local filesystem and S3 or S3-compatible storage,
running many operations in parallel. Project home: <https://github.com/mherkazandjian/rs5cmd>.

Everything here was checked against release **v0.1.0** and its source. The binary is the
authority: run `rs5cmd --help` or `rs5cmd <command> --help` when a flag matters, and re-read
[known issues](references/known-issues.md) after an upgrade, because several entries are bugs
that a later release may fix.

## Working rules

1. **Preview first.** Put `--dry-run` in front of a `cp`, `mv` or `sync` you have not run
   before and read the `(dry-run) cp <src> <dst>` lines. Check that every destination is where
   you expect. Preview a wildcard `rm` with `ls` and the same pattern; its dry-run prints
   nothing.
2. **Never pass a bare relative directory as a source** (`cp data dst/`, `sync ./data dst/`).
   In v0.1.0 this maps files to the wrong place and can empty the source files. Write `data/`
   or `'data/*'`. See [Paths](#paths-wildcards-and-where-things-land).
3. **Quote wildcards** (`'s3://bucket/logs/*'`) so the shell does not expand them.
4. **Set a region** (`AWS_REGION` or `--region`), also for MinIO and other custom endpoints.
   Without one, start-up waits a few seconds on instance-metadata lookups.
5. **Ask before deleting.** `rm` with a wildcard, `sync --delete`, `mv` and `rb --force` remove
   data. Show the user the preview and get a yes, unless they already told you to proceed.
6. Do not echo secrets. Use environment variables or a profile, not keys on the command line.

## Install

Prebuilt files are attached to each release at
<https://github.com/mherkazandjian/rs5cmd/releases>:

| Platform | File | Notes |
|---|---|---|
| Linux x86_64 / aarch64 | `rs5cmd-v<ver>-<arch>-unknown-linux-gnu.tar.gz` | needs glibc 2.28 or newer; includes `--fast` |
| macOS x86_64 / aarch64 | `rs5cmd-v<ver>-<arch>-apple-darwin.tar.gz` | no `--fast` |
| Debian / Ubuntu | `rs5cmd_<ver>-1_amd64.deb`, `_arm64.deb` | Debian 10+, Ubuntu 18.10+ |
| RHEL / Rocky / Alma / Fedora | `rs5cmd-<ver>-1.x86_64.rpm`, `.aarch64.rpm` | RHEL family 8+ |
| Arch | `rs5cmd-<ver>-1-x86_64.pkg.tar.zst`, `-aarch64` | also a `PKGBUILD` in `packaging/` |

```bash
# tarball (each one has a matching .sha256 file)
sha256sum -c rs5cmd-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
tar xzf rs5cmd-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
sudo install rs5cmd-v0.1.0-x86_64-unknown-linux-gnu/rs5cmd /usr/local/bin/

sudo dpkg -i rs5cmd_0.1.0-1_amd64.deb             # Debian / Ubuntu
sudo rpm -i rs5cmd-0.1.0-1.x86_64.rpm             # RHEL family (or: dnf install ./<file>)
sudo pacman -U rs5cmd-0.1.0-1-x86_64.pkg.tar.zst  # Arch
rs5cmd --version
```

The packages install `/usr/bin/rs5cmd`. Release binaries do **not** contain the `mount` command;
that needs a build from source with `--features mount`
(see [fast path and mount](references/fast-path-and-mount.md)).

## Connect: credentials, endpoint, region

```bash
export AWS_ACCESS_KEY_ID=...  AWS_SECRET_ACCESS_KEY=...   # or use --profile NAME
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=https://s3.example.com            # only for non-AWS storage
rs5cmd ls                                                 # lists buckets
```

- **Credentials** come from the AWS SDK default chain: environment variables, the shared
  `~/.aws/credentials` and `~/.aws/config` files (`--profile NAME`, `--credentials-file PATH`),
  and container or instance metadata. SSO (IAM Identity Center) profiles need a build with the
  `sso` feature; release binaries and packages leave it out. With those, run
  `eval "$(aws configure export-credentials --profile NAME --format env)"` first.
- **Endpoint**, first match wins: `--endpoint-url`, `AWS_ENDPOINT_URL`, a `--s3cfg` file,
  `~/.rs5cmd`, then `S3_ENDPOINT_URL`. With a custom endpoint, requests use path-style
  addressing unless `--addressing-style virtual` is given.
- **Region**: `--region` or `AWS_REGION`, else the profile, else `us-east-1`.
- **TLS**: `--ca-certs-file bundle.pem` trusts a private CA. `--no-verify-ssl` turns off
  certificate and hostname checks; use it only for a trusted test endpoint. Key exchange is
  classic (X25519, P-256, P-384) unless `--tls-post-quantum` is given, which offers the
  post-quantum hybrid X25519MLKEM768 first, `--fast` included. That makes setting up each TLS
  connection somewhat slower (about 22 % more CPU per new connection; 1-4 % on 10,000 small
  objects, nothing on large transfers), and prints a notice saying so on stderr (none with
  `--json`).
- **From s3cmd**: `rs5cmd --s3cfg ~/.s3cfg ls` uses an s3cmd config for one run.
  `rs5cmd import-s3cfg ~/.s3cfg` saves it for later runs. It writes `~/.rs5cmd` and replaces
  the `[default]` profile in `~/.aws/credentials`; pass `--profile NAME` or `--no-credentials`
  to avoid that.

Details, proxy settings and the full `.s3cfg` key mapping:
[configuration](references/configuration.md).

## Commands

| Command | What it does |
|---|---|
| `ls [target]` | List buckets (no argument), or objects under a prefix or wildcard |
| `cp <src>... <dst>` | Copy local↔S3, S3↔S3 (server-side), local↔local; wildcards, several sources |
| `mv <src>... <dst>` | Same flags as `cp`; deletes each source after it was copied |
| `rm <target>...` | Delete objects or local files; wildcards and prefixes |
| `cat <src>` | Print one object, or all objects matching an S3 wildcard, to stdout |
| `mb` / `rb <s3://bucket>` | Create / remove a bucket (`rb --force` empties it first) |
| `sync <src> <dst>` | Copy what is new or changed; `--delete` removes extras |
| `du [target]` | Total size and object count |
| `tree <target>` | Hierarchical listing |
| `pipe <s3://bucket/key>` | Upload stdin |
| `head <s3://bucket[/key]>` | Object metadata, or check that a bucket exists |
| `presign <s3://bucket/key>` | Presigned GET URL, or PUT with `--put` |
| `select <src>` | Run an S3 Select SQL query |
| `run [file]` | Run many rs5cmd commands from a file or stdin, in parallel |
| `bucket-version <s3://bucket>` | Show or `--set Enabled|Suspended` versioning |
| `mount <s3://...> <dir>` | FUSE mount (only in builds with the `mount` feature) |
| `import-s3cfg <path>` | Convert an s3cmd config |
| `completion <shell>` | Print a completion script (bash, zsh, fish, powershell, elvish) |

Every flag with examples: [commands](references/commands.md).

Global flags go before or after the command. The ones used most:
`--endpoint-url`, `--region`, `--profile`, `--json`, `--dry-run`, `--numworkers N` (default 256),
`--retry-count N` (default 10), `--no-verify-ssl`, `--limit-upload RATE` / `--limit-download RATE`
(for example `10MB`, bytes per second, shared by all workers), `--proxy URL`,
`--source-region` / `--destination-region`, `--use-list-objects-v1`.

## Paths, wildcards and where things land

In an S3 path, `*` matches any characters including `/`: `'s3://b/p/*.log'` finds `.log` keys at
any depth. In a local path, wildcards work as in a shell (`*` stays inside one directory, `**`
crosses directories), and a matched directory is copied whole. So `'data/*'` is recursive on
both sides, but `'data/*.log'` matches top-level files only; for every `.log` file below `data`
write `--include '*.log' 'data/*'`.

The destination must end in `/` (or be a bucket or an existing local directory) to receive
several files. Results below are for a destination of `s3://b/p/`; a local destination `out/`
behaves the same way.

| Source | Copies | Lands at |
|---|---|---|
| `file.txt` | one file | `p/file.txt` |
| `data/` or `'data/*'` | everything under `data`, recursively | `p/<path below data>` |
| `/abs/data` or `parent/data` | everything under `data` | `p/data/<path below data>` |
| `data` or `./data` | **broken in v0.1.0, do not use** | keys containing `../`; see known issues |
| `'s3://b/p/*'` | every object under the prefix, recursively | relative layout kept |
| `s3://b/p/` | only objects directly under the prefix | no sub-prefixes |
| `s3://b/key` | one object | `<dst>/key`, or exactly `<dst>` if it has no trailing `/` |

- `cp file s3://b/p` (no trailing slash) writes an object named `p`.
- `ls`, `du`, `cp`, `sync` and `rm` on an S3 prefix ending in `/` do not descend. Add `*`.
- `--include` / `--exclude` take globs matched against the path relative to the source root.
  Exclude wins. They apply only when a source expands, never to a single named object.

## Recipes

```bash
# Bulk upload and download (quote the wildcard; keep the trailing slash)
rs5cmd cp 'data/*' s3://bucket/data/
rs5cmd cp 's3://bucket/data/*' ./restore/
rs5cmd cp --exclude '*.tmp' --include '*.parquet' 'data/*' s3://bucket/data/

# Large objects: multipart above --part-size MiB (default 50), --concurrency parts per object
# (default 5); the defaults are s5cmd's
rs5cmd cp --part-size 64 --concurrency 16 big.bin s3://bucket/big.bin

# Bucket to bucket (server-side copy), then a move
rs5cmd cp 's3://src-bucket/logs/*' s3://dst-bucket/logs/
rs5cmd mv 's3://bucket/inbox/*' s3://bucket/archive/

# Sync with delete: wildcard source, preview, cap the deletions
rs5cmd --dry-run sync --delete --max-delete 100 'results/*' s3://bucket/results/
rs5cmd sync --delete --max-delete 100 'results/*' s3://bucket/results/
rs5cmd sync --size-only 's3://bucket/results/*' ./copy/       # ./copy must not be empty

# Batch file: one command per line, '#' comments allowed, global flags are inherited
cat > jobs.txt <<'EOF'
cp 's3://bucket/a/*' /data/a/
rm s3://bucket/tmp/old.bin
EOF
rs5cmd --numworkers 32 run jobs.txt
rs5cmd ls --show-fullpath 's3://bucket/tmp/*' > keys.txt      # one s3:// URL per line
sed "s|.*|rm '&'|" keys.txt | rs5cmd run                      # lines are split like a shell

# Presigned URLs
rs5cmd presign --expire 15m s3://bucket/report.pdf
rs5cmd presign --put --expire 1h s3://bucket/upload.bin

# Pipes
tar czf - ./project | rs5cmd pipe s3://bucket/backups/project.tgz
rs5cmd cat s3://bucket/backups/project.tgz | tar xzf -
rs5cmd cat 's3://bucket/logs/2026-01-*.log' > january.log
```

## `--fast`: many small objects (Linux)

`cp --fast` and `mv --fast` switch to an io_uring engine built for many small objects.

- It needs a build with the `fast` feature (Linux release binaries and packages have it) and an
  endpoint from `--endpoint-url`, `AWS_ENDPOINT_URL` or a config file. `S3_ENDPOINT_URL` alone is
  not enough: `error: --fast currently requires --endpoint-url`.
- It handles upload, download and S3→S3 copy, with `mv`, `--json`, `--dry-run`, `--retry-count`,
  `--no-verify-ssl`, `--numworkers`, and `--include` / `--exclude`.
- It **silently ignores** `--storage-class`, `--acl`, `--content-type`, `--preserve-timestamps`,
  `--if-none-match`, `--links`, `--version-id`, `--all-versions`, `--part-size`, `--concurrency`,
  `--proxy`, the bandwidth limits and the per-side region and endpoint flags.
- Each object is held whole in memory and sent in one request. Do not use it for large objects.

Requirements, tuning and limits: [fast path and mount](references/fast-path-and-mount.md).

## Output for scripts and agents

`--json` prints one JSON object per line on stdout. Progress bars and the `sync` summary go to
stderr.

```
{"destination":"s3://b/p/a.txt","operation":"cp","source":"data/a.txt","success":true}
{"destination":"s3://b/p/a.txt","error":"...","operation":"cp","source":"data/a.txt","success":false}
{"etag":"...","key":"a.txt","last_modified":"2026/01/02 03:04:05","size":6,"storage_class":"STANDARD","type":"file"}
{"count":3,"size":16,"source":"s3://b/p/*"}
{"url":"https://..."}
```

These are a `cp`/`mv`/`rm`/`sync`/`mb`/`rb` result (success, then failure), an `ls` record, a
`du` summary and a `presign` result. `rm`, `mb` and `rb` results have no `destination`.
`--dry-run` adds `"dryRun":true`. A `pipe` result has a `destination` and no `source`. `tree`
refuses `--json` (use `ls --json`), and `cat` prints the object's bytes.
In `--json` mode, failed operations are also printed on stdout, so check `success`.

Exit codes: `0` success, `1` at least one operation failed, `2` bad command line, `130`
interrupted (SIGINT or SIGTERM), `101` internal panic (see the broken-pipe pitfall below).

## Pitfalls that bite

Full list with details and workarounds: [known issues](references/known-issues.md).

- **Bare relative directory source.** `cp data out/` and `mv data out/` truncate the source files
  to 0 bytes (`mv` then deletes them). `cp data s3://b/p/` uploads to keys with a literal `../`.
  Use `data/` or `'data/*'`.
- **`sync` into a missing or empty local directory fails** with `no match found for "dir/*"`.
  Do the first copy with `cp`, then `sync`.
- **`sync --delete` with a bare S3 prefix source** (`s3://b/p/`) deletes destination files in
  subdirectories, because the source lists only one level. Use `'s3://b/p/*'` and `--max-delete`.
- **`--dry-run rm 's3://b/p/*'` prints nothing.** Preview with `ls` and the same pattern.
- **Piping into `head` or another early-exiting reader** makes rs5cmd panic with
  `failed printing to stdout: Broken pipe` and exit `101`. Read all output, or use a file.
- **Symlinks are always followed.** There is no `--no-follow-symlinks`. A link to a directory
  inside a wildcard source fails. `cp --links` stores links as placeholder objects instead.
- **Uploads are `application/octet-stream`** unless `--content-type` is given. `cp` and `sync`
  do not guess the type from the file extension.
- **`--fast` memory grows with `--numworkers`.** It holds each object (up to its 8 MiB part)
  in memory per transfer in flight, so the default of 256 workers can use over 1 GB with
  megabyte-sized objects. Lower it on small machines. The default path streams instead (tens of
  MB for 256 transfers of 1 MiB objects).
- Tested against MinIO only. Check behaviour on real AWS S3 before relying on multipart edge
  cases, versioning, encryption, cross-region copies or dual-stack endpoints.

## Coming from s5cmd

Commands, wildcard rules, `run` files, `--json`, `--dry-run`, `--numworkers` and the `sync`
comparison follow s5cmd. Main differences between rs5cmd v0.1.0 and s5cmd v2.3:

- Missing on `cp` / `mv`: `-n/--no-clobber`, `-s/--if-size-differ`, `-u/--if-source-newer`,
  `-f/--flatten`, `--raw`, `--sse`, `--sse-kms-key-id`, `--cache-control`, `--expires`,
  `--metadata`, `--metadata-directive`, `--show-progress`, `--no-follow-symlinks`.
- Missing elsewhere: `rm --all-versions`, the global `--log` and `--stat`, the `-r` short flag,
  and the `version` command (use `--version`; set `RUST_LOG=debug` for logs).
- Different: `select` takes `--input-format csv|json|parquet` instead of subcommands. Region
  and endpoint per side are global flags. `cp --fast` uses 8 MiB parts unless `--part-size` is
  given (the default path uses s5cmd's 50).
- Added: `tree`, `mount`, `import-s3cfg` and `--s3cfg`, `cp --fast`, several sources in one
  `cp`, `--if-none-match`, `--links`, `--client-copy`, `--preserve-timestamps`, `sync --checksum`
  and `--max-delete`, `rb --force`, `ls --newer-than` / `--older-than` / `--summarize`,
  bandwidth limits, `--proxy`, `--color`, `--ca-certs-file`.
