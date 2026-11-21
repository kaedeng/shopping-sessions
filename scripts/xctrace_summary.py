#!/usr/bin/env python3
"""
Summarizes an Instruments System Trace of one run (macOS): every syscall the process made,
every virtual-memory fault, and where in PROCESS the faults happened.

Usage: xctrace_summary.py TRACE [PROCESS]   (PROCESS defaults to shopping-sessions)
Record with `make systrace-mac`, or:
  xctrace record --template 'System Trace' --output run.trace --launch -- target/profiling/shopping-sessions
Times are summed over threads. Wall time includes blocking (a thread join is a long mach_msg2_trap);
cpu is time on a core. Each table shows its 15 largest rows by wall time.
"""
import re
import subprocess
import sys
import xml.etree.ElementTree as ET


def rows(trace, schema):
    """Yields each row of one table as {column: value}, with xctrace's id/ref sharing resolved."""
    xpath = f'/trace-toc/run[@number="1"]/data/table[@schema="{schema}"]'
    export = subprocess.Popen(["xctrace", "export", "--input", trace, "--xpath", xpath], stdout=subprocess.PIPE)
    seen, columns = {}, []
    for _, element in ET.iterparse(export.stdout):
        tag, ref = element.tag, element.get("ref")
        if tag == "mnemonic":
            columns.append(element.text)
        elif tag == "row":
            yield {name: seen.get(cell.get("ref") or cell.get("id")) for name, cell in zip(columns, element)}
            element.clear()
        elif element.get("id") and not ref:
            if tag == "binary":
                value = element.get("name")
            elif tag == "frame":
                binary = element.find("binary")
                value = (element.get("name"), seen.get(binary.get("ref") or binary.get("id")) if binary is not None else None)
            elif tag == "backtrace":
                value = [seen.get(frame.get("ref") or frame.get("id")) for frame in element.iter("frame")]
            elif tag in ("syscall", "process", "thread"):
                value = element.get("fmt")
            else:
                value = element.text
            seen[element.get("id")] = value
    if export.wait() != 0:
        sys.exit(f"xctrace export failed for table {schema}")


def short(name):
    """Drops generic arguments, which make Rust closure names unreadable."""
    while (shorter := re.sub(r"<[^<>]*>", "", name)) != name:
        name = shorter
    return name.replace("&lt;", "<").replace("&gt;", ">")


def table(title, totals, megabytes=False):
    """totals: key -> [count, wall ns, on-core ns, bytes]; the 15 largest by wall time."""
    print(f"\n{title:76}{'count':>9}{'wall ms':>10}{'cpu ms':>9}" + ("       MB" if megabytes else ""))
    for key, (count, wall, cpu, size) in sorted(totals.items(), key=lambda item: -item[1][1])[:15]:
        line = f"  {key[:74]:74}{count:>9}{wall / 1e6:>10.1f}{cpu / 1e6:>9.1f}"
        print(line + (f"{size / 1e6:>9.0f}" if megabytes else ""))


def add(totals, key, row):
    total = totals.setdefault(key, [0, 0, 0, 0])
    total[0] += 1
    total[1] += int(row.get("duration") or 0)
    total[2] += int(row.get("cputime") or 0)
    total[3] += int(row.get("size") or 0)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    trace, process = sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "shopping-sessions"
    mine = lambda row: (row.get("process") or "").startswith(process + " (")

    calls = {}
    for row in rows(trace, "syscall"):
        if mine(row):
            add(calls, row["call"], row)
    table(f"syscalls of {process}", calls)

    ops, where = {}, {}
    for row in rows(trace, "virtual-memory"):
        if mine(row):
            add(ops, row["operation"], row)
            # Inlined frames come without module paths, so one frame alone is often just `write`.
            frames = [short(name) for name, binary in row["backtrace"] or [] if binary == process][:3]
            add(where, f"{row['operation']}: {' < '.join(frames) or '(outside the binary)'}", row)
    table(f"virtual memory of {process}", ops, megabytes=True)
    table("faults by stack (innermost 3 frames in the binary)", where)


if __name__ == "__main__":
    main()
