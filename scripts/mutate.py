#!/usr/bin/env python3
"""Spoils a generated dataset in the ways the positional pipeline's fast lane has to notice:
event IDs and users swapped between rows, copies that win or lose dedup with another user or a
bad row, times out of order, IDs spelled other ways, people merged and split by links, clicks
moved between people. The classic pipeline is the oracle for the result.
Usage: mutate.py IN_DIR OUT_DIR SEED [PER_KIND]"""
import datetime
import os
import random
import shutil
import sys

src, dst, seed = sys.argv[1], sys.argv[2], int(sys.argv[3])
per_kind = int(sys.argv[4]) if len(sys.argv) > 4 else 150
rng = random.Random(seed)
shutil.rmtree(dst, ignore_errors=True)
os.makedirs(dst)
for name in ("manifest.json", "products.csv", "campaigns.csv"):
    shutil.copy(os.path.join(src, name), os.path.join(dst, name))


def load(folder):
    files = []
    for name in sorted(os.listdir(os.path.join(src, folder))):
        with open(os.path.join(src, folder, name), "rb") as f:
            lines = f.read().split(b"\n")
        if lines and lines[-1] == b"":
            lines.pop()
        files.append([name, lines])
    return files


def save(folder, files):
    os.makedirs(os.path.join(dst, folder))
    for name, lines in files:
        with open(os.path.join(dst, folder, name), "wb") as f:
            f.write(b"".join(line + b"\n" for line in lines))


def fields(line):
    return [f[1:-1] if len(f) >= 2 and f[:1] == b'"' and f[-1:] == b'"' else f for f in line.split(b",")]


def rows(files, width):
    """(file, line) of every data line with `width` fields and no stray quote."""
    out = []
    for fi, (_, lines) in enumerate(files):
        for li in range(1, len(lines)):
            parts = lines[li].split(b",")
            if len(parts) == width and lines[li].count(b'"') == 2 * sum(len(p) >= 2 and p[:1] == b'"' and p[-1:] == b'"' for p in parts):
                out.append((fi, li))
    return out


def get(files, at):
    return fields(files[at[0]][1][at[1]])


def put(files, at, parts):
    files[at[0]][1][at[1]] = b",".join(parts)


def add(files, parts, quote=False):
    target = rng.choice([f for f in files if len(f[1]) > 1])
    if quote:
        parts = [b'"' + p + b'"' if rng.random() < 0.5 else p for p in parts]
    target[1].insert(rng.randrange(1, len(target[1]) + 1), b",".join(parts))


def shift_f1(text, seconds):
    t = datetime.datetime.strptime(text.decode(), "%Y-%m-%dT%H:%M:%SZ") + datetime.timedelta(seconds=seconds)
    return t.strftime("%Y-%m-%dT%H:%M:%SZ").encode()


def base36(n, width):
    digits = b"0123456789abcdefghijklmnopqrstuvwxyz"
    out = bytearray()
    for _ in range(width):
        out.append(digits[n % 36])
        n //= 36
    return bytes(reversed(out))


events, clicks, links = load("events"), load("clicks"), load("links")
ev, ck, lk = rows(events, 7), rows(clicks, 4), rows(links, 2)
pick = lambda: rng.choice(ev)
counts = {}


def kind(name, times, action):
    done = 0
    for _ in range(times):
        try:
            action()
            done += 1
        except (ValueError, IndexError):
            pass
    counts[name] = done


def swap(files, pool, column):
    a, b = rng.choice(pool), rng.choice(pool)
    x, y = get(files, a), get(files, b)
    x[column], y[column] = y[column], x[column]
    put(files, a, x)
    put(files, b, y)


def copy_with(change, quote=False):
    row = get(events, pick())
    change(row)
    add(events, row, quote)


def earlier_with_other_user(row):
    row[1] = get(events, pick())[1]
    row[3] = shift_f1(row[3], -3600)


def later_with_other_time(row):
    row[2] = get(events, pick())[2]
    row[3] = shift_f1(row[3], 100)


def earlier_and_bad(row):
    row[3] = shift_f1(row[3], -7200)
    row[rng.choice([1, 2, 4, 5])] = rng.choice([b"", b"x"])


