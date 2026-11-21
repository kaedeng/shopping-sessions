#!/bin/sh
# Profiles one run in the Linux VM, where the scorer's kernel behaviour is: 4 KB pages, glibc
# malloc with the image's tunables, transparent huge pages. Needs `make profile-image` and the
# data in the Docker volume $VOLUME (`make half linux-data`). Results land in prof/.
#
#   profile_linux.sh flame            CPU flamegraph by timer sampling   prof/linux-cpu.svg, .txt
#   profile_linux.sh annotate         hot instructions of the last flame prof/linux-annotate.txt
#   profile_linux.sh faults           where page faults happen           prof/linux-faults.svg, .txt
#   profile_linux.sh syscalls         strace -c and perf stat            prof/linux-strace.txt, linux-stat.txt
#   profile_linux.sh trace            per-task timeline (SS_TRACE)       prof/linux-trace.json
#   profile_linux.sh bench ARGS...    scripts/bench.py in the container on /data
#   profile_linux.sh shell            a shell in the container
#
# The VM has no hardware counters (no PMU under Apple's hypervisor): perf samples on a timer and
# counts only software events. The run is privileged so perf sees kernel stacks and bench.py
# can drop the page cache: --prepare 'echo 3 > /proc/sys/vm/drop_caches'.
#
# On an x86 machine (WSL2 or Linux), profile the scorer's AVX2 code natively (x86.md):
#   PLATFORM=linux/amd64 VOLUME=ss-p20 scripts/profile_linux.sh flame
# with the profile image built from the amd64 release image. WSL2 has no PMU either.
set -eu
IMAGE="${PROFILE_IMAGE:-shopping-sessions:profile}"
VOLUME="${VOLUME:-ss-half}"
CPUS="${CPUS:-8}"
PLATFORM="${PLATFORM:-linux/arm64}"
BIN=/app/shopping-sessions-profiling
mkdir -p prof
# Git Bash on Windows: hand docker.exe a Windows path and stop MSYS rewriting /prof, /data etc.
export MSYS_NO_PATHCONV=1
PROF=$(cd prof && { pwd -W 2>/dev/null || pwd; })

run() {
  docker run --rm $TTY --privileged --platform "$PLATFORM" --cpus "$CPUS" -e NCPU="${CPUS%.*}" \
    -e DATA_DIR=/data -e OUTPUT_DIR=/tmp/out -v "$VOLUME":/data:ro -v "$PROF":/prof -w /prof \
    "$IMAGE" "$@"
}
TTY=
mode="${1:-}"
[ $# -gt 0 ] && shift
# One untimed run first, so the input is in the page cache. perf's inline expansion goes through
# addr2line, which fails on this binary; samply on the Mac shows inlined frames. On arm64 perf
# adds the sampled link register as the leaf's caller, which is scratch data inside a function
# that saved it: those come out [unknown] and are dropped from the SVG.
warm="$BIN >/dev/null 2>&1"
report="perf report --stdio --no-children --no-inline --percent-limit 0.5 2>/dev/null"
collapse="rustfilt | inferno-collapse-perf | sed 's/;\[unknown\]//g'"
svg="perf script --no-inline 2>/dev/null -i"
case "$mode" in
  flame) run sh -c "$warm; perf record -F 999 -g -o linux-cpu.data $BIN >/dev/null \
    && $report -i linux-cpu.data | rustfilt > linux-cpu.txt \
    && $svg linux-cpu.data | $collapse | inferno-flamegraph --title 'CPU, $PLATFORM' > linux-cpu.svg \
    && grep -E '^ +[0-9.]+%' linux-cpu.txt | head -25" ;;
  annotate) run sh -c "perf annotate -i linux-cpu.data --stdio --no-source 2>/dev/null | rustfilt > linux-annotate.txt \
    && grep -E 'Percent \\||^ +([1-9][0-9]*|0\\.[5-9])[0-9.]* :' linux-annotate.txt | head -80" ;;
  faults) run sh -c "$warm; perf record -e page-faults -c 1 -g -o linux-faults.data $BIN >/dev/null \
    && $report -i linux-faults.data | rustfilt > linux-faults.txt \
    && $svg linux-faults.data | $collapse | inferno-flamegraph --title 'Page faults, $PLATFORM' --countname faults > linux-faults.svg \
    && grep -E '^ +[0-9.]+%' linux-faults.txt | head -25" ;;
  syscalls) run sh -c "$warm; strace -f -c -w -o linux-strace.txt $BIN && cat linux-strace.txt \
    && perf stat -r 3 -e task-clock,page-faults,minor-faults,major-faults,context-switches,cpu-migrations \
       -o linux-stat.txt $BIN && cat linux-stat.txt" ;;
  trace) run sh -c "$warm; SS_TRACE=/prof/linux-trace.json SS_TIMING=1 $BIN" ;;
  bench) run python3 /app/bench.py --data /data "$@" ;;
  shell) TTY=-it; run bash ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
