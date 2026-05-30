#!/usr/bin/env bash
# Head-to-head benchmark: Go s5cmd vs rs5cmd, many small objects, against MinIO.
#
# Env knobs:
#   N        number of objects   (default 10000; QUICK=1 sets 1000)
#   SIZE     object size bytes    (default 4096)
#   WORKERS  client concurrency   (default 256)
#   VARIANTS space-separated list of clients to run (default "s5cmd rs5cmd")
#            ("rs5cmd-fast" is added later once the io_uring path exists)
#
# Measures, per client, for upload and download: wall seconds, client CPU
# seconds (user+sys), peak RSS (MB), and derived requests/sec. Prints a
# markdown table.
set -euo pipefail

N="${N:-10000}"
SIZE="${SIZE:-4096}"
WORKERS="${WORKERS:-256}"
[ "${QUICK:-0}" = "1" ] && N=1000
EP="${AWS_ENDPOINT_URL:-http://minio:9000}"
VARIANTS="${VARIANTS:-s5cmd rs5cmd}"
ITERS="${ITERS:-3}"

CORPUS=/tmp/corpus
OUT=/tmp/out
STAMP="$(date +%s)"

echo "==> Building rs5cmd (release)"
cargo build --release ${RS5CMD_FEATURES:+--features "$RS5CMD_FEATURES"} 2>&1 | tail -2
RS5CMD=target/release/rs5cmd

# Optional: run against the in-memory discard-S3 stub (STUB=1) to expose the
# pure client ceiling without a server bottleneck or server-side noise.
if [ "${STUB:-0}" = "1" ]; then
  echo "==> Building + launching in-memory s3stub"
  cargo build --release --features bench-stub --bin s3stub 2>&1 | tail -1
  STUB_OBJECT_SIZE="$SIZE" STUB_LIST_COUNT="$N" STUB_PORT=9100 target/release/s3stub &
  STUB_PID=$!
  trap 'kill $STUB_PID 2>/dev/null' EXIT
  for _ in $(seq 1 50); do (echo >/dev/tcp/127.0.0.1/9100) 2>/dev/null && break; sleep 0.1; done
  EP="http://127.0.0.1:9100"
  echo "  stub up; EP=$EP"
fi

echo "==> Generating corpus: N=$N objects x SIZE=$SIZE bytes (workers=$WORKERS)"
rm -rf "$CORPUS"; mkdir -p "$CORPUS"
python3 - "$CORPUS" "$N" "$SIZE" <<'PY'
import os, sys
d, n, size = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
blob = b"x" * size
for i in range(n):
    with open(os.path.join(d, f"obj_{i:06d}.dat"), "wb") as f:
        f.write(blob)
print(f"  wrote {n} files")
PY

# measure <label> <statfile> <preclean-dir|-> -- <cmd...>
# Runs the command ITERS times, keeping the best (min wall) "e u s m" line.
# `preclean-dir` is wiped (untimed) before each iteration so iterations stay
# independent (e.g. the download target), avoiding overwrite/re-work artifacts.
measure() {
  local label="$1" stat="$2" preclean="$3"; shift 3; shift  # drop the '--'
  : > "/tmp/${label}.iters"
  local k
  for k in $(seq 1 "$ITERS"); do
    if [ "$preclean" != "-" ]; then rm -rf "$preclean"; mkdir -p "$preclean"; fi
    /usr/bin/time -f '%e %U %S %M' -o "/tmp/${label}.one" "$@" >/dev/null 2>"/tmp/${label}.err" || {
      echo "  !! $label FAILED:"; tail -5 "/tmp/${label}.err"; return 1; }
    cat "/tmp/${label}.one" >> "/tmp/${label}.iters"
  done
  sort -n -k1 "/tmp/${label}.iters" | head -1 > "$stat"
}

declare -A WALL CPU RSS RPS

