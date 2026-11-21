# Alternatives

Every option considered for each slot, so each can sit behind its own Cargo feature and be benchmarked. `decisions.md` holds the current pick; this file holds the field.

Times are wall time at full size (60–62M rows), 8 threads, on an Apple-silicon Mac. A result that starts with "built" comes from the real crates (their `examples/` benchmarks, or the whole binary; `implementation.md` has the detail); the others come from the standalone crates in `prototypes/`. Nothing here has run on amd64. "Reported" numbers come from papers or other people's benchmarks, not ours; their sources are in the research doc, [Shopping Sessions: Fast-Algorithm Sources](https://claude.ai/artifact/QFQSeVY55mCaNksMb3YjQ1). The 60–62M are event rows; the generator calls 75M rows its full size (comment at `generator/src/plan.rs:134`).

Status:

- **pick**: the current choice in `decisions.md`
- **slower**: measured here and lost
- **faster in part**: measured faster on one piece, not end to end
- **untested**: no number of ours
- **rejected**: ruled out without a benchmark

Features only add code, so several alternatives for one slot can be compiled in together. When a slot has more than one, an env var picks, and the pick is the default.

## What this Mac hides

Three costs are small here and will not be on the scorer, a Linux Xeon. The figures are estimates unless they say measured, and the cache and TLB sizes assume Cascade Lake, which the README's `target-cpu` suggests. The measured ones come from the built binary in a Linux arm64 container on this Mac (4 KB pages, scorer flags, data on a Docker volume) at half size, 30M events and 7.5M clicks; that VM has 12.6 GB, too little for the full set.

- **Fresh memory.** Linux hands out 4 KB pages, this Mac 16 KB. The dedup scatter alone writes 2 GB of new memory: about 500,000 page faults, each with a zeroing, so perhaps 0.5 to 1 µs each. Measured: the dedup and person-scatter stage, which writes the most fresh memory per row, takes 91–102 ms in the container against 64 ms natively; the parse takes 162–165 ms against 184 ms, and the whole run 0.35–0.37 s on both. On the Mac, fresh output buffers add 70 ms and a fresh mapping 61 ms to a 238 ms warm parse at full size.
- **Cache and TLB reach.** This Mac has 16 MB of L2 per four cores. Cascade Lake has 1 MB per core and 1,536 second-level TLB entries, which cover 6 MB in 4 KB pages. A sweep that showed no difference here (partition count, bucket count) has to be run again there, and the 128 MB link table misses the TLB on every lookup. The partition count pulls two ways there: more than about 1,024 write heads per worker overflow the TLB, while at 1,024 partitions each dedup table (about 1.5 to 2 MB) already overflows L2. `scatter-wc` is what lets the count rise without the first cost.
- **Exit.** The kernel unmaps everything on one thread before the process is gone, and `fast-exit` does not skip that. 1BRC entries measured 40 to 180 ms for a 13 GB mapping. Measured: the process ends about 24 ms after its last output at half size in the container (about 5 GB to unmap), about 7% of that run.

Test these first on amd64, in this order: `hugepages` for the link table, `scatter-wc`, `lookup-filter`, then the partition and bucket sweeps.

## Reader

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `mmap` each file | `reader-mmap` | pick | built: a fresh mapping adds 61 ms to the full-size parse over an already faulted one (Mac, 16 KB pages) |
| `pread` into a reused per-worker buffer | `reader-pread` | untested | reported about 5% faster on a warm cache in one Linux run; nothing to unmap at exit |
| Ask the kernel to read every file ahead at start | `readahead` | untested | only matters if the scorer's cache is cold |
| `MADV_SEQUENTIAL` on each mapping | `reader-seq` | untested | Crotty, Leis and Pavlo (CIDR 2022): mmap reads ahead less than `read()`, and this advice suits a one-pass scan |
| Fault in each chunk's pages with one `MADV_POPULATE_READ` before parsing it (Linux 5.14+) | `reader-populate` | measured | built, picked with `SS_READER=populate`: no gain (Linux arm64 container on the Mac, half size, median wall of 6 to 8 runs: 337 ms against 336 with the glibc tunables, 393 against 389 without). One call per 1 MB chunk instead of a fault per 64 KB of fault-around, made by each worker for its own chunks; `MAP_POPULATE` would fault the whole file on one thread at map time |
| Chunk size 256 KB, 1 MB, 4 MB | env var `SS_CHUNK_KB` | measured | 1 MB is the pick. In the Linux container at half size: 256 KB 338 ms against 336 for 1 MB (its serial chunk planning takes 13 ms against 7); 4 MB with `reader-populate` 337. Reported best is 0.25–1.0 × L3 per thread |

