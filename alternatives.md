# Alternatives

Every option considered for each slot, so each can sit behind its own Cargo feature and be benchmarked. `decisions.md` holds the current pick; this file holds the field.

Times are wall time at full size (60–62M rows), 8 threads, on an Apple-silicon Mac. A result that starts with "built" comes from the real crates (their `examples/` benchmarks, or the whole binary; `implementation.md` has the detail); the others come from the standalone crates in `prototypes/`. Results marked **x86** come from an AMD Ryzen 7 7800X3D running the amd64 image in Docker Desktop on WSL2 (4 KB pages, scorer flags) on 20M events and 5M clicks from seed 7, the scorer's size; `x86.md` has the machine, the measurements and the procedure. Nothing here has run on the scorer's Xeon. "Reported" numbers come from papers or other people's benchmarks, not ours; their sources are in the research doc, [Shopping Sessions: Fast-Algorithm Sources](https://claude.ai/artifact/QFQSeVY55mCaNksMb3YjQ1). The 60–62M are event rows; the generator calls 75M rows its full size (comment at `generator/src/plan.rs:134`).

Status:

- **pick**: the current choice in `decisions.md`
- **slower**: measured here and lost
- **faster in part**: measured faster on one piece, not end to end
- **untested**: no number of ours
- **rejected**: ruled out without a benchmark

Features only add code, so several alternatives for one slot can be compiled in together. When a slot has more than one, an env var picks, and the pick is the default.

## What this Mac hides

Several costs are small on this Mac and are not small on the scorer, a Linux Xeon. The figures are estimates unless they say measured, and the scorer's cache and TLB sizes assume Cascade Lake, which the README's `target-cpu` suggests. "Container" means the built binary in a Linux arm64 container on this Mac (4 KB pages, scorer flags, data on a Docker volume) at half size, 30M events and 7.5M clicks; that VM has 12.6 GB, too little for the full set. "x86" means the Ryzen box (`x86.md`). There the process takes 0.50–0.54 s back to back and 0.60–0.72 s on a first run after idle, against about 1.0 s on the scorer.

- **L1 size and the scatters (x86, measured).** The M4's 128 KB L1D holds a worker's 1,024 partition tails (64 KB) and their `Vec` headers (24 KB); a 32 KB L1D does not.
  - On x86, the parse on 8 workers reaches 7.2 GB/s with 1,024 partitions and 11.3 GB/s with 2. The scatter costs about a third of the parse, and 16% of the parse's samples sit on the `EventRec` store.
  - The person scatter after dedup has the same cost: 25% of that stage's samples are on its 16-byte store.
  - Schuhknecht et al. (PVLDB 2015), on a Xeon with a 32 KB L1D, single-threaded: a plain scatter takes 2.41 s to 1,024 partitions and 0.79 s to 32. Software write-combining buffers with streaming stores bring 1,024 down to 0.96 s.
  - `scatter-wc` (staged buffers of 4 lines, streaming stores) brings the parse to 10.4–10.6 GB/s and takes 16.5–19% off the scorer-like run; see Event dedup and Person lookup.
- **Serial first touch (x86, measured).** The main thread allocates every worker's buffers before the parse (8 × 1,024 `Vec`s) and before the dedup stage (8 × 2,048). glibc writes a header into every chunk, so with transparent huge pages that thread faults in and zeroes nearly every reserved 2 MB page while seven workers wait.
  - On x86 that is 62 ms before the parse and 40 ms before dedup, about 20% of the run.
  - `clear_huge_page` reached from libc is 6% of all samples. On the Mac the allocation takes 0.2 ms, because nothing touches 16 KB pages until they are written.
  - Building the same `Vec`s on 8 threads did not help the parse: glibc's heap growth and the page faults contend for the address-space lock. One region per worker did: `alloc-parallel` takes 19.5% off the scorer-like run.
- **Fresh memory.** Linux hands out 4 KB pages, this Mac 16 KB. The dedup scatter alone writes 2 GB of new memory: about 500,000 page faults, each with a zeroing, so perhaps 0.5 to 1 µs each.
  - Measured in the container: the dedup and person-scatter stage, which writes the most fresh memory per row, takes 91–102 ms against 64 ms natively. The parse takes 162–165 ms against 184 ms, and the whole run 0.35–0.37 s on both.
  - On the Mac, fresh output buffers add 70 ms and a fresh mapping 61 ms to a 238 ms warm parse at full size.
  - On x86 with the glibc tunables, most buffers get 2 MB pages (33K faults per run), and kernel zeroing is 7% of the CPU time.
