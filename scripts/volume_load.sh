#!/bin/sh
# Copies a dataset folder into a Docker volume, so runs read it from the VM's own disk and not
# through a bind mount (9p on WSL2, virtiofs on macOS): volume_load.sh DIR VOLUME
#   generator/target/release/generate --seed 7 --events 20000000 --clicks 5000000 --output p20
#   scripts/volume_load.sh p20 ss-p20
set -eu
[ $# -eq 2 ] || { sed -n '2,6p' "$0"; exit 2; }
export MSYS_NO_PATHCONV=1
SRC=$(cd "$1" && { pwd -W 2>/dev/null || pwd; })
docker volume create "$2" >/dev/null
docker run --rm -v "$2":/data -v "$SRC":/src:ro debian:trixie-slim \
  sh -c 'find /data -mindepth 1 -delete && cp -a /src/. /data/ && du -sh /data && cat /data/manifest.json'