## Tokenizer and row parse

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Mask tokenizer (AVX2 on amd64, NEON on arm64), two passes per 8 KB batch: block masks to comma and newline positions, then one row per newline | `tok-mask` | pick | built: the whole parse (events, clicks, links, open included) 0.335 s, 17.0 GB/s, 2.13 GB/s per worker; with pages already faulted and buffers reused 25.4 GB/s, 3.37 GB/s on one core; the tokenizer alone 7.6 GB/s per core. A kept mask is reported at 8.0 GB/s on dense matches, AVX-512 no faster than AVX2 |
| Delimiter index for the whole chunk, then rows at a fixed stride of 7 | `tok-index` | untested | partly taken: `tok-mask` as built indexes positions per 8 KB batch, so rows are already independent work; the whole-chunk index and the fixed stride are not built |
| Batch size 8 / 16 / 32 / 64 KB in `tok-mask` | constant `BATCH` | measured | 8 and 16 KB tie, 32 KB is 1.5% slower, 64 KB 3% slower (one core); 8 KB is the pick for the Xeon's smaller L1 |
| NEON bitmask by `vld4q` and a `vsri` chain instead of AND-with-weights and pairwise adds | in `tok-mask` | pick | 1.7% faster overall |
| AVX-512BW: 64-byte compares straight into mask registers | `tok-avx512` | untested | intrinsics and target features stable since Rust 1.89; light integer ops rarely lower the clock on Skylake-era Xeons (Lemire), and Sep 0.10's notes count the cost of moving between mask and vector registers. simdjson keeps its AVX2 kernel on Cascade Lake, because its AVX-512 kernel needs Ice Lake's VBMI2 |
| `memchr` per field | `tok-memchr` | untested | reported 1.06 GB/s on dense matches |
| Scalar byte loop | `tok-scalar` | slower | built: 0.698 s, 8.2 GB/s, 1.02 GB/s per worker; the baseline, the fallback on other CPUs and the tests' oracle (`SS_TOK=scalar`) |
| Two to four cursors per worker in one loop, each on its own piece of the chunk | `tok-lanes` | untested | the core overlaps the rows; one 1BRC entry went from 1.34 to 1.23 s with it and the winner used three. Only for the one-pass tokenizer: `tok-index` already makes rows independent |
| Strip quotes with arithmetic and test for a malformed row once, not per field | in `tok-mask` | pick | built into `tok-mask`, no feature of its own: a line is well-formed when its quotes are exactly twice its wrapped fields. 20% of fields are quoted at random, so a branch per field mispredicts about 1.4 times a row |
| Speculative chunk parsing (Ge et al.; ParPaRaw) | none | rejected | built for chunks that may start inside a quoted newline; no value holds a newline here, so every `\n` is a safe cut |

Row validity comes from the same masks, whichever tokenizer: a line needs exactly 6 commas (events), 3 (clicks) or 1 (links), and a field may hold quotes only as its first and last bytes, at least 2 bytes long, with none inside, so count the quotes that sit at both ends of a field and compare with the popcount of the quote mask over the line. `\r` is an ordinary byte (fixture E20). The overall shape follows Instant Loading (Mühlbauer et al., PVLDB 2013): parallel chunks, rows written straight into typed records, no intermediate strings.

