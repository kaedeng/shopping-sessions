# Dedup, hash tables, membership filters and partitioning for the sessions engine

Scope: cut the CPU and memory traffic of (a) exact dedup of ~21.6M event lines on event_id (keep min ingest_ts; dedup before validation; global because copies can name different users) and (b) the scatters around it (parse scatter, person scatter, link lookup), on 8 SMT vCPUs (about 4 physical cores) of a Cascade-Lake-or-newer Xeon with AVX-512.

Workload arithmetic used throughout (derived from the facts in the brief, not from a source):
- 21.59M lines, 20.0M distinct ids; 796K ids have copies and add 1.59M lines, so a duplicated id has 3.0 lines on average and **2.39M lines (11.1%) belong to multi-copy groups**; 1.59M lines (7.4%) are losers; 89% of lines are singletons.
- Cross-user copies: about 1/64 of duplicated ids, i.e. about 12.4K ids (about 37K lines) in the test set, but unbounded in general.
- Bytes per 21.6M records: 32 B = 691 MB, 16 B = 345 MB, 12 B = 259 MB, 8 B = 173 MB. Person records: up to 20M x 16 B = 320 MB. Link lookups today: up to 20M random 64 B lines = 1.28 GB of DRAM reads (64 MB table is far above any L3 share of a 4-core slice).
- With 1024 by-id partitions: about 21.1K records per partition.

---

## Q0 (synthesis). Which ideas, ranked, cut dedup and scatter CPU the most for this engine?

### Takeaway
On a 4-physical-core slice, every global random-access structure over 20M keys (Bloom filter, counting sketch, concurrent hash table) costs at least one 64 B cache-line miss per key, which is several times more memory traffic than streaming a compact 8-16 B record through a radix partition and back. So the best plan is partition-first with tiny records, not filter-first: scatter a compact by-id record (16 B, or 8 B in a stretch variant), dedup exactly per partition in an L2-resident table, kill losers in place through back-pointers (race-free, no atomics), and keep the planned by-user main scatter so the 64 MB link lookups become L2-local slices. Then shrink and recycle the person scatter.

