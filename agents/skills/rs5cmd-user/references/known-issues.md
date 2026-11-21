# Known issues and limitations

Everything in sections 1 and 2 was reproduced with the v0.1.0 release binary. Several entries
are plain bugs. After an upgrade, test an entry again before relying on its workaround, and
remove it from this file when it is fixed.

## 1. Can lose or misplace data

### 1.1 A bare relative directory as the source

Applies to `cp`, `mv` and `sync` when the source is a directory in the current directory, named
without a trailing slash: `data` or `./data`.

```
$ rs5cmd cp data out/
cp data/a.txt data/a.txt          <- copied onto itself; the file is now 0 bytes
$ rs5cmd cp data s3://bucket/prefix/
cp data/a.txt s3://bucket/prefix/../data/a.txt      <- key contains a literal "../"
```

- Local destination: every source file is truncated to 0 bytes. `mv` then deletes it.
- S3 destination: the keys contain `../`. The data is uploaded, but not where you meant, and
  such keys are awkward to list and delete.

Safe forms:

| Write | Result under the destination |
|---|---|
| `data/` or `'data/*'` | the contents of `data` |
| `/abs/path/data` or `parent/data` | a `data/` directory with the contents |

`--dry-run` shows the wrong mapping, so a preview catches it. To copy a directory in the current
directory including its own name, give its absolute path (`"$PWD/data"`) or name the directory in
the destination (`cp 'data/*' s3://bucket/prefix/data/`).

### 1.2 `sync --delete` with a source that lists only one level

An S3 source ending in `/` (`s3://bucket/p/`) lists only the objects directly under the prefix.
The destination is always compared recursively. With `--delete`, every destination file in a
subdirectory therefore counts as extra and is removed.

```
$ rs5cmd --dry-run sync --delete s3://bucket/p/ mirror/
(dry-run) rm mirror/sub/b.log
```

Use a wildcard source (`'s3://bucket/p/*'`), preview with `--dry-run`, and always pass
`--max-delete N` with `--delete`. When the limit is exceeded, nothing is copied or deleted.

### 1.3 Destination without a trailing slash

`rs5cmd cp file.txt s3://bucket/prefix` creates one object named `prefix`. To put the file under
the prefix, write `s3://bucket/prefix/`.

### 1.4 `mv` and interruption

`mv` deletes a source after that object's copy succeeded. Ctrl-C (or SIGTERM) ends the process
at once with exit code 130 and does not wait for or roll back operations in flight. After an
interruption, some objects are moved and some are not, and a multipart upload that was running
may leave unfinished parts on the service. Run the command again to finish. Configure a bucket
lifecycle rule to expire incomplete multipart uploads if you interrupt large uploads often.

### 1.5 `import-s3cfg` rewrites the default AWS profile

It replaces the `[default]` section of `~/.aws/credentials`. Use `--profile NAME` or
`--no-credentials` when that profile belongs to something else.

## 2. Fails or misleads

### 2.1 `sync` into a missing or empty local directory

```
$ rs5cmd sync 's3://bucket/p/*' newdir/
error: no match found for "newdir/*"
```

The first download into a new directory must be a `cp`. Once the directory holds at least one
file, `sync` works. S3 destinations are not affected: syncing to an empty prefix works.

### 2.2 `--dry-run rm` prints nothing for a remote wildcard or prefix

`rs5cmd --dry-run rm 's3://bucket/tmp/*'` exits 0 without output, and deletes nothing. The same
holds for the object deletions of `--dry-run rb --force`. To preview, run `ls` with the same
pattern and filters. `--dry-run rm` on a single object and `--dry-run sync --delete` do print
their `(dry-run) rm ...` lines.

### 2.3 `--no-sign-request` does nothing

The flag is accepted, but requests are still signed. Without credentials the command fails with
`no providers in chain provided credentials`. Anonymous access to public buckets is not
available in v0.1.0.

### 2.4 Broken pipe makes rs5cmd panic

```
$ rs5cmd ls 's3://bucket/*' | head -1
... failed printing to stdout: Broken pipe (os error 32) ...      exit code 101
```

When the reader of stdout exits early, rs5cmd panics instead of stopping quietly. In scripts
with `set -o pipefail` this fails the pipeline. Write the output to a file first, or use a
reader that consumes everything (`sed -n 1p`, `awk 'NR==1'`).

### 2.5 Slow start and warnings when no region is set

Without `--region`, `AWS_REGION` or a profile region, the SDK queries the instance metadata
service before it falls back to `us-east-1`. Outside a cloud instance this adds a few seconds
and warning lines to every invocation. Always set `AWS_REGION`.

### 2.6 `--json` gaps

- `pipe`, `tree`, `cat`, `import-s3cfg` and `completion` print plain text with `--json`.
- In JSON mode a failed operation is reported on **stdout** as `{"success":false,...}` (in text
  mode it goes to stderr). Parse every line and test `success`; also test the exit code.
