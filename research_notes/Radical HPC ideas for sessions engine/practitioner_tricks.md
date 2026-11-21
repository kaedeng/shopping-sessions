# Practitioner performance tricks (1BRC, Gjengset, Groot Koerkamp, Lemire/Muła, Muratori, DB engines) mapped onto the sessions engine

How to read this file:
- **Slot names** (`tok-lanes`, `dedup-filter`, `lookup-distinct`, ...) are the engine's own alternative slots in [alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md). The x86 measurements are in [x86.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md).
- **Converting one-core CPU to wall time:** this file assumes about 5× effective parallelism on 8 workers. `perf stat` shows 5.99 CPUs busy on average on the x86 box ([x86.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)), and the scorer's 4 cores × 2 SMT threads will likely give less. Every "expected effect" below is an estimate unless it is credited to a source.
- **The leading competitor's design** is cited from the assignment brief only. No code from unlicensed repositories was read or copied.

## Q0. Ranked catalogue: which practitioner tricks transfer to this engine, in priority order

### Takeaway
The engine already has most classic 1BRC micro-tricks: mmap, SIMD/SWAR parsing, IDs packed into a `u64`, each key hashed once, prefetched probes, write-combined scatters, work-stealing 1 MB chunks and fast exit. The largest remaining wins are structural ways to skip work:
- move each event once;
- prove most event IDs unique with a cheap filter, so only the rest go through the exact dedup table;
- look up the person once per distinct user ID;
- sort only 4-byte times per person, using sorting networks for small persons and 30-minute bins for heavy ones.

Of the 1BRC micro-tricks, only three still have measured headroom elsewhere: interleaved cursors (ILP), AVX-512 dispatch and branch engineering driven by data statistics. Judge all three on a 4-core × 2-SMT setup, because SMT erases most ILP gains.

