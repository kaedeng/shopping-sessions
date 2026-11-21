#!/bin/sh
# Entry point. Runs with no arguments; exit 0 means "done, check my output".
# The binary reads DATA_DIR, OUTPUT_DIR and NCPU itself and needs no temp directory.
exec /app/shopping-sessions