run_variant() {
  local name="$1"; local bucket="bench-${name}-${STAMP}"
  local up_stat="/tmp/${name}.up" dn_stat="/tmp/${name}.dn"
  local mb cp_up cp_dn rm rb

  case "$name" in
    s5cmd)
      mb=( s5cmd --endpoint-url "$EP" mb "s3://$bucket" )
      cp_up=( s5cmd --endpoint-url "$EP" --numworkers "$WORKERS" cp "$CORPUS/*" "s3://$bucket/up/" )
      cp_dn=( s5cmd --endpoint-url "$EP" --numworkers "$WORKERS" cp "s3://$bucket/up/*" "$OUT/" )
      rm=( s5cmd --endpoint-url "$EP" rm "s3://$bucket/up/*" )
      rb=( s5cmd --endpoint-url "$EP" rb "s3://$bucket" )
      ;;
    rs5cmd|rs5cmd-fast)
      local fast=(); [ "$name" = "rs5cmd-fast" ] && fast=( --fast )
      mb=( "$RS5CMD" --endpoint-url "$EP" mb "s3://$bucket" )
      cp_up=( "$RS5CMD" --endpoint-url "$EP" --numworkers "$WORKERS" cp "${fast[@]}" "$CORPUS/*" "s3://$bucket/up/" )
      cp_dn=( "$RS5CMD" --endpoint-url "$EP" --numworkers "$WORKERS" cp "${fast[@]}" "s3://$bucket/up/*" "$OUT/" )
      rm=( "$RS5CMD" --endpoint-url "$EP" rm "s3://$bucket/up/*" )
      rb=( "$RS5CMD" --endpoint-url "$EP" rb "s3://$bucket" )
      ;;
    *) echo "unknown variant $name"; return 1;;
  esac

  echo "==> [$name] bucket=$bucket"
  "${mb[@]}" >/dev/null 2>&1 || true

  rm -rf "$OUT"; mkdir -p "$OUT"
  echo "  upload..."
  measure "${name}-up" "$up_stat" - -- "${cp_up[@]}"
  echo "  download..."
  measure "${name}-dn" "$dn_stat" "$OUT" -- "${cp_dn[@]}"

  local downloaded; downloaded=$(find "$OUT" -type f | wc -l)
  echo "  downloaded $downloaded/$N files"

  read -r e u s m < "$up_stat"; WALL[$name,up]=$e; CPU[$name,up]=$(awk "BEGIN{print $u+$s}"); RSS[$name,up]=$(awk "BEGIN{printf \"%.0f\", $m/1024}"); RPS[$name,up]=$(awk "BEGIN{printf \"%.0f\", $N/$e}")
  read -r e u s m < "$dn_stat"; WALL[$name,dn]=$e; CPU[$name,dn]=$(awk "BEGIN{print $u+$s}"); RSS[$name,dn]=$(awk "BEGIN{printf \"%.0f\", $m/1024}"); RPS[$name,dn]=$(awk "BEGIN{printf \"%.0f\", $N/$e}")

  echo "  cleanup..."
  "${rm[@]}" >/dev/null 2>&1 || true
  "${rb[@]}" >/dev/null 2>&1 || true
}

for v in $VARIANTS; do run_variant "$v"; done

echo
echo "## Benchmark: $N x ${SIZE}B objects, workers=$WORKERS, best of $ITERS, endpoint=$EP"
echo
printf "| client | op | wall(s) | req/s | CPU(s) | peakRSS(MB) |\n"
printf "|---|---|---:|---:|---:|---:|\n"
for v in $VARIANTS; do
  for op in up dn; do
    opname=$([ "$op" = up ] && echo upload || echo download)
    printf "| %s | %s | %s | %s | %s | %s |\n" \
      "$v" "$opname" "${WALL[$v,$op]:-?}" "${RPS[$v,$op]:-?}" "${CPU[$v,$op]:-?}" "${RSS[$v,$op]:-?}"
  done
done
