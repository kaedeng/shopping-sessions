#!/bin/sh
# The default pipeline (opaque IDs) and the positional one against the classic one, exactly, on
# generated data.
# Run inside a container that has the release binary, the generator, python3 and Polars:
#   BIN       the binary (default /target/release/shopping-sessions)
#   GEN       the generator (default /gen/release/generate)
#   PY        a python with polars (default python3)
#   WORK      a scratch folder, best a tmpfs (default /work)
# pos_check.sh seeds "SEED:EVENTS ..." ["VAR=value ..." ...]
#     each dataset as generated, under the default settings and each quoted group of settings
# pos_check.sh spoiled "SEED:EVENTS:MUTATION_SEED:PER_KIND ..."
#     each dataset spoiled by scripts/mutate.py first: IDs and users swapped between rows,
#     copies that win dedup with another user, times out of order, people merged and split
set -u
BIN=${BIN:-/target/release/shopping-sessions}
GEN=${GEN:-/gen/release/generate}
PY=${PY:-python3}
WORK=${WORK:-/work}
HERE=$(cd "$(dirname "$0")" && pwd)
mode=$1
specs=$2
shift 2
fail=0

compare() {  # LABEL SETTING...: the positional run under the settings, against $WORK/classic
  label=$1
  shift
  rm -rf "$WORK/pos"
  mkdir -p "$WORK/pos"
  line=$(env DATA_DIR="$WORK/data" OUTPUT_DIR="$WORK/pos" SS_TIMING=1 "$@" "$BIN" 2>&1 | grep "^positional:" | sed 's/positional: //')
  if "$PY" "$HERE/compare_outputs.py" "$WORK/classic" "$WORK/pos" >"$WORK/cmp.txt" 2>&1; then
    echo "ok    $label [$*] $line"
  else
    echo "FAIL  $label [$*] $line"
    head -24 "$WORK/cmp.txt"
    fail=1
  fi
}

for spec in $specs; do
  seed=$(echo "$spec" | cut -d: -f1)
  events=$(echo "$spec" | cut -d: -f2)
  rm -rf "$WORK/clean" "$WORK/data" "$WORK/classic"
  "$GEN" --seed "$seed" --events "$events" --clicks $((events / 4)) --output "$WORK/data" >/dev/null 2>&1 || { echo "generate failed for $spec"; fail=1; continue; }
  if [ "$mode" = spoiled ]; then
    mv "$WORK/data" "$WORK/clean"
    "$PY" "$HERE/mutate.py" "$WORK/clean" "$WORK/data" "$(echo "$spec" | cut -d: -f3)" "$(echo "$spec" | cut -d: -f4)" | cut -c1-120
  fi
  mkdir -p "$WORK/classic"
  env DATA_DIR="$WORK/data" OUTPUT_DIR="$WORK/classic" SS_PIPE=classic "$BIN" || { echo "classic failed for $spec"; fail=1; continue; }
  if [ "$mode" = spoiled ]; then
    compare "$spec" SS_PIPE=opaque
    compare "$spec" SS_PIPE=opaque SS_DECODE=row NCPU=2
    compare "$spec" SS_PIPE=opaque SS_CLICK_IDS=whole
    compare "$spec" SS_PIPE=auto
  else
    compare "$spec" SS_PIPE=opaque
    for setting in "$@"; do
      # shellcheck disable=SC2086
      compare "$spec" $setting
    done
  fi
done
exit $fail
