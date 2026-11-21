# The positional pipeline

On generated data the binary now runs a second pipeline, `src/positional.rs`, and keeps the earlier one ("classic": parse, dedup and person scatter, sessions) as the fallback for any dataset the new one does not recognise. This file says why it exists, how it works, why its output cannot differ from the classic pipeline's, what was measured, and what was tried and dropped. `decisions.md` has the one-line picks; the slots are `pipe-positional`, `pos-fused` (with `tok-shape` and `pos-wide` behind it) and `attr-interval`.

## Why

**The score is CPU work, not 8-core wall time.** The track page says a run gets "8 pinned vCPUs (hyperthreads)": four cores' worth. Our timings on the x86 box gave each worker a whole core, so they rewarded removing serial gaps and stalls. Two copies of a run at once, one on logical CPUs 0-7 and one on 8-15, put two workers on every core, and the classic pipeline went from 472 to 736 ms at 20M events, against 0.83 s on the scorer. `scripts/bench_smt.py` runs that comparison; judge a change by it and by the instruction count of one single-core run, not by `bench_docker.py` alone.

**The data carries a numbering.** The generator writes every event, user and click ID as a letter and the base 36 of `scramble(number, key)`: a fixed, reversible 40-bit mix with one key per ID kind and dataset (`generator/src/text.rs`). The numbers are not random:

- events are numbered `0..` person by person, each person's in time order;
- a user ID's number is `person << 9 | alias`.

So with the event key, an `event_id` is the row's index in a table that is deduplicated, grouped by person and sorted by time as soon as every row sits at its number. No hash table, no link lookup per row, no sort.

## How it runs

1. **Keys** (`Ingest::pos_plan`). From the first 5,000 rows of the largest event chunk. The most common user ID is a whale's, and the whales are persons 0 to 4, so its number is one of a few thousand candidates; the right key is the only one that gives the sampled IDs small person numbers. The rows of the lowest person have the lowest event numbers, so the event key is found by counting up from 0 for one of them. The search itself took about 0.2 ms at 20M. One sampled row in a hundred may fall outside. No keys found: the classic pipeline runs.
2. **Parse** (`ingest::pos`). One 16-byte `PosEvent` per event line into the list of its number's partition, a range of 2^12 to 2^16 numbers; a valid purchase's `pay` beside it; a click into the list of its person's range. The links are read beside the parse. A worker reads each chunk with one `pread` into a buffer it keeps (`Ingest::read`); the mapping is the fallback.
   - `pos-fused`, the pick (`pos::wide::fused`): no tokenizer and no fields. Per line, scalar code finds the separators (two 64-byte windows for an event line: the first three commas, then three commas and the newline; one window for a click line), checks nothing else, and copies what the decode needs: each window's separator and quote bits, and the raw bytes at the fields as words. That is 88 instructions a line. Per 8 rows the decode does the rest in registers: the quote rules (`pairs`: one subtraction shows every quote is one of a wrapping pair), where each field starts and how long it is, the fields' words with a wrapping quote shifted off (`cut`), the dates found among the table's calendar months (`Dates::date_wide`, no lookup), both IDs, `ts` in all three forms, `ingest_ts`, and for clicks the `click_id` packed (`types::id::wide::pack`). A row that is plainly ordinary becomes a record straight from the registers. A purchase keeps that record and has only its price and product read row by row. Every other row (dirty, odd, not of the usual shape) is tokenized again from its line's offset and goes through the row function `event_row`.
   - `pos-wide` and `tok-shape`, kept as `SS_POS_DECODE=wide`: the same kernels fed by a tokenizer (`Shape`: the two windows, as fields) and a collector that copies each field's words.
3. **Place** (`place`). Each partition writes its rows into a 16-byte slot per event number, keeping the copy with the smallest `ingest`; an invalid kept copy leaves no event. Then it lists the kept purchases and the runs: stretches of the table whose events are one person's, read 8 slots a step (a step whose slots all continue the open run is taken from the registers). Each click range is grouped by person with a counting scatter. The links are read by shape, each ID with its scrambled value (`Ingest::pos_links`), and become alias masks grown in one pass (`Links::build`): a person's mask starts with its first link and takes every link that touches it; what stays apart after a fixpoint is a second component, and that person goes to the classic lane.
4. **Sessions**, partition by partition: a run goes straight through `sessions::positional::person`: the gaps, the purchases, the credit and the counters of the classic code, with the assists counted per stretch of sessions (`attr-interval`) instead of once per session. A run that crosses partitions belongs to the one it starts in.

## Why the output is the classic pipeline's

Nothing rests on the generator keeping a promise, or on a key being right.

