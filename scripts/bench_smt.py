#!/usr/bin/env python3
"""
Times images the way the scorer's CPUs run them: two copies at once, one on logical CPUs 0-7
and one on 8-15, so every physical core of a 16-thread machine runs two worker threads, as the
scorer's "8 pinned vCPUs (hyperthreads)" do. One copy alone has a core per worker and hides
what the scorer charges for: on the x86 box a run went from 472 to 736 ms this way, and the
scorer took 0.83 s (positional.md).

Usage: bench_smt.py ROUNDS VARIANT...      VOL=ss-full for another data volume (default ss-p20)
A VARIANT is an image, optionally followed by VAR=value words:
  bench_smt.py 5 "shopping-sessions:latest SS_PIPE=classic" shopping-sessions:latest
The variants take turns; the table is the median of every SS_TIMING stage over both copies.
A cpuset alone does not do this on WSL2: Hyper-V does not keep sibling vCPUs on one core.
"""
import os
import re
import shlex
import statistics
import subprocess
import sys
import time

ROUNDS = int(sys.argv[1])
VARIANTS = sys.argv[2:]


def command(cpuset, variant):
    words = shlex.split(variant)
    cmd = [
        "docker", "run", "--rm", "--network", "none", "--read-only",
        "--tmpfs", "/tmp:rw,size=64m,mode=1777", "--tmpfs", "/output:rw,size=4g,mode=1777",
        "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--pids-limit", "4096",
        "--cpuset-cpus", cpuset, "--memory", "12g", "--memory-swap", "12g", "--user", "1000:1000",
        "-v", os.environ.get("VOL", "ss-p20") + ":/data:ro", "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output",
        "-e", "NCPU=8", "-e", "SS_TIMING=1", "-e", "PROFILE=1",
    ]
    for e in words[1:]:
        cmd += ["-e", e]
    return cmd + [words[0]]


def parse(stderr, wall):
    stages = {}
    for line in stderr.splitlines():
        m = re.match(r"^(\w+)\s+([\d.]+) ms$", line.strip())
        if m:
            stages[m.group(1)] = float(m.group(2))
    for m in re.finditer(r"([\w+]+)=([\d.]+)s", stderr):
        stages[m.group(1)] = float(m.group(2)) * 1e3
    stages["docker"] = wall
    return stages


def paired(variant):
    t0 = time.perf_counter()
    procs = [subprocess.Popen(command(c, variant), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
             for c in ("0-7", "8-15")]
    out = []
    for p in procs:
        _, err = p.communicate()
        if p.returncode:
            print("FAILED", variant, err[-400:])
        out.append(parse(err, (time.perf_counter() - t0) * 1e3))
    return out


results = {v: [] for v in VARIANTS}
paired(VARIANTS[0])  # warm-up
for _ in range(ROUNDS):
    for v in VARIANTS:
        results[v] += paired(v)
for v, runs in results.items():
    keys = [k for k in runs[0] if k != "docker"]
    print(f"{v:<55}", "  ".join(f"{k} {statistics.median(x[k] for x in runs):.0f}" for k in keys))