## Timestamps

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Date-string lookup table; a miss makes the `ts` bad, which drops a click but only sets an event's invalid bit | `ts-table` | pick | built: 4.2 ns per row for `ts` and 1.9 for `ingest_ts` on one core, of about 22 ns per event row in all |
| General calendar conversion with validation (Neri and Schneider, SP&E 2023; Hinnant) | `ts-calendar` | untested | reported 1.5–4 ns per conversion, so about 20 ms in total; February 29 then needs a leap-year test (Hüffner: three instructions) |
| Convert as F1 and as F3 both and pick by content (byte 10 is `T`, else all digits), F2 behind a branch | `ts-select` | slower | measured inside `ts-table` before it was dropped, no feature built: 10% slower than branches on the M4, and the fully branch-free form 29.4 ns per row against 25.9. Untested on the Xeon, where a mispredict costs more. The form is random per row (58/10/32%). Not by length: an F3 with leading zeros can be 20 or 25 bytes |
| Validate a whole F1 or F2 string in one SIMD compare against per-position bounds (Lemire, 2023) | `ts-simd` | untested | replaces the per-field digit and range checks; the date table still settles the date |
| `atoi_simd` crate for digit runs | none | rejected | accepts a sign, so the digits-only rule needs its own check anyway, and field lengths are already known |

F3 skips leading zeros, compares its digit count with the row's own window bounds (events: `window_start` and `window_end`; clicks: `window_start` − 7 days and `window_end`), then parses 8 + 5 digits with SWAR (Lemire). The table settles only the date: hour 24, second 60, a lowercase `t` or `z`, a space for `T`, no zone and fractional seconds are all generated, and bad offsets (`+24:00`, `+05:60`, `+0200`) are in fixture E17, so those fields need their own checks.

## IDs

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `u64`, 7 bits per byte, up to 9 ASCII bytes, slow path beyond | `id-u64` | pick | built (`pext` on amd64, three shift-and-mask steps elsewhere); not timed apart from the parse. Every generated ID is a letter plus base36 of a 40-bit value, at most 9 bytes (`generator/src/text.rs`), so the slow path never runs on generated data. An ID holding byte `0x00` must take it too, since that byte packs like padding |
| 16 bytes inline, up to 15 bytes | `id-u128` | untested | doubles the key width; see record width |
| A dense `u32` per ID from one global intern table | `id-dense` | untested | the research doc's layout; 62M lookups into a 4.3M-ID table before any partitioning, which the packed `u64` avoids |
| 64-bit hash as the identity | none | rejected | collides about once in 10,000 runs at 60M keys |

## Record width in the dedup scatter

Scatter only, 62M records into 1,024 partitions (`prototypes/lookupbench`).

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| 32-byte record | `rec-32` | pick | 0.061 s |
| 24-byte record, purchase price and category in a side list | `rec-24` | faster in part | 0.049 s; a natural fit once only purchases carry a price and category (`decisions.md`, **Purchases**) |
| 16-byte dedup tuple, payload left in place with dead flags | `rec-16` | faster in part | 0.033 s |
| 48-byte record (16-byte IDs) | none | slower | 0.09 s |
| Struct-of-arrays, one array per field | none | rejected | fine on one thread; in a scatter every field becomes its own write stream, multiplying the write heads per partition |

## Hash and table layout

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| One multiply of the packed `u64` by an odd constant; top bits pick the partition, the next bits the slot | `hash-mul` | pick | what every prototype uses; each key is hashed once and its bits reused |
| foldhash or rapidhash | `hash-foldhash`, `hash-rapidhash` | untested | only for slow-path byte IDs; the packed `u64` needs no byte hash |
| Linear probing | `table-linear` | pick | what the prototypes use; Richter, Alvarez and Dittrich (PVLDB 2015) find it among the fastest with a good hash at moderate load. Built slot by slot for the product and campaign maps |
| Linear probing over 64-byte lines of four slots: an ID's home is its line's first slot and all four are compared without a branch | in `links` (the link table) | pick | built: the person lookup costs 0.027–0.032 s against 0.067 s slot by slot and 0.053 s with only the first slot branch-free. 10.4% of lines are full on the real links, so about 6% of rows read a second line |
| Swiss table (hashbrown `HashTable`): one control byte per slot, 16 compared at once | `table-swiss` | untested | a miss usually reads only control bytes, so for the link table they act as a built-in filter; a hit reads two lines instead of one |

