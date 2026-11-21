# The positional pipeline

On generated data the binary now runs a second pipeline, `src/positional.rs`, and keeps the earlier one ("classic": parse, dedup and person scatter, sessions) as the fallback for any dataset the new one does not recognise. This file says why it exists, how it works, why its output cannot differ from the classic pipeline's, what was measured, and what was tried and dropped. `decisions.md` has the one-line picks; the slots are `pipe-positional`, `pos-fused` (with `tok-shape` and `pos-wide` behind it), `attr-interval` and `out-direct`.

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
3. **Clicks and links.** Each click range (the persons `r << click_shift ..`) is grouped by person with a counting scatter into its stretch of one array that holds every click, with one end position per person (`Grouped`, `group_clicks`). Persons are numbered largest first, so the first range holds a fifth of all the clicks: a range with more than a 16th of them is grouped by all workers together, each counting and scattering its own list to the positions the serial order would give (`group_clicks_wide`). The links are read by shape, each ID with its scrambled value (`Ingest::pos_links`), and become alias masks grown in one pass (`Links::build`): a person's mask starts with its first link and takes every link that touches it; what stays apart after a fixpoint is a second component, and that person goes to the classic lane.
4. **Place and sessions, one task per partition** (`fused_stages`, `Fused::partition`). The task writes the partition's rows into a 16-byte slot per event number in the worker's own 256 KB of slots, keeping the copy with the smallest `ingest` (`place_rows`; an invalid kept copy leaves no event, and the kept purchases are listed in slot order through a bit per slot). It reads the slots 4 a step into two columns and the runs, the stretches whose events are one person's (`scan`). Then, while the slots are in cache, every run but the partition's first and last goes straight through `sessions::positional::person`: the gaps, the purchases, the credit and the counters of the classic code, with the assists counted per stretch of sessions (`attr-interval`). There is no table of all event numbers.
   - A run that crosses into the next partition is its partition's last and the next one's first: those two runs of every partition are set aside with their events (`Edge`) and joined afterwards, a task per person (`Fused::joined`; the whales are all of this kind).
   - A person whose events are two stretches with other persons' between is found when its second run is taken (a byte per person, set atomically). Its first run is in a worker's counters by then, so the fused stages return with nothing kept and the three stages they replaced run from the same lists (`split_stages`: one table of all event numbers, the runs joined and judged on one thread, then the sessions). On generated data that never happens; the spoiled datasets of `pos_check.sh` all take that path.
5. **Classic lane, then output.** The rows of everyone not in the fast lane go by person hash into 64 buckets, each worker filling its own, and through `sessions::bucket_stretch`. Each worker writes its `user_daily` rows as one Parquet file by hand (`output::direct`): the rows are already columns, so the file is the page and column headers and the footer around them, with `user_id` written once per run of equal ids (a dictionary page and RLE runs) and the integer columns copied as they lie. `campaign_daily` (28K rows) stays with arrow-rs, summed and written by a thread of its own meanwhile.

The workers are threads kept for the whole run (`types::par`, the pool), started once the inputs are mapped. Nothing in a parallel stage allocates from the system, spawns a thread or unmaps: see "What cost time and was not work" below.

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
| `SS_POS_STAGES` | `fused`, `split` | A partition placed and its sessions cut in one task, or the table placed whole and then read (also what `fused` falls back to) |
| `SS_POS_CLICK_WIDE` | a number of clicks, default 65,536 | A click range is grouped by all workers together when it holds more than this and more than a 16th of the clicks; 1 sends every range that way (tests) |
| `SS_OUT` | `direct`, `arrow` | Who writes `user_daily`: the writer in `output::direct`, or arrow-rs |
| `SS_PAR` | `pool`, `spawn` | Workers kept for the run, or seven threads spawned for every stage |

`SS_TIMING=1` prints the stages (`inputs`, `parse`, `clicks`, `sessions`, `edges`, `classic`, `output`; `place`, `runs` and `sessions` under `split`), the runs sent to the classic lane and why.

## Measured

x86 box (Ryzen 7 7800X3D, Docker Desktop on WSL2), `ss-p20`: seed 7, 20M events. Every container on this box has speculative store bypass disabled (`x86.md`).

