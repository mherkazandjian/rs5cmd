#!/usr/bin/env bash
# Build rs5cmd distro packages (.deb, .rpm, Arch .pkg.tar.zst) for both
# x86_64 and aarch64 from the prebuilt Linux release tarballs, using nfpm.
#
# Usage:
#   build-packages.sh <version> <stage-dir> <out-dir>
#
#   <version>    bare semver, e.g. 0.1.0 (no leading "v")
#   <stage-dir>  directory containing the extracted release trees:
#                  rs5cmd-v<version>-x86_64-unknown-linux-gnu/{rs5cmd,README.md,LICENSE}
#                  rs5cmd-v<version>-aarch64-unknown-linux-gnu/{...}
#   <out-dir>    where the package files are written
#
# Requires `nfpm` on PATH (or set $NFPM to its path).
set -euo pipefail

VERSION="${1:?usage: build-packages.sh <version> <stage-dir> <out-dir>}"
STAGE="${2:?missing <stage-dir>}"
OUT="${3:?missing <out-dir>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NFPM="${NFPM:-nfpm}"

mkdir -p "$OUT"

# rust target arch -> nfpm (GOARCH) arch
declare -A NFARCH=( [x86_64]=amd64 [aarch64]=arm64 )

tmpcfg="$(mktemp)"
trap 'rm -f "$tmpcfg"' EXIT

for rustarch in x86_64 aarch64; do
  dir="$STAGE/rs5cmd-v${VERSION}-${rustarch}-unknown-linux-gnu"
  [ -x "$dir/rs5cmd" ] || { echo "ERROR: missing binary $dir/rs5cmd" >&2; exit 1; }

  ARCH="${NFARCH[$rustarch]}"
  # Render the nfpm template (env expansion in nfpm's `contents.src` is
  # unreliable, so substitute the placeholders ourselves). `#` delimiter
  # because the values are filesystem paths.
  sed -e "s#\${ARCH}#${ARCH}#g" \
      -e "s#\${VERSION}#${VERSION}#g" \
      -e "s#\${BINARY}#${dir}/rs5cmd#g" \
      -e "s#\${DOCDIR}#${dir}#g" \
      "$HERE/nfpm.yaml" > "$tmpcfg"

  for fmt in deb rpm archlinux; do
    echo ">> $fmt / $rustarch (nfpm arch=$ARCH)"
    "$NFPM" package -f "$tmpcfg" -p "$fmt" -t "$OUT"
  done
done

echo
echo "=== built packages ==="
ls -1 "$OUT"
