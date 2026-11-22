# ID resolution: union-find over the links, and the event/click-to-person join

Scope: ways to resolve user IDs to persons (connected components of the link graph, named by the smallest ID) and to group about 24.6M event and click rows by person with less CPU, memory traffic and serial time. Repo facts come from the files linked below (paths relative to this notes file). External facts carry web links. Arithmetic on repo numbers is shown so it can be checked. "Dev box" means the x86 test machine in `x86.md` (Ryzen-class, 1 MB L2 per core, **96 MB L3**). "Scorer" means the Xeon in the fresh Docker container (8 vCPUs, L3 unknown).

---

## Q0. Ranked ideas: how to cut the CPU, memory traffic and serial time of ID resolution and the person join

### Takeaway
The per-row lookup only has to choose a **bucket**. It does not have to name the person. Most of the measured cost in this area is the second (person) scatter and the random lookups into a 64 MB table, not the union-find logic. The exact wins, in order:
1. A 1.5–3 MB **"bucket oracle"**: a static function over the 1.2M linked IDs that is exact for linked IDs and returns a fixed but meaningless bucket for every other ID. The meaningless answer does no harm, because an unlinked ID is its own person. Exact persons are then resolved per bucket in an L1/L2-sized table.
2. Use the oracle so the parse's existing 1,024-way partition **is** the person bucket, which deletes the second scatter. If links-first ordering is unacceptable, the planned raw-ID scatter with busiest-ID homing moves only 18.8% of rows.
3. A slimmer links build: order-preserving ranks, 4-byte Rem nodes, and no 64 MB table.

Swapping the algorithm (Afforest, label propagation, Shiloach-Vishkin) does not pay for this forest.

### Cited Findings
**Repo measurements (dev box, 20M-event set)**
- Each run uses 3.47 s of CPU in total (3.05 s user, 0.36 s system), with 5.99 CPUs busy and 0.579 s elapsed. The "Dedup and person scatter stage" takes **13.9%** of CPU samples, and its tasks take 65–68 ms of wall time. — [x86.md](../../x86.md)
- Inside that stage, **24.8%** of samples sit on the store of a 16-byte `PersonEvent` into one of 1,024 person buckets. Another **9.8%** stall just after a prefetch into the link table, "presumably with the line fill buffers full". — [x86.md](../../x86.md)
- With 8 workers, the parse's own 1,024-way scatter costs about a third of the parse. Parse throughput by partition count: 256 partitions 7.9 GB/s, 1,024 partitions 7.2 GB/s. — [x86.md](../../x86.md)
- Transparent huge pages: baseline run 558 ms with THP off, 472 ms with `always`, 483 ms with `madvise`. — [x86.md](../../x86.md)
- Dev box: L2 1 MB per core, L3 96 MB, STLB 3,072 entries. Scorer: L2 1 MB per core and STLB 1,536 entries are *assumed*, L3 unknown, huge-page mode unknown. — [x86.md](../../x86.md)
- `LinkTable::new(keys)` makes `keys.div_ceil(3).next_power_of_two()` lines of 64 bytes (four [id, value] slots each). It is called with `keys = 2 × pairs`, and the source says generated data fills it about 0.4. — [crates/links/src/lib.rs](../../crates/links/src/lib.rs), [crates/links/src/links_serial.rs](../../crates/links/src/links_serial.rs)
- The links build runs three prefetched passes (NUMBER_AHEAD 16, UNITE_AHEAD 32, NAME_AHEAD 8):
  1. Number the IDs into the table.
  2. Run Rem's algorithm with splicing over `Node { parent_id: Id, parent: u32 }`, linking the larger ID under the smaller, so "a root is its tree's minimum".
  3. Overwrite every slot with its root's ID, so the numbering table doubles as the lookup table.
  The union loop opens with `if nx.parent == ny.parent { return }`, an immediate parent check. — [crates/links/src/links_serial.rs](../../crates/links/src/links_serial.rs)
- The person scatter computes the bucket as `person_hash(person) >> (64 - bucket_bits)` and prefetches 64 rows ahead. — [crates/links/src/lookup_prefetch.rs](../../crates/links/src/lookup_prefetch.rs)
- Per bucket, `intern(event.person)` gives a local u32, and the sort key is built as `intern << ts_bits | ts`. — [crates/sessions/src/partitioned.rs](../../crates/sessions/src/partitioned.rs)

