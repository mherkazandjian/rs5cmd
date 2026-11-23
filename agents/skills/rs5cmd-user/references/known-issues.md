# Known issues and limitations

Everything in sections 1 and 2 was reproduced with the v0.1.0 release binary. Several entries
are plain bugs. An entry is removed when its bug is fixed, so this file follows the current
source: a v0.1.0 binary also has the bugs fixed since (the project's closed issues list them).
After an upgrade, test an entry again before relying on its workaround.

## 1. Can lose or misplace data

### 1.1 Destination without a trailing slash

`rs5cmd cp file.txt s3://bucket/prefix` creates one object named `prefix`. To put the file under
the prefix, write `s3://bucket/prefix/`.

### 1.2 `mv` and interruption

`mv` deletes a source after that object's copy succeeded. Ctrl-C (or SIGTERM) ends the process
at once with exit code 130 and does not wait for or roll back operations in flight. After an
interruption, some objects are moved and some are not, and a multipart upload that was running
may leave unfinished parts on the service. Run the command again to finish. Configure a bucket
lifecycle rule to expire incomplete multipart uploads if you interrupt large uploads often.

### 1.3 `import-s3cfg` rewrites the default AWS profile

It replaces the `[default]` section of `~/.aws/credentials`. Use `--profile NAME` or
`--no-credentials` when that profile belongs to something else.

## 2. Fails or misleads

### 2.1 `--json` gaps

- `cat`, `import-s3cfg` and `completion` print plain text with `--json`; `tree` refuses it.
- In JSON mode a failed operation is reported on **stdout** as `{"success":false,...}` (in text
  mode it goes to stderr). Parse every line and test `success`; also test the exit code.
- The `sync` summary line is not printed in JSON mode.

### 2.2 Uploads get a generic content type

`cp` and `sync` upload local files as `application/octet-stream` unless `--content-type` is
given, and one `--content-type` applies to every file of the command. They do not guess from the
file extension (`pipe` does). For a static web site, upload each file type separately:

```bash
rs5cmd cp --content-type text/html --include '*.html' 'site/*' s3://bucket/site/
rs5cmd cp --content-type text/css  --include '*.css'  'site/*' s3://bucket/site/
```

Copies from S3 to S3 keep the source object's content type unless `--content-type` is given.

### 2.3 `sync` always follows symbolic links

`cp` and `mv` follow links unless `--no-follow-symlinks` is given. `sync` has no
`--no-follow-symlinks` (it is rejected as an unknown argument) and always follows them:

- A link to a file is copied as the file's content.
- A link to a directory is walked and its files are copied, whether the source is a directory
  (`data/`), a wildcard (`'data/*'`) or the link itself.
- `cp --links` uploads a link as a small object named `<name>.s5cmdlink` that holds the link
  target, and recreates the link on download. The printed line shows the name without the
  suffix. A link to a directory is stored as a link when a wildcard matches it or it is the
  source itself; inside a directory source, links to directories are still walked.
- `cp --no-follow-symlinks` skips the links inside a directory source and a source that is a
  link; a link a wildcard matches is still copied when it points to a file, as in s5cmd.

### 2.4 `sync` may copy a just-uploaded file once more

The default comparison copies a file when the source is newer than the destination or the sizes
differ. rs5cmd keeps an object's modification time to the whole second, while local times are
finer, so a file changed in the same second it was uploaded can be copied again by the next
`sync`. AWS S3 stores whole seconds, so s5cmd does the same there; on servers that return
milliseconds (MinIO, for one) s5cmd keeps them and rs5cmd does not. Use `--size-only` when you
need repeat runs to be exact no-ops.

## 3. Resource use

- **Memory.** The default path streams: uploads over 256 KiB are read in 128 KiB chunks and
  downloads are written as they arrive, so 256 transfers of 1 MiB objects take tens of MB.
  `--fast` holds each object, up to its 8 MiB part, in memory per transfer in flight: 50,000
  objects of 1 MiB with the default 256 workers peaked at 0.9 to 1.2 GB (v0.1.0). `pipe` holds
  the `--concurrency` parts in flight and the one it is reading, each of `--part-size`: about
  300 MiB by default. Lower `--numworkers` (with `--fast`) or `pipe`'s settings on small machines.
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
- Finding a bucket's region when none is configured (one `HeadBucket` request; see the user
  skill's configuration reference).

## 5. Not implemented

Compared with s5cmd v2.3:

- `sync`: `--no-follow-symlinks`, `--raw`, `--sse`, `--sse-kms-key-id`, `--cache-control`,
  `--expires`, `--content-encoding`, `--content-disposition`, `--metadata`,
  `--metadata-directive`, `--part-size`, `--concurrency`, `--ignore-glacier-warnings`.
- `cat --raw`, `--concurrency`, `--part-size`; `pipe --no-clobber`, `--raw`, `--metadata`,
  `--expires`.
- Global `--log` and `--stat`, the short flag `-r`, and the `version` command.

Closest substitute for a `sync` that needs one of the flags: `cp` with the flag and a wildcard
source, plus `-u` (`--if-source-newer`) or `-s` (`--if-size-differ`, as `sync --size-only`) to
skip what is already there. It deletes nothing (no `--delete`).

Also absent: bucket policy, tagging and lifecycle commands, customer-provided encryption keys,
byte-level progress, and preserving file permissions.