## Event dedup

62M 32-byte records, 60.06M survivors (`prototypes/dedupbench`).

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Scatter by `event_id` hash, small table per partition | `dedup-partitioned` | pick | 0.115–0.120 s (scatter 0.054–0.073, dedup 0.047–0.065). Built: the dedup alone 0.028–0.051 s on a loaded machine, where the prototype ran 0.052–0.088 s the same day |
| Table slots per row 2 / 4 / 8 | constant `ROOM` | measured | one thread 0.322 / 0.223 / 0.200 s. 4 is the pick: its table is 1 MB, the Xeon's whole L2, and 8's cannot fit. Sweep on amd64 |
| Prefetch each row's slot 16 rows ahead | in `dedup-partitioned` | pick | one thread 0.315–0.321 s against 0.336–0.341 s without; 8, 16 and 32 ahead the same |
| Tags in the slots' spare bits, so a collision skips the read of the kept list | none | measured | no gain on the M4 (0.313–0.320 s against 0.316–0.318 s, one thread), whose L2 holds the kept list; might pay on the Xeon |
| One shared lock-free table, prefetch 16 rows ahead | `dedup-shared` | slower | 0.19–0.20 s. Xue and Marcus (PVLDB 2025) report a purpose-built shared table matching partitioning, with heavy-hitter keys as its weak spot |
| The same without prefetch | `dedup-shared` | slower | 0.23–0.30 s |
| Sort each partition by `event_id` and scan | `dedup-sort` | untested | Müller et al. (SIGMOD 2015): hashing and sorting are the same partitioning work; a radix sort of a partition, then one scan keeps the smallest `ingest_ts` |
| A Bloom pass over each partition marks maybe-duplicates; only those go through the exact table | `dedup-filter` | untested | 2–4% of events have copies; costs a second pass over the partition, so only worth it if the tables miss L2 on the Xeon |
| Partition count 512 / 2,048 / 4,096 | env var | measured | 0.113–0.118 / 0.115–0.119 / 0.116–0.120 s, no difference here |
| Write-combining buffers with streaming stores instead of `Vec::push` | `scatter-wc` | untested | reported to matter above 64 partitions on x86 (Balkesen et al., ICDE 2013; Polychroniou and Ross, SIGMOD 2014); Schuhknecht et al. (PVLDB 2015) measure buffering, streaming stores and page size for exactly this scatter. On Cascade Lake it lets the partition count pass about 1,024, and streaming stores keep the 2 GB of writes out of L3 |
| Invert the generator's ID scramble to get dense event numbers, dedup with a bitmap | none | rejected | needs the per-seed key, and leans on the generator, not the rules |

Every event check (user, `ts`, type, product, price, late) only sets the invalid bit; nothing but a malformed line or an empty `event_id` leaves before dedup (rules, Events 1–2; fixture E5, where only the earliest copy has a bad `ts`).

## Person lookup

62M rows against 3.4M linked IDs in a 128 MB table, 56% hits (`prototypes/lookupbench`). Times are the cost added to the person scatter.

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Per row, prefetch 64 rows ahead | `lookup-prefetch` | pick | 0.061–0.067 s; batched, prefetched probes are what Kersten et al. (PVLDB 2018) find hides hash-probe misses best. Built on the line table: 0.027–0.032 s, and 32, 64 and 128 ahead measure the same |
| Per row, prefetch 16 rows ahead | `lookup-prefetch` | slower | 0.10 s |
| Per row, plain loop | `lookup-plain` | slower | 0.11–0.14 s |
| Once per distinct user: scatter on user ID first, move linked rows after | `lookup-distinct` | untested | 4.3M lookups instead of 62M, plus a third scatter for about a third of the rows |
| A Bloom filter or an occupancy bitmap in front of the table | `lookup-filter` | untested | 44% of rows miss, and a 1 to 4 MB filter answers those from cache; the usual pre-filter for a join probe. Design: a cache-line-blocked Bloom filter (Putze, Sanders and Singler, 2007), 3.4M keys at about 10 bits each = 4 MB, one line per probe, about 1% false positives, and a false positive costs only today's probe. A 1 MB occupancy bitmap fits L2 but passes about a third of the misses. It needs only the set of linked IDs, not the union-find result, so it is built in one pass split over the workers. Xor and binary fuse filters (Graf and Lemire) are smaller and more accurate but read 3 lines per probe. Estimate: about 40% fewer DRAM lines and page walks in the lookup. Matters less on the line table: a miss now costs one line and no mispredict, like a hit |
| Radix-partition the probe side | none | rejected | reported to win 1 of 59 joins in a real system |