- **Idle reclaim (x86, measured; likely WSL2 only).** A run after 10 s idle takes 690–720 ms against 505–514 ms back to back. Its system time is 630–735 ms against 346–429, and `clear_page_erms` is 10.4% of samples against 4.8%: the same pages, zeroed at half the speed.
  - The likely cause is the hypervisor reclaiming freed guest memory; not verified.
  - A bare-metal scorer would not show it, a cloud VM might. Every fresh byte counts double there.
- **Cache and TLB reach.** This Mac has 16 MB of L2 per four cores. Cascade Lake has 1 MB per core and 1,536 second-level TLB entries, which cover 6 MB in 4 KB pages.
  - A sweep that showed no difference here (partition count, bucket count) has to be run again there, and the 128 MB link table misses the TLB on every lookup.
  - The partition count pulls two ways there. Many write heads per worker overflow L1 (measured above) and then the TLB; at 1,024 partitions each dedup table (about 1.5 to 2 MB) already overflows L2. `scatter-2level` and `scatter-wc` let the dedup partitions stay small without the first cost.
- **Instruction count.** x86-64 has 16 integer registers against arm64's 31, so the large inlined row closure spills. The M4's wider core hides extra instructions.
  - With the scatter nearly gone (2 partitions), one x86 core parses 1.95 GB/s. On the M4 one core parses 3.37 GB/s warm; the conditions differ, so this is only a hint.
  - The `flatten` stores of the index pass are about 17% of the x86 parse.
- **Exit.** The kernel unmaps everything on one thread before the process is gone, and `fast-exit` does not skip that. 1BRC entries measured 40 to 180 ms for a 13 GB mapping.
  - Measured in the container: the process ends about 24 ms after its last output at half size (about 5 GB to unmap), about 7% of that run.
  - x86 at 20M: about 40 ms between the internal total and the process's end, process start included.

One cost is larger here than on the scorer: at full size this Mac (24 GB) can run short of memory, here with the OrbStack VM running beside it. An Instruments System Trace of one run shows 7,916 "Decompress Memory" faults taking 522 ms summed over threads, beside 291,505 zero fills (1.45 s, 4.8 GB) and 362,284 faults on the mapped input (0.41 s). Full-size timings on the Mac carry that noise; stop the VM (`orbctl stop`) before timing natively, and `make systrace-mac` shows whether a run hit it.

Test these first on amd64, in this order:

1. `alloc-parallel`: done, the pick (−19.5% on the scorer-like run).
2. `scatter-wc` for both scatters: done, the pick (a further −16.5 to −19.2% on the scorer-like run). `scatter-2level` stays untested, with little left to win.
3. `rec-16` or `rec-24`.
4. `reader-pread`.
5. `hugepages` for the link table.
6. `lookup-filter`.
7. The partition, bucket and `ROOM` sweeps.

Run them on the Ryzen box with `scripts/bench_docker.py` (`x86.md`) before deciding.

