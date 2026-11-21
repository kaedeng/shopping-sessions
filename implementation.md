# Implementation

How the picks in `decisions.md` are built, and every decision made while building them. `alternatives.md` keeps the options and their measurements; `rules.md` is the spec.

## Layout

A Cargo workspace. The root package is the binary; each stage is a crate under `crates/`. `prototypes/` and `generator/` are outside the workspace.

| Crate | What it holds |
| --- | --- |
| `types` | The contracts between stages: the ID, the hash, the records, the output columns, `Ctx` (everything read before the parse), the thread runner |
| `ingest` | Lookup files, reader, chunk plan, tokenizer, timestamps, the per-row parse of events, clicks and links |
| `links` | The union-find, the link table, the person lookup that scatters rows into person buckets |
| `dedup` | Event dedup of one partition |
| `sessions` | One person bucket: sort, sessionize, attribute, aggregate |
| `output` | The two Parquet tables |
| `shopping-sessions` (`src/main.rs`) | The stages in order |

Each crate's `lib.rs` opens with its public contract. The binary calls only that, so a crate's inside can change without touching another.

## Pipeline

Four thread scopes (`types::par`), each joined before the next starts. The first two joins are the barriers of decision "Parallelism". Before each of stages 2 to 4, `types::par::init` builds the workers' buffers for it, each worker's on its own thread (slot `alloc-parallel`). The dedup partitions and person buckets are lists carved out of one allocation per worker (`types::parts::Parts`), so their pages are faulted in by the workers' own writes, in parallel, not by the allocator.

