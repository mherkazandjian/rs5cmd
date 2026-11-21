#!/usr/bin/env bash
# Binary-size analysis for the review (A4). Runs inside the pinned bench container.
#
#   bash bench/review/size.sh breakdown   # sections, crates, functions, duplicates, Go packages
#   bash bench/review/size.sh knobs [k..] # release-profile knob matrix -> /opt/bins/knob-<k>
#
# Knobs are applied with CARGO_PROFILE_RELEASE_* environment overrides, so Cargo.toml is not
# touched; each knob is built exactly like the headline binary (zigbuild, glibc 2.28,
# --features fast, stripped) and recorded in /opt/bins/manifest.jsonl as knob-<name>.
set -euo pipefail
RS=/src/rs5cmd
OUT="$RS/bench/review/results/size"
BINS=/opt/bins
mkdir -p "$OUT"

breakdown() {
    echo "==> sections" >&2
    for b in rs5cmd rs5cmd-default s5cmd s5cmd-golatest; do
        size -A "$BINS/$b" > "$OUT/sections-$b.txt"
    done

    echo "==> cargo bloat (crates, functions; host target, --features fast)" >&2
    export CARGO_TARGET_DIR=/target/bloat
    (cd "$RS" && cargo bloat --release --features fast --bin rs5cmd --crates -n 60 \
        > "$OUT/bloat-crates.txt" 2>/dev/null)
    (cd "$RS" && cargo bloat --release --features fast --bin rs5cmd -n 120 \
        > "$OUT/bloat-functions.txt" 2>/dev/null)
    (cd "$RS" && cargo bloat --release --bin rs5cmd --crates -n 60 \
        > "$OUT/bloat-crates-default.txt" 2>/dev/null)

    echo "==> dependency duplicates and TLS stacks" >&2
    (cd "$RS" && cargo tree -d --features fast -e normal --locked > "$OUT/tree-duplicates.txt")
    {
        for spec in rustls@0.21.12 rustls@0.23.40 aws-lc-rs ring hyper@0.14.32 hyper-rustls@0.24.2 \
                    hyper-rustls@0.27.7 regex tracing-subscriber; do
            echo "### cargo tree -i $spec --features fast"
            (cd "$RS" && cargo tree -i "$spec" --features fast -e normal --locked --depth 4 2>&1 || true)
            echo
        done
    } > "$OUT/tree-inverse.txt"

    echo "==> Go symbol sizes by package (unstripped Go 1.22 twin)" >&2
    /opt/go1.22/bin/go tool nm -size -sort size "$BINS/s5cmd-go122-debug" | python3 -c '
import collections, re, sys
by = collections.Counter()
for line in sys.stdin:
    parts = line.split()
    if len(parts) < 4 or not parts[1].isdigit():
        continue
    name = parts[3]
    m = re.match(r"^(type:\.?|go:)?([\w./-]+?)(?:\.\(|\.[\w$]|$)", name)
    pkg = m.group(2) if m else name
    for pre in ("github.com/aws/aws-sdk-go/", "github.com/peak/s5cmd/v2/", "golang.org/x/",
                "github.com/", "vendor/"):
        if pkg.startswith(pre):
            seg = pkg[len(pre):].split("/")
            pkg = pre + "/".join(seg[:2 if pre in ("github.com/", "golang.org/x/") else 1])
            break
    else:
        pkg = pkg.split("/")[0]
    by[pkg] += int(parts[1])
tot = sum(by.values())
print(f"{tot:>12,}  total symbol bytes")
for k, v in by.most_common(40):
    print(f"{v:>12,}  {100*v/tot:5.1f}%  {k}")
' > "$OUT/go-packages.txt"
    echo "done: $OUT" >&2
}

declare -A KNOBS=(
    [base]=""
    [lto-thin]="CARGO_PROFILE_RELEASE_LTO=thin"
    [lto-fat]="CARGO_PROFILE_RELEASE_LTO=fat"
    [cgu1]="CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1"
    [fat-cgu1]="CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1"
    [fat-cgu1-abort]="CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_PANIC=abort"
    [panic-abort]="CARGO_PROFILE_RELEASE_PANIC=abort"
    [fat-cgu1-o2]="CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_OPT_LEVEL=2"
    [fat-cgu1-os]="CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_OPT_LEVEL=s"
    [fat-cgu1-oz]="CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_OPT_LEVEL=z"
    [fat-cgu1-abort-os]="CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_PANIC=abort CARGO_PROFILE_RELEASE_OPT_LEVEL=s"
)

knobs() {
    local names=("$@")
    [ ${#names[@]} -eq 0 ] && names=(lto-thin lto-fat cgu1 fat-cgu1 panic-abort fat-cgu1-abort fat-cgu1-o2 fat-cgu1-os fat-cgu1-oz fat-cgu1-abort-os)
    local k
    for k in "${names[@]}"; do
        echo "==> knob $k: ${KNOBS[$k]:-<none>}" >&2
        local t0=$SECONDS
        env ${KNOBS[$k]} CARGO_TARGET_DIR="/target/knob-$k" \
            cargo zigbuild --release --locked --target x86_64-unknown-linux-gnu.2.28 --features fast \
            --manifest-path "$RS/Cargo.toml" 2>&1 | grep -E '^error|Finished' >&2
        local out="/target/knob-$k/x86_64-unknown-linux-gnu/release/rs5cmd"
        cp "$out" "$BINS/knob-$k"
        strip "$BINS/knob-$k"
        local tgz
        tgz=$(mktemp -d)
        mkdir "$tgz/rs5cmd-knob"
        cp "$BINS/knob-$k" "$tgz/rs5cmd-knob/rs5cmd"
        cp "$RS/README.md" "$RS/LICENSE" "$tgz/rs5cmd-knob/"
        tar -C "$tgz" -czf "$tgz/k.tar.gz" rs5cmd-knob
        python3 - "$k" "$BINS/knob-$k" "$(stat -c %s "$tgz/k.tar.gz")" "${KNOBS[$k]}" "$((SECONDS - t0))" <<'PY'
import hashlib, json, os, sys, time
k, path, tgz, env, secs = sys.argv[1:]
rec = {"name": f"knob-{k}", "path": path, "size": os.path.getsize(path), "tgz_size": int(tgz),
       "sha256": hashlib.sha256(open(path, "rb").read()).hexdigest(), "stripped": True,
       "toolchain": f"zigbuild glibc 2.28 --features fast; {env or 'release profile as is'}",
       "build_s": int(secs), "built_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
m = "/opt/bins/manifest.jsonl"
lines = [l for l in open(m) if json.loads(l)["name"] != rec["name"]]
lines.append(json.dumps(rec) + "\n")
open(m, "w").writelines(lines)
print(f"  knob-{k}: {rec['size']:,} bytes, tar.gz {int(tgz):,}, build {secs}s", file=sys.stderr)
PY
        rm -rf "$tgz" "/target/knob-$k"
    done
}

cmd="${1:-breakdown}"; shift || true
case "$cmd" in
    breakdown) breakdown ;;
    knobs) knobs "$@" ;;
    *) echo "usage: size.sh breakdown | knobs [name...]" >&2; exit 2 ;;
esac
