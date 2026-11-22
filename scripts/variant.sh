#!/bin/sh
# One scorer submission as a branch: the current commit with the Dockerfile's run switches
# (and, with -f, its extra build flags) set, as one commit on a new branch `runner/NAME`,
# pushed. Prints the commit to submit. The working tree, the index and the checked-out
# branch are not touched.
#
#   scripts/variant.sh base                          every switch at the binary's default
#   scripts/variant.sh pin SS_PIN=on
#   scripts/variant.sh dedup4 SS_PIN=cores SS_W_DEDUP=4
#   scripts/variant.sh jcc -f "-C llvm-args=-x86-branches-within-32B-boundaries"
#   PUSH=0 scripts/variant.sh huge SS_HUGE=default   make the branch, do not push it
#
# Switches that only set the environment leave the binary as it is, so two such submissions
# differ in nothing but the switch. -f builds another binary.
set -eu
cd "$(dirname "$0")/.."
[ $# -ge 1 ] || { echo "usage: variant.sh NAME [-f FLAGS] [SWITCH=value...]" >&2; exit 1; }
name="$1"; shift
flags=""
if [ "${1:-}" = "-f" ]; then flags="$2"; shift 2; fi
switches="$*"
base=$(git rev-parse HEAD)
git show "$base:Dockerfile" | grep -q '^ENV SS_RUN=' || { echo "variant.sh: no 'ENV SS_RUN=' line in the commit's Dockerfile" >&2; exit 1; }
git show "$base:Dockerfile" | grep -q '^ARG EXTRA_RUSTFLAGS=' || { echo "variant.sh: no 'ARG EXTRA_RUSTFLAGS=' line in the commit's Dockerfile" >&2; exit 1; }
# awk, not sed: the flags hold characters a sed replacement would read.
blob=$(git show "$base:Dockerfile" | SWITCHES="$switches" FLAGS="$flags" awk '
  /^ENV SS_RUN=/ { line = "ENV SS_RUN=1"; if (ENVIRON["SWITCHES"] != "") line = line " " ENVIRON["SWITCHES"]; print line; next }
  /^ARG EXTRA_RUSTFLAGS=/ { print "ARG EXTRA_RUSTFLAGS=\"" ENVIRON["FLAGS"] "\""; next }
  { print }
' | git hash-object -w --stdin)
# The commit's tree with that Dockerfile, through an index of its own.
index="$(git rev-parse --git-dir)/variant-index.$$"
tree=$(GIT_INDEX_FILE="$index" sh -c 'git read-tree "$0" && git update-index --cacheinfo "100644,$1,Dockerfile" && git write-tree' "$base" "$blob")
rm -f "$index"
what="${switches:-every switch at its default}"; [ -z "$flags" ] || what="$what; built with $flags"
# The address of the commits before: GitHub refuses a private one.
commit=$(printf 'Scorer variant %s: %s\n%s' "$name" "$what" "${TRAILER:+
$TRAILER
}" | git -c user.email="$(git log -1 --format=%ae "$base")" commit-tree "$tree" -p "$base")
git branch -f "runner/$name" "$commit"
[ "${PUSH:-1}" = 0 ] || git push -q origin "runner/$name"
echo "runner/$name $commit  ($what)"