### Cited Findings
- Radix-partitioned joins are bandwidth-bound and their advantage depends on narrow tuples: "The RJ is up to three times faster than the BHJ for small tuples, but it completely loses the advantage once the tuple size exceeds 32 B"; RJ performance "degrades by a factor of 7" while the non-partitioned BHJ "remains constant for five times larger tuples" (Intel i9-9900X Skylake-X, 10C/20T, 1 MiB L2, 19 MiB LLC, 79.4 GiB/s) — [Bandle, Giceva, Neumann, SIGMOD 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- "As long as the build side fits into the LLC, the global hashtable does not suffer from cache misses, rendering partitioning useless. For larger hashtable sizes, prefetching reduces the cache misses for BHJ, while partitioning shows its strength" — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- Both partitioning passes and the join phase were measured as memory-bandwidth-bound ("confirms the futility of adding more hardware contexts, and why increasing the payload size hurts the performance") — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- On Ryzen 9 3950X (memory bandwidth per core 60% of Skylake-X) the RJ "reached the memory bandwidth limit much faster" and slowed down with more threads — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- A Bloom-filter pre-probe "introduces up to one cache miss per lookup"; when more than 50% of probe keys find a partner the unfiltered radix join overtakes the filtered one — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- Random DRAM access throughput on a Skylake-class core is capped by line fill buffers: about 80 ns latency / 12 LFBs = 6.67 ns theoretical; measured limit 7.4 ns per cache line single-threaded; with several cores, throughput approaches the 25.6 GB/s bandwidth limit (about 2.5 ns/key aggregate), reached at about 4 threads (i7-10750H, DDR4-3200) — [Groot Koerkamp, PtrHash post](https://curiouscoding.nl/posts/ptrhash/)
- Multi-threaded prefetching for random off-chip accesses saturated at 4 threads on a 6-core Xeon x5670 because the LLC's outstanding-miss queue (32 load entries) is shared — [Kocberber et al., AMAC, PVLDB 2015](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- Global concurrent tables can beat partitioning for GROUP BY, but on machines with huge caches (AMD EPYC 9454P, 48C, 256 MB L3; Xeon Gold 5412U, 45 MB L3). For "unique" keys (100M distinct) at 48 threads, Folklore* ticketing scaled only 13.8x (vs 37.6x for low cardinality); peak memory 4.1 GB (atomics) vs 5.1 GB (partitioned) — [Xue & Marcus, arXiv 2505.04153](https://arxiv.org/html/2505.04153v2)

### Inferences
Ranked list (estimates are back-of-envelope from the workload arithmetic plus cited per-access costs; verify with perf on the scorer-like Docker setup):

1. **Replace the planned global event_id filter with a compact by-id record scatter and exact per-partition dedup ("partition-first, tiny records").**
   - Mechanism: in the parse loop, for every event line (valid or not) emit a by-id record D16 = [event_id u64 | ingest u32 | main-slot ref u32] through the existing SWWC/streaming machinery, partitioned by the top bits of a multiply-shift hash of event_id. The ref is (worker, user-partition, index) of the line's 12 B main record (known at scatter time; 3 + 10 + 19 bits leaves ample room); invalid lines get a null ref, so validity needs no extra bit. Dedup task per by-id partition: insert into an L2-resident open-addressing table keyed by id; groups of size 1 (89% of lines) need nothing more; for groups of size 2 or more, pick min ingest; if the winner is invalid, kill all valid copies, else kill all other copies. A kill = set the drop bit in the main record through its ref.
   - Why it is exact and race-free: all copies of an id land in one partition (co-location by id is the only exact way to catch cross-user copies). Each main record is referenced by exactly one by-id record, so at most one task ever writes a given main record; a plain store to the record's own ts field cannot clobber neighbours (no atomics).
   - Why it beats the planned filter: a one-pass "seen before?" filter flags only second and later copies, so the 796K first copies need a second full pass (all 21.6M side records re-read plus a second probe structure). A 1%-FPR register-blocked Bloom filter needs about 12-14 bits/key (see Q2) = 30-35 MB for 20M keys, which is DRAM-resident here; pass 1 costs about one RFO plus one writeback line per insert (about 21.6M x 128 B = 2.8 GB) and pass 2 about 21.6M x 64 B = 1.4 GB, versus 2 x 345 MB = 0.69 GB for a D16 round trip (0.35 GB for D8). At the cited 7.4 ns per random line per core, the two filter passes alone are at least about 0.32 s of single-core time, at least about 80 ms wall over 4 physical cores, before any exact resolution.
   - Expected effect: the dedup stage reads 345 MB sequentially instead of 691 MB and does about 21.6M L2-resident table operations, about 2.4M group resolutions and 1.6M kills. Combined with idea 2, a plausible single-core cost for dedup plus person grouping is about 0.27-0.37 s versus 0.61 s today, i.e. about 0.25-0.35 s single-core saved (about 40-80 ms wall at a 4-6x SMT-effective speedup). This is an estimate.
   - Risks: two scatter streams in the parse double the SWWC state per thread (see idea 4); partition skew (a single id copied 1M times) must be handled by growable tables; the kill phase must finish before main records are consumed (phase barrier).
   - Test: differential test against a naive HashMap reference on generated data with injected corner cases (cross-user copies across workers and files, kept-copy-invalid, equal-ingest ties, a giant group, all ids forced into one partition); perf stat of the stage (cycles, LLC misses, dTLB misses) before and after.

2. **Keep and prioritise the planned by-user main scatter, and partition the link table by the same user hash (a radix join).**
   - Mechanism: main record (user u64, ts u32 with drop bit) scattered by hash(user); build the link table as 1024 slices by the same hash, so a user-partition task probes only its slice (64 MB / 1024 = 64 KB, L2-resident) instead of random DRAM lines.
   - Why: today about 20M lookups touch about 1.28 GB of random DRAM lines; Bandle's "build side larger than LLC" regime is exactly where partitioning pays, and the probe tuple is narrow (12 B, well under the 32 B cliff).
   - Expected effect: removes about 1.28 GB of random reads (at least about 150 ms single-core at 7.4 ns per line). Likely the largest single item in the current 0.61 s stage (unverified; see Gaps).
   - Risks: users with very many events (skew) inflate one partition; the link table build must use the identical hash.
   - Test: time the user stage with real lookups versus a dummy lookup to measure the lookup share; count LLC misses.

3. **Defer everything except the id to the rare path (fingerprint plus verify), enabling an 8 B by-id record (stretch variant D8).**
   - Mechanism: only the 2.39M lines in multi-copy groups ever need ingest or validity. D8 = [32-bit fingerprint of hash(id) independent of partition bits | 32-bit main-slot ref], and the main record carries a 4 B source-line locator (main becomes 16 B) so the rare path can re-parse the CSV line (id, ingest, validity) to verify fingerprint matches exactly and pick the winner. With 32-bit fingerprints, false matches are about C(21.1K,2)/2^32 = 0.05 per partition, about 53 pairs in total, all resolved by exact comparison.
   - Expected effect: by-id traffic 345 MB to 173 MB, main 259 MB to 345 MB: a net of about 86 MB less per pass, plus about 2.4M x 2 random accesses on the rare path (about 40-50 ms single-core). Modest; do it only after ideas 1-2 and only if input buffers stay mapped (mmap or retained page-cache-backed buffers).
   - Risks: requires retained input; must never decide on fingerprint equality alone.
   - Test: a debug build with 4-bit fingerprints forces thousands of false matches and exercises the verification path.

4. **SWWC tuning for two streams on SMT cores (cheap, mechanical).**
   - Mechanism: 1-2 cache lines per partition buffer at 1024 or more partitions; one 64 B `_mm512_stream_si512` per flush instead of two 256-bit stores; keep fill state and destination inside the buffer line for 1024 or more partitions (separate arrays below that); 2 MB pages for outputs; keep the total SWWC footprint of both hyperthreads well under the per-core L2 (for example 1024 by-user x 2 lines + 1024-2048 by-id x 1 line is about 192-256 KB per thread).
   - Why: today 1024 x 4 lines = 256 KB per worker for one stream; adding a second stream at 4 lines would put about 1 MB of buffers per physical core, the whole L2 on Cascade Lake.
   - Expected effect: parse-scatter cost roughly unchanged despite the second stream; Schuhknecht measured 1.08 to 0.96 s at 1024 partitions from the micro-layout alone (see Q5).
   - Test: sweep lines per buffer {1, 2, 4} and by-id fan-out {512, 1024, 2048} on the x86 box.

5. **Runtime-adaptive bit packing of the person record (16 B to 8 B).**
   - Mechanism: by the time the person scatter runs, all value ranges (ts min and max, pay max, number of persons) are known, so field widths can be chosen at run time, e.g. [dense person index | ts - ts_min | pay] in 64 bits, with a fallback to 16 B if they do not fit. Assign dense person indices when compacting link components.
   - Expected effect: person scatter 320 to 160 MB written and read; 160 MB less fresh memory.
   - Risk: none for exactness if the fallback exists; this is not hard-coding because widths come from the data at run time.

6. **Owner-computes person placement: person bucket = partition of a representative user (in-place, mostly no second scatter).**
   - Mechanism: in the user task, events whose person's representative user hashes to this same partition stay where they are (compact in place, no fresh buffer); only events of multi-user persons whose representative lives elsewhere are forwarded (scattered). Sessionize per partition over local plus received records.
   - Expected effect: data-dependent. If most persons are single-user, most of the 320 MB person scatter and its fresh memory disappears. Measure the fraction of events whose user's representative is in another partition.
   - Risk: load imbalance; the representative must be chosen deterministically (e.g. min user id).

7. **Memory recycling and page-fault hygiene.** Use chunked outputs from a shared free list so stage N+1 writes into chunks freed by stage N (fresh memory becomes about the peak live set instead of the sum); never DONTNEED or munmap mid-run; use THP for big arenas but avoid sparse per-partition reservations (THP zeroes whole 2 MB pages); if any thread is idle early, pre-fault with MAP_POPULATE there. Zeroing cannot be skipped for fresh anonymous memory (see Q5). Rule of thumb from a best-case of about 10^6 cycles per 2 MB huge page: about 0.15-0.2 s of single-core CPU per GB of fresh memory avoided (an inference from a single-setup number).

8. **Optional micro-optimisation: an L1 bitmap pre-pass inside each by-id partition.** No atomics because a partition belongs to one task. A 2^18-bit (32 KB) one-hash bitmap over about 21K keys flags second-and-later copies plus about 4% false positives; a second scan from L2 recovers first copies; only about 13-15% of records touch an exact table. Possibly 1.5-2x on the table part only. Measure before adopting.

9. **Not recommended here: the global filter-first design (planned), global concurrent hash tables, and sort-based dedup.** Reasons in Q1, Q2 and Q4. Global structures win on many-core, big-L3 servers (Xue & Marcus) or when the filtered side is small (Bandle's BRJ), neither of which holds here. Sorting only makes sense if a sort is already needed (hashing and sorting are equivalent asymptotically, per Müller et al.).

### Gaps
- The breakdown of the current 0.61 s single-core dedup stage into record reads, table inserts, link lookups and person scatter is not known; ideas 1-2 assume link lookups dominate. Measure first.
- The L3 share actually available to the scorer's 4-core slice is unknown (L3 is shared socket-wide, possibly with noisy neighbours). It decides whether any 16-32 MB structure could be partly cache-resident.
- The fraction of events belonging to multi-user persons (decides idea 6) is not given.
- The brief does not say how lines whose event_id itself fails to parse take part in dedup; the designs above assume such lines are dropped and do not join any group. Check the rules.

---

## Q1. What is the cheapest exact way to find the ~4% duplicated ids among 21.6M random 64-bit keys on 8 hyperthreads?

### Takeaway
Radix-partition compact records by hash(id), then run an exact, cache-resident table per partition. Partitioning wins on this hardware because the key set is far larger than any cache share and the records can be narrow (8-16 B, under Bandle's 32 B cliff). Non-partitioned shared tables win only when the table fits in the LLC or the machine has huge caches and many cores. Sort-based dedup is equivalent in theory but costs more passes. Filter-first cannot avoid touching all keys twice in a DRAM-sized structure.

### Cited Findings
- Literature split: "Balkesen et al., and Schuh et al. claim that radix partitioned joins are superior, while Lang et al. state the opposite"; Bandle et al. integrated a state-of-the-art radix join (SWWCBs, non-temporal streaming, two-pass partitioning) into Umbra; in TPC-H it brought "a noticeable improvement in only one out of all 59 joins", mainly because partitioning materialises tuples that never reach the result in selective joins — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- In microbenchmarks with near-optimal parameters the radix join is "10 to 40% faster than the BHJ for low selectivities", "up to three times faster for small tuples", and loses the advantage above 32 B tuples; skew (Zipf above 1) favours the non-partitioned join — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- Xue & Marcus's global "Folklore*" table: lockless linear probing adapted from Maier et al.; slots hold key plus ticket; 0 = empty, 1 = write in progress; inserts claim a slot with a single-word CAS; aggregates updated by atomics or thread-local vectors. "A form of fully concurrent aggregation reached parity or better in almost all cases", except high-cardinality heavy hitters; atomics are the best choice for high-cardinality or unique keys; resizing raised unique-key latency 5.2x (atomics) — [Xue & Marcus 2025](https://arxiv.org/html/2505.04153v2)
- Maier, Sanders, Dementiev: lock-free linear-probing tables extended to general types and growth; "considerably faster than the best algorithms with similar restrictions" and "an order of magnitude faster" than the best general tables (abstract only; no absolute numbers retrieved) — [arXiv 1601.04017](https://arxiv.org/abs/1601.04017)
- Müller et al. argue hashing has the same complexity as sorting in the external-memory model, and give an aggregation algorithm that adaptively switches between inserting into cache-sized hash tables (early aggregation) and appending to hash partitions without aggregating; up to 3.7x over the state of the art — [Müller, Sanders, Lacurie, Lehner, Färber, SIGMOD 2015](https://core.ac.uk/download/524940031.pdf)
- A 2020 critique accepts "hashing is in fact equivalent to sorting by hash value" but disputes that only hashing allows early aggregation — [arXiv 2010.00152](https://ar5iv.arxiv.org/html/2010.00152)
- Cross-domain evidence from k-mer counting (the canonical "which keys occur at least twice among billions" problem): KMC 2 builds on MSPKmerCounter's minimizer-based design, replacing minimizers with "signatures" (a selected subset of minimizers), and is "at least twice faster than the strongest competitors (Jellyfish 2, KMC 1)", using about 12 GB RAM; 28-mers of a 44x human read set (106 GB compressed) in about 20 min on a 6-core i7 with SSD (disk-based tool; the abstract does not detail the binning step) — [Deorowicz et al., KMC 2, arXiv 1407.1507](https://arxiv.org/abs/1407.1507)

### Inferences
- Cost model for this workload (per 21.6M keys):
  - Partition-first, D16: parse writes 345 MB with streaming stores (no RFO), the dedup stage reads 345 MB sequentially; table operations hit L2. About 0.69 GB of traffic, no atomics.
  - Global concurrent table (Folklore-style): 20M keys x 16 B slots at 50% load is about 640 MB, so nearly every insert is a DRAM line RFO plus writeback (about 128 B) plus a CAS: about 2.8 GB of traffic and LFB-bound latency. Xue & Marcus's favourable results come from 45-256 MB L3 machines with 24-48 cores and do not transfer to a 4-core slice.
  - Filter-first: two passes of random line-granular accesses into a 30+ MB structure (see Q2), about 4 GB.
  - Sort-based: an MSD radix pass equals the partition pass; the in-partition sort then needs 2 or more passes over each partition (or an AVX-512 sort) versus a single hash insert. Use only if records must be sorted anyway.
- Why the 4% rate does not favour early aggregation: Müller's adaptive scheme gains when in-cache tables collapse many duplicates; here only 7.4% of lines are redundant and they are uniformly shuffled across files, so in-cache pre-aggregation during the parse would almost never hit. Plain partitioning is the right branch of their algorithm.
- The k-mer counting field converged on partition-then-sort or partition-then-count designs (KMC) over shared tables, which matches the hardware argument here.

### Gaps
- Balkesen et al. ICDE 2013 and Schuh et al. numbers were not retrieved directly (only via Bandle's summary).
- No source measured dedup specifically (as opposed to joins or aggregation) on a 4-core SMT slice; the per-key costs above are composed from join, aggregation and random-access measurements.

---

## Q2. Filter-first designs: which filters, what FPR and size, what insert/probe cost, how to handle concurrency, and how to resolve candidates exactly?

### Takeaway
If a filter is used, it should be a register-blocked or cache-sectorized Bloom filter (one cache line per key; k = 3-5 bits in one word). But a global filter over 20M keys needs about 12-14 bits/key (30-35 MB) for about 1% FPR, which is DRAM-resident here. It needs atomic test-and-set to stay exact under concurrency, and it still misses first copies, forcing a second full pass. Xor and binary fuse filters are static and cannot detect duplicates at all. Filters pay off only inside partitions (L1/L2-resident, single owner, no atomics).

### Cited Findings
- Register-blocked Bloom filters (block = one 32- or 64-bit word) test all k bits with one load and one compare; f = 1% needs about 10 bits/key for a classic Bloom filter versus about 12 (64-bit blocks) or 14 (32-bit blocks) bits/key for register-blocked; with B = 32, f = 0.1% needs 32 bits/key — [Lang, Neumann, Kemper, Boncz, VLDB 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- For register blocking, k between 3 and 5 performs best; "filters with a k less than 3 are not practical altogether"; cache-sectorized filters sweet spots k = 6 or 8 with z = 2 words per cache line — [Lang et al. 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- SIMD batched lookups (one key per lane, gather) reduce cost "to less than two cycles" per lookup for low k on L1-resident filters; SIMD speedups up to 10x over scalar; AMD Ryzen gains less than 50% because of slow gathers — [Lang et al. 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- When a block exceeds one word without sectorization, lookup performance drops about 60% (cache-resident) and about 50% (DRAM) (Xeon E5-2680v4, 28 threads, k = 16). The Figure 5 axes top out near 4000 M lookups/s for a 16 KiB filter versus about 1000 M lookups/s for a 256 MiB filter (approximate reading of the plot axes) — [Lang et al. 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- Cuckoo filters access two cache lines, which "almost doubles their lookup costs compared to Bloom filters" once the filter spills past L2; cuckoo insertion "may fail" when buckets are full; Bloom filters win in high-throughput (low saved-work) settings, by up to 3x on Skylake-X (i9-7900X: 10C/20T, AVX-512, 32 KiB L1, 1 MiB L2, 14 MiB L3) — [Lang et al. 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- The performance-optimal filter minimises lookup cost plus f times saved work; a lower f "only pays off if" f x tw exceeds the extra lookup cost — [Lang et al. 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- Counting Bloom filters, spectral Bloom filters and the counting quotient filter (Pandey et al. 2017) can represent bags (multiplicities) — [Lang et al. 2019, related work](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- Umbra's Bloom-filtered radix join uses register-blocked filters built during the second partitioning pass, and "the writes to the Bloom filter can be done in parallel without synchronizing as two partitions cannot share blocks"; it ran about 40% faster for 5% foreign-key join partners — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- Binary fuse filters are within 13% of the space lower bound (8% for the smaller variant), versus 23% for xor filters and 44% for Bloom filters, and construct "more than twice as fast" as xor filters — [Graf & Lemire, arXiv 2201.01174](https://arxiv.org/abs/2201.01174)
- Xor-type filters assume all keys are available at construction time; the Go xorfilter package describes filters as immutable and advises avoiding many duplicated keys (search-result summaries; full docs not read) — [FastFilter/xorfilter docs](https://pkg.go.dev/github.com/FastFilter/xorfilter); [ar5iv 2201.01174](https://ar5iv.labs.arxiv.org/html/2201.01174)
- BFCounter (k-mer counting) uses a Bloom filter as a staging area: a k-mer goes into the hash table only once the filter says it has been seen; the method identifies every k-mer present more than once, at the cost of some false positives (numbers not retrieved; PMC and Springer pages blocked automated access) — [BFCounter project page](https://web.stanford.edu/group/pritchardlab/bfcounter.html); [Melsted & Pritchard 2011 PDF](https://bmcbioinformatics.biomedcentral.com/track/pdf/10.1186/1471-2105-12-333)
- Intel's SDM (as quoted in secondary sources) lists a LOCK among the events that can force out partially filled write-combining buffers; whether locked instructions order non-temporal stores exactly like MFENCE was left unresolved in an Intel forum thread — [Intel community thread](https://community.intel.com/t5/Intel-ISA-Extensions/The-memory-ordering-semantics-of-mfence-versus-those-of-locked/m-p/1181235)

### Inferences
- Size versus cache for a global filter over 20M distinct ids: L2 (1-2 MB) gives 0.4-0.8 bits/key, which is useless; an assumed L3 share of 8-16 MB gives 3-6 bits/key, i.e. register-blocked FPR of roughly 10-30%, too weak. About 1% needs 30-35 MB, which is DRAM-resident on this VM. So no global filter fits in cache.
- One-hash "seen-before" bit array math (one-pass test-and-set; FPR averaged over insertion order is 1 - (m/D)(1 - e^(-D/m))): m = 2^28 bits (32 MB) gives about 3.6% average, about 0.73M false flags; m = 2^29 (64 MB) gives about 1.8%. Register-blocked k = 3 at 13.4 bits/key ends near about 1% final FPR and lower on average.
- The first-copy problem is structural: a one-pass filter flags only the 1.59M second-and-later copies. The 796K first copies can only be found by a second pass over all 21.6M keys (probing a structure built from flagged hashes) or by a full index. Either way every key is touched twice in a large structure.
- 2-bit saturating counters (0, 1, 2+) as a count-min-style sketch detect "seen twice" in pass 2 without a second structure, but pass 2 sees the final, full sketch, so its FPR is the final-fill rate (for example about 7% at k = 1 with 2^28 counters), higher than the time-averaged rate of one-pass test-and-set.
- Concurrency and exactness:
  - A plain (non-atomic) read-modify-write of a shared filter word can lose another thread's bit. That is a false negative, a missed duplicate, and therefore wrong output.
  - Exact options: atomic test-and-set (`lock bts` for k = 1; a `lock cmpxchg` loop for multi-bit masks, because `lock or` does not return the old value), or byte-per-slot arrays with plain stores. Byte stores avoid lost neighbour updates but still race on "both threads saw 0" for the same key, so they need a counting second pass.
  - Per-thread filters merged with OR or "at least 2 of 8" are a bad fit: with 8 per-worker 4 MB one-hash bitmaps (about 2.7M lines each, 7.7% fill), a singleton's bit is also set in another worker's bitmap with probability 1 - 0.923^7, about 43%.
- Interplay with the scatter: about 21.6M locked operations in the parse loop would sit between streaming stores. If a LOCK forces out a partially filled WC buffer (two 256-bit halves of a line not yet combined), the result is partial-line writes. Full-line `_mm512_stream_si512` stores remove that window. This is a performance risk, not a correctness one.
- Filter economics in Lang's terms: the work a filter saves per negative key here is moving a 16 B record out and back (32 B sequential, a few ns), while a DRAM-resident filter lookup costs a random 64 B line (at least 7.4 ns per core when LFB-bound). f x tw can never exceed the lookup cost, so a global filter cannot pay for itself. Inside a partition the filter is L1-resident and costs about 1-2 cycles, so it can pay (ranked idea 8).
- Exact resolution for any filter variant: candidates (flagged plus first copies) go through an exact hash table keyed by the full id (or fingerprint plus verification through a back-pointer). Never decide on filter or fingerprint equality alone.
- A static filter (binary fuse or register-blocked Bloom) is useful elsewhere: if user-to-person mapping is ever done at parse time, a filter over linked users built once from the link file (about 1-2M keys at about 9-12 bits/key, about 2-3 MB) could skip link-table probes for unlinked users. This needs measurement of how many users are linked.

### Gaps
- No measured numbers were found for concurrent atomic inserts into a Bloom filter (Lang measures lookups only); the cost of `lock cmpxchg` on a missed line on Cascade Lake was not sourced.
- The exact SDM wording on LOCK and WC buffers was seen only via secondary sources (SDM Vol. 3A section 11.3.1 should be checked).
- BFCounter's measured memory and time figures could not be retrieved.
- Polychroniou & Ross "Vectorized Bloom filters" (DaMoN 2014) numbers were not retrieved; Lang reports that a SIMD classic Bloom filter of that kind "was never performance optimal" in their experiments.

---

## Q3. Hash table engineering for the per-partition (or any) dedup table

### Takeaway
For about 21K 8 B keys per partition at load factor 50% or below, held in L2, the simplest open-addressing scheme (linear probing with multiply-shift hashing, array-of-structs slots) is the right choice. Swiss-table tags, Robin Hood and cuckoo help at high load factors or with large keys, neither of which applies. Prefetching matters for DRAM-resident tables and can hurt cache-resident ones. Store only what the common case needs (key, or fingerprint plus ref).

### Cited Findings
- Multiply-shift ("Mult") was the fastest hash function with every scheme tested and "performs essentially always better than Murmur"; Mult shows more collisions on some structured (grid) distributions, a higher variance — [Richter, Alvarez, Dittrich, PVLDB 2015](https://www.vldb.org/pvldb/vol9/p96-richter.pdf)
- Insert ranking: LP, QP and RH have similar insert costs; RH is "slightly slower than LP and QP" (within under 10%); "For insertions, QP seems to be the best option in general. Even when LP or RH are sometimes better, the difference is rather small, less than 10%"; RH is "an excellent all-rounder" for lookups; cuckoo (4 tables) has stable but low insert throughput; at 50% load collisions are rare and "more sophisticated strategies for handling collisions can not benefit as much" (single-threaded Xeon X5690, THP enabled, prefetching, HT and turbo disabled) — [Richter et al. 2015](https://www.vldb.org/pvldb/vol9/p96-richter.pdf)
- Array-of-structs "is significantly superior to SoA for insertions"; with SIMD, SoA has an edge because keys are densely packed — [Richter et al. 2015](https://www.vldb.org/pvldb/vol9/p96-richter.pdf)
- Swiss tables split a 64-bit hash into H1 (57 bits, position) and H2 (7 bits) stored in a one-byte control entry per slot; SSE compares 16 control bytes at once to winnow candidates, letting the table search "very deep probe chains inexpensively"; one byte of overhead per entry — [Abseil Swiss tables design notes](https://abseil.io/about/design/swisstables)
- Prefetching: for a uniform 2 GB x 2 GB hash join, group prefetching, software-pipelined prefetching and AMAC gave 2.8x, 3.8x and 4.3x single-thread speedups over no prefetching; with skew, the best older technique gained only 39% while AMAC reached up to 2.8x; for an LLC-resident table (2 MB build side) the no-prefetch baseline was 32% faster than GP and SPP (AMAC +21%); AMAC is insensitive beyond about 8 in-flight lookups (Xeon x5670) — [Kocberber et al. 2015](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- Relaxed operator fusion loosens fused pipelines with small intermediate materialisation so inter-tuple parallelism (prefetch, SIMD) can be exploited — [Bandle et al. 2021, describing Menon et al.](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- Concurrent folklore table: slot claimed with a single-word CAS (0 = empty, 1 = write in progress), lock-free fast-path lookups — [Xue & Marcus 2025](https://arxiv.org/html/2505.04153v2); general and growable variants — [Maier et al.](https://arxiv.org/abs/1601.04017)
- FxHash-style multiplication by an odd constant is invertible mod 2^64 (collision-free on 64-bit keys); range reduction via the high half of a product (Lemire) needs one multiply and a mask, and Lemire's "fast mod" needs two multiplies for non-power-of-two sizes below 2^32 — [Groot Koerkamp, PtrHash](https://curiouscoding.nl/posts/ptrhash/)
- "Magic modulo" replaces division by a multiply-shift so filter and table sizes need not be powers of two — [Lang et al. 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)

### Inferences
- Per-partition table sizing (1024 partitions, about 21.1K keys): 32K slots x 8 B = 256 KB (load 0.64) or 64K x 8 B = 512 KB (load 0.32). Two hyperthreads per core each hold one table plus a streaming input, so 256 KB tables fit a 1 MB (Cascade Lake) L2. With 2048 partitions, 16K slots x 8 B = 128 KB at load 0.64.
- Slot content: [id-or-fingerprint | record index] 8 B, or 16 B slots [id u64 | ingest u32 | idx u32] if the min-ingest update should happen in the table. Since only 11% of lines are in multi-copy groups, a lean alternative is a table of [fp32 | idx32] that records "first index seen" and pushes collisions to a side list for the rare path.
- "Bucketised linear probing" with AVX-512: treat each 64 B line as 8 key slots and compare the probe key against all 8 with one `vpcmpeqq` plus a mask test (and the empty marker with a second compare). This gives the effect of Swiss-table SIMD probing without a control-byte array, since keys are only 8 B (an inference; no measured source).
- Prefetching policy: the per-partition tables are L2-resident, where AMAC's data shows prefetching can be neutral or harmful. Try prefetch distance 0, 8 and 16. Keep strong prefetching (or better, partitioning) for DRAM-resident structures such as today's 64 MB link table.
- Robin Hood, cuckoo and Swiss tables add per-insert work that only pays at high load or for unsuccessful lookups with long chains. Our insert-dominated, mostly-unique workload at low load is Richter's "simplest scheme wins" regime.
- Hash bits: take the partition index from the top bits of a single multiply (odd constant), the table index from the next bits and the fingerprint from the rest, so a single 64x64 multiply feeds all three. Correctness never depends on hash quality (collisions are resolved exactly); only skew and performance do.

### Gaps
- No source measured hash-table inserts on Cascade Lake or Ice Lake with two hyperthreads sharing one L2; table sizes above are reasoned from cache sizes.
- AVX-512 bucketised probing was not found measured in a primary source.

---

## Q4. Can dedup be fused into another pass, or done per raw-user bucket with a small global fix-up? Are there exact two-pass sketch-then-candidates schemes?

### Takeaway
Dedup cannot be made exact inside the user or person scatter alone, because cross-user copies land in different buckets and finding them requires co-locating every id by id at least once. What can be fused: emit the by-id record in the same parse loop as the main record, apply kills in place in the main records (no extra pass), fold validation into the parse as a drop bit, and fuse link lookup with the person scatter in the user task. Exact two-pass sketch schemes (BFCounter style) exist but cost two random passes over all keys, so they lose to partitioning on this hardware.

### Cited Findings
- Non-partitioned joins keep tuples in registers through operator fusion and do not materialise the probe side, which is why they win in selective queries; radix joins lose when most shuffled tuples never reach the result — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- BFCounter two-stage idea: the Bloom filter tracks keys seen so far and the hash table stores only keys seen at least twice; every repeated key is identified, with some false positives — [BFCounter project page](https://web.stanford.edu/group/pritchardlab/bfcounter.html)
- Bloom-filtered radix join: the filter is built during the second build-side partitioning pass and probed before partitioning the probe side, to drop tuples early; adaptively switched off by sampling when almost all tuples pass — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)

### Inferences
- Lower bound for exact dedup: every one of the 21.6M ids must be placed in some by-id structure (partition or global index) at least once, because any id can have a copy under a different user. A "per-user dedup plus small fix-up" is only exact if the fix-up already knows which ids are cross-user, which needs that same global structure. So the global by-id step stays; the goal is to make it as narrow as possible (16 B to 8 B records) and to keep everything else off it.
- Exact fused pipeline (recommended):
  1. Parse: main record by user (12 B) plus by-id record (16 B) in one loop; validity becomes the drop bit (invalid lines get a null ref but keep their by-id record because the kept-copy-invalid rule needs them).
  2. Barrier, then by-id tasks: exact dedup; kills written into the main records' drop bits (race-free: one writer per record).
  3. Barrier, then user tasks: skip dropped records; L2-local link lookup; person scatter (or owner-computes placement, ranked idea 6).
  4. Sessionize.
  The kept copy's user automatically decides the person, because only the winner's main record survives.
- Same-user copies (63/64 of duplicated ids) could be removed during the user task for free if the main record carried the id, but that adds 8 B per main record and still leaves the cross-user case. Not worth it once the by-id pass exists.
- A two-pass sketch scheme that is exact: pass 1, a global atomic test-and-set Bloom filter, with flagged (second-and-later) copies appended to a list; then a small exact set (or a 2-4 MB filter) over flagged fingerprints; pass 2, re-scan all keys probing that small set to pick up first copies; then exact resolution over about 2.4M plus false-positive candidates. Exact, but pass 1 is about 21.6M random RMWs into a 30+ MB structure (about 2.8 GB of traffic) and pass 2 re-reads every key. It is dominated by the partitioned approach unless records are very wide.
- The planned "sequential 16 B side record plus shared filter" is the two-pass scheme above with the side records as the re-scan input. The minimal change that makes it partition-first: scatter the side record by hash(id) instead of writing it sequentially, and drop the filter.
- Overlap opportunity: by-id dedup tasks (sequential reads plus L2 tables, compute-light) and other memory-latency-bound work could be co-scheduled on sibling hyperthreads, but the kill-before-consume dependency needs a barrier between dedup and user tasks. Measure whether a split barrier per user-partition (only that partition's kills) is worth the complexity.

### Gaps
- No source evaluates dedup fused with a downstream grouping in an engine like this; the fused pipeline is reasoned, not measured.

---

## Q5. Reducing scatter cost and fresh memory: record size, key compression, in-place partitioning, buffer reuse, page zeroing, fan-out versus TLB and L1 on hyperthreaded cores

### Takeaway
Use 1-2 line SWWC buffers with full-line streaming stores and 2 MB pages. Size the total buffer footprint for two hyperthreads per L2. Shrink records with runtime-adaptive packing, which is legal because ranges are known before the second scatter. Cut fresh memory by recycling chunks between stages rather than by kernel tricks: fresh anonymous memory is always zeroed (MAP_UNINITIALIZED is unavailable on normal kernels), and MAP_POPULATE only moves the zeroing earlier.

### Cited Findings
- Radix partitioning of 100M 8 B entries, single-threaded, Xeon E5-2407 2.2 GHz (L1 32 KB, L2 256 KB, L3 10 MB; dTLB 64 entries for 4 KB pages and 32 for 2 MB pages; L2 TLB 512): original 2.41 s at 1024 partitions; SW buffers 1.30 s; SW buffers plus streaming stores 1.08 s (best buffer 2 cache lines); plus fill state and destination stored in the buffer line 0.96 s; at 256 partitions 0.84 s. That is about 9.6-10.8 ns per entry at 1024 partitions — [Schuhknecht, Khanchandani, Dittrich, PVLDB 2015](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf)
- With streaming stores "one should use small buffers (less than 8 cache-lines)", with "the best size of a single cache-line from 2048 partitions on"; performance degrades once total buffer space passes about 2 MB (on that machine); the micro row layout gave 1.52x at 16384 partitions, but below 1024 partitions separate arrays were better — [Schuhknecht et al. 2015](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf)
- "In combination with software-managed buffers and streaming stores, the 4KB pages do never pay off" (2 MB pages better) — [Schuhknecht et al. 2015](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf)
- Doubling entry size from 8 to 16 B slowed single-threaded partitioning only 1.31x-1.69x: "not data transfer is the limiting factor but the processing of individual elements in terms of random access costs" (single thread). Contrast: multi-threaded radix joins were bandwidth-bound in Bandle et al. — [Schuhknecht et al. 2015](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf); [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- "With AVX512 instructions, modern Intel processors can store a full cache line at once"; Umbra does not use SWWC buffers for tuples above 64 B because padding costs outweigh streaming benefits — [Bandle et al. 2021](https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf)
- Anonymous mappings: "its contents are initialized to zero"; MAP_UNINITIALIZED ("Don't clear anonymous pages") "is honored only if the kernel was configured with the CONFIG_MMAP_ALLOW_UNINITIALIZED option", which "because of the security implications" is "normally enabled only on embedded devices"; MAP_POPULATE "Populate (prefault) page tables for a mapping" — [mmap(2) man page](https://man7.org/linux/man-pages/man2/mmap.2.html)
- THP: smaller mTHP sizes reduce latency spikes because less memory is cleared per fault; touching one byte of a large mapping can allocate a whole 2 MB page — [Linux kernel THP documentation](https://docs.kernel.org/6.16/admin-guide/mm/transhuge.html)
- Allocating and zeroing a huge page costs about 10^6 cycles in the best case (search-result summary of the paper; not verified in full text) — [PAR/NSF paper](https://par.nsf.gov/servlets/purl/10380347)
- Zeroing accounts for up to 40% of page-fault time on modern hardware (search-result summary) — [PLDI paper PDF](https://static.aminer.org/pdf/20170130/pdfs/pldi/wfzk8xx2umewss7ltuoglgde1injphb6.pdf); in a 100 GB touch-one-byte-per-4 KB benchmark, THP gave about 500x fewer faults and nearly 4x better performance (search-result summary) — [Alibaba Cloud HawkEye write-up](https://www.alibabacloud.com/blog/hawkeye-an-efficient-fine-grained-management-solution-for-huge-pages_595114)

### Inferences
- Fan-out on SMT cores: two hyperthreads share L1D (32-48 KB) and L2 (1-2 MB). Today's 1024 x 4 lines = 256 KB per worker is 512 KB per core for one stream. With two streams (main by user plus by-id), 2 lines per partition (main) and 1 line per partition (by-id, 8 records of 8 B or 4 records of 16 B per line) keep it about 192-256 KB per thread, in line with Schuhknecht's 1-2 line optimum for streaming at 1024 or more partitions.
- Use one `_mm512_stream_si512` per 64 B flush. It halves store instructions and leaves no half-filled WC buffer that a LOCK or interrupt could flush partially.
- Record shrinking that stays exact:
  - By-id record: 16 B now (exact id + ingest|valid + ref); 8 B fingerprint plus ref if the main record carries a source locator (Q0 idea 3).
  - Exact key compression: with an invertible 64-bit hash (odd multiply, optionally xor-shift), the partition index (top P bits) is implied by position, so storing the remaining 64 - P bits keeps the id exact (54 bits at P = 10).
  - Person record: runtime-adaptive widths (dense person index, ts - ts_min, pay) packed into 8 B with a 16 B fallback.
  - Main record: 12 B as planned; the user-partition bits are implied, so with an invertible user hash, 10 bits are spare (room for the drop bit or flags).
- Fresh-memory budget (today about 1.75 GB): today's 32 B parse scatter (691 MB) plus 16 B person records (320 MB) are about 1.0 GB of it. Planned plus D16: 259 + 345 + 320 = 924 MB (about the same). Adding 8 B person records gives 764 MB; adding chunk recycling (person output written into consumed by-id or main chunks) brings the fresh peak to roughly the parse output, about 0.6 GB. By the about 10^6 cycles per 2 MB figure, that is about 0.15-0.2 s of single-core CPU per GB avoided.
- In-place alternatives: the person scatter could become "partition locally in place, gather globally" (each user task permutes its own array by person bucket; person tasks read 1024 small runs). This avoids allocating a second 320 MB array at the cost of an in-place permutation pass; or use owner-computes placement (Q0 idea 6) so local events never move.
- Avoid THP over-zeroing: never reserve per-(worker, partition) regions with slack, because THP will zero whole 2 MB pages for 16K mostly-empty regions. Carve lists from one arena (as the current Parts allocator does) or from fixed chunks.
- MAP_POPULATE does not save zeroing (pages are zero-initialised either way); it only batches faults. It helps wall time only if run on a thread that would otherwise be idle.

### Gaps
- Cascade Lake, Ice Lake and Sapphire Rapids dTLB/STLB sizes and their sharing between hyperthreads were not sourced.
- Whether 512-bit streaming stores trigger AVX-512 frequency licence drops on the scorer's Xeon was not verified; measure.
- No direct per-GB zeroing measurement on the target was found; the 10^6-cycles-per-2 MB figure is a single-setup best case from a search summary.
- Polychroniou & Ross SIGMOD 2014 (in-place and buffered partitioning throughput) was not retrieved directly.

---

## Q6. Competitive-programming style tricks for "find duplicates in a huge array" that beat standard approaches

### Takeaway
The useful tricks are: a single multiply (odd constant, invertible) for hashing, partitioning and fingerprints at once; high-half range reduction instead of modulo; quotienting (store only bits not implied by position) for exact compact keys; birthday-bound fingerprint sizing with exact verification; and "sort by hash equals hash partition" thinking. None of them changes the asymptotics; they cut instructions and bytes per key.

### Cited Findings
- Multiply-shift is the lightest hash (one multiply, one shift) and fastest across schemes — [Richter et al. 2015](https://www.vldb.org/pvldb/vol9/p96-richter.pdf)
- Multiplying by an odd constant is invertible mod 2^64; the high half of a product maps to a range with one multiply and a mask; Lemire's fast mod handles non-power-of-two sizes with two multiplies — [Groot Koerkamp, PtrHash](https://curiouscoding.nl/posts/ptrhash/)
- Magic-number division turns arbitrary-size modulo into multiply-shift (about 10 cycles for 8 SIMD lanes versus 15 with floating-point division) — [Lang et al. 2019](https://www.vldb.org/pvldb/vol12/p502-lang.pdf)
- Hashing is sorting by hash value (external-memory equivalence) — [Müller et al. 2015](https://core.ac.uk/download/524940031.pdf); [arXiv 2010.00152](https://ar5iv.arxiv.org/html/2010.00152)
- SIMD tag comparison of 16 slots at once (Swiss tables) — [Abseil design notes](https://abseil.io/about/design/swisstables)

### Inferences
- One multiply, three uses: h = id x C (C odd). Top 10 bits give the partition, the next 15-16 bits give the table slot, and the low 32 bits give the fingerprint. Because h is a bijection, (partition, remaining 54 bits) is an exact representation of the id, which is "quotienting".
- Birthday sizing: with about 21.1K keys per partition, 32-bit fingerprints give about 0.05 false pairs per partition (about 53 total); 24-bit gives about 13 per partition (about 13K total). Both are negligible next to the 2.39M genuine group members, as long as every match is verified exactly.
- Test-by-sabotage: build with a 4-bit fingerprint or a hash that maps everything to one partition to prove the exact-verification and skew paths; build with ingest ties and kept-copy-invalid groups to prove the rules.
- Branch-free min-ingest update: keep (ingest << 32 | ref) as one u64 and take a min, so ties need no special case (equal-ingest copies are identical by rule).
- Sorting alternative: an AVX-512 sort of each partition by (fingerprint, ingest) followed by adjacent compares gives winners directly and needs no table, but costs 2 or more in-cache passes. It is only a win if a sorted order is consumed later.

### Gaps
- I found no competitive-programming source with measured results for 64-bit duplicate finding at this scale beyond the database and hashing literature above; Lemire's and Neumann's blogs and DuckDB, Umbra or Velox engineering posts were not reviewed in this pass.
