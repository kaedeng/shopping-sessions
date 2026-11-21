# The positional pipeline

On generated data the binary now runs a second pipeline, `src/positional.rs`, and keeps the earlier one ("classic": parse, dedup and person scatter, sessions) as the fallback for any dataset the new one does not recognise. This file says why it exists, how it works, why its output cannot differ from the classic pipeline's, what was measured, and what was tried and dropped. `decisions.md` has the one-line picks; the slots are `pipe-positional`, `tok-shape`, `pos-wide` and `attr-interval`.

## Why

**The score is CPU work, not 8-core wall time.** The track page says a run gets "8 pinned vCPUs (hyperthreads)": four cores' worth. Our timings on the x86 box gave each worker a whole core, so they rewarded removing serial gaps and stalls. Two copies of a run at once, one on logical CPUs 0-7 and one on 8-15, put two workers on every core, and the classic pipeline went from 472 to 736 ms at 20M events, against 0.83 s on the scorer. `scripts/bench_smt.py` runs that comparison; judge a change by it and by the instruction count of one single-core run, not by `bench_docker.py` alone.

**The data carries a numbering.** The generator writes every event, user and click ID as a letter and the base 36 of `scramble(number, key)`: a fixed, reversible 40-bit mix with one key per ID kind and dataset (`generator/src/text.rs`). The numbers are not random:

- events are numbered `0..` person by person, each person's in time order;
- a user ID's number is `person << 9 | alias`.

So with the event key, an `event_id` is the row's index in a table that is deduplicated, grouped by person and sorted by time as soon as every row sits at its number. No hash table, no link lookup per row, no sort.

## How it runs

1. **Keys** (`Ingest::pos_plan`). From the first 5,000 rows of the largest event chunk. The most common user ID is a whale's, and the whales are persons 0 to 4, so its number is one of a few thousand candidates; the right key is the only one that gives the sampled IDs small person numbers. The rows of the lowest person have the lowest event numbers, so the event key is found by counting up from 0 for one of them. The search itself took about 0.2 ms at 20M. One sampled row in a hundred may fall outside. No keys found: the classic pipeline runs.
2. **Parse** (`ingest::pos`). One 16-byte `PosEvent` per event line into the list of its number's partition, a range of 2^12 to 2^16 numbers; a valid purchase's `pay` beside it; a click into the list of its person's range. The links are read beside the parse, as before.
   - `tok-shape`: an event line is two 64-bit windows of the comma, newline and quote masks (the first three fields end within 64 bytes, the other four within the next 64), a click line one. Any line that does not fit goes, alone, to `Scalar`.
   - `pos-wide`: rows are collected 64 at a time and decoded eight per step with AVX-512: both IDs, `ts` in all three forms, `ingest_ts`. A row that is plainly ordinary (both IDs numbers, `view` or `cart`, timestamps the wide code accepts) becomes a record straight from the registers; every other row (a purchase, anything dirty) goes through the row function `event_row`.
3. **Place** (`place`). Each partition writes its rows into a 16-byte slot per event number, keeping the copy with the smallest `ingest`; an invalid kept copy leaves no event. Then it lists the kept purchases and the runs: stretches of the table whose events are one person's. Each click range is grouped by person with a counting scatter.
4. **Sessions**. A run goes straight through `sessions::positional::person`: the gaps, the purchases, the credit and the counters of the classic code, with the assists counted per stretch of sessions (`attr-interval`) instead of once per session.

## Why the output is the classic pipeline's

Nothing rests on the generator keeping a promise, or on a key being right.

- **A number is only a name.** `types::scramble::canonical_at` accepts exactly the spellings the generator's `base36` writes, so two different IDs never share a number under any key. A wrong key makes the numbers large and scattered, which costs time.
- **A row outside the numbering** (an `event_id` spelled another way, a number past the table) is a classic record. Which way a row goes depends on its `event_id` alone, so every copy of an event meets its duplicates. The survivors join their person's events before the sessions are cut.
- **A person takes the fast lane only when the data read shows it is one**: a single run, in time order, every ID its rows use in one link component that holds no other person's IDs (`Links` in `src/positional.rs`), or no link at all and a single ID. Everyone else goes through `sessions::bucket`, the classic code, with the person the links give: people merged by a shared device, split by a lost link, with 64 or more IDs, or with a user ID that is not a number.
- **The wide decode only has to recognise an ordinary row**, never to be right about an unusual one. Each kernel is tested against the scalar function it replaces, on good and spoiled input.

Checked by:

