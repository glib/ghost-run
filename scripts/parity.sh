#!/usr/bin/env bash
# Checks the Rust CLI (rtimer) gives exactly the Python CLI's output and writes
# byte-identical databases, on the example and fixture databases.
#
#   cargo build -p timer-cli && scripts/parity.sh
#
# Needs Python 3.10+ with PyYAML and Pillow (requirements-resolve.txt).
# RTIMER=/path/to/rtimer overrides the binary.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RTIMER=${RTIMER:-$ROOT/target/debug/rtimer}
PYTHON=${PYTHON:-python3}
export PYTHONPATH=$ROOT/src${PYTHONPATH:+:$PYTHONPATH}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

failures=0
checks=0

py() { "$PYTHON" -m resolve_timer.cli "$@"; }
rs() { "$RTIMER" "$@"; }

# compare DB ARGS...: same stdout+stderr and exit code from both CLIs.
compare() {
  local db=$1
  shift
  local p r pc rc
  p=$(py --db "$db" "$@" 2>&1)
  pc=$?
  r=$(rs --db "$db" "$@" 2>&1)
  rc=$?
  checks=$((checks + 1))
  if [[ $p == "$r" && $pc == "$rc" ]]; then
    return
  fi
  failures=$((failures + 1))
  echo "MISMATCH: $(basename "$db"): $*"
  diff <(printf '%s\nexit %s\n' "$p" "$pc") <(printf '%s\nexit %s\n' "$r" "$rc") | sed 's/^/  /'
}

for db in "$ROOT"/examples/timer_db.yaml "$ROOT"/tests/fixtures/*.yaml; do
  name=$(basename "$db")
  compare "$db" courses
  compare "$db" runs
  compare "$db" validate-db
  compare "$db" stats --course does-not-exist
  while IFS=$'\t' read -r course_id _; do
    compare "$db" runs --course "$course_id"
    compare "$db" stats --course "$course_id"
    compare "$db" stats --course "$course_id" --json
  done < <(py --db "$db" courses)

  # Writing: both tools normalize a copy; the files must be byte-identical.
  cp "$db" "$WORK/py-$name"
  cp "$db" "$WORK/rs-$name"
  p=$(py --db "$WORK/py-$name" normalize-db 2>&1)
  r=$(rs --db "$WORK/rs-$name" normalize-db 2>&1)
  checks=$((checks + 1))
  if [[ $p != "$r" ]] || ! cmp -s "$WORK/py-$name" "$WORK/rs-$name"; then
    failures=$((failures + 1))
    echo "MISMATCH: $name: normalize-db (output or written file)"
    diff <(echo "$p") <(echo "$r") | sed 's/^/  /'
    diff "$WORK/py-$name" "$WORK/rs-$name" | sed 's/^/  /'
  fi
  # And each tool reads the other's file the same way.
  compare "$WORK/rs-$name" runs
  compare "$WORK/rs-$name" validate-db
done

echo "$checks checks, $failures mismatches"
[[ $failures == 0 ]]