1. **Inputs** (serial): `ingest::load_ctx` reads `manifest.json`, `products.csv` and `campaigns.csv` into `Ctx`; `Ingest::open` maps the files and cuts the chunk tasks.
2. **Parse**: task 0 parses the links files and builds the link table (`ingest.links()`, then `links::build`), and its worker then joins the others on the chunk tasks, largest first. Each worker fills its own `ParseOut`: 1,024 dedup partitions of `EventRec` and one list of valid `ClickRec`.
3. **Dedup and person scatter**: one task per partition (`dedup::partition` over every worker's piece of it, then `links::scatter_events`), then one task per 65,536 clicks (`links::scatter_clicks`). Each worker fills its own 1,024 person buckets.
4. **Sessions**: one task per person bucket, the bucket with the most events first (`sessions::bucket`). Each worker appends to its own `user_daily` columns and campaign counters.
5. **Output**: every worker writes its `user_daily` file (`types::par::each`); the main thread then sums the campaign counters and writes `campaign_daily`.

Nothing is freed between stages; with `fast-exit` the process exits as soon as the files are written.

## Slots, features and env vars

Every slot of `alternatives.md` has one module with its contract stated at the top. A pick's feature is on by default and gates that module; the root `Cargo.toml` forwards every feature to its crate. Features only add code. When a slot has more than one implementation compiled in, its env var picks at start (`types::env::pick`, the pick when unset) and selects a monomorphized path, never a branch per row.

To add an alternative: a new file beside the pick that keeps the contract, a feature in the crate's and the root's `Cargo.toml`, and the slot's selection (the tokenizer in `Ingest::open` is the worked example).

| Slot | Pick (feature) | Also built | Env var | Code |
| --- | --- | --- | --- | --- |
| IDs | `id-u64` | | | `crates/types/src/id.rs` |
| Record width | `rec-32` | | | `crates/types/src/record.rs` |
| Hash | `hash-mul` | | | `crates/types/src/hash.rs` |
| Small tables | `table-linear` | | | `crates/types/src/table.rs` (products, campaigns) |
| Parallelism | `par-counter` | | `NCPU` | `crates/types/src/par.rs` |
| Buffer allocation | `alloc-parallel` | serial build (always built: the pre-slot threading, with the same carved lists) | `SS_ALLOC=parallel\|serial` | `crates/types/src/parts.rs` (`Parts`), `crates/types/src/par.rs` (`init`), `src/main.rs` |
| Reader | `reader-mmap` | `reader-populate` (Linux only, off by default) | `SS_CHUNK_KB`, `SS_READER=mmap\|populate` | `crates/ingest/src/lib.rs` (`plan`, `Buf`, `parse_with`) |
| Tokenizer | `tok-mask` | `tok-scalar` | `SS_TOK=mask\|scalar` | `crates/ingest/src/tok.rs` |
| Timestamps | `ts-table` | | | `crates/ingest/src/ts.rs` |
| Links | `links-serial` | | | `crates/links/src/links_serial.rs` |
| Person lookup | `lookup-prefetch` | | | `crates/links/src/lookup_prefetch.rs` |
| Event dedup | `dedup-partitioned` | | `SS_PART_BITS` | `crates/dedup/src/dedup_partitioned.rs` |
| Sessionize | `sess-partitioned` | | `SS_BUCKET_BITS` | `crates/sessions/src/partitioned.rs` |
| Purchase payload | `sess-purchase-side` | | | `crates/sessions/src/purchase_side.rs` |
| Sort in a bucket | `sort-std` | | | `crates/sessions/src/sort_std.rs` |
| Attribution | `attr-sweep` | | | `crates/sessions/src/attr_sweep.rs` |
| Aggregation | `agg-dense` | | | `crates/sessions/src/agg_dense.rs` |
| Output | `out-arrow` | | | `crates/output/src/arrow.rs` |
| Exit | `fast-exit` | | | `src/main.rs` |
| Allocator | the system's | `alloc-mimalloc` (off by default) | none: a process has one global allocator, so it swaps at build time | `src/main.rs` |
| Click dedup | `clicks-nodedup` | | | no code and no feature: the step does not exist |

Which slots swap cleanly: the ones whose contract is a function or a small type (tokenizer, timestamps, sort, purchase payload, attribution, aggregation, dedup of a partition, link build, person lookup, output). The ones in `types` (ID width, record width, hash) are layouts every stage reads, so an alternative there is a sibling module switched in `crates/types/src/lib.rs` and a rebuild of everything. Alternatives that change the pipeline's shape (`links-first`, `dedup-shared`, `sess-global`) need their own path in `src/main.rs`.

Knobs, each read once:

| Env var | Default | Meaning |
| --- | --- | --- |
| `DATA_DIR`, `OUTPUT_DIR` | `/data`, `/output` | Input and output folders |
| `NCPU` | 8 | Worker threads; the scorer sets it to the CPU quota |
| `SS_TIMING` | unset | Any value prints each stage's wall time to stderr |
| `SS_TRACE` | unset | With the `trace` feature (off by default, on in `make profiling`): a file to write every task as a span, Chrome trace JSON for ui.perfetto.dev |
| `SS_CHUNK_KB` | 1024 | Parse chunk size |
| `SS_PART_BITS` | 10 | Dedup partitions = 2^n, 1 to 20 |
| `SS_BUCKET_BITS` | 10 | Person buckets = 2^n |
| `SS_TOK` | `mask` | Tokenizer, among those compiled in |
| `SS_READER` | `mmap` | `populate` pre-faults each chunk, when `reader-populate` is compiled in on Linux |
| `SS_ALLOC` | `parallel` | `serial` builds every worker's buffers on the main thread, as before `alloc-parallel` |
| `GLIBC_TUNABLES` | set by the image | glibc malloc settings, see "Decisions outside the crates" |

## Shared layouts (`crates/types`)

- **ID** (`id.rs`): a `u64`. 0 is the empty field. Top bit clear: up to 9 ASCII bytes at 7 bits each, first byte highest, so two packed IDs compare as their bytes. Top bit set: an index into a global intern list, for an ID over 9 bytes or holding a byte of 0x80 or more or a 0x00. Equal bytes always give the equal `u64`, so hashing and `==` have no special case; only order does (`id::less`). `pack_at` is branch-free unless the ID is interned, and uses `pext` when BMI2 is compiled in.
- **Hash** (`hash.rs`): `hash(id) = id * C2` for everything keyed by an ID as read (dedup partitions, link table, products, campaigns); `person_hash(id) = id * C1` for the person buckets, so the two scatters are independent. Top bits pick the partition or bucket, the bits below them the slot.
- **One time axis**: every stored `ts` is a `u32` of milliseconds after `Ctx::origin` = window start - 7 days, for events and clicks alike. A valid event has `event_lo <= ts < ts_end`, a valid click `ts < ts_end`. A sort key is `local person << ts_bits | ts` (`ts_bits` is 31 for a 14-day window).
- **`EventRec`** (32 bytes): `id`, `user`, `ts`, `ingest`, `pay`, `flags`. `flags & VALID` says every check of rules "Events" 3 and 4 passed; without it the record is a tombstone that still takes part in dedup. `ingest` is seconds relative to origin - 2^30 s, clamped to `u32` (order-preserving from 34 years before the window to 102 after). `pay` is 0 unless a valid purchase: price in the low 24 bits, category index in the high 8.
- **`ClickRec`** (24 bytes): `user` (the person after the lookup), `click`, `ts`, `campaign` index. Only valid clicks ever become a record.
- **`PersonEvent`** (16 bytes): `person`, `ts`, `pay`.
- **Output columns** (`rows.rs`): `UserDaily` and `CampaignDaily` as plain `Vec`s. Counts are `i64` so they move into Arrow buffers without a copy; the person is an ID until the writer turns it into bytes; `top_category` is a category index, 0xFF for null.
- **`Ctx`**: the window, the derived bounds, the worker and partition counts, the categories in byte order with `unknown` among them (so a larger index is a larger name), product -> category and campaign -> index maps, and the campaigns with `organic` appended last.
- **`par`**: `run(states, tasks, f)` gives each worker its own state and hands out task indexes from one atomic counter, in order, so the caller orders tasks largest-first; `each(states, f)` runs once per worker; `init(workers, parallel, make)` builds the states, each on its own thread when `parallel`. Threads are scoped and spawned per stage; the calling thread is worker 0. Every `init`, `each` and `run` is one stage of the `SS_TRACE` timeline, so the main binary's trace names seven: `parse init`, `parse`, `dedup init`, `dedup`, `sessions init`, `sessions`, `output`.
- **`parts`**: `Parts<T>` is many append-only lists carved out of one allocation: list `i` owns slots `i * room ..` of a `Vec` whose capacity is `lists * room` and whose length stays 0, a `u32` length per list, and a `Vec` per list for what outgrows its room (`push_unchecked` branches once, and the spill is a cold call). `pieces(i)` returns the carved part and the spill, which is how the dedup partitions and person buckets reach `dedup::partition` and `sessions::bucket`, both of which already take a list as pieces. `spilled()` counts the lists that spilled; `SS_TIMING` prints the totals.
- **Why `init` and `Parts` exist** (x86, `x86.md`): glibc writes a header into every chunk it hands out, so whoever allocates a buffer takes its page faults, and with transparent huge pages each first touch zeroes 2 MB. Built on the main thread as `Vec`s, the parse buffers (8 × 1,024 partitions) and the stage-2 buffers (8 × 2,048 buckets) were serial gaps of 62 and 40 ms at 20M events on the x86 box, with seven workers waiting; on the Mac they took 0.2 ms, since nothing touches 16 KB pages before they are written. Building those `Vec`s on 8 threads did not help: glibc grows its per-thread arenas with `mprotect` and `mmap`, which take the address-space lock for writing, while each fault holds it for reading through its zeroing, so they took turns. One allocation per worker and record type leaves the faults to the workers' writes, holding that lock only for reading. A state built on another thread is still freely movable: glibc's per-thread arenas only decide where the memory comes from.

## Decisions outside the crates

- **Root package is the binary**, so `target/release/shopping-sessions`, the `Makefile` and the `Dockerfile` paths did not move.
- **Links are parsed by `ingest`, serially, on the union-find's worker**, and handed to `links::build` as ID pairs (about 43 MB at full size). That keeps every row parser in one crate and `links` free of CSV.
- **Clicks get their person in tasks of 65,536**, after the dedup partitions in the same stage.
- **Person buckets run largest first by event count**, so a whale's bucket is not the last to start.
- **`campaign_daily` is one file**, written after the workers' `user_daily` files; summing eight 28K-cell arrays and writing 28K rows takes about a millisecond.
- **`fast-exit` is on by default**: `std::process::exit(0)` once the files are written, so no destructor runs. `alternatives.md` listed it as untested; skipping the frees cannot be slower.
- **Build**: `opt-level` 3 (the release default), fat LTO, one codegen unit, `panic = "abort"`. Thin LTO and profile-guided optimisation both measured the same as this on the Mac. `.cargo/config.toml` compiles amd64 with `target-cpu=x86-64-v3` (AVX2, BMI2: every AVX-512 Xeon has them), so the AVX2 tokenizer and `pext` are chosen at compile time with no run-time detection; `cascadelake` stays an untested alternative. The Mac builds with `target-cpu=native`.
- **`Dockerfile`** copies the workspace and builds once. The starter's dependency-cache layer is gone: with six crates it needs a dummy source tree, and build time is not scored. `run.sh` only `exec`s the binary, which reads `DATA_DIR`, `OUTPUT_DIR` and `NCPU` itself and needs no temp directory.
- **The image sets `GLIBC_TUNABLES=glibc.malloc.hugetlb=1:glibc.malloc.top_pad=268435456:glibc.malloc.mmap_threshold=33554432`.** The system allocator then asks for 2 MB pages for its memory when the host's transparent huge pages are in `madvise` mode, grows the heap in 256 MB steps, which is what lets `always` mode use them, and serves every buffer under 32 MB from the heap. In a Linux arm64 container at half size it takes 13 to 14% off the wall time with huge pages in `madvise` or `always` mode and 4% with them off; `alternatives.md` has the matrix, including mimalloc, which lost in every mode. It needs no code and does nothing outside glibc, so native runs on the Mac are unchanged.
- **`serde_json` stays** for the manifest; Polars is gone from the binary.
- **Integer columns are `Int64`**, as in the starter. `out-narrow` (32-bit counts) is not built.
- **A bad lookup file or an unsupported window is an error exit**, not an empty output.

## Where the code departs from `decisions.md`

These came out of measurements while building. None changes a result; each is a different mechanism from the one the decision names.

- **Hash and tables.** The link table is linear probing over 64-byte lines of four `[id, person]` slots, an ID's home being the first slot of its line. A lookup compares a whole line with no branch per slot. The prototype's slot-by-slot probing measured 0.067 s for the lookup; this measures 0.027 to 0.032 s. The small maps in `types` (products, campaigns) keep the plain layout.
- **Event dedup.** The table has 4 slots per row, not the prototype's 2.
- **SIMD.** `tok-mask` is two passes over each 8 KB batch: block masks to comma and newline positions, then one row per newline with its fields read from the positions. That is the idea of `tok-index` at batch size, with the masks still giving row validity.
- **Timestamps.** The date table reaches 6 days further than window end + 1 day, so a late `ingest_ts` stays out of the calendar fallback. F3's fast path is exactly 13 digits with no leading zero; leading zeros and other lengths take a cold path with the general rule. The form is chosen by branches, which measured faster here than computing every form and selecting.
- **Output.** The file, the schema and the string columns go through `ArrowWriter`'s parts, but the integer and date columns are written with the `parquet` crate's typed column writers, because `ArrowWriter::write` gathers every column through an index vector. `top_category`, `campaign_id` and `channel` are dictionary arrays written as plain strings. The footer carries no Arrow schema.

## Limits

- The window can be at most 42 days: a `ts` is 32 bits of milliseconds from 7 days before it. The generator's is always 14. A longer one is an error exit (`shortcut:` in `crates/types/src/ctx.rs`).
- At most 255 categories including `unknown`: the category index shares `pay` with the price. The generator has 21. More is an error exit (`shortcut:` in `ctx.rs`).
- A single line of many megabytes grows the tokenizer's scratch to about 9 bytes per byte of the line (`shortcut:` in `crates/ingest/src/tok.rs`).
- A worker's `user_id` column can hold 2 GiB of bytes per file (Arrow `i32` offsets); full size uses about 8 MB.
- Peak memory at full size is about 5 GB of buffers plus the 5.3 GB of mapped input.

## Checks

- `cargo test --release --workspace`: each crate tests its contract. `ingest`: fixture rows record by record on both tokenizers, every chunk size from 1 byte, mask against scalar on `dev/`, timestamps against a reference parser. `links` and `dedup`: random inputs against naive references, slow-path IDs. `sessions`: 3,000 random buckets against a brute-force reference written from the rules, 19 fixtures built by hand. `output`: round trip through the `parquet` reader, and `tests/polars_check.py` for what the checker's Polars sees.
- `make native`: the 36 fixtures. All pass, also as an x86-64-v3 binary under Rosetta and through the image on arm64 and amd64.
- `make check-dev` against `dev-answer/`: an exact match with the defaults and with `SS_CHUNK_KB=1`, `NCPU` 1, 2, 3 and 12, 2 to 4,096 partitions and buckets, `SS_TOK=scalar`, and a build with only the scalar tokenizer.
- Full size has no published answer. On two seeds the rules' self-check holds: summed `sessions` and `revenue_cents` are equal in the two tables, no key repeats, `organic` never assists. The arm64 and x86 binaries write identical tables.
- Each crate has an `examples/*bench.rs` for its stage alone.
- README "Benchmarking and profiling" has the whole-run tools: `scripts/bench.py` (variants in turns, median per stage and rusage), samply and perf flamegraphs, Instruments and strace syscall summaries, page-fault flamegraphs, and the `trace` timeline. The `trace` feature (`types::par::trace`) costs two clock reads and a locked push per task: 0.6% on the median total at full size on the Mac, inside the noise. Its test runs with `cargo test --release -p types --features trace`.

## Measurements

Wall time from `SS_TIMING=1`, 8 workers, warm page cache. The Mac is an M4 Pro (8 performance cores, 16 KB pages). The x86 rows are the Ryzen box in Docker Desktop on WSL2 with the scorer's flags (`x86.md`, `scripts/bench_docker.py`), on the scorer-sized set: seed 7, 20M events, 5M clicks, 1.9 GB.

| Run | Total | Inputs | Parse | Dedup + scatter | Sessions | Output | Process wall |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Full size (seed 1, 60M events, 15M clicks, 5.3 GiB), Mac | 0.73 s | 9 ms | 345 ms | 150 ms | 182 ms | 40 ms | 0.79 s |
| Half size (seed 2, 30M events, 7.5M clicks, 2.7 GiB), Mac | 0.35 s | 8 ms | 184 ms | 64 ms | 75 ms | 19 ms | 0.39 s |
| Half size, Linux arm64 container on the same Mac (4 KB pages, scorer flags, data on a Docker volume), before the glibc tunables | 0.35 to 0.37 s | 7 ms | 162 to 165 ms | 91 to 102 ms | 74 to 78 ms | 16 ms | 0.38 to 0.39 s |
| The same through the image as built, with the glibc tunables | 0.32 s | 7 ms | 146 to 149 ms | 67 to 72 ms | 74 to 76 ms | 16 ms | 0.33 s |
| x86, 20M, HEAD `5c1e1f2`, median of 6 runs each after 12 s idle (the scorer-like run) | 0.742 s | 17 ms | 452 ms | 170 ms | 86 ms | 17 ms | |
| x86, 20M, with `alloc-parallel`, the same rounds | 0.597 s | 17 ms | 365 ms | 129 ms | 79 ms | 16 ms | |
| x86, 20M, with `alloc-parallel`, median of 10 back to back | 0.561 s | 17 ms | 320 ms | 118 ms | 87 ms | 16 ms | |

- Full size on the Mac spends 4.3 s of user time and 1.2 to 1.3 s of system time, the latter mostly first-touch page faults.
- The Linux container is a VM with 12.6 GB, too little for the full set next to its page cache (runs of 1.0 to 2.2 s under memory pressure), so only the half set is a fair number there.
- Linux container, half size, warm, through the profiling image (`make linux-syscalls`, `make linux-bench`): 0.364 s ± 1.3% elapsed over 3 runs at 7.0 CPUs, 26.2K minor faults and no major ones (the glibc tunables give most buffers 2 MB pages), 3,724 syscalls (2,039 `write`, 325 `futex`, 136 `openat` and `close`, 174 `mmap`), peak RSS 5.5 GB. Of the page faults 82% are taken in `parse_with` (the mapped input and the partition buffers it fills), 11% in `run`. The profiling build times the same as release there (median total 322.5 against 329.9 ms, inside the noise).
- In the Linux container the process ends about 24 ms after its last output at half size: the kernel unmapping about 5 GB. `fast-exit` does not remove that. With the glibc tunables that gap is about 10 ms, since 2 MB pages are quicker to unmap.

## Decisions in `crates/ingest`

Decisions `decisions.md` left open. Times are from an Apple M4 Pro, warm page cache, `full/` (5.72 GB, 64.3M event lines, 15.1M valid clicks). "Single core" means one worker parsing 400 chunks into reused buffers, best of 15 runs. The machine was shared with other agents, so differences under about 1% are noise.

- **`tok-mask` shape:** two passes per batch of about 8 KB, so the batch is still in L1 for the second pass. The index pass turns each 64-byte block into comma, newline and quote bitmasks. It writes out comma and newline positions (simdjson's flattening) and, per block, its masks with the comma and quote counts before it. The row pass handles one line per newline. The commas in the line (prefix count + one masked popcount) must be N-1, and the quotes in the line must be exactly 2 × the fields that start and end with a quote. The fields are the comma positions at index `commas before the line + j`, so a malformed line needs no resync. Tokenizer alone (trivial row consumer): 7.6 GB/s single core. Index pass alone: 13.4 GB/s.
- **Batch cut:** a batch is indexed up to its byte budget, its rows run up to its last newline, and the next batch starts after that newline. The partial line is indexed twice, about 0.5% extra, and no `memrchr` is needed. If no newline fits, the batch doubles. That means a pathological multi-GB line would grow the scratch to ~9 bytes per byte of the line (`shortcut:` comment in `tok.rs`).
- **Batch size 8 KB:** single core, 8 and 16 KB tie (3.37 GB/s), 32 KB is 1.5% slower and 64 KB 3% slower. 8 KB was picked for the Xeon's 32 to 48 KB L1.
- **Flatten widths:** 8 unconditional stores per block for commas and 2 for newlines. For commas, widths 6, 7 and 8 measure equal, 5 is 15% slower, 4 is 22% slower and 12 is 4% slower. For newlines, 2 beats 4 by about 1%. Links (about 3 newlines per block) take the rare loop more often, costing about 1 ms serially.
- **NEON bitmask:** `vld4q_u8` de-interleaves the block, then a `vsri` chain and one `shrn` produce the 64-bit mask in byte order, about 9 ops per character class instead of AND-with-weights plus pairwise adds. 1.7% faster overall. A block's comma prefix count is the running flatten count, which saves one popcount per block. AVX2 uses `movemask`, one op per 32 bytes.
- **`tok-scalar` as oracle:** it uses the per-field form of the rule: a field has no quote, or it is at least 2 bytes with a quote at each end and exactly 2 quotes. That is independent of `tok-mask`'s per-line count, so the differential tests compare two formulations. On targets without AVX2 or NEON, `tok-mask` is a re-export of `Scalar`.
- **Picking the tokenizer:** `SS_TOK` is read once in `Ingest::open` and selects a pair of monomorphized functions (chunk parse, links parse) stored as fn pointers. That is one indirect call per task.
- **Over-reads:** `PAD` = 64. Each file's last lines are copied to an owned buffer: `\n`, the bytes, `\n` if the file lacks a final one, then 64 zeros. The copy starts after the last `\n` before `len - 64`. So every segment has one readable byte before it (the header's newline, or the copy's leading `\n`) and 64 after it. A block load reads at most 63 bytes past the end, a field at most 27 (F2 `ts`). A file with at most 64 data bytes is copied whole and its mapping dropped.
- **Chunk cut:** a chunk ends after the first `\n` at or after `start + chunk - 1`, so a 1-byte chunk is one line per task. The copied tail is its own small task.
- **Input files:** names ending in `.csv` and not starting with `.` (glob semantics; the rules say `*.csv`), regular files (symlinks followed), in name order. 0-byte files are never mapped. A file with no `\n` is all header.
- **Task order:** stable sort by byte length, largest first, event and click chunks mixed.
- **Lookup files:** `products.csv` and `campaigns.csv` go through `Scalar` with N = 2, so the CSV dialect (quotes, malformed lines) applies. Rows with an empty key are skipped, since `IdMap` rejects the empty ID.
- **Date table:** a perfect hash of bytes 3..11 (`Y-MM-DDT`, `T` included). Odd multipliers are tried until the dates are injective, with at least 4 slots per date (128 to 256 slots of 32 bytes). Both bytes 0..8 and 3..11 are compared, so the whole `YYYY-MM-DDT` must match exactly. A free slot holds a date that hashes elsewhere, so a field of NUL bytes cannot hit it. The span is day(origin) - 1 to day(window_end - 1) + 1, plus 6 more days so a late event's `ingest_ts` stays out of the fallback.
- **Timestamp form, branchy:** byte 10 is `T`: F1 (length 20, `Z`), else F2 (length 25) behind a second branch. Otherwise the F3 fast path: exactly 13 digits, no leading zero, 5 + 8 digit SWAR. Everything else goes to one cold path: bad values, leading zeros, other lengths, a window whose bounds are not 13 digits. Single core: branch-free F1/F2 + F3 select 29.4 ns/row against 25.9 branchy, F2 behind its own branch 23.6 against 24.3, branch-free F1 + F3 with F2 branched 10% slower than branchy. Not measured on the Xeon: `ts-select` there is worth one submission if the parse dominates.
- **SWAR checks:** `HH:MM:SS` and `±HH:MM` are validated by adding per-byte constants that carry into bit 0x10 exactly when a digit exceeds its maximum (hour tens 2, minute and second tens 5, units 9), plus a colon mask. Then hour ≤ 23 is checked numerically.
- **`ingest_ts`:** F1 only (length 20, `Z`) through the table. A miss goes to `types::time::parse_f1`. If that fails too (including an F2 `ingest_ts`, which the rules exclude), the event gets `ingest = u32::MAX` and no `VALID`.
- **Late check in ms:** `ingest_ms - ts_ms <= 86_400_000`, both relative to `origin`.
- **Price:** up to 8 bytes are right-aligned in one word, filled with `'0'`, then SWAR-checked and parsed. Empty or longer than 8 bytes (only leading zeros can be valid there) takes a cold path.
- **Event type:** one `u64` load, compared masked to `view` and `cart` (length 4) and whole to `purchase` (length 8).
- **Clicks:** the campaign probe runs only after the other checks pass.
- **Buffer sizing:** rows are estimated from the newline density of the first 256 KB of the largest segment. Each partition gets a room of `rows / workers / parts * 5/4 + 16` in the worker's `Parts` and the click list `rows / workers * 5/4 + 16`. The OS maps lazily and `Parts::new` touches only the region's first page, so `new_out` takes under 1 ms per worker (`SS_TRACE`, x86); the pages fault in as the parse writes them. No partition spills on the 20M set (`SS_TIMING`); full size is not checked.
- **`SS_PART_BITS` range:** `open` rejects values outside 1 to 20. 0 would shift by 64 into an unchecked index.

### Measurements

- `examples/parse_bench.rs` on `full/` with `NCPU=8` (open + parse + links, fresh mapping and fresh buffers each run, best of 5): `tok-mask` 0.335 s, 17.0 GB/s, 2.13 GB/s per worker. `tok-scalar` 0.698 s, 8.2 GB/s, 1.02 GB/s per worker. Both produce 64,305,628 event lines (62,913,797 valid), 15,132,359 valid clicks and 2,655,321 links.
- Warm (mapping already faulted in, buffers reused): `tok-mask` 3.37 GB/s single core and 25.4 GB/s on 8 workers (3.2 per worker). From that warm 238 ms, a fresh mapping adds 61 ms and fresh output buffers 70 ms (first-touch page faults on 16 KB pages). Both costs belong to other slots (`reader-populate`, `hugepages`, `buf-pool`).
- Single-core cost split: index 6 ns/row, fields 4, `ts` 4.2, `ingest_ts` 1.9, partition push 1.4, the rest (ID packing, type, flags) about 5.

## Decisions in `crates/links`

Beyond what `decisions.md` fixes (Links, Hash and tables, IDs). Times are from an Apple M4 Pro
on a shared, noisy machine: `cargo run --release -p links --example linksbench`, best of 5,
62M rows and 3.4M linked IDs for the lookup, the real `full/links` pairs (2,655,755 pairs,
3,590,627 distinct IDs) for the build. Nothing here has run on amd64.

### Table (`LinkTable`, shared by both slots)

- **One cache line holds four `[id, person]` slots, 64-byte aligned, and an ID's home is the first
  slot of the line its hash's top bits pick.** It is still linear probing (on to the next line
  when one is full), but a line's used slots are always a prefix of it, so "no match and the last
  slot is free" ends a search. `person()` compares all four slots with no branch and branches once
  per line. On the real pairs 10.4% of lines are full and 2.5% of IDs sit past their home line,
  so with 44% of rows missing, about 6% of rows go on to a second line. Lookup cost over a bare scatter, 8 threads:
  plain linear probing with a branch per slot (the prototype's loop) 0.067 s; branch-free first
  slot 0.053 s; whole line 0.027-0.032 s. One thread: 0.65, 0.47, 0.25 s. Same-day prototype
  under the same load: 0.077-0.085 s against 0.029-0.030 s.
- **The line test is one integer, `min(last key, key ^ id for each slot) == 0`, and the person
  comes from `select_unpredictable`.** Written as `found | free`, LLVM's x86-64 backend split it into
  five data-dependent branches (aarch64 used a `ccmp` chain, which is why the M4 did not show it).
  Checked in the x86-64-v3 assembly: four `cmov`s for the person, four for the test, one `je`.
- **Size: `next_power_of_two(2 * pairs / 3)` lines, so at most 3/4 full even if every pair brought
  two new IDs; the real pairs fill it 0.43** (8.39M slots, 128 MB). Sized from the pair count
  because the numbering table becomes the lookup table, before the distinct count is known.
- **Allocated with `Box::new_zeroed_slice`.** Above 16-byte alignment std zeroes it with a memset
  instead of calloc: about 6 ms at full size, on the links worker beside the parse, so under 1 ms
  of wall time. The `hugepages` alternative needs its own aligned mapping and would replace this.

### Build (`links-serial`)

- **Three passes over dense nodes:** number every ID into the final table (slot value = dense
  index, a node pushed per new ID, self-links dropped from the edge list), Rem's unions over the
  `u32` edge list, then a pass over the table that writes each slot's root ID over its index.
  The edge list (21 MB) costs a sequential write and read, and in exchange the unions can
  prefetch their nodes ahead.
- **A node is `{parent_id: u64, parent: u32}` (16 bytes): it holds its parent's ID, so Rem's
  comparison of the two parents reads only the two nodes it is at. A root holds its own ID,** so a
  splice is a plain copy of the other side's node. `less` (byte order, slow IDs included) is the
  only ID comparison; the "same parent" test compares indices.
- **Numbering scans the line without a branch per slot** (the same `min`/`select` form, plus a count
  of used slots for the insert position), and branches once on hit or insert: 0.050 s down to 0.037 s.
- **Naming: the node is prefetched 16 lines ahead and its parent 8 lines ahead (two stages), and
  "the node or its parent is the root" returns at once.** On the real pairs after the unions,
  32% of nodes are roots, 56% depth 1, 10% depth 2, 1% deeper; deeper paths are walked and pointed
  at the root. One prefetch 4 lines ahead: 0.065 s; two stages: 0.032 s; with the shallow return: 0.027 s.
- **Prefetch distances: 16 pairs when numbering, 32 edges in the unions.** 8 and 64 were slower,
  16 and 32 within noise of each other.
- **Pairs with an empty side are skipped.** Ingest drops them already (rules, Linked IDs 1). Skipping
  them keeps the empty ID, which matches a free slot, out of the table, so every index stays valid
  for the unchecked indexing whatever the input.
- **Result: `build` 0.078-0.081 s on one thread** (first working version 0.120 s): numbering
  0.037, unions 0.015, naming 0.027. Pre-faulting the memory saved about 8 ms of numbering here
  (16 KB pages); Linux's 4 KB pages will cost more. While the build is shorter than the parse
  it costs about 1/8 of its own time in stage-1 wall time, since its worker parses less.

### Lookup and scatter (`lookup-prefetch`)

- **64 rows ahead, as decided; 32, 64 and 128 measured the same on the line table.** The first 64
  rows of each call are prefetched before the loop, since each call is one dedup partition.
- **Tombstones are neither prefetched nor looked up.**
- **One generic loop serves both `scatter_events` and `scatter_clicks`.** It asserts
  `bucket_bits` in `1..=32` and `out.lists() == 1 << bucket_bits` once per call (`bucket_bits = 0`
  would shift by 64), then appends to the buckets unchecked (`Parts::push_unchecked`).

### Ideas not taken, for later

- A filter in front of the table (`lookup-filter`) matters less now: a miss costs the same one
  line as a hit and no mispredict.
- The same line layout would suit `types::table::IdMap` (products per purchase, campaigns per
  click), which still branches per slot.

## Decisions in `crates/dedup`

Beyond what `decisions.md` fixes (Event dedup, Hash and tables). Times are from an Apple M4 Pro
on a shared, noisy machine: `cargo run --release -p dedup --example dedupbench`, 62M 32-byte
records (every 32nd a copy), 1,024 partitions of about 60.5K rows, best of 5. `NCPU=1` runs
were stable to about 2%, 8-thread runs varied by up to 40% with the load from other work.
Nothing here has run on amd64.

- **Table: 4 slots per row, rounded up to a power of two (the prototype used 2).** At full size
  a partition holds about 60.5K rows, just under 2^16, so 2x rounds to 2^17 and ends 0.46 full,
  while 4x ends 0.23 full. Fewer collisions mean fewer reads of the kept list and fewer mispredicts
  on the "slot free?" branch. One thread: 2x 0.322 s, 4x 0.223 s, 8x 0.200 s. 8 threads: 2x
  0.045-0.073 s, 4x 0.032-0.051 s, 8x 0.028-0.045 s. 4x is the pick because its table is 1 MB,
  the Xeon's whole L2, while 8x's 2 MB cannot fit. **Sweep 2x/4x/8x (`ROOM`) on amd64 first.**
- **Each row's slot is prefetched 16 rows ahead in its piece.** One thread, at 2x: 0.336-0.341 s
  without, 0.315-0.321 s with; 8, 16 and 32 ahead measured the same.
- **The kept list is a `Vec` with `push`, reserved to the partition's row count first.** A slot
  holds 1 + the index it was given right after a push, so the unchecked read of the kept list is
  in bounds whatever the `pieces` iterator yields the second time.
- **A later copy replaces the kept one only on a strictly smaller `ingest`.** Copies that tie are
  identical (rules), so keeping the first seen is the same answer.
- **The table is cleared with `clear` + `resize` (a 1 MB memset per partition)**, not by clearing
  only the used slots, which would cost one probe per kept row.
- **Asserted once per partition: `part_bits + table bits <= 64`, and rows below `u32::MAX`.**
- **Tags in the slots' spare high bits, so a collision with another key skips the read of the
  kept list: measured, not taken.** One thread, 0.313-0.320 s against 0.316-0.318 s without, since
  this Mac's 16 MB L2 holds the kept list. On the Xeon the kept list (about 1.9 MB) spills its
  1 MB L2, so they might pay there. At 4x the collisions they would save are already rare.
- **Result: 0.028-0.051 s on 8 threads** (alternatives.md's prototype figure 0.047-0.065 s;
  same-day prototype at 2^11 partitions under the same load 0.052-0.088 s, against 0.035-0.043 s).

## Decisions in `crates/sessions`

What `crates/sessions` decided beyond `decisions.md` (Sessionize, Attribution, Aggregation). Times are from `examples/sessions_bench.rs` on the M4 Pro, 8 threads, while other agents loaded the machine (load average 3 to 14), so differences under about 10% are noise. Per-thread splits come from temporary per-phase timers (since removed): thread CPU time, summed over a round's buckets, divided by 8.

- **Slot files.** `partitioned.rs` (`sess-partitioned`), `purchase_side.rs` (`sess-purchase-side`), `sort_std.rs` (`sort-std`), `attr_sweep.rs` (`attr-sweep`), `agg_dense.rs` (`agg-dense`), each stating its contract at the top. `lib.rs` keeps the public contract and forwards to them. Everything per-row or per-session is `#[inline(always)]` and generic (closures, no `dyn`).
- **Sort contract:** `sort(keys, key_bits, scratch)`. `sort-std` ignores `key_bits` and `scratch`; they are there so `sort-radix` knows its passes and has a buffer without changing the caller. `key_bits = ts_bits + bits(persons)`.
- **Purchase slot contract:** `clear(rows)`, `extend((key, pay)…)`, `sort(key_bits)`, `key(i)`, `purchases(session rows, f) -> summed price`. It is shaped so `sess-wide` (sorting `(key, pay)` records) can keep it: `purchases` gets the session's row range, which the side list does not need but a wide layout does.
- **Bucket-local person table:** open addressing with 16-byte `(person, local)` slots and linear probing, sized `next_pow2(rows / 8)` (at least 64) per bucket. The slot is picked by the `person_hash` bits just below the bucket bits. Person 0 (the empty ID, never a person) marks a free slot. It doubles and rehashes from the `local -> person` list when more than half full. At about 30 rows per person it seldom grows, and the average bucket's table is 128 KB.
- **Local numbers in first-seen order.** Every local number has at least one row and keys sort by local first, so the scan walks persons `0..n` in order with no person lookup. Each person's click slice is found by local number.
- **Gather with `Vec::extend` per piece**, not a push per row: over a slice iterator, std keeps the length in a register (trusted length), while a push loop stores it on every row. Gather + intern went from 27.5 to 12 ms per thread (the prototype's gather + intern was 18 ms wall).
- **Purchase side list:** `(key, pay)` pairs, sorted with `sort_unstable_by_key` (equal keys are equal timestamps of one person, so one session; their order is free) and ended by a `u64::MAX` sentinel. Each session takes the purchases whose key is at most the session's last key: one compare per session when there are none.
- **One compare per row for sessions.** A `u64::MAX` sentinel follows the sorted keys, and the inner loop is `key[i] - key[i-1] <= 1_800_000`, with no bounds check and no separate person test. A person change is always a gap over 30 minutes, because every `ts >= event_lo` = 7 days (a `const` assert says so). The person-change test runs once per session.
- **Clicks are grouped by a counting scatter, not sorted per bucket.** Pass 1 probes each click's person (lookup only, after every event is interned, so a person without events gets no number and its clicks are dropped there), counts per person and keeps each click's local number. Pass 2 re-reads the pieces (the iterator is `Clone`) and scatters 16-byte `Click { ts, campaign, id }` into per-person slices. Click gather: about 5.5 ms per thread for 1.9M clicks.
- **Per-person click order:** `ts` ascending and, among equal `ts`, the smallest `click_id` (`id::less`) last. The window's last click is then the credit, so the sweep never handles ties. Each slice is sorted when the scan reaches its person (still in cache). Up to 16 clicks: `sort_unstable_by` with that comparator. Above 16: sort `ts << 32 | position` as plain `u64`, gather, then put each equal-`ts` run in `click_id` order (rare). Microbenchmark, ns per click, comparator vs index sort: 8 clicks 5.6 / 5.9, 16 7.3 / 7.1, 24 10.8 / 4.6, 430 13.6 / 6.5, 150,000 18.1 / 10.5.
- **Sweep state in locals.** `lo`, `hi` and the count/position arrays are local in `Attr::session`; held in `self`, they were reloaded after every `push`. `at[campaign]` (the campaign's place in the active list) makes removal `swap_remove`. `end_person` zeroes only the active campaigns' counts.
- **Counters are day-major**, `[day][campaign]`, so all of a session's assists fall in one day's row (8 KB at 2,001 campaigns). Credited cells are 16 bytes (`sessions: u32`, `purchasing: u32`, `revenue: u64`, one cache line per credit) and assists a separate `u32` array. Counts are `u32` per worker (a worker sees under 2^32 sessions), summed into `i64` at the end. `assists(day)` returns a closure over the day's row, so the row's pointer and length stay in registers through the assist loop.
- **Purchasing session** = its summed price is above 0 (every valid price is at least 1).
- **`top_category`:** a 256-entry price sum per category index plus a list of the categories touched that day. At the day's flush, the highest sum wins and a tie goes to the smaller index (the list is in arrival order, so the tie rule is explicit). Only touched entries are reset.
- **`user_daily`:** a `Day` accumulator that writes the previous person-day when a session's day differs, and the last one at the person's end.
- **Bounds:** unchecked only in per-row loops (table probes, key reads, the click scatter), each with a SAFETY line. Indexing by campaign and day happens per session or per click and stays checked, which also turns a broken input contract (a campaign or day index out of range) into a panic instead of corruption. `debug_assert`s check the other input contracts (event `ts` in `[event_lo, ts_end)`, person not 0).
- **Measured and dropped:** reserving `user_daily` capacity up front (the scan was 24 to 25 ms per thread either way); a plain `ts` key for the click sort (about 5 ms, within noise, and wrong on ties without a fix-up anyway).
- **Measured and kept as decided:** the assist loop (one counter increment per active campaign per session, 14.35 assists per session in the bench, 121M in all) costs 3 to 5 ms per thread, found by switching it off. Counting each campaign's active stretch per day when it leaves the window would do about 10 times fewer increments for heavy persons. It changes the attribution slot's contract, so it is a candidate alternative (`attr-interval`), not built.
- **Tests:** a brute-force reference written from the rules' text (plain `HashMap`s, quadratic window scan), compared on 3,000 random cases. Those cases have windows starting at midnight or mid-day and lasting up to 20 days; events clustered at gaps of 0, 1 ms, 29:59.999, 30:00.000 and 30:00.001 and around midnights; equal timestamps; 25% purchases with tie-prone prices; clicks at exactly the start, 1 ms after, exactly 7 days before, 7 days ± 1 ms, same-millisecond pairs with other IDs and campaigns, exact copies, persons with clicks but no events; short packed and long slow-path IDs mixed; 1 to 3 buckets in 1 to 4 pieces (some empty) over 1 to 3 workers. A table-grow test (4,000 persons). Planting each of nine likely bugs (exact-30-minute split, 7-day bound, click-at-start, tie order, category tie, count reset, missing ID tie-break, day flush, rehash) fails at least one test. Fixture tests build the surviving records by hand and compare with `fixtures/<case>/expected/*.csv` directly: E1, E2, E3, E8, E9, E14, E16, L1, A1 to A5, A8 to A12, L4.
- **Bench data:** events exactly as `prototypes/sortbench` `SESSIONS=1` (same person and session draws, 8,472,366 sessions), 3% purchases. Clicks follow the generator's rules (`generator/src/plan.rs`, `user.rs`): 65% of people click, aimed at 45% of their sessions; offsets are exactly 7 days, 7 days + 1 ms, 0 or 1 ms after at 0.4% each, 1 s to 6 days on a log scale, or 25% beyond 7 days; 1% each to the 5 whales; 1% same-millisecond pairs; 4 campaigns take 30% of 2,000. 62.3% of sessions are credited (the generator aims at about 60%).
- **Bench result** (best of 5, wall): whole stage 0.140 to 0.160 s; events only 0.107 s, against the prototype's gather + intern + sort + scan at 0.080 to 0.084 s run the same hour (0.083 to 0.091 s in `alternatives.md`). The prototype's scan only counts sessions; this one builds both tables. Split per thread with clicks: gather + intern 12 ms, sort 65 to 70 ms, click gather 5.5 ms, scan 55 to 60 ms (about 23 of it is the events' sessionize and aggregation; the click sort and the sweep make the rest).

## Decisions in `crates/output`

Measured with `examples/output_bench.rs` (7M `user_daily` rows, 8 files on 8 threads, best of 5) on the M4 Pro, unless stated. "Encode only" is the same run with every part file a symlink to `/dev/null` (`NULL=1`), so the kernel write is left out. macOS timings vary by about 10% run to run; the numbers below are from quiet runs, back to back.

- **Integer columns skip `ArrowWriter::write`.** `ArrowWriter` gives the schema conversion, the file writer and the string column writers (`into_serialized_writer`, `create_column_writers`); the four `Int64` columns and `day` go to the parquet crate's typed column writers as slices (`next_column().typed::<Int64Type>().write_batch(values, None, None)`), which copy them into PLAIN pages. `ArrowWriter::write` materializes a `Vec<usize>` of `0..rows` for every column and gathers each 1,024-row mini-batch through it into a temporary `Vec` before the same copy (14% of its samples in level building alone). Same file layout either way. Encode only 0.028 → 0.021 s; with the files 0.057 → 0.043 s.
- **The string columns stay on arrow-rs's `ArrowColumnWriter`.** Its byte-array encoder reads a `StringArray` directly; the typed writer wants a `ByteArray` (a refcounted `Bytes`) per row. What it still costs and no property removes: the `0..rows` index vector (and the `Vec<i16>` definition levels for `top_category`), and one `memmove` call per value as it appends to the page.
- **`top_category`, `campaign_id` and `channel` are `DictionaryArray`s over the names**, the keys being the moved `Vec<u8>` / `Vec<u32>` (the campaign keys shared by the two columns). Dictionary encoding is off, so the writer looks up each name and writes it PLAIN: the same bytes as from a `StringArray`, and the rows' strings are never built. `top_category`'s nulls are the keys' validity bitmap (`BooleanBuffer::collect_bool`, 64 rows per word). Encode only, on the `ArrowWriter::write` path: 0.028–0.032 s against 0.031–0.034 s for a built `StringArray`. `DictionaryArray::try_new` checks the keys (one pass over bytes), so a bad index from upstream is an error, not undefined behaviour.
- **No Arrow schema in the footer** (`with_skip_arrow_metadata(true)`). Readers type the columns from the Parquet logical types alone, so the dictionary-backed columns read as plain strings (Polars `String`, arrow-rs `Utf8`), never as `Categorical` or `Dictionary`, and the footer skips an IPC-encoded, base64 copy of the schema.
- **`user_id` loop:** 9 bytes stored per row (`spread`'s word and ninth byte) into a buffer of `rows * 9`, the end advanced by the length, the `i32` offset stored before each row, all through raw pointers; the only branch is the `SLOW` test, whose cold path unpacks the interned bytes and reserves 9 bytes per remaining row so the invariant (capacity ≥ end + 9 × rows left) holds. One check that the total fits `i32` offsets (2 GiB per file) at the end: the offsets only grow, so the last one is the largest. `StringArray::new_unchecked`: packed IDs are ASCII, interned ones come from input that is UTF-8 by the rules. 1.0–2.2 ms per worker for 875K rows.
- **Properties:** dictionary off, statistics `None`, offset index off; compression is already uncompressed and bloom filters already off by default. Page size limit (1 MB), page row limit (20,000) and write batch (1,024) stay at the defaults: row limits from 8K to 131K measured within noise, 4K was slower (0.033–0.036 s encode only, `ArrowWriter::write` path), and pages of 8 MB or more were much slower (0.080 s against 0.058 s with the files; 0.047–0.055 s against 0.028 s encode only), because each page's buffer starts empty, grows by doubling and touches fresh memory.
- **One row group per file**, opened once by hand, so no row limit can split it.
- **An empty table writes no row group:** the footer alone, with the full schema. Polars 2.1.0 reads it as 0 rows with every dtype right. A row group of zero rows would have column chunks without pages, which readers handle less uniformly.
- **The sink is the `File` itself.** The parquet crate wraps it in an 8 KB `BufWriter`, which coalesces the small page headers and hands each page (larger than 8 KB) straight to `write(2)` without a second copy, about two syscalls per page. A 1 MB `BufWriter` around the file: same time (0.043–0.045 s); an 8 MB one: 0.045–0.064 s; the whole file in a `Vec` and one `write`: 0.058–0.065 s (one more copy of ~45 MB per file into fresh pages).
- **Only `top_category` is nullable** (`OPTIONAL`); every other column is `REQUIRED`, so it has no definition levels, which is also what lets the typed writers take a plain slice.
- **Where the rest of the time goes** (phase timings per worker on the `ArrowWriter::write` path: encode 17–24 ms, then `write(2)` of ~45 MB): about half the wall time is the kernel write. On this Mac the 8 files' `write(2)` calls serialize in the kernel: per-worker write time steps from 6 ms to 36 ms across the 8 workers. Freeing the page buffers costs `madvise` in macOS's allocator (20% of encode-only samples); glibc reuses arena memory instead. Neither is measured on Linux.
- **Bench data:** the generator's 20 category names plus `unknown` (`generator/src/plan.rs`), so `top_category`'s bytes match real output; otherwise the prototype's shape (about 3.5 rows per 9-byte ID, 10% null categories).