**External**
- Skylake-X (i7-7820X) latencies: L2 14 cycles (1 MB); L3 68 cycles at 3.6 GHz (11 MB shared by 8 cores); RAM "79 cycles + 50 ns". The STLB has 1,536 entries for 4 KB and for 2 MB pages; an L1 DTLB miss costs 9 cycles. — [7-cpu Skylake-X](https://www.7-cpu.com/cpu/Skylake_X.html)
- PtrHash (minimal perfect hash):
  - Space: 2.0–3.0 bits/key. Pilots are one byte per bucket; Fast config 2.99 bits/key, Default 2.40, Compact 2.12.
  - A query needs "only a single memory access" for most keys (the pilot), plus a rare remap access.
  - At 10⁹ integer keys: 12.5 ns/query in a plain loop and 8.8 ns streaming (Fast), close to the cited 7.4 ns bound for a random memory access.
  - Construction: about 20 ns/key on 6 threads.
  - Hardware: i7-10750H pinned at 2.6 GHz, 256 KiB L2, 12 MiB L3.
  — [PtrHash paper (HTML)](https://arxiv.org/html/2502.15539v2), [arXiv abstract](https://arxiv.org/abs/2502.15539), [curiouscoding.nl blog](https://curiouscoding.nl/posts/ptrhash/)
- BuRR (bumped ribbon retrieval) stores an r-bit value per key with space overhead "well below 1%", is faster than most retrieval structures, and doubles as a filter with false-positive rate 2^-r. — [arXiv 2109.01892](https://arxiv.org/abs/2109.01892)

### Inferences (ranked list)

**R1. "Bucket oracle": replace the exact 64 MB `id → person` lookup with a cache-resident static function `id → home bucket`, then resolve exact persons per bucket.**
- *Mechanism.*
  1. After the union-find, every person has a home bucket (today's `person_hash(name) >> (64-bits)` works).
  2. Build a retrieval structure over the 1.2M linked IDs whose value is the home bucket (10 bits). Candidates:
     - a PtrHash-style MPHF (0.3–0.45 MB) plus a `u16` value array (2.4 MB), or a `u8` array (1.2 MB) at 256 buckets;
     - a ribbon/BuRR structure with r = 10 (about 1.5 MB, one contiguous access);
     - a 3-probe XOR-style static function.
  3. Every event and click row computes `bucket = oracle(user_id)`.
  4. Scatter the 1.2M linked IDs, each with its person's name, to their home buckets as **seeds** (about 5% of the row count).
  5. Per bucket, intern each row by **user ID** in the bucket-local table, pre-seeded with that bucket's linked IDs, which already carry local person numbers. A miss is a singleton person named by its own ID.
- *Why it is exact.* For a linked ID the oracle is exact, so the row lands with all of its person's other IDs. For an unlinked ID the oracle returns a meaningless value, but that value is a fixed function of the ID's hash, so all rows of that ID (events and clicks) land in the same bucket. Since the ID is its own person, that is all that grouping needs, and the bucket-local intern separates IDs by their real 64-bit value. No membership filter is needed.
- *Refinements.*
  - Store `bucket XOR g(id)` and return `value XOR g(query)`, with g a hash. Linked IDs stay exact, and the meaningless values for other IDs become uniform, so no bucket skew comes from them.
  - Reuse the 64-bit hash the pipeline already computes, so a query costs only one or two loads.
- *Why it fits here.*
  - 39% of event rows (and about 44% of all lookups) are unlinked and need no exact answer.
  - The link side is small and ready early.
  - A per-bucket intern already exists in `partitioned.rs`.
  - The working set drops from 64 MB to about 1.5–2.9 MB, 22–43× smaller.
- *Expected effect.*
  - On the dev box the lookup stall is about 47 ms of CPU per run (0.098 × 0.139 × 3.47 s), with the 64 MB table **sitting in the 96 MB L3**.
  - On the scorer the table almost certainly misses to DRAM (Skylake-class L3 is about 1.4 MB per core, and the STLB reaches 6 MB with 4 KB pages), so today's cost there is probably several times larger.
  - Estimate as a drop-in in `lookup_prefetch.rs`:
    - Dev box: a small net gain (0–30 ms of CPU). The stall today is about 47 ms, and the oracle's own L2/L3 loads take back part of that.
    - Scorer: 25–100 ms of CPU per run, because the 64 MB probes there go to DRAM.
  - The build runs on the links worker beside the parse, as today. Its main value is enabling R2.
- *Risks.*
  - Build time for 1.2M keys is unmeasured (PtrHash reports about 20 ns/key on 6 threads at 10⁹ keys).
  - Bucket-local tables grow from about 650 persons to about 1,500 IDs plus seeds per bucket, roughly 64 KB, still L2-sized.
  - Interned long IDs must hash identically in links and events.
  - The empty ID 0 must stay excluded.
- *Test.*
  - Property tests on random forests with chains, stars, self-links, repeats, long IDs and empty sides: compare every row's local person with `links_serial` as the reference, and assert `oracle(id) == home(person(id))` for every linked ID at build time.
  - Check bucket balance (max/mean).
  - Microbenchmark ns per row in `prototypes/lookupbench`.
  - Run full A/B comparisons after 12 s idle.
  - Emulate the scorer's small L3: co-run an L3 thrasher, or use a 4-core/8-thread AVX-512 cloud Xeon.

**R2. One scatter in total: resolve links first so the parse's existing 1,024-way partition is the person bucket.**
- *Mechanism.*
  1. Parse the small links file on all 8 workers. At 7.2 GB/s this is a few ms.
  2. Run the union-find in parallel (R5) and build the oracle.
  3. Run the event and click parse with partition index `oracle(user)`.
  4. Do dedup, intern, sort and sessionize per bucket in one pass.
  Dedup still works, because duplicate rows share a user ID and so land in the same bucket.
- *Why.*
  - Today every row is scattered twice.
  - The person scatter's stores alone are about 120 ms of CPU per run (0.248 × 0.48 s), plus about 47 ms of lookup stalls.
  - The parse already pays for a 1,024-way scatter, so moving the person partitioning into it costs only an oracle query per row, an L2/L3 load that can overlap with tokenizing.
- *Expected effect.* About 100–150 ms of CPU per run on the dev box (3–4% of 3.47 s), and probably more on the scorer.
- *Risks.*
  - A links-first phase of an estimated 15–30 ms wall on all threads sits on the critical path, so it needs a parallel union-find and a parallel oracle build.
  - The heavy person (about 400K events) now lands in one parse partition, and `Parts` lists must absorb that.
  - *Variant ("late binding"):* keep links on a side worker. The parse uses raw-hash partitions until the oracle is ready, then switches, and only rows parsed before the switch need R3's fix-up.
- *Test.* Use `SS_TRACE` to measure the links-first gap, check parse GB/s with oracle partitioning, and run the exactness tests from R1.

**R3. The planned redesign, refined: partition by raw user ID, resolve once per distinct ID inside the cache, and move only rows of IDs that are not their person's home.**
- *Mechanism.*
  1. Partition the link side by the same raw-ID hash, so each bucket holds a link fragment of about 1.2K IDs (about 20 KB).
  2. Per bucket: dedup, intern user IDs, resolve each distinct ID against the local fragment (a partition-wise join, about 2.0M lookups in all instead of 24.6M, all in cache), and count rows per ID.
  3. A small global step picks each multi-bucket person's home as the bucket of its **busiest** ID, with a fixed tie-break.
  4. Only the rows of non-home IDs move: 18.8% of event rows, about 4.6M.
- *Why.* Links-first ordering is not needed. It removes about 81% of the person-scatter stores (about 97 ms of CPU) and nearly all of the DRAM lookups. Partitioning until the hash table fits in cache is the standard cache-conscious join (see Q3).
- *Risks.* More moving parts: the home election, a second intern for moved rows, and a person whose IDs span many buckets moving to a single one.
- *Test.* Measure the moved-row fraction on generated data (expect about 18.8%), and compare exactness and time against R2.

**R4. A slimmer links build, still serial.**
- *Changes.*
  - Number the 1.78M endpoints in byte order with a radix sort, so each linked ID gets an order-preserving rank. Rem's algorithm can then compare **indices**, and the root is the smallest rank, which is the person's name. `Node` shrinks from 16 B (`parent_id` plus `parent`, about 19 MB for 1.2M nodes) to a 4-byte parent.
  - Drop the 64 MB `LinkTable` and its naming scan over 64 MB once R1 or R3 replaces the per-row lookup.
  - Drop self-links during the parse.
- *Expected effect.*
  - Footprint falls from about 83 MB of randomly touched memory to about 21 MB: 7 MB of edges, 4.8 MB of parents, 9.6 MB of sorted IDs.
  - The 94 ms build should fall to an estimated 30–50 ms on one core, and the remaining work splits across threads.
- *Risks.* Long (interned) IDs need the byte-wise comparator inside the rank sort. An adversarial ID order can deepen trees; the result stays exact and only gets slower (see Q1 and Q4). Test with `crates/links/examples/linksbench.rs` and adversarial chains, such as IDs sorted along a chain fed in chain order or reverse.

**R5. Parallel connected components, needed only if links go first (R2).** Use ConnectIt's UF-Rem-CAS with SplitAtomicOne and no sampling. Its splice rule produces min labels, so with order-preserving ranks the root is still the name. Estimated 5–10 ms wall on 8 threads for 0.89M edges (scaled from ConnectIt's times; see Q1). Risk: concurrent splicing must use a combination proven correct; ConnectIt excludes FindCompress with SpliceAtomic as incorrect.

**R6. If the per-row global lookup stays: size it right, filter it, and use huge pages.**
- The table is sized from the 2 × pairs upper bound and rounded up to a power of two: 64 MiB for 1.2M IDs, about 53 B per ID, 0.29 slot fill.
  - Size it by the real distinct count, using multiply-high range reduction (as PtrHash's `reduce` does) instead of power-of-two rounding.
  - Put a filter of about 1 MB in front, for example a ribbon filter with r = 7 and false-positive rate 1/128, so unlinked rows skip the probe.
  - Keep THP; 558 ms became 472 ms on the dev box.
- Expected: about 40% fewer DRAM probes. R1 subsumes this.

**R7. Skew in the per-bucket stage.**
- Sort the largest buckets first (longest processing time first).
- Detect mega-persons at link time from component size and give them dedicated buckets or a parallel sort, so the person with about 400K events does not finish last.

**R8. Fuse the join and the group-by into one hash table per bucket (groupjoin).** The seeded intern in R1 and R3 is exactly this. Evidence in Q3.

**R9. Latency hiding (group prefetching, AMAC, coroutines): low value here.** The scatter already prefetches 64 rows ahead, and the profile shows stalls with full fill buffers, so the memory-level parallelism is already saturated. These techniques add parallelism, not bandwidth. The only remaining use is the deeper finds of the union-find, which are not prefetched beyond the first level.

**R10. Not recommended.**
- Label propagation, Shiloach-Vishkin, or Afforest/BFS sampling: chains hundreds of links long mean many rounds, and there is no giant component to skip.
- Global dense renumbering of event IDs during the parse: it needs the same 24.6M random probes as today.
- An MPHF over all event IDs: the set is unknown until after the parse.
- Pruning link-only IDs: wrong, because they can bridge other IDs and can be the person's smallest ID, which is its name.

### Gaps
- No measured query time for PtrHash or BuRR at n ≈ 1.2M (cache-resident) was found. PtrHash reports none below 2·10⁷ and says batching gives little benefit at small n. Measure it locally.
- The scorer's L3 size, huge-page mode and SMT layout are unknown, so the size of the DRAM penalty there is an estimate.
- It is unclear whether the "serial between parse and dedup" gap (38–40 ms in `x86.md`) includes waiting for the links worker. Check with `SS_TRACE` before ranking R4 against R1.

---

## Q1. Fastest sequential and parallel union-find for a forest of about 1.2M nodes and 0.89M edges; getting the smallest-ID name cheaply

### Takeaway
Sequential Rem's algorithm with splicing and an immediate parent check is the measured state of the art. It is what the repo already runs, and it already links the larger ID under the smaller, so the root is the name. What remains is memory footprint and numbering, not the linking rule. For a parallel version, ConnectIt's UF-Rem-CAS without sampling is the robust choice. Sampling methods (Afforest, k-out, BFS, LDD) gain nothing on a sparse forest with no giant component.

### Cited Findings
- Patwary, Blair and Manne (SEA 2010) found that Rem's simple 1976 algorithm is the fastest, despite a worse worst-case bound than the "best" algorithms. — [SEA 2010 record (DOI 10.1007/978-3-642-13193-6_35)](https://api.crossref.org/works/10.1007%2F978-3-642-13193-6_35)
- Alistarh, Fedorov and Koval (OPODIS 2019):
  - Rem's algorithm was best sequentially; concurrent variants "slightly outperform it".
  - Full compression was slowest, with about 7× more load cache misses; splitting and halving performed about the same.
  - The **immediate parent check "significantly improves the performance"**.
  - Plain writes instead of barriers were up to 40% faster; only 0.002% of CAS calls failed in the worst cases.
  - The number of cache misses induced is "the critical determining factor".
  - Hardware: 4 × Xeon Gold 6150 (144 threads), EPYC, ThunderX; graphs of 1.6M–14M vertices.
  — [ar5iv 1911.06347](https://ar5iv.labs.arxiv.org/html/1911.06347)
- ConnectIt (Dhulipala, Hong, Shun, PVLDB 2021):
  - UF-Rem-CAS is "consistently the fastest finish algorithm"; FindNaive with SplitAtomicOne is the robust choice.
  - The Rem splice "atomically swaps the parent of the higher index vertex in the path to the lower index vertex", so labels are minimum-based.
  - Machine: 72 cores, 4 × Xeon E7-8867 v4 at 2.4 GHz, 144 threads.
  - Times without sampling: road_usa 28.0 ms, LiveJournal 12.7 ms, com-Orkut 19.1 ms, Twitter 0.316 s, Friendster 0.902 s.
  - Times with k-out sampling: LiveJournal 8.96 ms, Twitter 0.117 s.
  - BFS sampling stops when one component holds more than 10% of vertices; the finish phase skips the most frequent label.
  - LDD sampling is poor on high-diameter graphs.
  - "For the sparse case where n = m, k-out sampling and no sampling were fastest."
  — [ar5iv 2008.03909](https://ar5iv.labs.arxiv.org/html/2008.03909)
- ConnectIt's abstract: hundreds of variants; an average speedup of 12.4× over earlier static multicore algorithms; 3.5B vertices and 128B edges in under 10 s on 72 cores. — [arXiv 2008.03909](https://arxiv.org/abs/2008.03909)
- Afforest (Sutton, Ben-Nun, Barak, IPDPS 2018) runs Shiloach-Vishkin on sampled subgraphs and reports up to 67× over the state of the art on multi-core CPUs (Broadwell, POWER8) and 23× on GPUs (Pascal). — [HUJI record](https://cris.huji.ac.il/en/publications/optimizing-parallel-graph-connectivity-computation-via-subgraph-s/)
- ECL-CC (Jaiganesh and Burtscher, HPDC 2018) initializes each vertex "with the ID of the first neighbor that has a smaller ID", uses a union-find designed for GPUs, and runs lock-free and asynchronously. An OpenMP C++ version exists, under its own license. — [ECL-CC page](https://userweb.cs.txstate.edu/~burtscher/research/ECL-CC/). The GPU code beats the fastest prior CPU and GPU codes on most of 18 inputs. — [HPDC'18 paper](https://userweb.cs.txstate.edu/~mb92/papers/hpdc18.pdf)
- Randomized linking (linking by index, with the elements in a uniformly random order) together with compression, splicing or halving runs in expected amortized O(α(n, m/n)) per find, as efficient as linking by rank. The worst case for linking by index is O(m log_{m/n+2} n) (Tarjan and van Leeuwen). — [Goel, Khanna, Larkin, Tarjan slides](https://users-cs.au.dk/gerth/madalgo/old-madalgo.au.dk/img/SS2013/Course%20material/H-DisjointSetUnionWithRandomizedLinkingMS_Tarjan.pdf), [Princeton record](https://collaborate.princeton.edu/en/publications/disjoint-set-union-with-randomized-linking/)
- Jayanti and Tarjan's concurrent union-find fixes a uniformly random total order and links the smaller root under the larger, using CAS. — [Distributed Computing 2021](https://link.springer.com/article/10.1007/s00446-020-00388-x)

### Inferences
- Our graph has m/n ≈ 0.74. It is a forest whose largest person has thousands of IDs, far below 10% of 1.2M. BFS sampling's trigger never fires, Afforest's skip saves nothing, and LDD suffers on the long chains. ConnectIt's own n = m finding says to use no sampling.
- The 94 ms is mostly memory: about 1.78M probes into the 64 MB table, unions over a 19 MB node array, and a scan of 64 MB to write names. Order-preserving ranks plus 4-byte parents (R4) keep the same algorithm with about 4× fewer bytes in the hot array.
- Naming comes free from minimum-index linking. With ranks in byte order, the root is the smallest ID, and it stays canonical under concurrency because ConnectIt's splice also links toward the lower index.
- The generator's IDs are random 40-bit values, so byte order is effectively random with respect to the graph structure. Linking by minimum ID then behaves like Goel et al.'s randomized linking, near-constant expected work per operation. An adversarial order only slows it down, and the result stays exact.
- Parallel cost, by rough scaling: ConnectIt runs LiveJournal in 12.7 ms on 144 threads. Our graph is far smaller but has 8 threads, so an estimated 5–10 ms wall. This is worth it only if links move onto the critical path (R2).
- ECL-CC's "first smaller neighbor" initialization is the same as one hooking pass over the edge list. Rem's algorithm already does this implicitly, so the gain here is minor.

### Gaps
- Patwary et al.'s per-graph timings and speedup factors were not captured; only the headline conclusion was.
- No published union-find benchmark at about 1M nodes on a 4-core/8-thread Xeon was found. The figures above are scaled estimates.
- The graph sizes for ConnectIt's road_usa run were not captured in the fetch.
- ECL-CC's CPU timings were not captured.
- I could not identify the suggested source "Esmaili/Pirouz".

---

## Q2. Avoiding work: resolving only IDs that appear, Bloom/semi-join filters, caching per distinct ID, dense renumbering, perfect hashing

### Takeaway
Two kinds of avoided work matter most:
- **Lookup exactness that is not needed:** only linked IDs must be exact, which allows retrieval structures (MPHF plus values, ribbon) at 1.5–3 MB instead of a 64 MB exact map.
- **Repeated lookups:** resolve once per distinct ID per bucket (about 2.0M instead of 24.6M).
Global dense renumbering or perfect hashing of *event* IDs during the parse does not pay, and link-only IDs must never be pruned.

### Cited Findings
- PtrHash queries mostly cost one memory access (the pilot byte), plus a rare remap access. Slot selection uses multiply-high reduction `hi(C·x)`. Space is 2.12–2.99 bits/key across the paper's three configurations (the abstract quotes 2.0 for its default and 2.4 at 10⁹+ keys). At 10⁹ keys: 12.5 ns in a plain loop, 8.8 ns streaming. When many cores saturate RAM, throughput reaches about 2.5 ns/key. The paper says a 2·10⁷-key structure (about 6 MB) "easily fit[s] in L3" but gives no small-n timings, and it does not discuss non-member queries. — [PtrHash HTML](https://arxiv.org/html/2502.15539v2), [blog](https://curiouscoding.nl/posts/ptrhash/)
- BuRR: r bits per key with less than 1% overhead, and a false-positive rate of 2^-r when used as a filter. — [arXiv 2109.01892](https://arxiv.org/abs/2109.01892)
- Skylake-X: L2 14 cycles, L3 68 cycles, RAM 79 cycles + 50 ns. The STLB has 1,536 entries: a 6 MB reach with 4 KB pages, 3 GB with 2 MB pages. — [7-cpu](https://www.7-cpu.com/cpu/Skylake_X.html)
- Repo:
  - The link table is 64 MiB for 0.89M pairs (2^20 lines of 64 B, sized for 1.78M keys).
  - About 44% of rows miss the table at random (comment in `LinkTable::person`).
  - The lookup stall is 9.8% of the dedup/scatter stage.
  — [crates/links/src/lib.rs](../../crates/links/src/lib.rs), [x86.md](../../x86.md)

### Inferences
- **Exact for members, meaningless but fixed for others.** This is the defining contract of a retrieval structure or static function, and it is all the scatter needs (R1). PtrHash's query steps (hash, pilot, slot, remap if the slot is n or more) return some index in [0, n) for any input, with no membership check. This is inferred from the algorithm as described; the paper does not state it.
- **Bloom or semi-join prefilter.** Worth it only if the exact global lookup stays (R6). A filter of about 1 MB lets 40–44% of probes skip DRAM. R1 makes the filter unnecessary.
- **Resolve only what appears.** The union-find must still process **all** links. A link-only ID can bridge two IDs that have events, and it can be the person's byte-wise smallest ID, which is its name; dropping it would change the output. What can be avoided is per-row resolution: resolve per distinct ID per bucket (R3), or resolve per row against a cache-resident oracle (R1).
- **Caching the person per distinct ID** is exactly the bucket-local intern table after partitioning (about 1.5K distinct IDs per bucket: (1.29M + 0.2M link-only IDs) / 1,024). It fits in L1/L2, so a "cache" in front of the global table is unnecessary once rows are partitioned.
- **Dense renumbering during the parse** (coordinate compression of all event IDs) needs a global table probed by every row, the same random-access cost as today's lookup. Do it per bucket after the scatter instead, where it already happens (`intern`). For **linked IDs**, dense, order-preserving renumbering by radix sort is cheap (1.78M keys) and makes the union-find smaller and the naming free (R4).
- **Perfect hashing (PtrHash, CHD, RecSplit).** Useful on the 1.2M linked IDs, which are known before the event scatter, as the core of R1. Useless on the 1.29M event IDs, which are known only after the parse and would need another pass. PtrHash is reported at least 2.1× faster to query than the alternatives on 300M strings, so CHD and RecSplit are only worth using if build time dominates.

### Gaps
- Construction time for PtrHash or BuRR at n ≈ 1.2M, cache-resident, single thread: no data found.
- Binary fuse / XOR static-function query and build numbers were not verified in this session.
- No measured comparison was found between a 1.5 MB ribbon (one access with more computation) and a 2.9 MB MPHF plus array (two accesses) on Xeon.

---

## Q3. Join and partitioning research for skewed, mostly-small groups; cache-conscious joins with prefetching; fusing the person lookup into the parse

### Takeaway
Partition until the hash tables fit in cache, then join and group in one table per partition (a partition-wise join fused into a groupjoin). For this workload that means a single scatter keyed by the person's home bucket (R2), or a raw-ID scatter followed by a small fix-up move (R3). Prefetching variants (group prefetching, AMAC, coroutines) add memory-level parallelism, which the current code already saturates. Skew is handled by scheduling, not by finer partitioning, because one person must stay together.

### Cited Findings
- Balkesen et al. (ICDE 2013): a tuned radix join reaches "close to 200 million tuples per second". "Cache consciousness becomes the decisive factor", favouring radix join over no-partitioning. Machine details were not captured. — [Balkesen ICDE 2013](https://www.comp.nus.edu.sg/~hebs/course/cs6284/papers/Balkesen-icde2013.pdf)
- Blanas, Li and Patel (SIGMOD 2011): with uniform data, radix join beats no-partitioning only when carefully tuned. The no-partitioning join "far outperforms" radix join **under skew**. — [Blanas et al.](https://pages.cs.wisc.edu/~jignesh/publ/hashjoin.pdf)
- Müller et al. (SIGMOD 2015), "hashing is sorting": hashing and sorting are equivalent for cache efficiency. Their adaptive aggregation switches between hashing and partitioning during execution, runs near memory bandwidth, and is up to 3.7× faster than the state of the art. — [core.ac.uk PDF](https://core.ac.uk/download/524940031.pdf), [TU Dresden record](https://fis.tu-dresden.de/portal/en/publications/cacheefficient-aggregation(47c83997-1d8d-4dc2-a88c-4b403ed8b38a).html)
- Fent and Neumann (PVLDB 2021): groupjoin with contention-free parallel execution speeds some TPC-H queries up to 2×. Shared aggregation state in a groupjoin table otherwise causes contention. — [PVLDB 14(11)](https://vldb.org/pvldb/vol14/p2383-fent.pdf)
- AMAC (Kocberber, Falsafi, Grot, PVLDB 2015) keeps state per lookup, so a new lookup starts whenever any in-flight one finishes. It matches group and software-pipelined prefetching on regular patterns and is up to 2.3× faster on irregular lookups. — [EPFL record](https://infoscience.epfl.ch/record/220654)
- Chen et al. (ICDE 2004) report that GRACE hash join spends over 73% of user time stalled on cache misses, which motivated group and software-pipelined prefetching. — [ICDE 2004 PDF](https://www.pdl.cmu.edu/ftp/Database/icde04.pdf)
- Psaropoulos et al. (PVLDB 2017): coroutine interleaving for index joins performs similarly to earlier interleaving techniques while staying close to the original code (SAP HANA; binary search and CSB+-tree). The speedup figures were not captured. — [EPFL record](https://infoscience.epfl.ch/record/231318)
- Repo: the parse's 1,024-way scatter costs about a third of the parse with 8 workers. In the dedup stage, 24.8% of samples sit on the person-bucket store and 9.8% are stalls after the link-table prefetch (fill buffers full). — [x86.md](../../x86.md)

### Inferences
- **Partition-wise join.** If links are partitioned by the same hash as rows, each bucket's join runs in L1/L2. Balkesen's result says this beats one large shared table when that table outgrows the cache, which on the scorer the 64 MB table does. Blanas's skew warning applies to partitioning *within* a heavy key. Here a heavy person must stay together anyway, so skew becomes a scheduling problem (R7), not a partitioning one.
- **Groupjoin.** Seeding the bucket-local intern with the bucket's linked IDs makes one hash table perform both the join (ID to person) and the grouping (person to local number). That is the groupjoin pattern, and it is contention-free because each bucket belongs to one worker.
- **Eager aggregation, or the semijoin idea.** Resolve once per group (distinct ID) instead of per row, which is R3's 12× cut in lookups. Sessionization itself is not decomposable, so rows cannot be pre-aggregated, only pre-grouped.
- **"Small group" versus "large group".** 42% of rows belong to persons with only one event-bearing ID, and those need no move in R3. The persons with 2+ IDs (354K, 58% of rows) are where the 18.8% move happens. The mega-person (about 400K events, roughly 1.6–2% of rows) dominates one bucket: schedule it first, or give it a parallel sort.
- **Prefetch ceiling.** With fill buffers already full during a prefetch distance of 64, group prefetching, AMAC and coroutines cannot help the scatter. Only fewer or cache-resident misses can (R1 to R3). AMAC fits the union-find's variable-length find walks better, but most nodes are a root or a root's child (88% per the repo comment), so the payoff is small.
- **Fusing into the parse without serializing.** There are three options:
  - Links-first on all threads (R2): a short all-threads phase, then a single scatter.
  - A side-worker union-find with raw-ID partitions plus a fix-up move (R3): no serialization, 18.8% of rows move.
  - "Late binding": switch partition keys mid-parse once the oracle is ready, so fewer rows need the fix-up.
  Which is best depends on whether the links-first phase can stay under about 20 ms wall. That needs R4 and R5.

### Gaps
- No primary source was found that gives per-core lookups per second for production hash-join probes on a 4-core/8-thread Xeon.
- The exact machine behind Balkesen's 200M tuples/s figure was not captured.
- The coroutine paper's speedups were not captured.

---

## Q4. Competitive-programming DSU techniques

### Takeaway
Union by size or rank is unnecessary here. Linking by minimum ID, which the name rule requires, is a form of linking by index, and on random-looking IDs it matches rank in theory and is only slightly slower in practice. Use halving, splitting or Rem's splicing rather than full compression. Persistent or rollback DSU and small-to-large merging do not apply: the input is static and there are no per-set lists to merge.

### Cited Findings
- Union by size or rank with path compression runs in amortized O(α(n)); either one alone gives O(log n). Linking by **random index** "has the same time complexity as union by size" and is "slightly slower" in practice. Coin-flip linking is worse, a "common misconception". Extra information can be stored at the representative and updated on union. — [cp-algorithms DSU](https://cp-algorithms.com/data_structures/disjoint_set_union.html)
- Full compression causes about 7× more load misses than halving or splitting, which perform about the same. Pseudo-random priorities "slightly outperform" true random ones. — [Alistarh et al.](https://ar5iv.labs.arxiv.org/html/1911.06347)
- Randomized linking by index with compression, splicing or halving runs in expected O(α(n, m/n)), matching rank. — [Goel et al. slides](https://users-cs.au.dk/gerth/madalgo/old-madalgo.au.dk/img/SS2013/Course%20material/H-DisjointSetUnionWithRandomizedLinkingMS_Tarjan.pdf)

### Inferences
- **Getting the minimum ID.** There are two standard ways:
  - Store the minimum at the root (cp-algorithms' "extra information at the representative"). This costs an extra field and an extra read.
  - Link by an index that respects byte order. The repo does this already, comparing a `parent_id` copy held in each node.
  Order-preserving ranks make the second way cost 4 bytes per node (R4).
- **Offline processing.** All links are known before any event needs them, so the classic offline trick applies: sort-based numbering and dedup of the edge list. That also absorbs the 8% repeated pairs and the 1% self-links before the union-find runs; both are nearly free under the immediate parent check anyway.
- **Chains hundreds long.** Rem's splicing shortens paths during the unions themselves. A recursive find is safe at this depth but an iterative one is cleaner, as the repo has.
- **Small-to-large merging** solves a different problem (merging explicit member lists). Here grouping happens later, by bucket partitioning plus the per-bucket intern, so it is not needed.

### Gaps
- No Codeforces-style benchmark of DSU variants at about 1M elements with order-dependent linking was found.

---

## Q5. How production engines and graph frameworks handle connected components and the join, and their throughput

### Takeaway
SQL engines express connected components as iterated label propagation (recursive CTEs), whose round count equals the graph diameter, which is wrong for chains hundreds of links long. Graph frameworks (GBBS, ConnectIt) use concurrent Rem-style union-find and process million-edge graphs in milliseconds. Hash-join engines reach roughly 10⁸ tuples/s on many-core machines when partitioned to fit the cache. The repo's approach (union-find, then a cache-conscious grouping) matches the best practice, and the remaining gains are in data movement.

### Cited Findings
- DuckDB 1.3+ supports `USING KEY` recursive CTEs. Key columns identify rows and the remaining columns are payload that is updated each iteration. A DuckDB talk lists connected components as a natural use, with the node ID as key and the current component as state. — [DuckDB blog](https://duckdb.org/2025/05/23/using-key.html), [Grust slides](https://blobs.duckdb.org/events/duckdb-in-science-london-meetup/torsten-grust-ducks-love-running-in-circles.pdf)
- A worked DuckDB example finds graph components by duplicating edges to make them undirected and taking `MIN(front)` over reachable nodes, a search-style approach. — [Max Halford](https://maxhalford.github.io/blog/graph-components-duckdb)
- ConnectIt/GBBS: LiveJournal in 12.7 ms, road_usa in 28.0 ms, Twitter in 0.316 s with UF-Rem-CAS on 144 threads; 3.5B vertices and 128B edges in under 10 s on 72 cores. Incremental ingest runs at "several billion edges per second". — [ar5iv 2008.03909](https://ar5iv.labs.arxiv.org/html/2008.03909), [arXiv abstract](https://arxiv.org/abs/2008.03909)
- Radix hash join: close to 200M tuples/s (Balkesen). Under skew, no-partitioning wins (Blanas). — [Balkesen](https://www.comp.nus.edu.sg/~hebs/course/cs6284/papers/Balkesen-icde2013.pdf), [Blanas](https://pages.cs.wisc.edu/~jignesh/publ/hashjoin.pdf)

### Inferences
- No engine offers a built-in connected-components operator that beats a dedicated union-find. A recursive-CTE solution would need hundreds of iterations on the long chains, each touching all of the state.
- At about 10⁸ probes/s for a whole many-core machine, 24.6M probes come to tens to hundreds of ms of machine time. That is consistent with the repo's measured stall share, and it is why R1 and R2 aim to remove probes rather than speed them up.

### Gaps
- No published connected-components or join internals with throughput numbers were found for Umbra, Velox or Polars.
- No per-core probes-per-second figure was found for DuckDB's hash join.
