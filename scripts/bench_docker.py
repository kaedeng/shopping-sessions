#!/usr/bin/env python3
"""
Times images the way the scorer runs them: every run in a fresh container with the scorer's
flags, reading a Docker volume. Standard library and the docker CLI only, so it runs the same
from Windows (PowerShell or Git Bash), macOS and Linux. x86.md has the walkthrough.

Usage: bench_docker.py [-n RUNS] [--volume VOL] [--idle SECONDS] [--warmup N] VARIANT...
A VARIANT is an image, optionally followed by VAR=value words passed to the container:

  bench_docker.py -n 6 shopping-sessions:base shopping-sessions:latest
  bench_docker.py --idle 12 shopping-sessions:latest "shopping-sessions:latest SS_PART_BITS=8"

The variants take turns, so drift on the machine hits all of them alike. The table shows, per
variant, the median (min) of every stage SS_TIMING prints and of the `docker run` wall time,
and the change of each median against the first variant; `~` marks a change inside the noise
(the interquartile ranges overlap). Compare stages and `total`, not `docker run`: the client,
the container's start and its teardown add about 0.45 s on Docker Desktop.

--idle S waits S seconds before each timed run. On WSL2 the host reclaims memory a run freed
after a few seconds idle, and the next run's page zeroing then costs about twice as much: runs
after 10 s idle measured 690-720 ms against 505-514 ms back to back (x86.md). The scorer's
timed run follows an idle machine, so --idle 12 is the scorer-like number; --idle 0 (the
default) is quieter for comparing compute. Compare variants under the same setting.
"""
import argparse
import shlex
import statistics
import subprocess
import sys
import time


def scorer_command(image, env, volume, cpus, memory):
    """The scorer's `docker run` flags (scripts/docker_run.sh), output to a tmpfs."""
    command = [
        "docker", "run", "--rm",
        "--network", "none", "--read-only", "--tmpfs", "/tmp:rw,size=64m,mode=1777",
        "--tmpfs", "/output:rw,size=4g,mode=1777",
        "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
        "--pids-limit", "4096", "--cpus", str(cpus), "--memory", memory, "--memory-swap", memory,
        "--user", "1000:1000",
        "-v", f"{volume}:/data:ro",
        "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output", "-e", "SCRATCH_DIR=/tmp", "-e", "HOME=/tmp",
        "-e", f"NCPU={int(float(cpus))}", "-e", f"MEM_GB={memory.rstrip('gG')}", "-e", "SS_TIMING=1",
    ]
    for name, value in env.items():
        command += ["-e", f"{name}={value}"]
    return command + [image]


def parse_variant(text):
    words = shlex.split(text)
    image, env = words[0], {}
    for word in words[1:]:
        name, _, value = word.partition("=")
        env[name] = value
    return image, env


def run_once(text, args):
    image, env = parse_variant(text)
    command = scorer_command(image, env, args.volume, args.cpus, args.memory)
    start = time.perf_counter()
    proc = subprocess.run(command, capture_output=True, text=True, errors="replace")
    wall = time.perf_counter() - start
    output = proc.stderr + proc.stdout
    if proc.returncode != 0:
        hint = ""
        if "exec /app/run.sh" in output:
            hint = "\nrun.sh has CRLF line endings: see .gitattributes, then rebuild the image."
        sys.exit(f"{text}: exit {proc.returncode}\n{output}{hint}")
    metrics = {}
    for line in output.splitlines():
        words = line.split()
        if len(words) == 3 and words[2] == "ms":
            metrics[words[0]] = float(words[1])
    if "total" not in metrics:
        sys.exit(f"{text}: no SS_TIMING lines in the output\n{output}")
    metrics["docker run"] = wall * 1e3
    return metrics


def quartiles(values):
    return statistics.quantiles(values, n=4)[::2] if len(values) > 1 else [values[0], values[0]]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("-n", "--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1, help="untimed rounds first (default 1)")
    parser.add_argument("--volume", default="ss-p20", help="Docker volume holding the dataset (default ss-p20)")
    parser.add_argument("--idle", type=float, default=0, help="seconds to wait before each timed run")
    parser.add_argument("--cold", action="store_true",
                        help="drop the Docker VM's page cache before each timed run (a privileged container "
                             "writes /proc/sys/vm/drop_caches), so the input is read from disk")
    parser.add_argument("--cpus", default="8")
    parser.add_argument("--memory", default="24g")
    parser.add_argument("variants", nargs="+")
    args = parser.parse_args()

    for _ in range(args.warmup):
        for text in args.variants:
            run_once(text, args)
    runs = {text: [] for text in args.variants}
    for round in range(args.runs):
        for text in args.variants:
            if args.cold:
                drop = ["docker", "run", "--rm", "--privileged", "debian:trixie-slim", "sh", "-c",
                        "sync; echo 3 > /proc/sys/vm/drop_caches"]
                subprocess.run(drop, check=True, capture_output=True)
            if args.idle:
                time.sleep(args.idle)
            metrics = run_once(text, args)
            runs[text].append(metrics)
            stages = " ".join(f"{name} {value:.0f}" for name, value in metrics.items())
            print(f"round {round + 1}/{args.runs} {text}: {stages}", file=sys.stderr, flush=True)

    labels = [chr(ord("A") + i) for i in range(len(args.variants))]
    print()
    for label, text in zip(labels, args.variants):
        print(f"{label}: {text}")
    cold = ", page cache dropped before each" if args.cold else ""
    print(f"{args.runs} runs each, volume {args.volume}, idle {args.idle:g} s before each{cold}; median (min) in ms\n")
    names = list(dict.fromkeys(name for metrics in runs[args.variants[0]] for name in metrics))
    print(f"{'':12}" + "".join(f"{label:>18}" for label in labels) + "".join(f"{label + ' vs A':>12}" for label in labels[1:]))
    for name in names:
        cells, stats = [], []
        for text in args.variants:
            values = [metrics.get(name, 0.0) for metrics in runs[text]]
            stats.append((statistics.median(values), quartiles(values)))
            cells.append(f"{stats[-1][0]:.1f} ({min(values):.1f})".rjust(18))
        changes = []
        base, (base_low, base_high) = stats[0]
        for median, (low, high) in stats[1:]:
            change = f"{(median / base - 1) * 100:+.1f}%" if base else "-"
            noise = low <= base_high and base_low <= high
            changes.append(f"{change}{' ~' if noise else '  '}".rjust(12))
        print(f"{name:12}" + "".join(cells) + "".join(changes))


if __name__ == "__main__":
    main()
