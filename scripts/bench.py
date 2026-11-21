#!/usr/bin/env python3
"""
Times whole runs of one or more variants, the way DataFusion's `bench.sh compare` and DuckDB's
regression runner do: the variants take turns, so drift on a shared machine hits all of them,
and each metric is the median over the runs. Needs only the standard library.

Usage: bench.py [-n RUNS] [--warmup N] [--data DIR] [--prepare CMD] VARIANT...
A VARIANT is a command, optionally led by VAR=value words:

  bench.py -n 10 target/release/shopping-sessions 'SS_TOK=scalar target/release/shopping-sessions'

Each run gets DATA_DIR, a fresh empty OUTPUT_DIR and SS_TIMING=1. The table has every stage
SS_TIMING prints, the process wall time (exec to reaped, so the kernel's unmapping counts),
and the kernel's rusage: user and system CPU, page faults, context switches, peak RSS.
Columns after the first variant's show the change of the median against it; `~` marks a change
inside the noise (the two interquartile ranges overlap).
`--prepare` runs through the shell before every timed run, for example to drop the page cache.
Time a script that starts a container and you time the docker client, not the binary.
"""
import argparse
import os
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def parse_variant(text):
    words, env = shlex.split(text), {}
    while words and "=" in words[0] and not words[0].startswith(("/", ".")):
        name, value = words.pop(0).split("=", 1)
        env[name] = value
    return words, env


def run_once(text, data, prepare):
    command, env = parse_variant(text)
    if prepare:
        subprocess.run(prepare, shell=True, check=True)
    out = tempfile.mkdtemp(prefix="bench-out-")
    try:
        env = {**os.environ, **env, "DATA_DIR": data, "OUTPUT_DIR": out, "SS_TIMING": "1"}
        start = time.perf_counter()
        proc = subprocess.Popen(command, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        stderr = proc.stderr.read().decode(errors="replace")
        _, status, usage = os.wait4(proc.pid, 0)
        wall = time.perf_counter() - start
        proc.returncode = os.waitstatus_to_exitcode(status)
    finally:
        shutil.rmtree(out, ignore_errors=True)
    if proc.returncode != 0:
        sys.exit(f"{text}: exit {proc.returncode}\n{stderr}")
    metrics = {}
    for line in stderr.splitlines():
        words = line.split()
        if len(words) == 3 and words[2] == "ms":
            metrics[words[0] + " ms"] = float(words[1])
    # ru_maxrss is bytes on macOS and KiB on Linux.
    rss = usage.ru_maxrss / (1 << 20 if sys.platform == "darwin" else 1 << 10)
    metrics.update({
        "wall ms": wall * 1e3, "user ms": usage.ru_utime * 1e3, "sys ms": usage.ru_stime * 1e3,
        "minor faults": usage.ru_minflt, "major faults": usage.ru_majflt,
        "vol ctxsw": usage.ru_nvcsw, "invol ctxsw": usage.ru_nivcsw, "max rss MB": rss,
    })
    return metrics


def quartiles(values):
    return statistics.quantiles(values, n=4)[::2] if len(values) > 1 else [values[0], values[0]]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("-n", "--runs", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1, help="untimed rounds first (default 1)")
    parser.add_argument("--data", default=os.environ.get("DATA_DIR", "full"))
    parser.add_argument("--prepare", help="shell command before every timed run")
    parser.add_argument("variants", nargs="+")
    args = parser.parse_args()
    data = os.path.abspath(args.data)

    for _ in range(args.warmup):
        for text in args.variants:
            run_once(text, data, None)
    runs = {text: [] for text in args.variants}
    for round in range(args.runs):
        for text in args.variants:
            runs[text].append(run_once(text, data, args.prepare))
        print(f"\rround {round + 1}/{args.runs}", end="", file=sys.stderr, flush=True)
    print(file=sys.stderr)

    labels = [chr(ord("A") + i) for i in range(len(args.variants))]
    for label, text in zip(labels, args.variants):
        print(f"{label}: {text}")
    print(f"{args.runs} runs each on {data}; median (min)\n")
    names = list(dict.fromkeys(name for metrics in runs[args.variants[0]] for name in metrics))
    header = f"{'':14}" + "".join(f"{label:>22}" for label in labels) + "".join(f"{label + ' vs A':>12}" for label in labels[1:])
    print(header)
    base = None
    for name in names:
        cells, changes, stats = [], [], []
        for text in args.variants:
            values = [metrics.get(name, 0) for metrics in runs[text]]
            stats.append((statistics.median(values), quartiles(values)))
            digits = 1 if name.endswith("ms") else 0
            cells.append(f"{stats[-1][0]:.{digits}f} ({min(values):.{digits}f})".rjust(22))
        base = stats[0]
        for median, (low, high) in stats[1:]:
            change = f"{(median / base[0] - 1) * 100:+.1f}%" if base[0] else "-"
            noise = low <= base[1][1] and base[1][0] <= high
            changes.append(f"{change}{' ~' if noise else '  '}".rjust(12))
        print(f"{name:14}" + "".join(cells) + "".join(changes))


if __name__ == "__main__":
    main()