## Reader

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `mmap` each file | `reader-mmap` | pick | built: a fresh mapping adds 61 ms to the full-size parse over an already faulted one (Mac, 16 KB pages) |
| `pread` into a reused per-worker buffer | `reader-pread` | untested | reported about 5% faster on a warm cache in one Linux run; nothing to unmap at exit. x86 at 20M: `perf stat` puts the process's end about 40 ms after its internal total (0.579 s elapsed against about 0.54 s), process start included. By estimate most of that is unmapping the 1.8 GB file mapping, which is mapped in 4 KB entries (page-cache pages are not huge pages here), about 470,000 of them. `pread` also removes the mapping's faults (`filemap_map_pages` 0.4% of samples) and adds a copy of 1.9 GB spread over 8 workers |
| Ask the kernel to read every file ahead at start | `readahead` | untested | only matters if the scorer's cache is cold. Measured cold in the Linux container at half size (guest page cache dropped before each run, the Mac's cache under the virtual disk still warm; `scripts/bench.py --prepare`, median of 3): 467 ms total against 330 warm, 230 major faults. Almost all of it is the serial `inputs` stage, 133 ms against 7, which reads the lookup files, the first 256 KB of the largest file and every file's tail on one thread; the parse adds 12 ms. On the Mac the session's first full-size run, cache cold but not controlled, took 3.88 s: `inputs` 774 ms, parse 2.6 s |
| `MADV_SEQUENTIAL` on each mapping | `reader-seq` | untested | Crotty, Leis and Pavlo (CIDR 2022): mmap reads ahead less than `read()`, and this advice suits a one-pass scan |
| Fault in each chunk's pages with one `MADV_POPULATE_READ` before parsing it (Linux 5.14+) | `reader-populate` | measured | built, picked with `SS_READER=populate`: no gain (Linux arm64 container on the Mac, half size, median wall of 6 to 8 runs: 337 ms against 336 with the glibc tunables, 393 against 389 without). One call per 1 MB chunk instead of a fault per 64 KB of fault-around, made by each worker for its own chunks; `MAP_POPULATE` would fault the whole file on one thread at map time |
| Chunk size 256 KB, 1 MB, 4 MB | env var `SS_CHUNK_KB` | measured | 1 MB is the pick. In the Linux container at half size: 256 KB 338 ms against 336 for 1 MB (its serial chunk planning takes 13 ms against 7); 4 MB with `reader-populate` 337. Reported best is 0.25–1.0 × L3 per thread |

## Tokenizer and row parse

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Mask tokenizer (AVX2 on amd64, NEON on arm64), two passes per 8 KB batch: block masks to comma and newline positions, then one row per newline | `tok-mask` | pick | built: the whole parse (events, clicks, links, open included) 0.335 s, 17.0 GB/s, 2.13 GB/s per worker; with pages already faulted and buffers reused 25.4 GB/s, 3.37 GB/s on one core; the tokenizer alone 7.6 GB/s per core. A kept mask is reported at 8.0 GB/s on dense matches, AVX-512 no faster than AVX2. x86, `parse_bench` with fresh mapping and buffers: 7.2 GB/s on 8 workers (0.90 per worker), 1.36 GB/s on one; about a third of the 8-worker time is the 1,024-way partition scatter (see `scatter-2level`), and the `flatten` stores of the index pass are about 17% of the parse's samples |
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