def respell(column, pool, files):
    at = rng.choice(pool)
    row = get(files, at)
    value = row[column]
    if len(value) < 2:
        raise ValueError
    row[column] = rng.choice([
        value.upper(), value[:1] + b"0" + value[1:], value + b"Z", b"x" + value[1:], value[:1] + value[1:] * 2,
        value[:1] + b"1", value[:1] + base36(rng.randrange(36 ** 8), 8), value[:1] + base36(rng.randrange(40_000), 4), b" " + value,
    ])
    put(files, at, row)


def retime():
    at = pick()
    row = get(events, at)
    row[2] = get(events, pick())[2]
    put(events, at, row)


kind("event ids swapped", per_kind, lambda: swap(events, ev, 0))
kind("event users swapped", per_kind, lambda: swap(events, ev, 1))
kind("earlier copy, other user", per_kind, lambda: copy_with(earlier_with_other_user))
kind("later copy, other time", per_kind, lambda: copy_with(later_with_other_time))
kind("earlier copy, bad row", per_kind, lambda: copy_with(earlier_and_bad))
kind("identical copy, other quotes", per_kind, lambda: copy_with(lambda row: None, quote=True))
kind("event id respelled", per_kind, lambda: respell(0, ev, events))
kind("event user respelled", per_kind, lambda: respell(1, ev, events))
kind("event ts from another row", per_kind, retime)

kind("click users swapped", per_kind, lambda: swap(clicks, ck, 1))
kind("click user respelled", per_kind, lambda: respell(1, ck, clicks))


def click_at_event():
    at = rng.choice(ck)
    row = get(clicks, at)
    row[3] = get(events, pick())[2]
    put(clicks, at, row)


twins = [0]


def click_twin():
    row = get(clicks, rng.choice(ck))
    # A new click_id each time: two clicks with one click_id and different values would break
    # the rules' promise that copies of a click are identical.
    twins[0] += 1
    tag = base36(twins[0], 4)
    row[0] = rng.choice([b"k0" + tag, b"K" + tag.upper(), b"k" + base36(36 ** 8 - twins[0], 8), b"k" + tag + b"a", b"a" + tag])
    row[2] = get(clicks, rng.choice(ck))[2]
    add(clicks, row, quote=True)


kind("click ts at an event's", per_kind, click_at_event)
kind("click twin at the same ts", per_kind, click_twin)


def link(a, b):
    add(links, [a, b], quote=True)


user = lambda: get(events, pick())[1]
kind("links merging two users", per_kind, lambda: link(user(), user()))
kind("links to new ids", per_kind, lambda: link(user(), rng.choice([b"zzzz%d" % rng.randrange(50), user().upper(), b"u1", b"U1", b"a"])))
kind("self links", per_kind // 4, lambda: (lambda u: link(u, u))(user()))


def unlink():
    at = rng.choice(lk)
    files_lines = links[at[0]][1]
    files_lines[at[1]] = rng.choice([b"", b",", files_lines[at[1]].split(b",")[0] + b","])


kind("links dropped", per_kind, unlink)

# The rules promise that copies of an event with equal ingest_ts are identical; which of two
# that differ is kept would be anyone's choice. Keep the promise: a row that shares
# (event_id, ingest_ts) with a different row gets a later ingest_ts of its own.
seen = {}
moved = 0
for fi, (_, lines) in enumerate(events):
    for li in range(1, len(lines)):
        parts = lines[li].split(b",")
        if len(parts) != 7 or lines[li].count(b'"') != 2 * sum(len(q) >= 2 and q[:1] == b'"' and q[-1:] == b'"' for q in parts):
            continue
        row = fields(lines[li])
        if not row[0]:
            continue
        while True:
            first = seen.setdefault((row[0], row[3]), row)
            if first == row:
                break
            try:
                row[3] = shift_f1(row[3], 1 + rng.randrange(50))
            except ValueError:
                row[3] = b"2000-01-01T00:00:%02dZ" % rng.randrange(60)
            lines[li] = b",".join(row)
            moved += 1
counts["ingest moved apart"] = moved

save("events", events)
save("clicks", clicks)
save("links", links)
print("mutated:", ", ".join(f"{name} {n}" for name, n in counts.items()))