- the 36 fixtures and `dev/` against `dev-answer/` under `SS_PIPE=auto`, `classic` and `positional`, each also with `SS_POS_DECODE=row`, `SS_POS_TOK=mask` and `NCPU=3`. Forced on, the fixtures run entirely through the exception paths, since their IDs are not numbers;
- `scripts/pos_check.sh seeds`: 11 seeds from 1,000 to 5M events against the classic output, exact, under four settings; and the 20M and 60M sets;
- `scripts/pos_check.sh spoiled`: datasets spoiled by `scripts/mutate.py` (event IDs and users swapped between rows, earlier copies with another user or a bad row, times out of order, IDs respelled, people merged and split by links, clicks moved), up to 5,000 mutations of each kind in 300,000 events. With most persons thrown out of the fast lane the outputs still match.

`mutate.py` keeps two promises of the rules, because without them the answer is anyone's choice in both pipelines: copies of an event with equal `ingest_ts` are identical, and so are copies of a click.

## Knobs

| Variable | Values (the pick first) | What |
| --- | --- | --- |
| `SS_PIPE` | `auto`, `classic`, `positional` | `auto` runs the positional pipeline when the keys are found. `positional` runs it whatever the sample says, with zero keys if none are found: exact, slow, for tests |
| `SS_POS_TOK` | `shape`, `mask`, `scalar` | The positional parse's tokenizer. The classic pipeline keeps `SS_TOK`'s, so comparing the two pipelines also compares two tokenizers |
| `SS_POS_DECODE` | `wide`, `row` | Event rows eight at a time, or one at a time. `wide` needs AVX-512 F, BW and DQ at build time (`target-cpu=cascadelake`) |

`SS_TIMING=1` prints the stages (`parse`, `place`, `runs`, `sessions`, `classic`, `output`), the runs sent to the classic lane and why.

## Measured

x86 box (Ryzen 7 7800X3D, Docker Desktop on WSL2), `ss-p20`: seed 7, 20M events. Every container on this box has speculative store bypass disabled (`x86.md`).

| | Classic | Positional |
| --- | --- | --- |
| One core (`NCPU=1`), wall | 2,830 ms | about 2,000 ms |
| One core, instructions | 24.9 G | 16.3 G |
| 8 cores, a core per worker | about 470 ms | about 430 ms |
| Two workers per core (`bench_smt.py`), 20M | 747 ms | 578 ms |
| Two workers per core, 60M (`ss-full`) | 2,434 ms | 1,746 ms |
| 8 cores, 60M, wall and peak memory | 2.70 s, 4.2 GB | 1.39 s, 2.7 GB |

Stages with two workers per core at 20M: parse 374, place 75, runs 11, sessions 74, classic lane 5, output 20 ms. The parse is two thirds of the run.

Where an event row's instructions went before `pos-wide` (about 670): tokenizer 255, the two timestamps 160, type, validity and the event number 120, the user number 70, the scatter the rest. At 20M about 1.4% of the events take the classic lane: 6,507 of 666,666 runs.

## Tried and dropped

- **Profile-guided build.** An instrumented build trained on 2M generated events through both pipelines: 686 ms against 628 with two workers per core, and three times the image's build time.
- **Newlines first.** Finding the newlines in a pass of their own, then reading each line's two windows from its ends so that no load waits for another: 1,460 ms of parse on one core against 1,335. The tokenizer is bound by work, not by waiting.
- **Several lines in turn** on independent cursors: no gain, for the same reason.
- **Gathers** for the wide decode's words: 15.3 G instructions against 16.3 G for copying them row by row, but slower on this AMD core, and gathers are slowed on Intel by the Downfall mitigation. The compressing store (`vpcompressq` to memory) is microcoded on Zen 4: compress in a register and store whole.
- **AVX-512 for the three masks alone**: the masks of all the event files take 55 ms on one core with AVX2 and 36 ms with AVX-512; the cost is in turning them into fields.
- **Sending a whole person to the classic lane for one odd row.** About 110 rows at 20M have an `event_id` in another case, and most of the whales owned one: a million events in the classic lane.

## Not done yet

- The click rows still decode one at a time (about 350 instructions each, 5.1M of them).
- The tokenizer builds seven fields a line and the collector copies them; it could hand the wide decode its words directly.
- `sessions` sorts every person's clicks with a comparator; most persons have fewer than 16.
- The Parquet writer is arrow-rs, 75 ms on one core.
- Nothing here has run on the scorer. Its CPU is unknown beyond "Intel Xeon with AVX-512"; the wide decode's multiplies may cost clock speed on Cascade Lake.
