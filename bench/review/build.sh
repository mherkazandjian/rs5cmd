#!/usr/bin/env bash
# Builds every binary the review measures into /opt/bins (the `bins` volume). Runs inside the
# pinned `bench` container; every compile uses at most CARGO_BUILD_JOBS / GOMAXPROCS jobs.
#
#   bash bench/review/build.sh [target...]      # default: all
#
# Targets:
#   tools      launch + iouring-probe (static C)
#   stub       s3stub (release, feature bench-stub)
#   rs5cmd     headline binary exactly as .github/workflows/release.yml builds it:
#              cargo zigbuild --release --locked --target x86_64-unknown-linux-gnu.2.28
#              --features fast, then strip; plus the release-style tar.gz
#   sizes      size-only feature sets (default, mount, fast+mount), same recipe
#   profiling  rs5cmd --features fast with [profile.profiling] (debug info, frame pointers)
#   published  the published rs5cmd v0.1.0 linux x86_64 tarball, for the cross-check
#   s5cmd      official v2.3.0 release binary; local Go 1.22 build with the release flags
#              (+ unstripped twin); latest-Go build ("best-case s5cmd")
#
# Outputs: /opt/bins/<name> and /opt/bins/manifest.jsonl (one line per binary: sizes, sha256,
# toolchain). Source trees: /src/rs5cmd (this repo) and /src/s5cmd (upstream clone).
set -euo pipefail

BINS=/opt/bins
RS=/src/rs5cmd
S5=/src/s5cmd
ZIGTARGET=x86_64-unknown-linux-gnu.2.28
TRIPLE=x86_64-unknown-linux-gnu
S5_VERSION=v2.3.0
S5_COMMIT=991c9fb
RS_VERSION=v0.1.0
# The sources are bind-mounted and owned by another uid; let git (Go's VCS stamping, as in
# goreleaser's build) read them anyway.
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0='*'
mkdir -p "$BINS"
MANIFEST="$BINS/manifest.jsonl"

log() { printf '==> %s\n' "$*" >&2; }