## Links

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Serial union-find, larger root under the smaller, run beside the parse | `links-serial` | pick | built: 0.078–0.081 s on one thread for the real full-size links (2.66M pairs, 3.59M IDs): numbering 0.037, unions 0.015, naming 0.027. Rem's algorithm with splicing was the fastest sequential variant in Patwary, Blair and Manne (SEA 2010) |
| Concurrent union-find | `links-concurrent` | untested | not on the critical path while the parse takes longer; CAS-based linking (Anderson and Woll; Jayanti and Tarjan; Alistarh et al., OPODIS 2019), and ConnectIt (Dhulipala, Hong and Shun, PVLDB 2021) benchmarks the combinations on multicore |
| Union-find inside the lookup table: a slot is a node, and one pass at the end writes each slot's root | `links-intable` | untested | no intern step before and no table build after. Half taken by `links-serial` as built: its numbering table becomes the lookup table, but the unions run over separate dense nodes |
| Links first on all 8 threads, then the person is looked up inside the parse loop | `links-first` | untested | the payload scatters straight into person buckets, so the per-row pass after dedup and its scatter go away; needs `rec-16` and `links-concurrent`. Copies of one event can name different users (1 in 64 duplicated events; fixture E4), so dedup must still key on `event_id`, with the losing copy marked dead in its person bucket |
| Afforest: link a sample of edges, find the giant component, skip the edges inside it (Sutton, Ben-Nun and Barak, IPDPS 2018) | none | rejected | there is no giant component: the largest person, a shared device, holds thousands of people, well under 1% of the IDs |
| Union by rank with a separate smallest-ID array | none | rejected | reported 53% slower than linking by index with splicing |

Measured: `links-serial` takes 0.08 s on one thread at full size, plus the serial parse of the 52 MB of links before it, against a parse of 0.34 s, so it hides beside the parse on the Mac. It runs on one of the 8 workers and must finish before the first person lookup, so its slack is the parse alone.

## Sessionize

60M packed `u64` keys with the generator's skew (`prototypes/sortbench`). Timestamps are uniform over the window unless `SESSIONS=1` is set, which gives them the generator's session shape (8.47M sessions): a session in its own two-hour slot, under 30 minutes long. The generator's data has that shape, so the `SESSIONS=1` numbers are the ones that predict the scorer.

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Scatter by person hash, sort each bucket | `sess-partitioned` | pick | 0.10–0.11 s (scatter 0.02, gather and intern 0.02, sort 0.06–0.07). Built, after the scatter and with attribution and both tables: 0.140–0.160 s on its bench (events alone 0.107 s), 0.18 s in the full run. Per thread: gather and intern 12 ms, sort 65–70, click gather 5.5, scan 55–60 |
| One global `par_sort_unstable` | `sess-global` | slower | 0.22–0.25 s |
| Global parallel radix sort (rdst, voracious; IPS²Ra in C++) | `sess-radix-global` | untested | needs a global person index; IPS⁴o and IPS²Ra (Axtmann et al.) are the fastest reported parallel sorts |
| Counting scatter to dense person numbers, then sort each person's packed `u64` of ts offset (32 bits), category (8) and price (24) | `sess-dense` | untested | the research doc's layout; needs `id-dense` and a dense number per person |
| Bucket count 256 / 1,024 / 4,096 | env var | measured | 0.104–0.119 / 0.104–0.113 / 0.109–0.126 s |
| Single-thread `sort_unstable`, for reference | none | slower | 0.69 s |
| No sort: per person and 30-minute bin keep the smallest and largest `ts` and the row count, then sort only the bins; falls back to the sort when a bucket's rows do not cluster | `sess-bins` | faster in part | with `SESSIONS=1`, after the scatter: 0.061–0.072 s, against 0.083–0.091 s for the pick (gather, intern, sort, scan) and about 0.06 s for the 11-bit radix. Binning the rows is 0.042 s and sorting the 10.3M bins the rest. On uniform timestamps 964 of 1,024 buckets fall back and it costs 0.10 s. Correct under the rules: a 30-minute bin cannot hold a gap over 30 minutes, so sessions break only between bins |
| Bin per user ID before the person lookup, then move bins instead of rows | none | rejected | half of a person's rows use their other IDs, so by estimate the bins are only about 2.5 times fewer than the rows |

