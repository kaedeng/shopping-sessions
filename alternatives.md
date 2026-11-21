# Alternatives

Every option considered for each slot, so each can sit behind its own Cargo feature and be benchmarked. `decisions.md` holds the current pick; this file holds the field.

Times are wall time at full size (60–62M rows), 8 threads, on an Apple-silicon Mac, from the standalone crates in `prototypes/`. Nothing here has run on amd64. "Reported" numbers come from papers or other people's benchmarks, not ours.

Status:

- **pick**: the current choice in `decisions.md`
- **slower**: measured here and lost
- **faster in part**: measured faster on one piece, not end to end
- **untested**: no number of ours
- **rejected**: ruled out without a benchmark

Features only add code, so several alternatives for one slot can be compiled in together. When a slot has more than one, an env var picks, and the pick is the default.

## What this Mac hides

Three costs are small here and will not be on the scorer, a Linux Xeon. None is measured; the figures are estimates, and the cache and TLB sizes assume Cascade Lake, which the README's `target-cpu` suggests.

- **Fresh memory.** Linux hands out 4 KB pages, this Mac 16 KB. The dedup scatter alone writes 2 GB of new memory: about 500,000 page faults, each with a zeroing, so perhaps 0.5 to 1 µs each.
- **Cache and TLB reach.** This Mac has 16 MB of L2 per four cores. Cascade Lake has 1 MB per core and 1,536 second-level TLB entries, which cover 6 MB in 4 KB pages. A sweep that showed no difference here (partition count, bucket count) has to be run again there, and the 128 MB link table misses the TLB on every lookup.
- **Exit.** The kernel unmaps everything on one thread before the process is gone, and `fast-exit` does not skip that. 1BRC entries measured 40 to 180 ms for a 13 GB mapping.

## Reader

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `mmap` each file | `reader-mmap` | pick | untested |
| `pread` into a reused per-worker buffer | `reader-pread` | untested | reported about 5% faster on a warm cache in one Linux run; nothing to unmap at exit |
| Ask the kernel to read every file ahead at start | `readahead` | untested | only matters if the scorer's cache is cold |
| Chunk size 256 KB, 1 MB, 4 MB | env var | untested | 1 MB is the pick; reported best is 0.25–1.0 × L3 per thread |

## Tokenizer and row parse

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Mask tokenizer, one pass (AVX2 on amd64, NEON on arm64) | `tok-mask` | pick | untested; a kept mask is reported at 8.0 GB/s on dense matches, AVX-512 no faster than AVX2 |
| Delimiter index for the whole chunk, then rows at a fixed stride of 7 | `tok-index` | untested | expected fastest, because rows become independent work; build and time first |
| `memchr` per field | `tok-memchr` | untested | reported 1.06 GB/s on dense matches |
| Scalar byte loop | `tok-scalar` | untested | the baseline, and the fallback on other CPUs |
| Two to four cursors per worker in one loop, each on its own piece of the chunk | `tok-lanes` | untested | the core overlaps the rows; one 1BRC entry went from 1.34 to 1.23 s with it and the winner used three. Only for the one-pass tokenizer: `tok-index` already makes rows independent |
| Strip quotes with arithmetic and test for a malformed row once, not per field | `quote-branchless` | untested | 20% of fields are quoted at random, so a branch per field mispredicts about 1.4 times a row |

## Timestamps

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Date-string lookup table, a miss drops the row | `ts-table` | pick | untested |
| General calendar conversion with validation | `ts-calendar` | untested | reported 1.5–4 ns per conversion, so about 20 ms in total |
| Convert as F1 and as F3 both and pick by field length, F2 behind a branch | `ts-select` | untested | the form is random per row (58/10/32%), so a three-way branch mispredicts on about 4 rows in 10 |

## IDs

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `u64`, 7 bits per byte, up to 9 ASCII bytes, slow path beyond | `id-u64` | pick | untested as such; every prototype uses 8-byte IDs |
| 16 bytes inline, up to 15 bytes | `id-u128` | untested | doubles the key width; see record width |
| 64-bit hash as the identity | none | rejected | collides about once in 10,000 runs at 60M keys |

## Record width in the dedup scatter

Scatter only, 62M records into 1,024 partitions (`prototypes/lookupbench`).

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| 32-byte record | `rec-32` | pick | 0.061 s |
| 24-byte record, purchase price and category in a side list | `rec-24` | faster in part | 0.049 s |
| 16-byte dedup tuple, payload left in place with dead flags | `rec-16` | faster in part | 0.033 s |
| 48-byte record (16-byte IDs) | none | slower | 0.09 s |