### Cited Findings
- ILP from interleaving cursors: 4 cursors per thread took 1BRC from 1.34 to 1.23 s, reordering took it to 1.11 s and full interleaving to 1.05 s. IPC rose from 2.20 to 2.95 (i7-10750H, 6 cores) — [Groot Koerkamp, 1BRC](https://curiouscoding.nl/posts/1brc/)
- SMT interacts with ILP. Hyperthreading gave about 10% before the ILP work. Afterwards, 12 threads with HT ran 0.92 s against 0.90 s for 6 threads without HT — [Groot Koerkamp, 1BRC](https://curiouscoding.nl/posts/1brc/)
- Hyperthreading gave about 62% on most machines in lehuyduc's C++ 1BRC (38% on one). He kept changes that helped at 2× threads even when they slightly hurt at 1× — [1BRC discussion #138](https://github.com/gunnarmorling/1brc/discussions/138)
- Branch engineering from data statistics: a simulated 2-bit predictor mispredicted 50% on `nameLen > 8` and 2.5% on `nameLen > 16`. Restructuring took 2.4 s to 1.8 s, branch misses from 0.657 to 0.084 per row and IPC from 1.89 to 2.19 — [Topolnik, QuestDB](https://questdb.com/blog/billion-row-challenge-step-by-step/)
- Perfect hashing gave 23 → 17 s single-threaded in an early 1BRC stage, and a faster PtrHash lookup later gave 1.67 → 1.55 s — [Groot Koerkamp, 1BRC](https://curiouscoding.nl/posts/1brc/)
- Batched, prefetched lookups: PtrHash takes 12.5 ns/query in a plain loop against 8.8 ns streaming (10⁹ keys). In the static search tree post, batching took 115 → 45 ns and prefetching 45 → 30 ns — [PtrHash](https://curiouscoding.nl/posts/ptrhash/); [S-trees](https://curiouscoding.nl/posts/static-search-tree/)
- Sorting networks are at least 25% faster than insertion sort on small sets — [Marianczuk, arXiv 1908.08111](https://arxiv.org/abs/1908.08111)
- The 1BRC exit cost was about 100 ms of unmapping at process exit, which Würthinger hid with a child process — [Topolnik](https://questdb.com/blog/billion-row-challenge-step-by-step/)
- When a PID namespace's init exits, the kernel kills every other task in the namespace and waits until they are all gone — [Linux kernel/pid_namespace.c](https://raw.githubusercontent.com/torvalds/linux/master/kernel/pid_namespace.c)
- The engine's status for these ideas: `tok-lanes`, `tok-avx512`, `ts-simd`, `dedup-filter`, `lookup-distinct`, `lookup-filter`, `links-first`, `hugepages` and `reader-pread` are untested. `sess-bins` and `sort-radix` are "faster in part". PGO showed no gain on the Mac and is untested on amd64. `fast-exit`, `scatter-wc`, `alloc-parallel`, prefetching and 1 MB work-stealing chunks are already built — [alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)
- The leader moves each event once as a 12-byte record keyed by raw user ID. It dedups with 16-byte side records and a small bitmap that proves about 97% of IDs unique, and looks up the person once per distinct user ID. It sorts 4-byte times with small sorting networks, counts heavy users in 30-minute bins, and uses PGO and runtime-selected AVX-512 — (assignment brief)

### Inferences
The ranked catalogue follows. Each entry gives the trick, its source evidence, the mechanism, where it applies, the expected effect, the risks and a test.

**0. Do this first: measure on the scorer's shape (4 cores × 2 SMT threads, Intel, with counters).**
- **Evidence.**
  - Ragnar's HT result reverses after the ILP work.
  - lehuyduc tuned for 2× threads.
  - Topolnik steered by instructions per row (2,031 → 945 → 607 → about 120) and branch misses per row ([QuestDB](https://questdb.com/blog/billion-row-challenge-step-by-step/)).
  - The WSL2 box has no PMU and gives each worker its own core ([x86.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)).
- **Mechanism.**
  - Run `docker run --cpuset-cpus=` on 4 sibling pairs, read from `/sys/devices/system/cpu/cpu*/topology/thread_siblings_list` inside the Docker VM. Two workers then share each core's L1/L2, as on the scorer.
  - For instructions and branch misses per row, run once on a bare-metal Intel AVX-512 host (a cloud "metal" instance).
- **Where.** Every decision about ILP, partition and bucket counts, and AVX-512.
  - The dedup tables were sized at about 1 MB, a whole Cascade Lake L2. Under SMT, two of them compete for that L2.
- **Expected effect.** No direct speedup, but it stops us adopting changes that only win when a worker has a core to itself.
- **Risk.** WSL2 may not expose a faithful sibling topology.
- **Test.** Run the baseline under 8c×1t and 4c×2t. If dedup degrades more than the parse under 4c×2t, re-sweep `SS_PART_BITS` (2,048 or 4,096) and `ROOM` there.

**1. Move each event once, and prove most event IDs unique with a small per-partition bitmap; send only the rest through exact dedup.**
- **Evidence.**
  - The leader does this (brief).
  - DuckDB keeps 1–2 bytes of hash in its pointer array, so most probes avoid a random payload read ([DuckDB](https://duckdb.org/2022/03/07/aggregate-hashtable.html)).
  - In the engine, 24.8% of the dedup stage's samples sit on the 16-byte person-scatter store. `rec-16` scatters in 0.033 s against 0.061 s for `rec-32` (Mac), and `dedup-filter` is untested ([alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).
- **Mechanism.**
  - In the parse, write:
    - (a) a compact payload of 12 bytes or less (`ts` offset, a type/product/price code, a user reference) into its sessionization bucket;
    - (b) a 16-byte side record (packed `event_id`, `ingest_ts`, back-reference) into its dedup partition.
  - Each dedup partition holds about 21K side records at 1,024 partitions, about 340 KB, so it stays in L2.
    - Pass 1 sets a "seen" bit and, on a repeat, a "seen-twice" bit in bitmaps indexed by spare hash bits.
    - Pass 2 accepts every record whose seen-twice bit is clear: no other record in the partition shares its bits, so it is provably unique. Only flagged records reach today's exact table.
  - Each losing copy is marked dead through its back-reference, or in a per-line bit array, before sessionization.
- **False-positive math.** A unique ID is wrongly flagged with probability about 1 − e^(−n/m).
  - n = 21K and m = 2^20 bits (128 KB, L2): about 2%.
  - m = 2^18 (32 KB, L1): about 7.7%.
  - With about 4% of events having copies, roughly 90–94% of side records skip the table.
- **Where.** Replaces most of "dedup+scatter 0.61 s". It also shrinks the parse's output from 32 bytes to 12 + 16 bytes per event, and cuts fresh memory, which costs double after idle on the box.
- **Expected effect.** About −0.3 to −0.45 s of one-core CPU, roughly −60 to −90 ms wall.
- **Risks.**
  - Copies of one event can name different users (fixture E4), so the dead mark must reach the payload wherever it went.
  - The smallest-`ingest_ts` winner must be chosen before validation (fixture E5).
  - Back-references add 4 bytes per record and a random write for about 4% of rows.
  - The bitmaps must be cleared or epoch-tagged per partition.
- **Test.** In `prototypes/dedupbench`, measure the flagged fraction against m, and time it against `dedup-partitioned` at 1,024 and 2,048 partitions under 4c×2t.

**2. Look up the person once per distinct user ID, or resolve links first, instead of once per event.**
- **Evidence.**
  - The leader looks up the person once per distinct user ID (brief). That is 21.6M event rows against 1.29M distinct IDs, about 17× fewer probes.
  - DataFusion's 2–3× grouping speedup came from mapping each row to a dense group index first, then running type-specialized vectorized updates over columnar per-group state ([DataFusion](https://arrow.apache.org/blog/2023/08/05/datafusion_fast_grouping/)).
  - In the engine, 9.8% of the dedup stage's samples stall right after the link-table prefetch ([x86.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)). `lookup-distinct` and `links-first` are untested.
- **Mechanism, variant (a).** Bucket events by raw user-ID hash in the parse.
  - Per bucket, intern user IDs into dense local indices with a small L1/L2 table.
  - Probe the link table once per distinct ID, with prefetching.
  - Forward only linked users' rows, or their compact summaries, to the person buckets. Singleton users sessionize in place.
- **Mechanism, variant (b), `links-first`.** Run union-find before the event parse, look up the person inside the parse loop with prefetching, and scatter straight into person buckets. The post-dedup pass and its scatter disappear.
- **Expected effect.** About 20M fewer random probes into a 64–128 MB table. Perhaps −50 to −150 ms of one-core CPU; variant (b) also removes one whole pass over the events.
- **Risks.**
  - Variant (a) adds a third scatter for about a third of the rows (repo estimate).
  - Variant (b) puts union-find before the parse. It takes about 0.08 s on one thread at full size, unless overlapped with the click and lookup parses.
- **Test.** A `lookupbench` variant per option, timing parse and dedup together.

**3. Per person, sort only 4-byte times: sorting networks for small persons, 30-minute bins for heavy ones.**
- **Evidence.**
  - Marianczuk: sorting networks are at least 25% faster than insertion sort on small sets. As the base case of Super Scalar Sample Sort they gain "over 6%", less because of L1 I-cache limits ([arXiv 1908.08111](https://arxiv.org/abs/1908.08111)).
  - Bingmann, Marianczuk and Sanders (2020) are reported to find networks with hand-coded conditional moves 2.4–5.3× faster than insertion sort. This figure comes from a search-result snippet and was not verified in the paper ([arXiv 2002.05599](https://arxiv.org/pdf/2002.05599)).
  - Alexandrescu's CppCon 2019 talk "Speed Is Found in the Minds of People", according to secondary notes: hand small arrays to a different sort, and use an unguarded insertion sort once the minimum is at the front ([notes](https://yduf.github.io/fast-sort)).
  - In the engine, the sort is about half the sessions stage (65–70 ms per thread). `sess-bins` takes 0.061–0.072 s against 0.083–0.091 s on clustered data, and an 11-bit radix sort takes 0.039–0.041 s against 0.063–0.066 s for `sort_unstable` ([alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).
  - The median person has 14 events, and persons with at least 256 events hold a third of the rows (brief).
- **Mechanism.** Give each person a dense number (from #2) and counting-sort rows by person. Then, per person:
  - n ≤ 16 or 32: a fixed sorting network on `u32` values (AVX2 8 lanes or AVX-512 16 lanes of min/max plus permutes, or a scalar conditional-move network for tiny n).
  - 33 ≤ n < 256: radix or insertion hybrid.
  - n ≥ 256: 30-minute bins holding min, max, count and the payload counters, then sort only the non-empty bins.
  - This is exact. Two events in one half-open 30-minute bin are less than 30 minutes apart, so a session can only break between consecutive non-empty bins, and the test min(next) − max(prev) > 30 min is exact.
- **Where.** The sessions stage (0.70 s one-core).
- **Expected effect.** About −0.15 to −0.2 s of one-core CPU if the sort share halves.
- **Risks.**
  - Code size: Marianczuk's smaller in-algorithm gain is blamed on the I-cache.
  - The payload must follow the times; the engine keeps purchases in a side list.
  - Bins need care at the gap boundary (> versus ≥), for the session start that anchors the 7-day attribution window, and for days across midnight.
- **Test.** `sortbench` driven by the real person-size histogram, timed per size class.

**4. Interleave 2–3 independent row cursors in the row pass ("loop triplication").**
- **Evidence.**
  - Ragnar's numbers above.
  - Würthinger ran 3 cursors per 2 MB chunk ([source file](https://github.com/gunnarmorling/1brc/blob/main/src/main/java/dev/morling/onebrc/CalculateAverage_thomaswue.java)). Morling says the loop was written three times, and four copies were slower on that machine ([InfoQ talk](https://www.infoq.com/presentations/1brc)).
  - lehuyduc parsed two lines at a time ([#138](https://github.com/gunnarmorling/1brc/discussions/138)).
- **Mechanism.** Each row is a long dependency chain:
  - load the fields, pack the IDs (`pext`), multiply-hash, pick the partition, store into the staging buffer;
  - the `ts` table lookup;
  - the product lookup.

  With rows of roughly 150–200 µops, the reorder buffer sees only one or two rows at once. Processing two sub-streams of the batch's row list in one loop body hands the scheduler independent chains.
- **Where.** Slot `tok-lanes`. The repo says it suits only the one-pass tokenizer. With the per-batch index, the two streams can simply be the two halves of each batch's row list.
- **Expected effect.** Ragnar measured −22% with a core to himself. Next to a busy SMT sibling, perhaps −5 to −10% of the row parse.
- **Risk.** x86 has 16 general registers, and the engine's x86 build already spills. I-cache and tail handling add cost.
- **Test.** 2-way and 3-way variants in `parse_bench`, under 8c×1t and 4c×2t.

**5. AVX-512BW tokenizer behind runtime dispatch, plus VBMI/VBMI2 where present.**
- **Evidence.**
  - The leader uses runtime-selected AVX-512 (brief).
  - Muła and Lemire's AVX-512 base64 runs at nearly memcpy speed, with several times fewer instructions than earlier SIMD codecs ([arXiv 1910.05109](https://arxiv.org/abs/1910.05109)).
  - The engine's `x86-64-v4` build measured −3.7%, inside the noise, on Zen 4, which runs 512-bit operations as 2×256 ([alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).
- **Mechanism.** One 64-byte compare per character class, straight into a mask register (no movemask), plus mask-register logic. On Ice Lake or later, add `vpcompressb` to gather field bytes and `vpermb` as a 64-entry lookup table.
- **Expected effect.** The index pass needs roughly half the instructions. Perhaps −5 to −10% of the parse.
- **Risks.**
  - Cascade Lake lacks VBMI and VBMI2; simdjson keeps AVX2 there for this reason (repo note).
  - Two code paths to test.
  - The Zen 4 box understates the gain.
- **Test.** Dispatch with `is_x86_feature_detected!` and `#[target_feature(enable="avx512bw,avx512vl")]`, and time on an Intel AVX-512 host.

**6. Engineer branches from data statistics: one homogeneous batch per timestamp form, and `select_unpredictable` where a 50/50 choice remains.**
- **Evidence.**
  - Topolnik's result above.
  - Würthinger credits Hamala: avoiding the misprediction between the <8 and 8–16 cases "is a big win" ([source file](https://github.com/gunnarmorling/1brc/blob/main/src/main/java/dev/morling/onebrc/CalculateAverage_thomaswue.java)).
  - Ragnar: removing branches alone gave almost nothing (1.70 → 1.67 s), and a rarely taken branchy min/max beat the branch-free version ([1BRC](https://curiouscoding.nl/posts/1brc/)).
  - Jon Gjengset uses `select_unpredictable` ([brrr](https://raw.githubusercontent.com/jonhoo/brrr/main/src/main.rs)).
  - In the engine, the `ts` form is random per row (58/10/32%). A branch-free select measured 29.4 ns per row against 25.9 for branches on the M4, and is untested on the Xeon ([alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).
- **Mechanism.** Classify the forms of `ts` and `ingest_ts` in the index pass: byte 10 is `T`, and the last byte is `Z` or an offset sign. Then either:
  - push row indices onto per-form lists, or
  - parse column-wise per batch: every F1 `ts`, then every F2, then every F3.

  Each list runs its own branch-free loop, and the only branches left are loop exits.
- **Expected effect.** About one mispredict per row × 21.6M × 15–20 cycles ≈ 0.08–0.11 s of one-core CPU.
- **Risk.** Extra stores and an indirection; the two timestamp fields' forms are independent.
- **Test.** `perf stat -e branch-misses` per row on bare metal, before and after.

**7. Skip the file mapping's exit cost with `pread` into reused per-worker buffers. The 1BRC fork/subprocess trick does not transfer.**
- **Evidence.**
  - 1BRC: about 100 ms of unmapping at exit ([Topolnik](https://questdb.com/blog/billion-row-challenge-step-by-step/)). lehuyduc: munmap took over 50% of the time at 128 threads ([#138](https://github.com/gunnarmorling/1brc/discussions/138)). Ragnar: parallel munmap still serializes on a kernel lock ([1BRC](https://curiouscoding.nl/posts/1brc/)).
  - The kernel waits for every task in the PID namespace before init's exit completes ([pid_namespace.c](https://raw.githubusercontent.com/torvalds/linux/master/kernel/pid_namespace.c)).
  - The engine measured 35–39 ms from `exit_group` to the process being gone, almost all of it unmapping the 1.9 GB file mapping. Unmapping earlier did not help ([x86.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)).
- **Mechanism.** Read each chunk with `pread` into a per-worker, L2-sized, reused buffer. With no file mapping, there are no fault-arounds and nothing to tear down.
- **Expected effect.** −35 to −39 ms of exit, minus the copy (about 1.9 GB at about 10 GB/s per core, ≈0.19 s of CPU spread over the workers). Probably a small net gain.
- **Risk.** The copy may cancel the gain.
- **Test.** `reader-pread` under `bench_docker.py --idle 12`, plus `time docker run`.

**8. Perfect hashing (PtrHash) for products and campaigns, and as a compact membership index for links.**
- **Evidence.**
  - Ragnar's 1BRC perfect-hash gains above.
  - PtrHash uses about 2.4 bits per key, takes 8–12 ns per query on 10⁹ keys and about 2.5 ns per key across threads. Below about 20M keys (6 MB, inside L3), batching barely helps ([PtrHash](https://curiouscoding.nl/posts/ptrhash/)).
- **Mechanism.** Product ID (50K) or campaign ID (2K) → MPHF slot → verify the stored key → payload. There is no probe loop, and the product pilot table is about 15 KB, so it fits L1.
  - For links: an MPHF over the linked IDs (about 0.5–1 MB of pilots) plus a key array, with an occupancy filter for the non-linked rows (44% miss at full size).
- **Expected effect.** Small for products and campaigns. For links, many probes move from DRAM/TLB misses to L2/L3, maybe −20 to −40 ms of CPU. Moot if #2 cuts probes 17×.
- **Risks.**
  - Invalid IDs need a stored-key check, unlike 1BRC's closed key set.
  - Build cost is about 20–30 ns per key.
  - Check the crate's licence.
- **Test.** `lookupbench`.

**9. Vertical SIMD across rows for timestamps (one row per lane).**
- **Evidence.**
  - SimdMinimizers splits its input into 8 chunks that run in 8 SIMD lanes. It is up to 6.8× (w=5) and 3.4× (w=19) faster than a scalar implementation ([crate README](https://docs.rs/crate/simd-minimizers/latest/source/README.md); [SEA 2025 paper](https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.SEA.2025.20)).
  - Lemire's SIMD timestamp parser uses 65 instructions and 7.9 (ns) against 700 instructions and 46 for `strptime` ([Lemire](https://lemire.me/blog/2023/07/01/parsing-time-stamps-faster-with-simd-instructions/)).
- **Mechanism.** On #6's per-form lists, put one ISO stamp per 128-bit lane: 2 per AVX2 register, 4 per AVX-512 register. Validate with per-lane saturating-subtract bounds and combine digits with `maddubs`/`madd`. The existing date table still settles the date.
- **Expected effect.** The engine's `ts` + `ingest_ts` cost (4.2 + 1.9 ns per row, M4) might halve: about −0.06 to −0.1 s of one-core CPU.
- **Risk.** Transposition cost and complexity.

**10. Explicit huge pages for the random-access tables.**
- **Evidence.**
  - Ragnar rounds allocations up to multiples of 32 MB so they actually get huge pages, and reports a "significant speedup" for large layouts ([S-trees](https://curiouscoding.nl/posts/static-search-tree/)).
  - In the engine, THP `never` costs +18%, and the link table needs 32,768 4 KB pages against 1,536 second-level TLB entries ([alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).
- **Mechanism.** 2 MB-aligned allocations with `madvise(MADV_HUGEPAGE)` on the link table, the dedup tables and the `Parts` regions, plus `MADV_POPULATE_WRITE` from the workers.
- **Expected effect.** Nothing under THP `always`; under `madvise` without the glibc tunables, or under `never`, up to the +18% measured.

**11. PGO (and BOLT) trained on generator output, built for amd64.**
- **Evidence.** The leader uses PGO (brief). The engine measured no gain on the Mac and has not tried amd64 ([alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).
- **Mechanism.** Profile-driven block layout and spill placement in the large inlined row closure, which matters with x86's 16 general registers.
- **Expected effect.** Unknown; this pass found no public number.

**12. Micro-tricks still worth checking in the field parsers.**
- Lemire's SWAR parse of 8 digits: 3 multiplications and about 13 instructions, against about 24 ([Lemire](https://lemire.me/blog/2022/01/21/swar-explained-parsing-eight-digits/)).
- Ragnar's single-multiplication parse (1.48 s) and PDEP digit deposit (1.42 s), with PDEP emulated on Zen 1/2 ([1BRC](https://curiouscoding.nl/posts/1brc/)).
- Masks from lookup tables instead of variable shifts (Van Phu DO, credited in [Würthinger's file](https://github.com/gunnarmorling/1brc/blob/main/src/main/java/dev/morling/onebrc/CalculateAverage_thomaswue.java)).
- `u64` counters beat `u32` by about 0.05 s ([1BRC](https://curiouscoding.nl/posts/1brc/)).
- Avoid out-of-bounds `get_unchecked` UB by indexing the whole buffer; fixing that UB cost Ragnar 1.44 → 1.56 s.
- Where: `price_cents`, the epoch-ms tail and the `+HH:MM` offsets. Expected effect: small.

**13. Hash tags ("salt") in table slots.**
- DuckDB stores 1–2 bytes of hash next to each pointer ([DuckDB](https://duckdb.org/2022/03/07/aggregate-hashtable.html)). The engine measured no gain on the M4, "might pay on the Xeon".
- It matters most under SMT, where two threads share an L2.

**14. Fast-path and cold-path hygiene.**
- lehuyduc's 16-byte fast path covers about 99% of names, with a fallback ([#138](https://github.com/gunnarmorling/1brc/discussions/138)).
- Ragnar: a never-taken long-name loop is free only when wrapped in an always-false `if`, and an `unlikely` hint did not change the assembly ([1BRC](https://curiouscoding.nl/posts/1brc/)).
- Jon uses `cold_path` and `assert_unchecked` ([brrr](https://raw.githubusercontent.com/jonhoo/brrr/main/src/main.rs)).
- Move quotes, IDs over 9 bytes and malformed lines into `#[cold] #[inline(never)]` functions, to cut register pressure in the hot loop.

**Already in the engine; no action:**
- mmap and work-stealing chunks: 1BRC found about 2 MB best ([InfoQ](https://www.infoq.com/presentations/1brc)), and Topolnik measured 1.8 → 1.7 s; the engine uses 1 MB chunks from a shared counter.
- IDs held inline as `u64` keys: Ragnar 61 → 50 s.
- Hashing while scanning: van Kooten 25 → 20 s ([post](https://www.dannyvankooten.com/blog/2024/1brc/)).
- Prefetch distance: PtrHash saturates at about 16 queries ahead; the engine prefetches 16 and 64 rows ahead.
- Write-combined scatters.
- `fast-exit`.
- No allocation in the hot loop: the Java entries ran Epsilon GC for the same reason ([InfoQ](https://www.infoq.com/presentations/1brc)).

### Gaps
- No public number was found for PGO on a parse-heavy Rust workload like this one.
- The leader's exact mechanism for moving events by raw user ID and still sessionizing per person is unknown beyond the brief.
- The scorer's Xeon generation is unknown. It decides whether VBMI/VBMI2 (Ice Lake or later) exist, and the L2 size (1 MB on Cascade Lake, 1.25 MB on Ice Lake-SP, 2 MB on Sapphire Rapids per core; these sizes are general knowledge, not checked in this pass).

## Q1. One Billion Row Challenge: which tricks apply here, with measured gains?

### Takeaway
1BRC's gains came, in order of size:
1. parallel chunks;
2. a custom hash table with inline keys;
3. integer, SWAR, branchless parsing;
4. mmap;
5. branch-statistics engineering;
6. ILP by interleaving 2–4 cursors;
7. exit and startup tricks.

The engine has items 1–4. Items 5 and 6 remain open. Item 7's subprocess form cannot help inside a container where the binary is PID 1, because the kernel waits for the whole PID namespace before init's exit completes.

### Cited Findings
**Results and hardware**
- The official runs used 8 cores of a 32-core AMD EPYC 7502P (Zen 2).
  - Winner: Thomas Würthinger, Quan Anh Mai and Alfonso Peterssen, 1.535 s.
  - Artsiom Korzun 1.587 s, Jaromir Hamala 1.608 s.
  - 32 cores / 64 threads: Hamala 0.323 s.
  - A 188× speedup over the baseline.
  - All of the top 10 used `Unsafe`, and 8 of the top 10 were GraalVM native binaries ("the deciding difference" below 2 s) — [Morling, results](https://www.morling.dev/blog/1brc-results-are-in/)
- The 10K-key variant (names up to 100 bytes) punished entries tuned to the 413 original names. Korzun won it in 2.957 s; Hamala took 4.042 s — [Morling, results](https://www.morling.dev/blog/1brc-results-are-in/)
- Würthinger's entry runs "in 0.31 on an Intel i9-13900K" against 120.37 s for the reference — [CalculateAverage_thomaswue.java](https://github.com/gunnarmorling/1brc/blob/main/src/main/java/dev/morling/onebrc/CalculateAverage_thomaswue.java)

**Würthinger's design** — [source file](https://github.com/gunnarmorling/1brc/blob/main/src/main/java/dev/morling/onebrc/CalculateAverage_thomaswue.java); [InfoQ talk](https://www.infoq.com/presentations/1brc)
- A child worker process does the work, so that "clean up of the memory mapping" happens after the parent has relayed the output.
- Work stealing over 2 MB segments (credited to Korzun).
- Three cursors per chunk, advanced in one loop.
- A per-thread table of 131,072 slots holding each name's first 16 bytes as two `long`s.
- Hamala's avoidance of the <8 vs 8–16 misprediction, and Van Phu DO's mask lookup tables.
- Morling's talk: the loop was written "three times"; four copies were slower on that machine. The best chunk size people found was "2 megabytes". GraalVM as the JIT gave "like 5% or so", and native image saved "a few hundred milliseconds" of startup.

**Topolnik's step-by-step (Hetzner CCX33)** — [QuestDB](https://questdb.com/blog/billion-row-challenge-step-by-step/)
- 71 s (parallel streams) → 66 s (GraalVM).
- 17 s: mmap and parallel chunks; instructions per row 2,031 → 945.
- 11 s: integer temperature parse; 607 instructions per row.
- 6.6 s: open-addressing table of 2,048 slots; 2.2× fewer cache misses.
- 2.4 s: Unsafe plus SWAR `;` search, Korzun's name masking and Mai's branchless parse; about 120 instructions and 0.66 branch misses per row.
- 1.8 s: branch-statistics restructuring.
- Subprocess to hide about 100 ms of unmapping at exit.
- 1.7 s: small chunks pulled from an `AtomicInteger`.

**Ragnar Groot Koerkamp's Rust 1BRC (i7-10750H capped at 3.6 GHz, 6 cores)** — [curiouscoding](https://curiouscoding.nl/posts/1brc/)

Step times:

| Change | Time |
| --- | --- |
| Bytes instead of strings | 72 s (the body says 84) |
| Integer parse | 61 s |
| `u64` inline keys | 50 s |
| FxHash | 41 s |
| `memchr` crate | 29 s (libc `memchr` was slower, 47 s) |
| `get_unchecked` | 28 s |
| Unaligned head/tail `u64` key reads | 23 s |
| PtrHash | 17 s |
| 64-bit masks | 15 s |
| mmap | 12 s |
| Parallel | 2.0 s |
| Branchless parse | 1.7 s |
| Faster PtrHash | 1.55 s |
| ILP | 1.05–1.06 s |

Final numbers:
- 4.93 s on one thread at 4.6 GHz, excluding munmap.
- 0.90 s on 6 threads without HT; 0.92 s on 12 threads with HT.
- 1.06 s for the final compliant run at 3.6 GHz.

Other findings:
- One mmap per thread gave nothing, because parallel munmap serializes on a kernel lock.
- Emulated PDEP was about 3% slower than native on Zen 1/2.

**lehuyduc's C++ (discussion #138)** — [#138](https://github.com/gunnarmorling/1brc/discussions/138)
- Results: 0.145 s best (0.184 s typical) at 128 threads on 2× EPYC 9354; 9.096 s on 1 thread, 1.174 s on 8.
- Hash table: overflow hashing, linear probing with stored keys, and size 16,384.
- SIMD everywhere: separator search, hashing and compares.
- Two lines per thread at a time.
- Branchless parse: about 1% at best, first reported as 3–4%. Integer instead of float: about 3%.
- munmap took over 50% of the time at 128 threads; fixed with a subprocess.
- Parallel aggregation saved about 28 ms. HT gave about 62%.
- From commenters: a fork-based approach was about 50% faster than his, and numactl pinning was 10–20% slower in some cases.

**Danny van Kooten's C (Ryzen 4800U)** — [post](https://www.dannyvankooten.com/blog/2024/1brc/)
- 480 s → 97 s (hash map with linear probing) → 45 s (integer parse) → 25 s (`fread` in 64 MB chunks) → 20 s (hashing while scanning) → 6 s (16 threads) → 1.6 s (mmap).
- Branchless min/max gave nothing measurable, because GCC already emits `cmov`.
- `MAP_HUGETLB` was not usable for the file mapping.

### Inferences
The table maps each 1BRC trick onto the engine.

| 1BRC trick (gain) | Applies to | Engine status |
| --- | --- | --- |
| Parallel chunks with work stealing at 2 MB (1.8 → 1.7 s) | Parse | Done (1 MB chunks, shared counter) |
| Inline `u64` keys, one cheap multiplicative hash (61 → 50 s, 50 → 41 s) | Every ID | Done (`id-u64` with `pext`, `hash-mul`) |
| SIMD delimiter masks over 64-byte blocks (17 → 15 s) | Tokenizer | Done (AVX2). AVX-512 open (#5) |
| SWAR or branchless digit parsing | `ts` (epoch-ms), price, offsets | Mostly done (8 + 5 SWAR). Check price (#12) |
| Perfect hash for a known key set (23 → 17 s) | Products, campaigns, links (with key verification) | Open (#8) |
| Branch-statistics engineering (−25%) | `ts` forms, quotes | Quotes done branch-free; `ts` forms open (#6) |
| 2–4 interleaved cursors (−22%) | Row pass | Open (`tok-lanes`, #4); the SMT caveat applies |
| Subprocess or fork to skip munmap (about 100 ms) | Exit | Not transferable if the binary is PID 1 or the scorer times `docker run`. `reader-pread` is the alternative (#7) |
| One mmap per thread | Reader | Dead end, also in 1BRC |
| `MAP_HUGETLB` on the input | Reader | Not possible on a file mapping |
| "Assume one decimal", "temperatures < 100" | — | Only as fast paths with an exact fallback |

- The 10K-key variant is a warning for our "any valid input" constraint. Data-specific fast paths such as bins, ID lengths and `ts` forms each need an exact fallback, and the fallback must be cheap enough not to ruin adversarial inputs.
- Ragnar's "no counting" shortcut changed a result (Baghdad 22.7 instead of 22.8, per the page), which is why exactness checks against fixtures matter for every "skip work" idea.

### Gaps
- No primary write-up found by Korzun or Hamala on their own entries; their techniques are known only through credits in Würthinger's file and Topolnik's post.
- Morling's results post defers the technique write-up ("segmentation, SIMD/SWAR, branch mispredictions, process forking") to a later post, which this pass did not find.

## Q2. Jon Gjengset: a 1BRC stream, its techniques, and other relevant work

### Takeaway
Yes. Jon streamed "impl Rust: One Billion Row Challenge" (YouTube `tCY7p6dVAGE`), and the code is in `jonhoo/brrr`. Neither the stream page nor the repository gives timings. The transferable ideas are mostly Rust-specific levers: `select_unpredictable`, `assert_unchecked`, `cold_path`, unchecked splits, never unmapping, `MADV_SEQUENTIAL`, and parsing a short trailing field backwards from the end of the line.

### Cited Findings
- The video's title is "impl Rust: One Billion Row Challenge" — [YouTube](https://www.youtube.com/watch?v=tCY7p6dVAGE)
- The repo `jonhoo/brrr` says "Code from https://youtu.be/tCY7p6dVAGE". It has 167 commits and includes 100M-row test data — [GitHub](https://github.com/jonhoo/brrr)
- Techniques in the code:
  - mmap with `MADV_SEQUENTIAL`, never unmapped, and no huge pages;
  - equal static byte ranges per `available_parallelism()` thread, with results merged through a `sync_channel` into a `BTreeMap`;
  - newlines found with `portable_simd` 32-byte compares;
  - `;` found by a byte-wise backward scan from the line end, because the temperature suffix is short;
  - the standard `HashMap` with a custom multiplicative hasher that reads only the first 4 bytes, and 16-byte inline small strings as keys;
  - temperatures parsed into integer tenths, with `select_unpredictable` for sign and decimal and `assert_unchecked` for the minimum length;
  - `get_unchecked`, `unwrap_unchecked` and `split_at_unchecked`, plus `cold_path` hints;
  - no timings in the code — [brrr src/main.rs](https://raw.githubusercontent.com/jonhoo/brrr/main/src/main.rs)
- A February 2026 blog post compares temperature parsers against Jon's `parse_temperature`, which assumes the `;` position is already known. Jon's SIMD path needs unstable Rust. The author's version was "4x faster(!)" in an unfair comparison, with no hardware given — [Balaam](https://artificialworlds.net/blog/2026/02/20/billion-parse-temp/)

### Inferences
- **`core::hint::select_unpredictable`** forces a branchless select. It is the right tool for the engine's 50/50 choices (the per-row `ts` form if not batched, quote stripping) and avoids hand-written bit tricks. Benchmark it on the Xeon, since the branch-free `ts` select lost on the M4.
- **`assert_unchecked`** encodes invariants such as "this field is at least 13 bytes" or "this index is inside the batch". It removes bounds checks without `get_unchecked`'s UB trap, which cost Ragnar time when he fixed it.
- **Parsing from both ends.** Event lines end with short fields (`product_id`, `price_cents`), so a line can be parsed from both ends at once, giving two independent dependency chains inside one row. The per-batch delimiter index makes this unnecessary in today's design, but it would matter for a one-pass, `tok-lanes`-style parser.
- **Never unmapping** is equivalent to `fast-exit`. The kernel still unmaps at exit (35–39 ms measured), so `reader-pread` is the only way to remove that cost.

### Gaps
- The stream's date and length, any final timing, and the profiling workflow shown on stream (perf, flamegraphs) were not available from the fetched pages (the YouTube page was truncated).
- Jon's other work (the `left-right` and `flurry` concurrent maps, *Rust for Rustaceans*) was not researched in this pass. As concurrency structures for read-heavy shared maps, they seem irrelevant to a phase-separated batch pipeline with no concurrent readers and writers, but that is general knowledge, not checked here.

## Q3. Ragnar Groot Koerkamp's other posts: PtrHash, static search trees, SIMD minimizers, batching

### Takeaway
Ragnar's posts share one lesson: throughput beats latency when queries are independent. Batch them, prefetch about 16–32 ahead, interleave the stages of different queries, put independent streams in separate SIMD lanes, and use huge pages for large random-access arrays. The engine already batches and prefetches. The untried transfers are:
- a compact MPHF index instead of a 64–128 MB table;
- splitting multi-step lookups across in-flight batches;
- SIMD lanes across rows for field validation.

### Cited Findings
- **PtrHash design.** An MPHF using about 2.4 bits per key by default: 8-bit pilots per bucket, hash-and-evict construction, and a remap of overflow slots — [PtrHash](https://curiouscoding.nl/posts/ptrhash/)
- **PtrHash query times (10⁹ integer keys).**
  - Fast configuration: 12.5 ns per query in a loop, 8.8 ns streaming.
  - Default: 21.0 ns loop, 8.7 ns streaming.
  - 300M strings: 33 ns loop, 16 ns streaming.
  - About 2.5 ns per key across threads, saturating memory.
  - For about 20M keys (6 MB, inside L3), batching gives little benefit — [PtrHash](https://curiouscoding.nl/posts/ptrhash/)
- **PtrHash streaming.** Prefetching 32 queries ahead; throughput saturates at about 16 ahead. RAM latency is about 80 ns, and Skylake's 12 line-fill buffers give about 6.67 ns per read in theory and 7.4 ns in practice — [PtrHash](https://curiouscoding.nl/posts/ptrhash/)
- **PtrHash construction and keys.** About 20 ns per key (fast configuration, 6 threads). Integer keys are hashed by multiplying with an odd constant, which is invertible, so integer keys never collide in the 64-bit hash — [PtrHash](https://curiouscoding.nl/posts/ptrhash/)
- **Static search trees (i7-10750H at 2.6 GHz, HT off).**

  | Step | ns per query |
  | --- | --- |
  | Binary search (1 GB) | about 1150 |
  | Eytzinger layout | about 200 |
  | SIMD node search | 115 |
  | Batching (saturates at batch 16) | 45 |
  | Prefetching | 30 |
  | Pointer arithmetic | 28 |
  | Interleaving all layers across in-flight batches | 24 (from 29) |
  | Left-max layout | 22 |
  | Final, one thread, 4 GB | 27 (over 40× faster than binary search) |
  | Final, 6 threads (RAM bandwidth-bound) | 7 |

  — [S-trees](https://curiouscoding.nl/posts/static-search-tree/)
- **Huge pages in the S-tree post.** Allocations are rounded to multiples of 32 MB so they actually get huge pages. This gives a "significant speedup" for large sizes and sometimes a small one for 1–32 MB — [S-trees](https://curiouscoding.nl/posts/static-search-tree/)
- **SimdMinimizers** splits the input into 8 chunks processed in parallel in 8 SIMD lanes (AVX2 or NEON).
  - Up to 6.8× (w=5) and 3.4× (w=19) faster than a scalar implementation.
  - The 3.2 Gbp human genome in 5.2 s (canonical: 6.7 s).
  - The scalar version wins only for sequences of 100 or fewer.
  - These figures come from search-result summaries of the crate README and the SEA 2025 paper — [crate README](https://docs.rs/crate/simd-minimizers/latest/source/README.md); [paper](https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.SEA.2025.20)

### Inferences
- **Link table.** The 64–128 MB link table is the textbook case for "compact index plus streaming". An MPHF over the linked IDs is about 1 MB of pilots plus an 8-byte-per-key verification array. It turns most DRAM/TLB misses into L2/L3 hits, which matters most under SMT. With #2 (probing once per distinct ID), probes drop to about 1.3M and latency hiding matters much less.
- **Splitting lookups across batches.** "Interleave all layers across in-flight batches" (29 → 24 ns) maps onto any lookup with more than one step:
  - the link line, then the overflow line (about 6% of rows read a second line);
  - a dedup probe, then a kept-list read;
  - product lookup, then category.

  Issue stage k for batch i while stage k+1 runs for batch i−1, instead of one row at a time.
- **SIMD lanes across rows.** SimdMinimizers' "one independent stream per lane" carries over to validating 8–16 rows' fields at once (#9). The analogy is ours; nothing in the sources applies it to CSV.
- **Prefetch distance.** Ragnar's saturation at 16–32 ahead agrees with the engine's measured choice: 64 rows ahead for lookups, where 32 and 128 measure the same.

### Gaps
- Ragnar's Advent of Code speed work was not found or researched in this pass.
- A dedicated post on "many independent queries in parallel" beyond PtrHash and the S-tree post was not identified.
- The SimdMinimizers figures were not read from the paper's full text.

## Q4. Lemire, Muła, Muratori, Acton, Alexandrescu

### Takeaway
- **Lemire.** SIMD timestamp validation with per-lane bounds needs about 10× fewer instructions than `strptime`, and SWAR parses 8 digits in 3 multiplications. The engine already has the SWAR digits and a date table. Lemire's per-lane bounds check is the remaining piece (`ts-simd`).
- **Muła and Lemire.** AVX-512 VBMI lookup-table decoding reaches nearly memcpy speed. The engine needs no base-N decoding at all, because it packs raw ID bytes into a `u64`.
- **Muratori.** Waste is "the biggest multiplier"; the engine's waste is in data movement (two scatters, 32-byte records, per-row probes), not in arithmetic.
- **Alexandrescu and sorting networks.** Small sorts deserve their own code path.

### Cited Findings
- **Lemire's SIMD timestamps: method.** Subtract `'0'`, then saturating-subtract per-lane digit limits (`_mm_subs_epu8`). Combine digit pairs with `_mm_maddubs_epi16`. Range-check the fields with `_mm_subs_epu16` against 99, 99, 12, 31, 23, 59, 59. Days per month are left to scalar code — [Lemire 2023](https://lemire.me/blog/2023/07/01/parsing-time-stamps-faster-with-simd-instructions/)
- **Lemire's SIMD timestamps: results (Ice Lake, GCC 12).** `strptime` takes 700 instructions and 46 per stamp, SIMD 65 instructions and 7.9 ("6 times faster", "10 times fewer instructions"); the post gives no time unit — [Lemire 2023](https://lemire.me/blog/2023/07/01/parsing-time-stamps-faster-with-simd-instructions/)
- **Lemire's SWAR 8 digits.** Subtract `0x30…30`, combine pairs with `(val*10) + (val>>8)` (`*2561 >> 8`), then two masked multiplications. About 13 instructions with 3 `imul`, against about 24 for an unrolled loop. It assumes valid digits, so validation is separate — [Lemire 2022](https://lemire.me/blog/2022/01/21/swar-explained-parsing-eight-digits/)
- **Muła and Lemire's base64.** AVX-512 encoding and decoding run "at nearly the speed of a memory copy" on recent Intel processors (when the data is not in L1), with "several times fewer instructions" than previous SIMD codecs, and can switch base64 variants by changing constants — [arXiv 1910.05109](https://arxiv.org/abs/1910.05109)
- **Muła's integer-series parser.** The repo has scalar, SSE and AVX-512BW parsers for signed integers separated by runs of separator characters, and flags any other character as invalid. The article itself (0x80.pl) failed to load with a TLS certificate error — [parsing-int-series](https://github.com/WojciechMula/parsing-int-series)
- **Muratori on waste.** Waste is one of five multipliers, "the biggest multiplier that makes code run slowly". Performance comes from reducing the total instructions the CPU runs, or making them flow through it more efficiently — [Computer, Enhance! "Waste"](https://www.computerenhance.com/p/waste)
- **Muratori on how slow software is.** "Modern software is more like 1000 times slower than it should be"; software only "needs to not be horribly inefficient" — [Performance-Aware intro](https://www.computerenhance.com/p/welcome-to-the-performance-aware)
- **The second multiplier.** Search snippets of the companion posts place IPC second, after waste — [instructions per clock](https://www.computerenhance.com/p/instructions-per-clock)
- **Alexandrescu, through secondary notes.** In "Speed Is Found in the Minds of People" (CppCon 2019), small arrays go to a different sort, and an unguarded insertion sort can be used once the smallest element is at the front — [notes](https://yduf.github.io/fast-sort)
- **Sorting networks.** At least 25% faster than insertion sort on small sets, and over 6% better as the base case of a samplesort — [Marianczuk 2019](https://arxiv.org/abs/1908.08111)

### Inferences
- **"Don't decode, pack"** already beats base-N decoding. The engine packs up to 9 ID bytes into a `u64` at 7 bits per byte with `pext`.
- **If base36 decoding were ever needed** (for example, to get a dense 40-bit key for a direct-indexed bitmap):
  1. Map characters to digits with a lookup table (`vpermb` on VBMI, two `pshufb` on AVX2).
  2. Validate with per-lane bounds, as in Lemire's timestamps.
  3. Combine with multiply-adds: pairs × 36, then quads × 1,296, then × 36⁴ = 1,679,616 into 64 bits. 36² − 1 fits in 16 bits and 36⁴ − 1 in 32 bits.

  This adapts Lemire's 8-digit SWAR scheme. It is our derivation, not a source's.
- **Waste in the engine.** Muratori's waste lens, applied with the engine's own profile, points first at bytes moved:
  - 32-byte parse records, then 16-byte person records: two full scatters of fresh memory;
  - one link probe per event instead of per distinct user;
  - a full comparison sort where most persons have 14 events.

  The leader's design removes exactly these.
- **Data-oriented design.** Acton's principles ("where there's one, there's many", solve for the common case) match per-form homogeneous batches (#6) and per-size-class person processing (#3). No primary source for Acton was fetched; see Gaps.

### Gaps
- Muła's "Parsing series of integers with SIMD" article and its numbers could not be fetched (TLS certificate mismatch on 0x80.pl).
- Muratori's full list of the five multipliers and his measured examples are behind a paywall.
- No primary source was fetched for Mike Acton's "Data-Oriented Design and C++" (CppCon 2014), or for Alexandrescu's talk itself; the Alexandrescu points come from secondary notes.
- No Lemire or Muła work on base36 specifically was found.

## Q5. Other contests and benchmarks of a similar shape (DuckDB, DataFusion/ClickBench, H2O, AoC, fizzbuzz, TPC-H)

### Takeaway
High-cardinality grouping in database engines keeps finding the same few wins:
- thread-local pre-aggregation, then radix partitions merged independently;
- hash tags beside pointers;
- map rows to dense group indices first, then run columnar, type-specialized accumulators (DataFusion: 2–3×);
- normalized binary keys, so sorting becomes memcmp or radix.

The engine already uses partitioning and dense per-day arrays. Dense indices for persons and users (#2, #3), and radix or network sorts on normalized keys, are the transferable parts.

### Cited Findings
- **DuckDB's grouping table** — [DuckDB](https://duckdb.org/2022/03/07/aggregate-hashtable.html)
  - Linear probing over a separate pointer array (4- or 8-byte entries), with payload blocks holding the keys and states.
  - 1–2 bytes of hash "salt" in each pointer entry avoid most payload reads and key comparisons.
  - Hashes are stored, so a resize does not recompute them.
  - Each thread builds a local table and, past 10,000 rows, radix-partitions it by hash bits. Each partition is merged independently with no coordination.
  - Measured on a 10-core M1 Max against Pandas, Polars and Arrow; the speedups appear only in charts.
- **DataFusion 28.0.0** — [DataFusion blog](https://arrow.apache.org/blog/2023/08/05/datafusion_fast_grouping/)
  - "2-3x faster" than 27.0.0 for queries with at least 10,000 groups.
  - The changes: a two-phase, hash-partitioned group-by; inline single-column keys or one Arrow Row allocation for other keys; a key-to-group-index hash table; columnar `GroupsAccumulator` state in native `Vec<T>` updated by vectorizable loops; no per-group allocations.
  - ClickBench Q17 builds about 24M groups from about 100M rows. Runs were on a GCP e2-standard-8.
  - Grouping speed is close to DuckDB 0.8.1.
- **DuckDB's sort redesign** — [DuckDB sort](https://duckdb.org/2021/08/27/external-sorting.html)
  - Binary-comparable normalized keys: integers big-endian with the sign bit flipped, descending columns bit-inverted. A single memcmp then orders rows, and radix sort becomes possible.
  - Row layout for sorting, a branchless (predicated) merge, and Merge Path for parallel merges.
  - 100M integers sorted in just under 5 s on one thread (M1).

### Inferences
- **The engine's packed `u64` sort key** (bucket-local person, `ts`) is already a normalized key in DuckDB's sense, so radix sort applies; the engine's 11-bit LSD radix is "faster in part". The DataFusion lesson suggests going further: give each person a dense index first, then counting-sort by person and sort only 4-byte times per person (#3).
- **DuckDB's salt** maps onto the dedup tables and the link table, as "tags in slots' spare bits". The engine measured no gain on the M4, but on an SMT Xeon, whose L2 the dedup tables already fill, it may be worth re-measuring (#13).
- **Columnar accumulators** are already the engine's `agg-dense` design for `campaign_daily`; nothing new there.

### Gaps
- The H2O db-benchmark, hand-written TPC-H engines, the Advent of Code speed-run community (for example CodSpeed's Rust contest) and "fastest fizzbuzz" were not researched in this pass. Output is only 0.08 s of one-core CPU, so fizzbuzz-style output tricks (`vmsplice`) look irrelevant; that is an inference.
- Polars' own group-by design was not fetched.

## Q6. Skipping work entirely under exact semantics

### Takeaway
Exact-preserving skips exist and are where the leader's advantage lies:
- prove most event IDs unique with a cheap filter, so exact dedup runs only on the few percent flagged;
- look up the person once per distinct user ID, not per event;
- skip the comparison sort for heavy persons with 30-minute bins (min, max, count), which is exact under the gap rule;
- sort only a 4-byte time per person after a counting sort by person.

Lazy validation of dedup losers is not worth it, because re-reading text costs more than the roughly 4% of field work it saves.

### Cited Findings
- **Leader (assignment brief).** A small bitmap proves about 97% of IDs unique; the person lookup happens once per distinct user ID; heavy users are counted in 30-minute bins.
- **Bins are exact under the rules.** "A 30-minute bin cannot hold a gap over 30 minutes, so sessions break only between bins". `sess-bins` measured 0.061–0.072 s against 0.083–0.091 s on generator-shaped data, but fell back on 964 of 1,024 buckets with uniform timestamps (0.10 s) — [alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)
- **Validation order.** Every event check (user, `ts`, type, product, price, late) only sets the invalid bit. Nothing but a malformed line or an empty `event_id` leaves before dedup, and only the earliest copy may have a bad `ts` (fixture E5) — [alternatives.md](file:///C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)
- **DuckDB's salt** cuts random accesses because most probes are settled by the tag; this is the pre-filter principle behind a bitmap-proven "unique" — [DuckDB](https://duckdb.org/2022/03/07/aggregate-hashtable.html)
- **Overfitting warning.** Entries tuned to the 413 original keys lost ground on the 10K-key variant — [Morling](https://www.morling.dev/blog/1brc-results-are-in/)

### Inferences
- **Proven-unique dedup (#1).** It is exact because a record whose hash bits appear exactly once in its partition cannot have a copy: all copies share an `event_id` and therefore a partition and its bits. Flagged records get the full exact treatment.
- **Bins for heavy persons only.** Bins pay when many events share a bin. Applying them only above a person-size threshold (≥256 events, a third of the rows) avoids `sess-bins`' fallback cost on sparse or uniform persons. Below the threshold, a network sort on `u32` times is cheap.
- **One-session shortcut.** If a person's max(`ts`) − min(`ts`) ≤ 30 minutes, they have exactly one session and need no sort. This is exact and costs only a min/max pass, but how often it fires depends on the data. Under the generator's shape (sessions in separate two-hour slots) it fires only for single-session persons.
- **Singleton users.** Their person is themselves, so with user-ID bucketing (#2a) their rows never need a second move.
- **Lazy validation, rejected.** Parsing only `event_id` and `ingest_ts` up front and validating survivors later saves about 4% of field work. It needs a second pass over 1.9 GB of text, or random re-reads, which costs more.
- **Where 1BRC-style data shortcuts are acceptable.** Shortcuts such as "assume one decimal" fit only as fast paths behind an exact check: an ID of 9 bytes or less, an unquoted line, a common `ts` form. The exact fallback must always exist.

### Gaps
- Whether the rules let any other field's validation be skipped for some event types (for example, product validity on non-purchases) could not be checked from within this research scope; it needs a rules audit.
- The bitmap size and false-positive rate the leader actually uses are unknown. The math in #1 is our own estimate.
