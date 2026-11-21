#!/bin/sh
# Entry point. Runs with no arguments; exit 0 means "done, check my output".
set -eu
: "${DATA_DIR:=/data}" "${OUTPUT_DIR:=/output}" "${SCRATCH_DIR:=/scratch}"
export DATA_DIR OUTPUT_DIR SCRATCH_DIR
# Read-only root filesystem: everything that wants a temp or home dir gets /scratch.
export TMPDIR="$SCRATCH_DIR" HOME="$SCRATCH_DIR" POLARS_TEMP_DIR="$SCRATCH_DIR"
# The streaming engine spills to /var/tmp by default, which is read-only here.
export POLARS_OOC_SPILL_DIR="$SCRATCH_DIR/spill"
# Otherwise Polars sizes its thread pool from the host's cores, not the CPU limit.
if [ -n "${NCPU:-}" ]; then export POLARS_MAX_THREADS="${POLARS_MAX_THREADS:-$NCPU}"; fi
exec /app/shopping-sessions