## Event dedup

62M 32-byte records, 60.06M survivors (`prototypes/dedupbench`).

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Scatter by `event_id` hash, small table per partition | `dedup-partitioned` | pick | 0.115–0.120 s (scatter 0.054–0.073, dedup 0.047–0.065) |
| One shared lock-free table, prefetch 16 rows ahead | `dedup-shared` | slower | 0.19–0.20 s |
| The same without prefetch | `dedup-shared` | slower | 0.23–0.30 s |
| Partition count 512 / 2,048 / 4,096 | env var | measured | 0.113–0.118 / 0.115–0.119 / 0.116–0.120 s, no difference here |
| Write-combining buffers with streaming stores instead of `Vec::push` | `scatter-wc` | untested | reported to matter above 64 partitions on x86 |
| Invert the generator's ID scramble to get dense event numbers, dedup with a bitmap | none | rejected | needs the per-seed key, and leans on the generator, not the rules |

## Person lookup

62M rows against 3.4M linked IDs in a 128 MB table, 56% hits (`prototypes/lookupbench`). Times are the cost added to the person scatter.

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Per row, prefetch 64 rows ahead | `lookup-prefetch` | pick | 0.061–0.067 s |
| Per row, prefetch 16 rows ahead | `lookup-prefetch` | slower | 0.10 s |
| Per row, plain loop | `lookup-plain` | slower | 0.11–0.14 s |
| Once per distinct user: scatter on user ID first, move linked rows after | `lookup-distinct` | untested | 4.3M lookups instead of 62M, plus a third scatter for about a third of the rows |
| A Bloom filter or an occupancy bitmap in front of the table | `lookup-filter` | untested | 44% of rows miss, and a 1 to 4 MB filter answers those from cache; the usual pre-filter for a join probe |
| Radix-partition the probe side | none | rejected | reported to win 1 of 59 joins in a real system |

## Links

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Serial union-find, larger root under the smaller, run beside the parse | `links-serial` | pick | untested |
| Concurrent union-find | `links-concurrent` | untested | not on the critical path while the parse takes longer |
| Union-find inside the lookup table: a slot is a node, and one pass at the end writes each slot's root | `links-intable` | untested | no intern step before and no table build after |
| Links first on all 8 threads, then the person is looked up inside the parse loop | `links-first` | untested | the payload scatters straight into person buckets, so the per-row pass after dedup and its scatter go away; needs `rec-16` and `links-concurrent` |
| Union by rank with a separate smallest-ID array | none | rejected | reported 53% slower than linking by index with splicing |

An estimate, not a measurement: `links-serial` is about 5M random lookups to number the IDs and 2.6M unions over a structure far larger than the cache, so 0.15 to 0.3 s on one thread. That is the same order as the parse, so time it before trusting that it hides beside the parse.

## Sessionize

60M packed `u64` keys with the generator's skew (`prototypes/sortbench`). Timestamps are uniform over the window unless `SESSIONS=1` is set, which gives them the generator's session shape (8.47M sessions): a session in its own two-hour slot, under 30 minutes long.

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Scatter by person hash, sort each bucket | `sess-partitioned` | pick | 0.10–0.11 s (scatter 0.02, gather and intern 0.02, sort 0.06–0.07) |
| One global `par_sort_unstable` | `sess-global` | slower | 0.22–0.25 s |
| Global parallel radix crate (rdst, voracious) | `sess-radix-global` | untested | needs a global person index |
| Bucket count 256 / 1,024 / 4,096 | env var | measured | 0.104–0.119 / 0.104–0.113 / 0.109–0.126 s |
| Single-thread `sort_unstable`, for reference | none | slower | 0.69 s |
| No sort: per person and 30-minute bin keep the smallest and largest `ts` and the row count, then sort only the bins; falls back to the sort when a bucket's rows do not cluster | `sess-bins` | faster in part | with `SESSIONS=1`, after the scatter: 0.061–0.072 s, against 0.083–0.091 s for the pick (gather, intern, sort, scan) and about 0.06 s for the 11-bit radix. Binning the rows is 0.042 s and sorting the 10.3M bins the rest. On uniform timestamps 964 of 1,024 buckets fall back and it costs 0.10 s |
| Bin per user ID before the person lookup, then move bins instead of rows | none | rejected | half of a person's rows use their other IDs, so by estimate the bins are only about 2.5 times fewer than the rows |