- The `sync` summary line is not printed in JSON mode.

### 2.7 Uploads get a generic content type

`cp` and `sync` upload local files as `application/octet-stream` unless `--content-type` is
given, and one `--content-type` applies to every file of the command. They do not guess from the
file extension (`pipe` does). For a static web site, upload each file type separately:

```bash
rs5cmd cp --content-type text/html --include '*.html' 'site/*' s3://bucket/site/
rs5cmd cp --content-type text/css  --include '*.css'  'site/*' s3://bucket/site/
```

Copies from S3 to S3 keep the source object's content type.

### 2.8 Symbolic links are always followed

`--follow-symlinks` is on and cannot be switched off; `--no-follow-symlinks` is rejected as an
unknown argument.

- A link to a file is copied as the file's content.
- A link to a directory depends on how the source is written. With a directory source (`data/`)
  the linked directory is walked and its files are copied. With a wildcard source (`'data/*'`)
  the link is one entry that fails (`Is a directory`, or `the source path is neither a regular
  file nor a symlink to a regular file`). The other files are still copied and the exit code
  is 1.
- `cp --links` uploads a link as a small object named `<name>.s5cmdlink` that holds the link
  target, and recreates the link on download. The printed line shows the name without the
  suffix. With a directory source, links to directories are still walked, not stored.

### 2.9 `--fast` ignores many flags without warning

See `fast-path-and-mount.md`. If a flag such as `--storage-class`, `--acl`, `--content-type`,
`--proxy` or a bandwidth limit matters, do not use `--fast`.

### 2.10 `run` does not inherit every global flag

`--limit-upload`, `--limit-download`, the per-side region and endpoint flags, the dual-stack and
FIPS flags and `--color` given before `run` are not applied to the lines. Repeat them in each
line. See `commands.md`.

### 2.11 `sync` may copy a just-uploaded file once more

The default comparison copies a file when the source is newer than the destination or the sizes
differ. S3 stores modification times in whole seconds, so a file changed in the same second it
was uploaded can be copied again by the next `sync`. This matches s5cmd. Use `--size-only` when
you need repeat runs to be exact no-ops.

### 2.12 Cosmetic

`du -H` prints the unit twice: `18 B bytes in 3 objects`.

## 3. Resource use

- **Memory.** The default path streams: uploads over 256 KiB are read in 128 KiB chunks and
  downloads are written as they arrive, so 256 transfers of 1 MiB objects take tens of MB.
  `--fast` holds each object, up to its 8 MiB part, in memory per transfer in flight: 50,000
  objects of 1 MiB with the default 256 workers peaked at 0.9 to 1.2 GB (v0.1.0). `pipe` holds
  up to `--concurrency` parts of `--part-size`, 5 x 50 MiB by default, as s5cmd does. Lower
  `--numworkers` (with `--fast`) or `pipe`'s settings on small machines.
- **Open files.** rs5cmd raises its own open-file limit at start when it can. If it warns that
  `RLIMIT_NOFILE` is too low, or fails with `Too many open files`, run `ulimit -n <N>` first or
  lower `--numworkers`.

## 4. Not verified by the project's tests

The test suite runs against MinIO only. The project's own notes list these as implemented but
not exercised against a real service. Try them on a small sample first:

- Anything on real AWS S3, in particular multipart edge cases, versioning and server-side
  encryption.
- `--use-dualstack-endpoint` and `--use-fips-endpoint`.
- Copies between two regions or two endpoints (`--source-*` / `--destination-*` flags).
- `--force-glacier-transfer` and archived storage classes.
- The download side of the bandwidth limits.
- SSO and assume-role profiles.

There is no automatic detection of a bucket's region. Pass `--region` (or the per-side flags).

## 5. Not implemented

Compared with s5cmd v2.3:

- `cp` / `mv`: `-n/--no-clobber`, `-s/--if-size-differ`, `-u/--if-source-newer`, `-f/--flatten`,
  `--raw`, `--sse`, `--sse-kms-key-id`, `--cache-control`, `--expires`, `--metadata`,
  `--metadata-directive`, `--content-encoding`, `--content-disposition`, `--show-progress`,
  `--no-follow-symlinks`, `--ignore-glacier-warnings`.
- `rm --all-versions`; `cat --raw`, `--concurrency`, `--part-size`; `pipe --no-clobber`, `--raw`,
  `--metadata`, `--expires`.
- Global `--log` and `--stat`, the short flag `-r`, and the `version` command.

Closest substitutes: `cp --if-none-match` for `--no-clobber` when the destination is S3 and the
service supports conditional writes; `sync --size-only` for `--if-size-differ`; plain `sync` for
`--if-source-newer`; `pipe` when an upload needs encryption or cache headers and can be
streamed (`rs5cmd pipe --sse AES256 s3://bucket/key < file`).

Also absent: bucket policy, tagging and lifecycle commands, customer-provided encryption keys,
byte-level progress, and preserving file permissions.