record() { # record <name> <path> <toolchain> [tarball]
    local name="$1" path="$2" tc="$3" tgz="${4:-}"
    local tgz_size=null
    [ -n "$tgz" ] && tgz_size=$(stat -c %s "$tgz")
    python3 - "$name" "$path" "$tc" "$tgz_size" "$MANIFEST" <<'PY'
import hashlib, json, os, subprocess, sys, time
name, path, tc, tgz, manifest = sys.argv[1:]
data = open(path, "rb").read()
sections = {}
try:
    out = subprocess.run(["size", "-A", path], capture_output=True, text=True, check=True).stdout
    for line in out.splitlines()[2:]:
        parts = line.split()
        if len(parts) >= 2 and parts[0].startswith(".") and parts[1].isdigit():
            sections[parts[0]] = int(parts[1])
except Exception:
    pass
stripped = b".symtab" not in subprocess.run(["readelf", "-S", path], capture_output=True).stdout
rec = {"name": name, "path": path, "size": len(data), "tgz_size": None if tgz == "null" else int(tgz),
       "sha256": hashlib.sha256(data).hexdigest(), "stripped": stripped, "toolchain": tc,
       "sections": sections, "built_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
lines = [l for l in open(manifest)] if os.path.exists(manifest) else []
lines = [l for l in lines if json.loads(l)["name"] != name]
lines.append(json.dumps(rec) + "\n")
open(manifest, "w").writelines(lines)
print(f"  {name}: {len(data):,} bytes" + (f", tar.gz {int(tgz):,}" if tgz != "null" else ""), file=sys.stderr)
PY
}

# Release-style packaging, as in release.yml's "Package" step (binary + README + LICENSE).
package() { # package <bin> <stage-name> -> prints tarball path
    local bin="$1" stage="$2" tmp
    tmp=$(mktemp -d)
    mkdir -p "$tmp/$stage"
    cp "$bin" "$tmp/$stage/rs5cmd"
    cp "$RS/README.md" "$RS/LICENSE" "$tmp/$stage/"
    tar -C "$tmp" -czf "$BINS/$stage.tar.gz" "$stage"
    rm -rf "$tmp"
    echo "$BINS/$stage.tar.gz"
}

rust_tc() { echo "$(rustc -V | cut -d' ' -f1-2), zig $(zig version), cargo-zigbuild $(cargo-zigbuild --version | awk '{print $2}')"; }

zig_build() { # zig_build <name> <features|""> -> stripped binary at $BINS/<name>
    local name="$1" feat="$2"
    log "zigbuild $name (features: ${feat:-default})"
    CARGO_TARGET_DIR=/target/zig-${feat:-default} \
        cargo zigbuild --release --locked --target "$ZIGTARGET" ${feat:+--features "$feat"} \
        --manifest-path "$RS/Cargo.toml" 2>&1 | grep -E '^(error|warning: unused)|Finished' >&2 || true
    local out=/target/zig-${feat:-default}/$TRIPLE/release/rs5cmd
    test -x "$out"
    cp "$out" "$BINS/$name.unstripped"
    cp "$out" "$BINS/$name"
    strip "$BINS/$name"
}

t_tools() {
    log "launch + iouring-probe"
    gcc -O2 -static -Wall -o "$BINS/launch" "$RS/bench/review/launch.c"
    gcc -O2 -static -Wall -o "$BINS/iouring-probe" "$RS/bench/review/iouring-probe.c"
}

t_stub() {
    log "s3stub"
    CARGO_TARGET_DIR=/target/main cargo build --release --features bench-stub --bin s3stub \
        --manifest-path "$RS/Cargo.toml" 2>&1 | grep -E '^error|Finished' >&2
    cp /target/main/release/s3stub "$BINS/s3stub"
}

t_sdkfloor() {
    log "sdkfloor (E8 probe)"
    CARGO_TARGET_DIR=/target/main cargo build --release --features "bench-stub fast" --bin sdkfloor \
        --manifest-path "$RS/Cargo.toml" 2>&1 | grep -E '^error|Finished|-->' >&2
    cp /target/main/release/sdkfloor "$BINS/sdkfloor"
}

t_rs5cmd() {
    zig_build rs5cmd fast
    local tgz
    tgz=$(package "$BINS/rs5cmd" "rs5cmd-local-$TRIPLE")
    record rs5cmd "$BINS/rs5cmd" "$(rust_tc), --features fast, stripped" "$tgz"
}

t_sizes() {
    zig_build rs5cmd-default ""
    record rs5cmd-default "$BINS/rs5cmd-default" "$(rust_tc), default features, stripped" \
        "$(package "$BINS/rs5cmd-default" rs5cmd-default)"
    zig_build rs5cmd-mount mount
    record rs5cmd-mount "$BINS/rs5cmd-mount" "$(rust_tc), --features mount, stripped" \
        "$(package "$BINS/rs5cmd-mount" rs5cmd-mount)"
    zig_build rs5cmd-fast-mount "fast mount"
    record rs5cmd-fast-mount "$BINS/rs5cmd-fast-mount" "$(rust_tc), --features fast,mount, stripped" \
        "$(package "$BINS/rs5cmd-fast-mount" rs5cmd-fast-mount)"
}

t_profiling() {
    log "rs5cmd profiling build (frame pointers + debug info)"
    CARGO_TARGET_DIR=/target/prof RUSTFLAGS="-C force-frame-pointers=yes" \
        cargo build --profile profiling --features fast --bin rs5cmd \
        --manifest-path "$RS/Cargo.toml" 2>&1 | grep -E '^error|Finished' >&2
    cp /target/prof/profiling/rs5cmd "$BINS/rs5cmd-prof"
    record rs5cmd-prof "$BINS/rs5cmd-prof" "$(rustc -V | cut -d' ' -f1-2), profiling profile, frame pointers"
}

t_published() {
    log "published rs5cmd $RS_VERSION"
    local tmp stage="rs5cmd-$RS_VERSION-$TRIPLE"
    tmp=$(mktemp -d)
    # The project's GitHub repository, from the origin remote (or RS_REPO=owner/name).
    local repo="${RS_REPO:-$(git -C "$RS" remote get-url origin | sed -E 's#^(git@github.com:|https://github.com/)##; s#\.git$##')}"
    curl -fsSL -o "$tmp/$stage.tar.gz" \
        "https://github.com/$repo/releases/download/$RS_VERSION/$stage.tar.gz"
    tar -C "$tmp" -xzf "$tmp/$stage.tar.gz"
    cp "$tmp/$stage/rs5cmd" "$BINS/rs5cmd-published"
    cp "$tmp/$stage.tar.gz" "$BINS/"
    record rs5cmd-published "$BINS/rs5cmd-published" "release asset $stage.tar.gz" "$BINS/$stage.tar.gz"
    rm -rf "$tmp"
}

t_s5cmd() {
    log "official s5cmd $S5_VERSION"
    local tmp
    tmp=$(mktemp -d)
    curl -fsSL -o "$tmp/s5cmd.tar.gz" \
        "https://github.com/peak/s5cmd/releases/download/$S5_VERSION/s5cmd_${S5_VERSION#v}_Linux-64bit.tar.gz"
    tar -C "$tmp" -xzf "$tmp/s5cmd.tar.gz"
    cp "$tmp/s5cmd" "$BINS/s5cmd"
    cp "$tmp/s5cmd.tar.gz" "$BINS/s5cmd_${S5_VERSION#v}_Linux-64bit.tar.gz"
    record s5cmd "$BINS/s5cmd" "official release $S5_VERSION (goreleaser, Go 1.22, -s -w)" \
        "$BINS/s5cmd_${S5_VERSION#v}_Linux-64bit.tar.gz"
    rm -rf "$tmp"

    # The local clone differs from v2.3.0 in README.md only.
    local ld="-X github.com/peak/s5cmd/v2/version.Version=$S5_VERSION -X github.com/peak/s5cmd/v2/version.GitCommit=$S5_COMMIT"
    local go
    for go in go1.22 go-latest; do
        local name=s5cmd-${go/go1.22/go122}
        name=${name/go-latest/golatest}
        log "$name ($(/opt/$go/bin/go version | awk '{print $3}'))"
        (cd "$S5" && CGO_ENABLED=0 /opt/$go/bin/go build -mod=vendor -ldflags "-s -w $ld" -o "$BINS/$name" .)
        record "$name" "$BINS/$name" "$(/opt/$go/bin/go version | awk '{print $3}'), CGO_ENABLED=0, -s -w"
        if [ "$go" = go1.22 ]; then
            (cd "$S5" && CGO_ENABLED=0 /opt/$go/bin/go build -mod=vendor -ldflags "$ld" -o "$BINS/$name-debug" .)
            record "$name-debug" "$BINS/$name-debug" "$(/opt/$go/bin/go version | awk '{print $3}'), CGO_ENABLED=0, unstripped"
        fi
    done
}

# exp-<name> / b-<name>: an experiment (perf/exp-*) or Stage B (perf/b-*, perf/stage-b) worktree at
# /src/wt/<name>, built with the headline recipe so it differs from `rs5cmd` only by its change.
# EXP_SRC=<tree> builds another tree under <name>; EXP_NO_DEFAULT=1 adds --no-default-features.
t_exp() {
    local name="$1" src="${EXP_SRC:-/src/wt/$1}" nd="${EXP_NO_DEFAULT:+--no-default-features}"
    log "zigbuild $name from $src (features: fast${nd:+, $nd})"
    # One target dir for all experiments: they differ only in rs5cmd's own sources, so every
    # dependency is compiled once. rs5cmd itself must be cleaned first: cargo's fingerprint of a
    # path package does not include its location, so another worktree's build looks fresh.
    local mark; mark="$(mktemp)"
    CARGO_TARGET_DIR="/target/exp" cargo clean -q -p rs5cmd --release --target "$TRIPLE" \
        --manifest-path "$src/Cargo.toml"
    CARGO_TARGET_DIR="/target/exp" \
        cargo zigbuild --release --locked --target "$ZIGTARGET" --features fast $nd \
        --manifest-path "$src/Cargo.toml" 2>&1 | grep -E '^(error|warning: unused)|Finished|-->' >&2 || true
    local out="/target/exp/$TRIPLE/release/rs5cmd"
    rm -f "$BINS/$name"
    test -x "$out"
    # Guard against a stale binary: it must have been written by this build.
    [ "$out" -nt "$mark" ] || { echo "stale $out" >&2; rm -f "$mark"; return 1; }
    rm -f "$mark"
    cp "$out" "$BINS/$name"
    strip "$BINS/$name"
    record "$name" "$BINS/$name" "$(rust_tc), --features fast${nd:+ $nd}, stripped, $(git -C "$src" log -1 --format=%h 2>/dev/null || echo worktree)"
}

targets=("$@")
[ ${#targets[@]} -eq 0 ] && targets=(tools stub rs5cmd sizes profiling published s5cmd)
for t in "${targets[@]}"; do
    case "$t" in
        exp-*|b-*) t_exp "$t" ;;
        *) "t_$t" ;;
    esac
done
log "done: ${targets[*]}"
