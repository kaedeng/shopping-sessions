#!/bin/sh
# Correctness checks through Docker, for machines without Python and Polars (x86.md):
# all fixtures, then dev/ against dev-answer/, with the binary from $IMAGE.
#   scripts/check_docker.sh            IMAGE defaults to shopping-sessions:latest
# Extra arguments are VAR=value settings for the binary (for example SS_PART_BITS=4 NCPU=3).
set -eu
IMAGE="${IMAGE:-shopping-sessions:latest}"
CHECK="${CHECK_IMAGE:-shopping-sessions:check}"
# Git Bash on Windows: hand docker.exe Windows paths and stop MSYS rewriting /fixtures etc.
export MSYS_NO_PATHCONV=1
ROOT=$(cd "$(dirname "$0")/.." && { pwd -W 2>/dev/null || pwd; })
cd "$(dirname "$0")/.."
docker build -q -f scripts/check.Dockerfile --build-arg BASE="$IMAGE" -t "$CHECK" . >/dev/null
ENVS=""
for setting in "$@"; do ENVS="$ENVS -e $setting"; done
echo "== fixtures ($IMAGE $*)"
# shellcheck disable=SC2086
docker run --rm $ENVS -v "$ROOT/fixtures":/fixtures:ro "$CHECK" \
  python /app/scripts/run_fixtures.py /fixtures -- /app/shopping-sessions
echo "== dev against dev-answer"
# shellcheck disable=SC2086
docker run --rm $ENVS -v "$ROOT/dev":/dev-data:ro -v "$ROOT/dev-answer":/answer:ro "$CHECK" \
  sh -c 'DATA_DIR=/dev-data OUTPUT_DIR=/tmp/out /app/shopping-sessions && python /app/scripts/compare.py /tmp/out /answer'
