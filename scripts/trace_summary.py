#!/usr/bin/env python3
"""
Summarizes an SS_TRACE timeline (make trace, profile_linux.sh trace) per stage: when its first
task starts and its last ends, the gap since the previous stage (serial time on the calling
thread between `par` scopes, such as allocations), and each worker's busy time in tasks.
Standard library only.

Usage: trace_summary.py prof/linux-trace.json
"""
import json
import sys
from collections import defaultdict

WHOLE = -1  # an `each` job's span; `run` tasks are the spans with a task index


def main():
    events = json.load(open(sys.argv[1]))["traceEvents"]
    t0 = min(e["ts"] for e in events)
    stages = defaultdict(list)
    for e in events:
        stages[e["cat"]].append(e)
    previous_end = None
    print(f"{'stage':10}{'start':>9}{'end':>9}{'wall':>8}{'gap':>8}   busy ms per worker (tasks only)")
    for stage, spans in sorted(stages.items(), key=lambda item: min(e["ts"] for e in item[1])):
        start = (min(e["ts"] for e in spans) - t0) / 1e3
        end = (max(e["ts"] + e["dur"] for e in spans) - t0) / 1e3
        tasks = [e for e in spans if e["args"]["task"] != WHOLE] or spans
        busy = defaultdict(float)
        for e in tasks:
            busy[e["tid"]] += e["dur"] / 1e3
        gap = "" if previous_end is None else f"{start - previous_end:.1f}"
        print(f"{stage:10}{start:9.1f}{end:9.1f}{end - start:8.1f}{gap:>8}   "
              + " ".join(f"{busy[w]:.0f}" for w in sorted(busy)))
        previous_end = end
    print("\nTimes in ms from the first span. A stage's gap is time with no task running, which")
    print("includes the serial work before it and the time from the binary's start to the first span.")


if __name__ == "__main__":
    main()
