#!/usr/bin/env bash
# Runs rtimer's database and overlay commands on the example and fixture
# databases and prints one transcript: each command, its output and exit code,
# and the files normalize-db writes. tests/fixtures/golden.txt is that
# transcript as it was when the Python tool was retired (decision 0008): its
# outputs then matched the Python CLI's byte for byte (the old parity check).
#
#   scripts/golden.sh            # print the transcript
#   scripts/golden.sh --check    # compare with tests/fixtures/golden.txt
#   scripts/golden.sh --update   # rewrite it (only for a deliberate change)
#
# RTIMER=/path/to/rtimer overrides the binary (default target/debug/rtimer).
# The timer-cli crate's tests/golden.rs runs --check.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RTIMER=${RTIMER:-$ROOT/target/debug/rtimer}
GOLDEN=$ROOT/tests/fixtures/golden.txt
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
# DJI filenames hold camera-local time; pin the zone they're read in.
export TZ=America/Vancouver

# Paths in the transcript are relative to the repository.
rel() { sed -e "s|$ROOT/||g" -e "s|$WORK/|work/|g"; }

run() {
  local db=$1
  shift
  local out code
  out=$("$RTIMER" --db "$db" "$@" 2>&1)
  code=$?
  printf '### %s %s\n%s\nexit %s\n' "${db#"$ROOT"/}" "$*" "$out" "$code" | rel
}

transcript() {
  for db in "$ROOT"/examples/timer_db.yaml "$ROOT"/tests/fixtures/*.yaml; do
    name=$(basename "$db")
    run "$db" courses
    run "$db" runs
    run "$db" validate-db
    run "$db" stats --course does-not-exist
    while IFS=$'\t' read -r course_id _; do
      run "$db" runs --course "$course_id"
      run "$db" stats --course "$course_id"
      run "$db" stats --course "$course_id" --json
    done < <("$RTIMER" --db "$db" courses 2>/dev/null)
    cp "$db" "$WORK/$name"
    run "$WORK/$name" normalize-db
    printf '### written %s\n' "$name"
    cat "$WORK/$name"
    run "$WORK/$name" runs
  done

  local EXAMPLE=$ROOT/examples/timer_db.yaml
  local FIXTURE=$ROOT/tests/fixtures/parity_db.yaml
  local M=$ROOT/tests/fixtures/markers
  local cases=(
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
  local errors=(
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
  local case db args mode
  for case in "${cases[@]}"; do
    db=${case%%|*}
    read -ra args <<<"${case#*|}"
    for mode in best_lap optimal; do
      run "$db" overlay-payload "${args[@]}" --mode "$mode"
      run "$db" overlay-text "${args[@]}" --mode "$mode"
    done
  done
  for case in "${errors[@]}"; do
    db=${case%%|*}
    read -ra args <<<"${case#*|}"
    run "$db" overlay-text "${args[@]}"
  done
}

case "${1:-}" in
--check)
  if diff -u "$GOLDEN" <(transcript); then
    echo "rtimer matches tests/fixtures/golden.txt"
  else
    echo "rtimer's output changed (above). If that's deliberate: scripts/golden.sh --update" >&2
    exit 1
  fi
  ;;
--update) transcript >"$GOLDEN" ;;
"") transcript ;;
*)
  echo "usage: $0 [--check|--update]" >&2
  exit 2
  ;;
esac