Where the payload rides, since the sort key (bucket-local person, ts) has no room for it:

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Purchases' price and category in a per-bucket side list sorted the same way, merged during the scan | `sess-purchase-side` | pick | built, not timed apart; views and carts, 97% of rows, need only `ts`, so the key stays 8 bytes |
| Sort 16-byte (key, payload) records | `sess-wide` | untested | simplest; twice the bytes through the sort |

## Sort inside a bucket

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `sort_unstable` | `sort-std` | pick | 0.058–0.070 s; built: 65–70 ms per thread, about half the sessions stage |
| LSD radix, 11-bit digits | `sort-radix` | faster in part | 0.035–0.057 s, noisy |
| LSD radix, 8-bit / 12-bit digits | `sort-radix` | measured | 0.047–0.065 / 0.049–0.071 s |
| vqsort (AVX-512, C++ binding) | `sort-vqsort` | untested | reported 8–9 ns per key against about 20 for `sort_unstable` on a Xeon; best kept for the few huge buckets (the whales, the shared-device persons) |
| Intel x86-simd-sort (AVX-512 and AVX2, C++, used by NumPy and PyTorch) | `sort-x86simd` | untested | same use as vqsort, behind the same kind of binding |

With `SESSIONS=1`: `sort_unstable` 0.063–0.066 s, radix 8-bit 0.060–0.071 s, 11-bit 0.039–0.041 s, 12-bit 0.054–0.061 s.

## Click dedup

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Skip it | `clicks-nodedup` | pick | no cost; for the same reason a bad click is dropped at parse, which an event never is |
| Dedup on `click_id` | `clicks-dedup` | untested | only useful as a correctness cross-check |

## Attribution

Per person, sessions in order against the person's clicks sorted by `ts`; the window is [start − 7 days, start], both ends inclusive. The sweep costs O(sessions + clicks + assists written).

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Two pointers over the window; a count per campaign plus a sparse set of the campaigns with a count above zero | `attr-sweep` | pick | built: the assist loop (14 assists per session, 121M increments on the bench) costs 3–5 ms per thread. A sparse set (Briggs and Torczon, 1993) has O(1) insert, delete, test and clear, and iterates its members densely |
| Count each campaign's active stretch per day when it leaves the window, instead of one increment per active campaign per session | `attr-interval` | untested | about 10 times fewer increments for heavy persons; changes the slot's contract |
| Clicks grouped per person by a counting scatter, each person's sorted when the scan reaches them | in `sess-partitioned` | pick | built: 5.5 ms per thread to gather 1.9M clicks. Above 16 clicks a person's are sorted as `ts << 32 \| position`, 2 to 2.6 times faster than a comparator sort |
| The same, with a per-campaign "last session counted" stamp instead of the set | `attr-stamp` | untested | one array and no clear, but each session walks every click in its window rather than the active campaigns |
| Sliding-window aggregation (DABA, Tangwongsan et al.) | none | rejected | built for aggregates that cannot be subtracted; per-campaign counts just add and subtract |

