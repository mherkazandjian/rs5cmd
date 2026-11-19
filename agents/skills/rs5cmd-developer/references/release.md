# Releasing and packaging rs5cmd

Checked against `.github/workflows/release.yml` and `packaging/` on `main` at release v0.1.0.

## What a release produces

| Artifact | Built by | Notes |
|---|---|---|
| `rs5cmd-v<ver>-x86_64-unknown-linux-gnu.tar.gz`, `...-aarch64-...` | `cargo zigbuild` | glibc 2.28 or newer, feature `fast` |
| `rs5cmd-v<ver>-x86_64-apple-darwin.tar.gz`, `...-aarch64-...` | native `cargo build` | no features |
| a `.sha256` next to every tarball | `shasum -a 256` | |
| `rs5cmd_<ver>-1_amd64.deb`, `_arm64.deb` | nfpm | depends on `libc6 (>= 2.28)` |
| `rs5cmd-<ver>-1.x86_64.rpm`, `.aarch64.rpm` | nfpm | depends on `glibc >= 2.28` |
| `rs5cmd-<ver>-1-x86_64.pkg.tar.zst`, `-aarch64` | nfpm | Arch |

Each tarball holds one directory with `rs5cmd`, `README.md` and `LICENSE`. The packages install
`/usr/bin/rs5cmd` and the two documents under `/usr/share/doc/rs5cmd/`.

No artifact contains the `mount` feature.

## The workflow

`.github/workflows/release.yml` has two triggers:

- **A pushed tag matching `v*`**: builds everything and attaches it to the GitHub Release of
  that tag.
- **Manual run (`workflow_dispatch`)** with an optional `tag` input (default `dev`): builds the
  four tarballs and uploads them as workflow artifacts only. Nothing is attached to a release
  and the package job does not run. Use it as a dry run of the build.

Job `build` (matrix of four targets):

| Target | Runner | Build |
|---|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `cargo zigbuild --release --locked --target x86_64-unknown-linux-gnu.2.28 --features fast` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04` | the same with the aarch64 triple |
| `x86_64-apple-darwin` | `macos-15-intel` | `cargo build --release --locked --target <triple>` |
| `aarch64-apple-darwin` | `macos-14` | the same |

Then, per target: strip the binary, stage it with `README.md` and `LICENSE`, create the tarball
and checksum, upload as a workflow artifact, and on a tag attach to the release.

Job `package` (tags only, after `build`): downloads the two Linux tarballs, extracts them into
`stage/`, installs a pinned nfpm, and runs `packaging/build-packages.sh <ver> stage dist`. The
version is the tag without the leading `v`.

### Why it is built this way

- **`cargo-zigbuild` with a `.2.28` target suffix** links against glibc 2.28 symbols, so one
  binary runs on RHEL 8, Debian 10, Ubuntu 18.10 and newer. zig also supplies the C
  cross-toolchain for the AWS SDK's crypto library.
- **glibc 2.28 is the floor, not older**, because the fast-path runtime (monoio) calls `statx`,
  whose glibc wrapper first appeared in 2.28.
- **No static musl build**: monoio does not build against musl.
- **zig is pinned to 0.13.0.** The workflow comments explain that newer zig needs a newer clang
  for the bindgen step than the runners provide. Read those comments before bumping any pinned
  tool or action version.
- **`--locked`** makes the build fail if `Cargo.lock` is out of date, so the release uses
  exactly the committed dependency versions.
- **macOS builds are native per architecture**, so no C cross-compilation happens there, and
  they carry no features because `fast` is Linux-only.

## Cutting a release

1. Make sure `main` is what you want to ship and the test services pass (`test`, `test-fast`,
   `test-mount`). CI does not run tests.
2. Set `version` in `Cargo.toml`. Rebuild once so `Cargo.lock` records the new version, and
   commit both files. With a stale lock file the `--locked` build fails. Nothing checks that the
   tag matches `Cargo.toml`: if they differ, the file names follow the tag and
   `rs5cmd --version` follows `Cargo.toml`.
3. Optionally run the workflow manually first and check that all four targets build.
4. Tag and push the tag:

   ```bash
   git tag v<version>
   git push origin v<version>
   ```

   Pushing a tag publishes binaries. Do it only on the maintainer's explicit request.
5. When the workflow finishes, check the release page: four tarballs, four checksums, two
   `.deb`, two `.rpm`, two `.pkg.tar.zst`.
6. Follow-up commit on `main`:
   - `packaging/PKGBUILD`: set `pkgver`, and replace `sha256sums_x86_64` and
     `sha256sums_aarch64` with the values from the two Linux `.sha256` files.
   - `README.md` and `packaging/README.md`: file names in the install examples carry the
     version.
   - `agents/skills/rs5cmd-user/`: the version it says it was checked against, the install
     examples, and any known issue the release fixed.

## Building packages locally

```bash
# stage/ must contain the extracted release trees:
#   stage/rs5cmd-v<ver>-x86_64-unknown-linux-gnu/{rs5cmd,README.md,LICENSE}
#   stage/rs5cmd-v<ver>-aarch64-unknown-linux-gnu/{rs5cmd,README.md,LICENSE}
bash packaging/build-packages.sh <ver> stage dist
```

- Needs `nfpm` on `PATH`, or its location in `$NFPM`.
- `<ver>` is a bare version such as `0.1.0`, without `v`.
- The script requires both architectures to be present. It renders `packaging/nfpm.yaml` for
  each one (substituting `${ARCH}`, `${VERSION}`, `${BINARY}`, `${DOCDIR}` itself) and runs nfpm
  for `deb`, `rpm` and `archlinux`.
- `packaging/nfpm.yaml` is the single place for package metadata, file locations and the glibc
  dependency. If the glibc floor changes in the workflow, change it there too.
- `packaging/PKGBUILD` is an AUR-style `rs5cmd-bin` recipe that downloads the release tarball.
  It is independent of nfpm.

## Building a binary yourself

```bash
docker compose run --rm --no-deps dev cargo build --release --features fast
```

The result is `target/release/rs5cmd`, linked against the glibc of the dev image
(`rust:1-bookworm`). It may not start on an older distribution; the portable binaries come from
the release workflow. Add `mount` to the feature list for a binary with the FUSE mount.