On x86 the record store in the parse's scatter is the parse's largest single cost: 16% of its samples, with the store of the 16-byte `PersonEvent` taking 25% of the dedup stage. So `rec-16` and `rec-24` rank higher there than the Mac's numbers suggest. They also cut the fresh memory both scatters fault in. They combine with `scatter-2level` or `scatter-wc`, since fewer bytes per record means more records per buffered line. Untested on x86.

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
| Partition count 512 / 2,048 / 4,096 | env var `SS_PART_BITS` | measured | 0.113–0.118 / 0.115–0.119 / 0.116–0.120 s, no difference here. x86, `parse_bench`, 8 workers: 2 / 16 / 64 / 256 / 1,024 partitions parse 11.3 / 10.4 / 8.4 / 7.9 / 7.2 GB/s (one worker: 1.95 / 1.85 / 1.65 / 1.56 / 1.37). x86, whole run, 3 runs each, 64 / 256 / 4,096 partitions: parse 296–380 / 318–323 / 353–374 ms and dedup 177–227 / 151–161 / 148–157 ms, against parse 344–456 and dedup 142–164 at 1,024. Fewer partitions speed the parse and slow the dedup, whose tables then outgrow L2, so a single count cannot serve both: see `scatter-2level` |
| Write-combining buffers with streaming stores instead of `Vec::push` | `scatter-wc` | pick | built as a write mode of `types::parts::Parts` (`SS_SCATTER=combine\|plain`, the default `combine` on x86-64 and `plain` elsewhere; `SS_WC_LINES`, default 4). Each list stages records in a buffer of at least that many cache lines (8 `EventRec`s, 16 `PersonEvent`s, 16 `ClickRec`s in 6 lines), flushed whole with `_mm256_stream_si256`; the staged tail is a piece of the list, so nothing is copied at the end, and each task ends with an `sfence`. x86, `parse_bench`, 8 workers, 1,024 partitions: plain 7.6–7.9 GB/s, 1 line 9.4–9.7, 2 lines 9.5–10.1, **4 lines 10.4–10.6**, 8 lines 10.3–10.4, against 11.3 with 2 partitions and no scatter cost. Whole run against `alloc-parallel` alone, 20M, fresh containers: after 12 s idle 573 → 479 and 609 → 492 ms (−16.5%, −19.2%; parse −24%, dedup −24 to −26%); back to back 575 → 506 ms (−12.1%). User CPU per run 2.93–3.07 → 2.50–2.58 s. The sessions stage measured about 10 ms slower in the scorer-flag runs (79–82 → 89–92 ms), also with the person buckets written plain (`SS_SCATTER_BUCKETS=plain`, which loses dedup's gain: 549 ms), and not in a privileged container (72–96 against 74–79 ms); unexplained. Literature: reported to matter above 64 partitions on x86 (Balkesen et al., ICDE 2013; Polychroniou and Ross, SIGMOD 2014). Schuhknecht et al. (PVLDB 2015) measure exactly this scatter on a Xeon (32 KB L1D, single thread, 100M 8-byte entries): plain 2.41 s at 1,024 partitions against 0.79 s at 32; software-managed buffers 1.30 s (best size, 26 cache lines), with streaming stores 1.08 s (2 lines), one-line buffers with streaming stores and each partition's fill state and destination kept in its buffer line 0.96 s; up to about 64 partitions the plain loop is already near the best. On x86 here the plain store is 16% of the parse's samples. On Cascade Lake it lets the partition count pass about 1,024, and streaming stores keep the 2 GB of writes out of L3. Streaming stores need 32- or 64-byte aligned flushes |
| Two-level partitioning: 16 to 64 ways in the parse, the rest of the 1,024-way split inside each dedup task, in cache | `scatter-2level` | untested | lower priority since `scatter-wc`: the parse now reaches 10.4–10.6 GB/s of the 11.3 that 2 partitions give, so at most about 8% of the parse is left to win, against one more pass over every event in the dedup stage. Before `scatter-wc`, x86 pointed at it. The parse then writes to few enough partitions that their tails stay in L1: 64 partitions measured 8.4 GB/s and 16 measured 10.4 GB/s, against 7.2 at 1,024. Each dedup task splits its coarse partition (about 312K rows at 64 ways, 20M events) into L2-sized pieces before the table pass, so the tables stay at today's size. The second split is one more pass over each coarse partition, while it is warm in L2/L3. Simpler than `scatter-wc`, and the two combine |
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

The scatter the lookup feeds has the parse scatter's problem on x86. Each worker writes 16-byte `PersonEvent`s into 1,024 person buckets (`SS_BUCKET_BITS`), and 24.8% of the dedup stage's samples sit on that store. Another 9.8% stall right after the link-table prefetch, presumably with the line fill buffers full of store misses. The same two options apply:

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Write-combining buffers with streaming stores for the person buckets | `scatter-wc` | pick | built, as under Event dedup: the same `Parts` write mode, 16 `PersonEvent`s (4 lines) or 16 `ClickRec`s (6 lines) per buffer. `SS_SCATTER_BUCKETS=combine\|plain` overrides `SS_SCATTER` for the buckets alone. x86, after 12 s idle: the dedup stage 139 → 103 ms with both scatters combined, 138 ms with the buckets plain, so most of dedup's gain is this scatter |
| Two-level person buckets: 16 to 64 in the scatter, each split by the next person-hash bits inside its sessions task | `scatter-2level` | untested | a coarse bucket is about 16 to 64 times today's 20K rows at 20M events, too big to sort in L2, so the sessions task splits it in cache first. Changes `sessions::bucket`'s input, not its contract per fine bucket. The person-bucket count was swept only on the Mac (Sessionize) |

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
| Bucket count 256 / 1,024 / 4,096 | env var `SS_BUCKET_BITS` | measured | 0.104–0.119 / 0.104–0.113 / 0.109–0.126 s. Mac only; untested on x86, where the scatter into the buckets costs 25% of the dedup stage (Person lookup) |
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
| Each worker's lists carved out of one allocation per worker and record type (`types::parts::Parts`: the 1,024 dedup partitions in the parse, the 1,024 event and 1,024 click buckets in the dedup stage), built on the worker's own thread (`types::par::init`) | `alloc-parallel` | pick | built. x86, 20M, alternating rounds in fresh containers against HEAD `5c1e1f2`, after 12 s idle (the scorer-like run), 6 rounds: median total 742 → 597 ms (−19.5%), parse 452 → 365, dedup 170 → 129; back to back, 10 rounds: 587 → 561 ms (−4.4%, inside the noise), parse 340 → 320, dedup 130 → 118. `SS_ALLOC=serial` (carved, built on the main thread) measured 613 and 567 ms, so carving is most of the gain. `SS_TRACE`: the serial gaps before the parse and before dedup went from 60–68 and 30–85 ms to 2–3.5 ms each, and time in `mmap`, `mprotect` and `munmap` from 0.40 s to 0.10 s. The zeroing still happens, now inside the parallel stages as the workers write. A list that outgrows its room spills into a `Vec`: none of the 8,192 partitions do at 5/4 + 16, but at 9/8 + 64 about 635 of 8,192 event buckets and 330–360 of 8,192 click buckets do, from the skew in person sizes (`SS_TIMING` prints the counts) |
| The same buffers as 8 × 1,024 and 8 × 2,048 separate `Vec`s, built on the workers' own threads | none (first version of `alloc-parallel`) | slower | x86, 20M: after 12 s idle 678 → 620 ms against HEAD, back to back 592 → 582 ms; dedup gained 16–24%, the parse nothing. `SS_TRACE`: the parse's init stage took 63–68 ms on 8 threads against 60–62 ms on one; the workers' builds (8–29 ms each) took turns. glibc's per-thread arenas grow with `mprotect`, `mmap` and `munmap`, which take the address-space lock for writing, while every page fault holds it for reading through its 2 MB zeroing (kernel 5.15 has no per-VMA locks): `mprotect` averaged 1.37 ms a call against 31 µs on one thread. glibc's single main arena (`glibc.malloc.arena_max=1`) was no better. Superseded by carving, whose few large allocations leave the faults with only readers of that lock |
| Fixed-size blocks from a per-worker pool for every scatter, recycled as partitions are consumed | `buf-pool` | untested | no `Vec` growth copies, and the second scatter writes into warm pages instead of faulting new ones. Pre-sizing each worker's buffers from its bytes divided by an average line length gets part of this with plain `Vec`s. x86: at 20M the stage-2 buckets reserve about 0.5 GB of fresh memory (8 × 2,048 `Vec`s, events and clicks), which the parse partitions, dead once their dedup task ends, could lend. About −30 ms by estimate, more where first touch costs double (runs after idle on WSL2, `x86.md`). The person scatter would write block lists instead of `Vec`s; `sessions::bucket` already takes a bucket as pieces. Since `alloc-parallel` the lists are `types::parts::Parts`, one region per worker, so the step is a `Parts` built over recycled blocks rather than a new layout |
| Parse partition margin below 25% | constant in `Ingest::new_out` | untested | 8 × 1,024 partitions are reserved at rows / workers / partitions × 5/4 + 16: about 868 MB at 20M against 690 MB written. Before `alloc-parallel`, glibc's header in every chunk made the kernel zero every reserved 2 MB page, so the unused quarter was zeroed too: about 180 MB. Carved (`types::parts`), a page is zeroed only when a record lands in it; with 2 MB pages that is still nearly all of them, since each holds about 20 partitions' slots, while with 4 KB pages the untouched tail of each partition's room stays unfaulted. A smaller margin trades that for spills into a `Vec`; no partition spills at 5/4 + 16 on 20M (`SS_TIMING`) |
| Person bucket margin above 9/8 + 64 | constant in `src/main.rs` (`room`) | untested | at 20M about 635 of 8,192 event buckets and 330–360 of 8,192 click buckets spill (`SS_TIMING`), from the skew in person sizes; each spilled record takes a non-inlined call and a `Vec` push. A larger margin costs fresh memory; sizing a bucket from its share of the dedup partitions is the alternative |
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