- **A number is only a name.** `types::scramble::canonical_at` accepts exactly the spellings the generator's `base36` writes, so two different IDs never share a number under any key. A wrong key makes the numbers large and scattered, which costs time.
- **A row outside the numbering** (an `event_id` spelled another way, a number past the table) is a classic record. Which way a row goes depends on its `event_id` alone, so every copy of an event meets its duplicates. The survivors join their person's events before the sessions are cut.
- **A person takes the fast lane only when the data read shows it is one**: a single run, in time order, every ID its rows use in one link component that holds no other person's IDs (`Links` in `src/positional.rs`), or no link at all and a single ID. Everyone else goes through `sessions::bucket`, the classic code, with the person the links give: people merged by a shared device, split by a lost link, with 64 or more IDs, or with a user ID that is not a number.
- **The wide decode only has to recognise an ordinary row**, never to be right about an unusual one. Each kernel is tested against the scalar function it replaces, on good and spoiled input.
- **The fused parse gives those kernels what the tokenizer would.** `pos::wide::fused::tests` builds lines as the generator writes them, spoils some (quotes, commas, newlines and other bytes replaced, inserted, removed), and checks lane by lane that the words, lengths and dates the decode derives are the ones `Scalar` and the row code read, and that the lines it takes are exactly the well-formed ones. The same for click lines, the vector ID packer (`types::id::tests::wide_packs_as_pack_at`) and the dates (`tests::check_ts`).

Checked by:

- the 36 fixtures and `dev/` against `dev-answer/` under `SS_PIPE=auto`, `classic` and `positional`, each also with `SS_POS_DECODE=wide`, `SS_POS_DECODE=row`, `SS_POS_TOK=mask` and `NCPU=3`. Forced on, the fixtures run entirely through the exception paths, since their IDs are not numbers;
- `scripts/pos_check.sh seeds`: 11 seeds from 1,000 to 5M events against the classic output, exact, under four settings; and the 20M and 60M sets;
- the same script with chunks of 32, 33 and 40 KB, so that thousands of lines cross a chunk's cut, under both readers; and with 1 KB chunks under `SS_PIPE=positional` (a sample that small finds no keys, so `auto` would run the classic pipeline);
- `scripts/pos_check.sh spoiled`: datasets spoiled by `scripts/mutate.py` (event IDs and users swapped between rows, earlier copies with another user or a bad row, times out of order, IDs respelled, people merged and split by links, clicks moved), up to 5,000 mutations of each kind in 300,000 events. With most persons thrown out of the fast lane the outputs still match.

`mutate.py` keeps two promises of the rules, because without them the answer is anyone's choice in both pipelines: copies of an event with equal `ingest_ts` are identical, and so are copies of a click.

## Knobs

| Variable | Values (the pick first) | What |
| --- | --- | --- |
| `SS_PIPE` | `auto`, `classic`, `positional` | `auto` runs the positional pipeline when the keys are found. `positional` runs it whatever the sample says, with zero keys if none are found: exact, slow, for tests |
| `SS_POS_DECODE` | `fused`, `wide`, `row` | `fused`: lines read by shape straight into the 8-row decode. `wide`: a tokenizer, then the 8-row decode. `row`: a tokenizer, then one row at a time. `fused` needs AVX-512 F, BW, DQ and CD and BMI2 at build time (`target-cpu=cascadelake`), `wide` the first three; a date table of more than 16 calendar months also falls back to `wide` |
| `SS_POS_TOK` | `shape`, `mask`, `scalar` | The tokenizer of `wide` and `row`. The classic pipeline keeps `SS_TOK`'s, so comparing the two pipelines also compares two tokenizers |
| `SS_POS_READER` | `pread`, `mmap` | How the positional parse gets a chunk's bytes: read into the worker's buffer, or through the mapping |
| `SS_POS_SHIFT` | 10 to 16 | Event numbers per partition as a power of two, for measurements; the default follows the row count |

`SS_TIMING=1` prints the stages (`parse`, `place`, `runs`, `sessions`, `classic`, `output`), the runs sent to the classic lane and why.

## Measured

x86 box (Ryzen 7 7800X3D, Docker Desktop on WSL2), `ss-p20`: seed 7, 20M events. Every container on this box has speculative store bypass disabled (`x86.md`).

| | Classic | Positional, `pos-wide` | Positional, `pos-fused` |
| --- | --- | --- | --- |
| One core (`NCPU=1`), instructions | 24.9 G | 16.4 G | 9.7 G |
| 8 cores, a core per worker, 20M | about 470 ms | 396 ms | 304 ms |
| Two workers per core, 20M | 747 ms | 578 ms | 477 ms |
| 8 cores, 60M (`ss-full`) | 2.70 s | 1.39 s | 0.84 s |