## Aggregation

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `user_daily` streamed per person; `campaign_daily` in per-worker dense arrays `[campaigns + organic][n_days]`, summed at the end | `agg-dense` | pick | built, laid out day-major so one session's assists stay in one 8 KB row; not timed apart from the scan. A person's session days only grow, and 2,001 × 14 cells per counter stay cached |
| Hash aggregation on (person, day) and (campaign, day) | none | rejected | one key is already grouped and the other dense, so a hash table only adds probes |

`n_days = day(window_end - 1) - day(window_start) + 1` with `day(t) = t.div_euclid(86_400_000)`, which also counts a window that starts mid-day; a ceiling of the window length would be one short.

## Output

7M rows, 6 columns (`prototypes/pqbench`). "8 files" means one per worker, uncompressed, no statistics unless stated. The writer is arrow-rs or a hand-written one; the Polars rows are kept as measurements, and Polars is not a dependency.

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| arrow-rs, 8 files, no dictionary | `out-arrow` | pick | 0.051 s, 252 MB with `UInt32` counts. Built with `Int64` counts and the real category names: 0.044 s, 354 MB, the integer columns through the `parquet` crate's typed column writers; about half of it is the kernel write |
| The same through `ArrowWriter::write` for every column | `out-arrow` | slower | 0.057 s; with the prototype's settings and `Int64` 0.064–0.067 s. It gathers each column through a `0..rows` index vector |
| Each file built in one `Vec` and written with a single `write` | `out-arrow` | slower | 0.058–0.065 s; an 8 MB `BufWriter` 0.045–0.064 s, a 1 MB one the same as none |
| arrow-rs, 8 files, dictionary except `user_id` | `out-arrow` | slower | 0.057 s, 117 MB |
| arrow-rs, 8 files, defaults | `out-arrow` | slower | 0.069 s, 101 MB |
| arrow-rs, 1 file, 1 thread | `out-arrow` | slower | 0.16 s |
| Hand-written plain-page writer | `out-raw` | untested | pages are nearly a copy; risks a file the checker rejects. Needs the `PAR1` magic at both ends, PLAIN data pages, definition levels for the nullable `top_category`, and a Thrift compact footer that marks `day` as `DATE` (Parquet logical types spec); check every file it writes with pyarrow or Polars in `run_fixtures.py` |
| `Int32` for the count columns, `Int64` for `revenue_cents` | `out-narrow` | untested | the rules accept any integer width, so about 56 MB fewer bytes to write, and the kernel write is half the output time; revenue stays 64-bit for headroom. Needs `Vec<i32>` columns in `types::rows::UserDaily` |
| Polars low-level page API, 8 files, all plain | `out-polars-raw` | measured | 0.049 s, 252 MB: same speed as the pick, more code |
| Polars `ParquetWriter`, 8 files | `out-polars` | slower | 0.080 s, 71 MB |
| Polars `ParquetWriter`, 1 file, defaults (zstd, statistics) | `out-polars` | slower | 0.38 s |

Every worker writes its file even when empty: each table needs at least one `.parquet` (rules), and `scripts/run_fixtures.py` fails a folder with none. `day` is `Date32`, days since 1970: `ts.div_euclid(86_400_000)`.