## Sort inside a bucket

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| `sort_unstable` | `sort-std` | pick | 0.058–0.070 s |
| LSD radix, 11-bit digits | `sort-radix` | faster in part | 0.035–0.057 s, noisy |
| LSD radix, 8-bit / 12-bit digits | `sort-radix` | measured | 0.047–0.065 / 0.049–0.071 s |
| vqsort (AVX-512, C++ binding) | `sort-vqsort` | untested | reported 8–9 ns per key against about 20 for `sort_unstable` on a Xeon |

With `SESSIONS=1`: `sort_unstable` 0.063–0.066 s, radix 8-bit 0.060–0.071 s, 11-bit 0.039–0.041 s, 12-bit 0.054–0.061 s.

## Click dedup

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| Skip it | `clicks-nodedup` | pick | no cost |
| Dedup on `click_id` | `clicks-dedup` | untested | only useful as a correctness cross-check |

## Output

7M rows, 6 columns (`prototypes/pqbench`). "8 files" means one per worker, uncompressed, no statistics unless stated.

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| arrow-rs, 8 files, no dictionary | `out-arrow` | pick | 0.051 s, 252 MB |
| arrow-rs, 8 files, dictionary except `user_id` | `out-arrow` | slower | 0.057 s, 117 MB |
| arrow-rs, 8 files, defaults | `out-arrow` | slower | 0.069 s, 101 MB |
| arrow-rs, 1 file, 1 thread | `out-arrow` | slower | 0.16 s |
| Polars low-level page API, 8 files, all plain | `out-polars-raw` | measured | 0.049 s, 252 MB: same speed as the pick, more code |
| Polars `ParquetWriter`, 8 files | `out-polars` | slower | 0.080 s, 71 MB |
| Polars `ParquetWriter`, 1 file, defaults (zstd, statistics) | `out-polars` | slower | 0.38 s |
| Hand-written plain-page writer | `out-raw` | untested | pages are nearly a copy; risks a file the checker rejects |

## Parallelism and process

| Alternative | Feature | Status | Result |
| --- | --- | --- | --- |
| 8 workers, each pulling tasks largest-first from a shared counter | `par-counter` | pick | untested |
| rayon work stealing with per-task state | `par-rayon` | untested | |
| Pin the workers to one CPU socket | `pin-socket` | untested | needs a submission; only helps on a two-socket host |
| Huge pages for the large buffers | `hugepages` | untested | Linux only. The 128 MB link table first: 32,768 pages of 4 KB against 1,536 TLB entries |
| Exit without running destructors | `fast-exit` | untested | |
| Workers hand their buffers back with `MADV_DONTNEED` as each stage ends | `free-early` | untested | exit unmaps on one thread; this spreads it over eight, and `MADV_DONTNEED` takes the address-space lock shared where `munmap` takes it exclusive. Linux only |
| Fork a child to do the work; the parent exits once the output is written | `exit-child` | untested | the 1BRC trick, worth 7.5% to the winner. Probably nothing here: the binary is PID 1 in the container, and the kernel reaps the whole PID namespace before PID 1's exit completes. One `time docker run` settles it |
| Fixed-size blocks from a per-worker pool for every scatter, recycled as partitions are consumed | `buf-pool` | untested | no `Vec` growth copies, and the second scatter writes into warm pages instead of faulting new ones |
| mimalloc as the global allocator | `alloc-mimalloc` | untested | glibc maps and unmaps every large `Vec`; two lines to try, and pointless once `buf-pool` exists |

## Build

| Alternative | Where | Status | Result |
| --- | --- | --- | --- |
| `target-cpu=cascadelake` | Dockerfile | untested | the README permits it |
| Fat LTO against thin LTO | Cargo profile | untested | |
| Profile-guided optimisation trained on generator output during the image build | Dockerfile | untested | check the rules allow it |

## Language

| Alternative | Status | Result |
| --- | --- | --- |
| Rust with `std::arch` intrinsics, and `asm!` where the compiler's output is poor | pick | |
| Hand-written LLVM IR for the hot path | rejected | goes through the same optimiser and backend as rustc, so it cannot produce faster code than Rust that emits the same IR; it loses inlining across the boundary and needs a second version for arm64 |

## Prototypes

Standalone crates, one `main.rs` each. Run with `RAYON_NUM_THREADS=8 cargo run --release`. `pqbench` takes an output folder as its argument. Once the root becomes a workspace, add `exclude = ["prototypes"]`.