The process's own total (`SS_TIMING=1`), medians; the last two columns were measured together on 2026-11-21. Wall times on this box move by about 5% between sessions, the instruction count by a fraction of a percent: steer by the count, by `perf stat`'s task clock and cycles over several rounds, and by paired runs. 60M with two processes at once is not a fair number here (the VM's 15.6 GB starts reclaiming the page cache).

Stages with two workers per core at 20M: parse 267, place 82, runs 5, sessions 79, classic lane 6, output 24 ms. The image built from the tree, with the scorer's flags, against the same image of the tree before `pos-fused`: 860 ms against 952 ms of `docker run` wall time, of which about 350 ms is Docker's own on this box.

Where an event row's instructions are now, about 200 (it was 670 before `pos-wide` and 360 with it): the per-line loop 88, the decode 84 (640 per 8 rows), the scatter 25, and the rows that take the row code about 5 averaged over all. One core, 9.7 G in all: event lines 4.3 G, place 1.05 G, sessions 1.25 G with 0.6 G more in the day rows and the click sorts, click lines 0.8 G, links 0.4 G, output 0.4 G. At 20M about 1.4% of the events take the classic lane: 6,507 of 666,666 runs.

With 8 workers the kernel takes 19% of the cycles: 7% zeroing the fresh pages of the lists, the table and the output (about 1.2 GB), the rest copying the input and mapping pages.

## Tried and dropped

- **Profile-guided build.** An instrumented build trained on 2M generated events through both pipelines: 686 ms against 628 with two workers per core, and three times the image's build time.
- **Newlines first.** Finding the newlines in a pass of their own, then reading each line's two windows from its ends so that no load waits for another: 1,460 ms of parse on one core against 1,335. The tokenizer is bound by work, not by waiting.
- **Several lines in turn** on independent cursors: no gain, for the same reason.
- **Gathers** for the wide decode's words: 15.3 G instructions against 16.3 G for copying them row by row, but slower on this AMD core, and gathers are slowed on Intel by the Downfall mitigation. The compressing store (`vpcompressq` to memory) is microcoded on Zen 4: compress in a register and store whole.
- **AVX-512 for the three masks alone**: the masks of all the event files take 55 ms on one core with AVX2 and 36 ms with AVX-512; the cost is in turning them into fields.
- **Sending a whole person to the classic lane for one odd row.** About 110 rows at 20M have an `event_id` in another case, and most of the whales owned one: a million events in the classic lane.
- **Both windows checked in one block.** The compiler paired the two windows' bit operations in vector registers, 40 instructions of moves a line: each window now leaves by its own branch (`tok::fused::event_shape`).
- **All of a row's fields cut before the kernels run.** Twenty live vectors and the kernels' own did not fit the registers: 60 of the decode's 640 instructions a group were moves to and from the stack. Each kernel now asks for its field's words when it runs (`Fields`).
- **Handing a parsed chunk's pages back while the others parse** (`free-early` on the input mapping): each `madvise` flushes the TLB of every other worker's CPU. The parse took 179 ms with it and 159 without (8 workers, 20M), but without it the process spent 60 ms unmapping at exit. Reading the chunk with `pread` has neither cost: about 3% less CPU time than the mapping.
- **Keeping every input file open for those reads.** A few hundred open files grow the descriptor table while the workers run, and each growth waits out an RCU grace period: 40 ms each, 130 ms in the inputs stage. A worker keeps one file open, the one it read last: chunks come file by file, so at 60M that is 630 opens for 5,432 reads.
- **Other partition sizes** (`SS_POS_SHIFT` 12 to 16) and **other chunk sizes** (128 KB to 1 MB): no difference beyond the noise with one process.
- **The whole row code for a purchase**: 600 instructions each for 3% of the rows, 4% of the run. The decode's record is right for a purchase but for the price and the product.
- **A counting sort of the links and a union-find per person**: replaced by the alias masks, 0.2 G instructions less, the same classification.
- **A row's raw bytes copied as vectors** (16 or 32 bytes a field, a row's together) and taken apart 8 rows at a time in the decode, instead of a word at a time into one array per word: 9.69 G to 9.67 G instructions for 80 lines more. Not kept.

## Not done yet

- The place and sessions stages are scalar passes over a table of all event numbers (`times` and `users`, 8 bytes a number, zeroed by the kernel and written once). Doing a partition's sessions while its slots are still in cache would save the table and one pass, but a person's second run elsewhere could then only be answered by starting over with the classic pipeline.
- The decode still reloads about 60 constants and spills about 40 vectors per 8 rows.
- `sessions` sorts every person's clicks (0.4 G instructions at 20M); the attribution of persons with hundreds of clicks is about 0.4 G more.
- `user_daily` writes `sessions` and `events` as 64-bit integers and every `user_id` as a plain string; the rules allow any width and encoding.
- The Parquet writer is arrow-rs.
- `pos-wide` scored 0.73 s against the classic pipeline's 0.83 s; `pos-fused` has not run on the scorer. Its CPU is unknown beyond "Intel Xeon with AVX-512". The ratio of the scorer's time to the paired time here was the same for the classic pipeline (no AVX-512) and for `pos-wide`, so the wide multiplies do not seem to cost it much clock speed.