| | Classic | Positional, `pos-wide` | Positional, `pos-fused` |
| --- | --- | --- | --- |
| One core (`NCPU=1`), instructions | 24.9 G | 16.4 G | 9.7 G |
| 8 cores, a core per worker, 20M | about 470 ms | 396 ms | 304 ms |
| Two workers per core, 20M (`bench_smt.py`) | 747 ms | 623 ms | 528 ms |
| 8 cores, 60M (`ss-full`) | 2.70 s | 1.39 s | 0.84 s |

The process's own total (`SS_TIMING=1`), medians; the last two columns were measured together on 2026-11-21. Wall times on this box move by about 5% between sessions, the instruction count by a fraction of a percent: steer by the count, by `perf stat`'s task clock and cycles over several rounds, and by paired runs. 60M with two processes at once is not a fair number here (the VM's 15.6 GB starts reclaiming the page cache).

Stages with two workers per core at 20M: parse 301, place 89, runs 6, sessions 74, classic lane 6, output 25 ms (before `pos-fused`: 411, 79, 10, 74, 6, 23). The image built from the tree, with the scorer's flags, against the same image of the tree before `pos-fused`: 935 ms against 1,024 ms of `docker run` wall time, of which roughly 400 ms is Docker's own on this box with two containers starting at once.

The instructions fell by 41% and the time with a core per worker by 23%, but the time with two workers per core only by 15%: the fused parse takes almost exactly twice as long there (158 ms to 301), where the tokenizer path took 1.6 times as long. A thread that keeps the core's vector units busy leaves its sibling little. Which of the two the scorer resembles is not known: its two scores so far fit both (1.8 times the time with a core per worker, or 1.15 times the time with two per core).

Where an event row's instructions are now, about 200 (it was 670 before `pos-wide` and 360 with it): the per-line loop 88, the decode 84 (640 per 8 rows), the scatter 25, and the rows that take the row code about 5 averaged over all. One core, 9.7 G in all: event lines 4.3 G, place 1.05 G, sessions 1.25 G with 0.6 G more in the day rows and the click sorts, click lines 0.8 G, links 0.4 G, output 0.4 G. At 20M about 1.4% of the events take the classic lane: 6,507 of 666,666 runs.

With 8 workers the kernel takes 19% of the cycles: 7% zeroing the fresh pages of the lists, the table and the output (about 1.2 GB), the rest copying the input and mapping pages.

### After the stages were rebuilt (2026-11-21, later the same day)

| 20M, 8 workers | Before (`ca1cf70`) | Now |
| --- | --- | --- |
| One core, instructions | 9.7 G | 8.9 G |
| Whole process, a core per worker (median of 40) | about 300 ms | 247 ms |
| Whole process, a second copy looping on the sibling threads (median of 40) | | 341 ms |

Stages with a core per worker: inputs 6.7, parse 150, clicks 13.9, sessions 65.9, edges 6.7, classic lane 3.3, output 8.4 ms. At 60M the click stage went from 104-140 ms to 36.

**What cost time and was not work.** Most of the gain above did not come from fewer instructions:

- *Allocating inside a parallel stage.* Every click range's task allocated its own arrays. glibc grows a thread's heap with `mprotect`, which takes the address-space lock for writing, while the other tasks take page faults on fresh memory, which hold it for reading through the zeroing of a 2 MB page. A click cost 50 ns with 8 workers against 12 ns on one, and the stage took 28 ms for 66 ms of work. One array allocated before the tasks: 13 ms.
- *Spawning threads per stage.* A spawn is an `mmap`, an `mprotect` and a `clone`, the same lock for writing. The later workers of a stage started 2 to 5 ms after the first with two copies running. Kept for the run, the workers are 2% of the whole process alone and about 5% with a second copy (352 against 373 ms). Started before the inputs were mapped they held up `open`'s few hundred mappings instead: 15 ms with a second copy.
- *Leaving memory to the exit.* With `SS_FREE=exit` the process ended 100 ms after its last output with two copies running: one thread unmaps what eight could have released.
- *Serial ends.* One thread filling the classic lane's buckets (2 ms), `campaign_daily` after the `user_daily` files (2 ms), the `user_daily` columns doubling a row at a time on the workers' threads (some 800 reallocations).

`perf stat -e 'syscalls:sys_enter_mprotect,syscalls:sys_enter_mmap,syscalls:sys_enter_munmap,syscalls:sys_enter_clone3'` on a run (tracefs mounted in a privileged container) shows the first two at a glance: 107 `mprotect` and 85 `clone3` before, 32 and 9 after, almost all of them before the parse.

**How these were measured.** Single runs of a process move by 15% here, and two fresh containers started together by more. What separates 2%: one container that runs the two variants in alternation, 40 times each, each run timed whole from outside the process, with the same binary looping in a second container on the sibling threads, and the quartiles compared (A against A is within 3% that way, within 1% without the second copy). `perf stat`'s own duration is wrong for a process that spawns many short-lived threads: it waits for their counters. A run that writes over an earlier run's files in a tmpfs pays for truncating them.

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
- **Other partition sizes** (`SS_POS_SHIFT` 12 to 16) and **other chunk sizes** (128 KB to 1 MB): no difference beyond the noise, with one process or two.
- **The mapping against `pread` with two workers per core.** With each measured in the state its own runs leave, the mapping released per chunk, the mapping released per file and `pread` are within 2% (493, 503 and 498 ms). With a fixed order of variants the mapping once looked 12% faster and once 8% slower: see `bench_smt.py`.
- **Fewer workers than CPUs**, on the thought that a second thread adds nothing to a core whose first is in the decode: 8 workers a copy 515 ms, 6 the same, 5 slower, 4 572 ms.
- **The whole row code for a purchase**: 600 instructions each for 3% of the rows, 4% of the run. The decode's record is right for a purchase but for the price and the product.
- **A counting sort of the links and a union-find per person**: replaced by the alias masks, 0.2 G instructions less, the same classification.
- **A row's raw bytes copied as vectors** (16 or 32 bytes a field, a row's together) and taken apart 8 rows at a time in the decode, instead of a word at a time into one array per word: 9.69 G to 9.67 G instructions for 80 lines more. Not kept.
- **A whole click range put in `ts` order by radix while it is grouped** (a counting scatter per 11-bit digit of the `ts`, then the scatter by person, so that no person's clicks need sorting later): the same output and the same instruction count, but 68 ms of click stage against 33 with two copies running, for 11 ms less in the sessions. A person's few clicks sort inside L1; 2,048 write positions do not fit it.
- **The mapping with a prefetch 4 KB ahead of the tokenizer**: better than the mapping without (363 against 376 ms with a second copy), still behind `pread` (345 ms).
- **The links' persons numbered a batch ahead and their entries prefetched** in `Links::build`: no change on this box, whose 96 MB cache holds those arrays.
- **Sizes of chunk and of the scatter's staging** again, with the steadier harness: 256 KB and 512 KB chunks and 1 or 2 staged lines are all within its noise of the defaults.

## Not done yet

- The click lists of the parse are sized alike, and persons are numbered largest first: about 30% of the clicks overflow their list's room into a `Vec` each (`Parts::spill`, 0.8% of the cycles, and those vectors grow on the workers' threads). The event lists are sized for an uneven share of the chunks, 450 MB for 345 MB of rows, and with 2 MB pages the kernel zeroes all of it. Lists made of fixed chunks from one reserved block per worker would fix both.
- With 8 workers the kernel still takes about a fifth of the cycles: 7% copying the input, 6% zeroing fresh pages (about 1.1 GB).
- The decode still reloads about 60 constants and spills about 40 vectors per 8 rows.
- `sessions` sorts every person's clicks by comparing them (2.7% of the cycles with two copies running); a person's sessions are cut one slot at a time; `Day::flush` is 90 instructions a `user_daily` row.
- Three persons at 20M use 64 or more user IDs, which the alias masks cannot hold, and take the classic lane with about 30,000 events each.
- `user_daily` writes `sessions`, `events` and `revenue_cents` as plain 64-bit integers; the rules allow any width.
- `pos-wide` scored 0.73 s against the classic pipeline's 0.83 s; `pos-fused` has not run on the scorer. Its CPU is unknown beyond "Intel Xeon with AVX-512". On Skylake and Cascade Lake Xeons a core that runs 512-bit instructions lowers its clock (more for multiplies), and its sibling thread with it. The ratio of the scorer's time to the paired time here was 1.12 for the classic pipeline (no 512-bit code) and 1.15 to 1.19 for `pos-wide`: a small penalty at most, within what these measurements can tell. So the parse keeps its 512-bit decode, which is far cheaper than any 256-bit version of it, and nothing else uses a 512-bit register: the place stage's scan and its fill are written for 256-bit ones (the compiler made 512-bit code of both until told otherwise; `objdump` shows none in the place and sessions stages now). If the scorer does lower the clock, the next thing to try is the decode's 27 multiplies per 8 rows in two 256-bit halves each, which is the lighter of the two clock levels for about 2% more instructions.