## Parallelism and process

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| 8 workers, each pulling tasks largest-first from a shared counter | `par-counter` | pick | built, scoped threads spawned per stage; not timed against another runner. Morsel-driven scheduling (Leis et al., SIGMOD 2014) with the largest task first |
| rayon work stealing with per-task state | `par-rayon` | untested | work stealing as in Blumofe and Leiserson |
| Pin the workers to one CPU socket | `pin-socket` | untested | needs a submission; only helps on a two-socket host |
| glibc malloc tunables in the image: `GLIBC_TUNABLES=glibc.malloc.hugetlb=1:glibc.malloc.top_pad=268435456:glibc.malloc.mmap_threshold=33554432` | Dockerfile `ENV` | pick | Linux arm64 container on the Mac, half size, median wall of 6 to 8 runs, against the plain system allocator, by the host's transparent-huge-page mode: `madvise` 354 ms against 407 (and 330 against 392 through the image), `always` 334 against 390, `never` 383 against 398. `hugetlb=1` makes malloc `madvise` its memory for 2 MB pages, but glibc only does so in `madvise` mode: alone it gives 328–353 there and nothing under `always`. `top_pad` grows the heap in 256 MB steps (alone: 358 under `always`, 386 under `never`), and the threshold puts every buffer under 32 MB in the heap. Peak anonymous memory rises about 15% |
| Huge pages for the large buffers | `hugepages` | untested | mostly covered by the row above; an explicit `madvise` on the link table is what is left. Linux only. The 128 MB link table first: 32,768 pages of 4 KB against 1,536 TLB entries, or 64 pages of 2 MB. `madvise(MADV_HUGEPAGE)` on 2 MB-aligned buffers; a no-op if the host's transparent huge pages are off |
| Exit without running destructors | `fast-exit` | pick | built and on by default; not timed against a normal exit. The kernel's unmap remains: about 24 ms at half size on Linux |
| Workers hand their buffers back with `MADV_DONTNEED` as each stage ends | `free-early` | untested | exit unmaps on one thread; this spreads it over eight, and `MADV_DONTNEED` takes the address-space lock shared where `munmap` takes it exclusive. Linux only |
| Fork a child to do the work; the parent exits once the output is written | `exit-child` | untested | the 1BRC trick, worth 7.5% to the winner. Probably nothing here: the binary is PID 1 in the container, and the kernel reaps the whole PID namespace before PID 1's exit completes. One `time docker run` settles it |
| Fixed-size blocks from a per-worker pool for every scatter, recycled as partitions are consumed | `buf-pool` | untested | no `Vec` growth copies, and the second scatter writes into warm pages instead of faulting new ones. Pre-sizing each worker's buffers from its bytes divided by an average line length gets part of this with plain `Vec`s |
| mimalloc as the global allocator | `alloc-mimalloc` | slower | built, a build-time swap. Slower than the glibc tunables in every mode (Linux arm64 container on the Mac, half size, median wall of 6 to 8 runs): `madvise` 370 ms against 354, `always` 350 against 334, `never` 414 against 383, and under `never` slower than plain glibc (398). On the Mac, full size: median total 698–718 ms against 746–754 for the system allocator |
| jemalloc as the global allocator | `alloc-jemalloc` | untested | DuckDB ships it on Linux for allocation-heavy many-core work; the same caveat as mimalloc |

The union-find takes one of the 8 workers: under `--cpus 8` a ninth busy thread adds no CPU, only throttling.

## Build

| Alternative | Where | Status | Result |
| --- | --- | --- | --- |
| `target-cpu=x86-64-v3` for amd64 (AVX2, BMI2) | `.cargo/config.toml` | pick | built; the image passes the fixtures on amd64 under emulation, no timing |
| `target-cpu=cascadelake` | `.cargo/config.toml` | untested | the README permits it |
| Fat LTO against thin LTO, with `codegen-units = 1` and `panic = "abort"` | Cargo profile | measured | no difference on the Mac at full size (min/median total of 10 runs, two rounds: fat 711/744 and 729/764 ms, thin 710/728 and 749/760); fat is what is built. `opt-level` is 3, the release default |
| Profile-guided optimisation trained on generator output during the image build | Dockerfile | measured | no gain on the Mac at full size: 724/748 and 741/766 ms against fat LTO's 711/744 and 729/764, trained on a 5M-event set from another seed plus `dev/`. Not wired into the Dockerfile; untested on amd64, where it would need the generator in the image |

## Language

| Alternative | Status | Result |
| --- | --- | --- |
| Rust with `std::arch` intrinsics, and `asm!` where the compiler's output is poor | pick | |
| Hand-written LLVM IR for the hot path | rejected | goes through the same optimiser and backend as rustc, so it cannot produce faster code than Rust that emits the same IR; it loses inlining across the boundary and needs a second version for arm64 |

## Prototypes

Standalone crates, one `main.rs` each. Run with `RAYON_NUM_THREADS=8 cargo run --release`. `pqbench` takes an output folder as its argument. The root workspace excludes them. Each stage crate also has its own benchmark under `crates/*/examples/`.
