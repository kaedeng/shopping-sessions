#!/bin/sh
# The second try (src/fallback.rs) through Docker: dev/ run into every way a run can die
# (SS_FAULT), each output against dev-answer/, with the binary from $IMAGE.
#   scripts/check_faults.sh            IMAGE defaults to shopping-sessions:latest
#   SLOW=1 scripts/check_faults.sh     also the runs that must die for good
# One line a case: ok or FAIL, then exit code / answer or WRONG / second tries started /
# whether a stale file in the output folder was kept or cleared, then the settings.
# The SLOW cases end in the signal's own death, and where the kernel hands core dumps to a
# program (WSL2 does) that takes minutes each: the process has 64 GB of address space.
set -eu
IMAGE="${IMAGE:-shopping-sessions:latest}"
CHECK="${CHECK_IMAGE:-shopping-sessions:check}"
# Git Bash on Windows: hand docker.exe Windows paths and stop MSYS rewriting /dev-data etc.
export MSYS_NO_PATHCONV=1
ROOT=$(cd "$(dirname "$0")/.." && { pwd -W 2>/dev/null || pwd; })
cd "$(dirname "$0")/.."
docker build -q -f scripts/check.Dockerfile --build-arg BASE="$IMAGE" -t "$CHECK" . >/dev/null
docker run --rm -i -e SLOW="${SLOW:-}" -v "$ROOT/dev":/dev-data:ro -v "$ROOT/dev-answer":/answer:ro "$CHECK" sh -s <<'CASES'
out=/tmp/out
failed=0
one() {
  want=$1; shift
  # A file no run writes: the first try leaves it, a second try must clear it.
  mkdir -p $out/user_daily $out/campaign_daily
  find $out -type f -delete
  echo stale > $out/user_daily/part-99.parquet
  code=0
  env "$@" DATA_DIR=/dev-data OUTPUT_DIR=$out /app/shopping-sessions 2>/tmp/err || code=$?
  tries=$(grep -c "starting over" /tmp/err || true)
  stale=cleared
  if [ -e $out/user_daily/part-99.parquet ]; then stale=kept; find $out -name part-99.parquet -delete; fi
  if python /app/scripts/compare.py $out /answer >/tmp/cmp 2>&1; then same=answer; else same=WRONG; fi
  got="$code/$same/$tries/$stale"
  if [ "$want" = "$got" ]; then echo "ok    $got  $*"; else echo "FAIL  $got (want $want)  $*"; tail -3 /tmp/err; tail -3 /tmp/cmp; failed=1; fi
}
# No death: the first try's output; the stale file is not this run's to remove.
one 0/answer/0/kept NCPU=8
# A death as each stage ends, on the first thread.
for stage in inputs parse dedup sessions; do
  one 0/answer/1/cleared SS_FAULT=segv:$stage
done
# Every kind, on the first thread and on a thread started the way the workers are.
for kind in overflow abort ill fpe bus; do
  one 0/answer/1/cleared SS_FAULT=$kind:parse
done
for kind in segv overflow abort ill fpe bus; do
  one 0/answer/1/cleared SS_FAULT=$kind:dedup:thread
done
# The workers in the middle of their files, all dying at once; and pinned, a CPU each.
one 0/answer/1/cleared SS_FAULT=segv:files
one 0/answer/1/cleared SS_FAULT=abort:files SS_PIN=on
# An error from `run`.
one 0/answer/1/cleared SS_FAULT=error
# After the last file: the first try's output stands and the process ends well.
one 0/answer/0/kept SS_FAULT=segv:output
one 0/answer/0/kept SS_FAULT=abort:output:thread
# Fewer workers; the scorer's switches.
one 0/answer/1/cleared SS_FAULT=segv:dedup NCPU=3
one 0/answer/1/cleared SS_FAULT=segv:sessions SS_POPULATE=1024 SS_PART_BITS_OPAQUE=10 SS_HUGE=never
if [ -n "$SLOW" ]; then
  # No third try, and none at all when switched off: the process dies of the signal.
  one 139/WRONG/0/cleared SS_SECOND=1 SS_FAULT=segv:parse
  one 134/WRONG/0/cleared SS_SECOND=1 SS_FAULT=abort:parse
  one 139/WRONG/0/kept SS_FALLBACK=off SS_FAULT=segv:parse
fi
exit $failed
CASES
