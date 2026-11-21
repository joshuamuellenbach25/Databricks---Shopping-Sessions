#!/bin/sh
set -eu
DATA_ROOT=$1
ANSWER=$2
OUT_ROOT=$3
RUNS=${4:-3}
HERE=$(cd "$(dirname "$0")/.." && pwd)
PY=${PYTHON:-python3}
BIN="$HERE/target/release/solver"
[ -f "$BIN.exe" ] && BIN="$BIN.exe"

cargo build --release --locked --manifest-path "$HERE/Cargo.toml" > "$OUT_ROOT/build.log" 2>&1 || { tail -30 "$OUT_ROOT/build.log"; exit 1; }
echo "build ok"
"$PY" "$HERE/scripts/run_fixtures.py" "$HERE/fixtures" -- "$BIN" > "$OUT_ROOT/fixtures.log" 2>&1 || true
tail -1 "$OUT_ROOT/fixtures.log"
rm -rf "$OUT_ROOT/dev" && mkdir -p "$OUT_ROOT/dev"
DATA_DIR="$DATA_ROOT/dev" OUTPUT_DIR="$OUT_ROOT/dev" "$BIN" 2> "$OUT_ROOT/dev.log"
"$PY" "$HERE/scripts/compare.py" "$OUT_ROOT/dev" "$ANSWER" > "$OUT_ROOT/compare.log" 2>&1 || true
echo "dev: $(tail -1 "$OUT_ROOT/compare.log")"
i=0
while [ "$i" -lt "$RUNS" ]; do
  i=$((i + 1))
  rm -rf "$OUT_ROOT/m20" && mkdir -p "$OUT_ROOT/m20"
  DATA_DIR="$DATA_ROOT/m20" OUTPUT_DIR="$OUT_ROOT/m20" NCPU=8 "$BIN" 2> "$OUT_ROOT/m20-$i.log"
  grep -E "^[a-z]" "$OUT_ROOT/m20-$i.log" | grep -E " s " | sed -e 's/ (total/ total/' | awk -v run="$i" 'BEGIN { printf "run %s:", run } { printf " | %s", $0 } END { print "" }'
done
