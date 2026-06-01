# Distro packages

Native packages for rs5cmd, produced from the prebuilt **glibc 2.28** Linux
release binaries (so one package per format covers a wide range of distros).
They are attached to each [GitHub Release](../../releases) and built by the
`package` job in [`.github/workflows/release.yml`](../.github/workflows/release.yml).

| Format | File | Arches | Covers |
| --- | --- | --- | --- |
| Debian/Ubuntu | `rs5cmd_<ver>-1_<amd64\|arm64>.deb` | amd64, arm64 | Debian 10+, Ubuntu 18.10+ (22.04 / 24.04 / 24.10 / 26.04, Debian 12 / 13, …) |
| RHEL family | `rs5cmd-<ver>-1.<x86_64\|aarch64>.rpm` | x86_64, aarch64 | RHEL/Rocky/AlmaLinux 8+ (Rocky 9 / 10, Alma 9 / 10, …) |
| Arch Linux | `rs5cmd-<ver>-1-<x86_64\|aarch64>.pkg.tar.zst` | x86_64, aarch64 | Arch (and an AUR [`PKGBUILD`](PKGBUILD)) |

The package installs `rs5cmd` to `/usr/bin/rs5cmd` plus the README and LICENSE
under `/usr/share/doc/rs5cmd/`.

## Install

```bash
# Debian / Ubuntu
sudo dpkg -i rs5cmd_0.1.0-1_amd64.deb        # or _arm64.deb

# Rocky / AlmaLinux / RHEL / Fedora
sudo rpm -i rs5cmd-0.1.0-1.x86_64.rpm        # or .aarch64.rpm
#   (or: sudo dnf install ./rs5cmd-0.1.0-1.x86_64.rpm)

# Arch Linux
sudo pacman -U rs5cmd-0.1.0-1-x86_64.pkg.tar.zst
#   (or build from the AUR-style PKGBUILD in this directory)
```

## Building locally

Requires [`nfpm`](https://nfpm.goreleaser.com) on `PATH` (or set `$NFPM`).

```bash
# Stage the extracted release trees (rs5cmd-v<ver>-<arch>-unknown-linux-gnu/)
# into <stage-dir>, then:
bash packaging/build-packages.sh 0.1.0 <stage-dir> dist/
```

[`nfpm.yaml`](nfpm.yaml) is the package template; `build-packages.sh` renders it
per architecture and invokes nfpm for the deb/rpm/arch formats.
