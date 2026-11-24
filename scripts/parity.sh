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

# Overlay payloads and text for clips timed against the databases.
EXAMPLE=$ROOT/examples/timer_db.yaml
FIXTURE=$ROOT/tests/fixtures/parity_db.yaml
M=$ROOT/tests/fixtures/markers
# DJI filenames hold camera-local time; pin the zone both tools read it in.
export TZ=America/Vancouver
overlay_cases=(
  # A clip that is a stored run, and one that isn't.
  "$EXAMPLE|--course lower_whistler_a_line --markers $ROOT/examples/markers.csv --filename GX010123.MP4 --fps 59.94"
  "$EXAMPLE|--course lower_whistler_a_line --markers $ROOT/examples/markers.csv --filename GX010999.MP4 --fps 59.94 --capture-time 2026-06-01T08:00:00-07:00"
  # Compared as of a time between stored runs, after all, before all.
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename GX019999.MP4 --fps 59.94 --capture-time 2026-06-02T18:00:00Z"
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename GX019999.MP4 --fps 59.94 --date 2026-06-10"
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename GX019999.MP4 --fps 59.94 --date 2026-05-01"
  # Matches p2, so it's compared as of p2's own time, without p2.
  "$FIXTURE|--course pingu --markers $M/pingu_p2.csv --filename GX010200.MP4 --fps 29.97"
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename DJI_20260602120000_0001_D.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename GX019999.MP4 --fps 59.94 --source-path $M/pingu_new.csv"
  "$FIXTURE|--course cafe --markers $M/cafe.csv --filename x.MP4 --fps 50 --date 2026-07-01"
  "$FIXTURE|--course rounding --markers $M/rounding.csv --filename x.MP4 --fps 16 --date 2026-07-01"
  "$FIXTURE|--course empty --markers $M/rounding.csv --filename x.MP4 --fps 16 --date 2026-07-01"
)
overlay_errors=(
  "$FIXTURE|--course nope --markers $M/pingu_new.csv --filename x.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/missing_marker.csv --filename x.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/duplicate.csv --filename x.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/bad_columns.csv --filename x.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/bad_frame.csv --filename x.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/blank_name.csv --filename x.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/does_not_exist.csv --filename x.MP4 --fps 59.94"
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename x.MP4 --fps 0"
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename x.MP4 --fps 59.94 --capture-time nope"
  "$FIXTURE|--course pingu --markers $M/pingu_new.csv --filename x.MP4 --fps 59.94 --date 2026-13-01"
)
for case in "${overlay_cases[@]}"; do
  db=${case%%|*}
  read -ra args <<<"${case#*|}"
  for mode in best_lap optimal; do
    compare "$db" overlay-payload "${args[@]}" --mode "$mode"
    compare "$db" overlay-text "${args[@]}" --mode "$mode"
  done
done
for case in "${overlay_errors[@]}"; do
  db=${case%%|*}
  read -ra args <<<"${case#*|}"
  compare "$db" overlay-text "${args[@]}"
done

echo "$checks checks, $failures mismatches"
[[ $failures == 0 ]]
