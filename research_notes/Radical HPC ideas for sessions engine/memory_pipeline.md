# Memory-latency hiding, whole-pipeline fusion and memory/OS-level techniques for the sessions engine

Working numbers used below come from the assignment (not web sources): 21.6M event lines, 5.1M clicks, 0.9M links, 1.9 GB CSV; one-core CPU 2.83 s (parse 1.44, dedup+scatter 0.61, sessions 0.70, output 0.08); kernel page zeroing about 8% of CPU (about 0.23 core-seconds) and page faults about 12% (about 0.34 core-seconds); 1.75 GB of fresh memory touched per run; 35 ms process exit spent tearing down the input mapping's 4 KB PTEs; +18% run time with THP off on the test box (WSL2, kernel 5.15). Target: 0.31-0.36 s on 4 physical cores x 2 SMT threads, so about 1.2-1.4 core-seconds of total budget. Tags: "(abstract/search excerpt)" means only the abstract or a search snippet was read. "(full text)" means the PDF text was extracted and searched.

## Objective: ranked list of memory-system and execution-architecture techniques

### Takeaway
The largest remaining memory-level wins come from doing less work, not from smarter latency hiding. In priority order: (1) stop touching fresh memory, because kernel zeroing plus faults cost about 20% of CPU (about 0.57 core-seconds); (2) replace about 24.6M DRAM-latency lookups with about 2M lookups (one per distinct ID) and shrink records, so intermediate DRAM traffic falls from roughly 240 B/event to roughly 50-100 B/event; (3) make huge pages certain on the scorer (its THP mode is unknown, and we measured +18% without them), and remove the input mapping's per-4 KB-page costs (35 ms of serial exit time). Fancier interleaving (AMAC, coroutines, helper threads) gives 2-8x only on loops that stay DRAM-bound. It loses to plain code on cache-resident tables, and SMT already recovers part of the gain.

### Cited Findings
- Anonymous mappings are zero-filled by contract ("its contents are initialized to zero"). MAP_UNINITIALIZED works only with CONFIG_MMAP_ALLOW_UNINITIALIZED, which is "normally enabled only on embedded devices", so a container cannot skip kernel zeroing. — [mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html)
- Kernel huge-page clearing throughput on an Ice Lake Xeon is about 7.95 GB/s per core for 2 MB pages on the current path. Ankur Arora's multi-page clearing patch raises it to about 10.90 GB/s (2025 series, v3). On AMD Milan: 11.9 to 16.1 GB/s for 2 MB pages, and 16.5 to 42.8 GB/s for 1 GB pages. — [Arora v3 cover letter](https://patchew.org/linux/20250414034607.762653-1-ankur.a.arora@oracle.com) (search excerpt); the v7 cover reports about 39% gain for 2 MB pages — [v7](https://lkml.iu.edu/2509.2/03062.html); the series was still at v11 in Jan 2026, merge status unconfirmed — [v11](https://lkml.iu.edu/2601.0/06669.html)
- Streaming hash lookups bottom out near the single-thread random-access limit. PtrHash measures 8 ns/key streaming with prefetch vs 12 ns/key in a plain loop on 10^9 keys, against a hardware limit of 7.4 ns per cache line on one thread; 4 threads saturate DDR4 bandwidth. — [PtrHash, SEA 2025](https://arxiv.org/html/2502.15539v2) (search excerpt); [ptr_hash 2.1.1 docs](https://docs.rs/crate/ptr_hash/2.1.1): 12.0 ns loop / 8.3 ns streaming / 9.4 ns batched
- Memory-level parallelism (MLP, the number of misses in flight) is capped by about 10 L1-D miss registers per core (Xeon X5670). Across cores, the cap is the LLC queue for off-chip loads (32 entries on Nehalem) — [AMAC, PVLDB 2015](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf) (full text). The Xeon E5-2630 v4 "supports a maximum of 10 outstanding L1 cache references (per core)", yet the best prefetch group size was 16 — [ROF, PVLDB 2017](https://www.vldb.org/pvldb/vol11/p1-menon.pdf) (full text)
- Prefetching an LLC-resident structure hurts. With a 2 MB build table, the no-prefetch baseline was 32% faster than group prefetching (GP) and software-pipelined prefetching (SPP); AMAC beat the baseline by 21%. — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- SMT eats into interleaving gains. CoroBase was 1.8x faster than a prefetching baseline with hyperthreading off, but only 1.3x with it on. — [CoroBase, PVLDB 2020](https://www.vldb.org/pvldb/vol14/p431-he.pdf) (full text)
- MADV_COLLAPSE (Linux 6.1+) "ignores these settings and collapses ranges to PMD-sized huge pages unconditionally", so THP mode "never" does not fully disable THP. — [kernel transhuge doc](https://docs.kernel.org/admin-guide/mm/transhuge.html); [madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html)
- Docker's default seccomp profile blocks io_uring_setup, io_uring_enter and io_uring_register (moby#46762). — [Docker seccomp docs](https://docs.docker.com/engine/security/seccomp/); [moby issue 47532](https://github.com/moby/moby/issues/47532)
- Batched unmapping of PTE-mapped large folios cut munmap time by about 58% for 2 MiB folios, 31% for 32 KiB and 20% for 16 KiB. Order-0 (4 KiB) pages improved only about 1%. — [Hildenbrand, "optimize unmap/zap with PTE-mapped THP" v3](https://lkml.iu.edu/hypermail/linux/kernel/2402.1/08491.html) (search excerpt)

### Inferences
Ranked by expected cut in total CPU work times confidence. All effects are estimates derived from the cited numbers plus the assignment's measurements.

**1. Shrink fresh-touched memory: recycle one chunk arena across stages, use smaller records, size tables exactly**
- Mechanism: one 2 MB-aligned arena per worker, carved into fixed chunks (for example 64 KB to 2 MB) that feed chunked partition lists. When a consumer finishes reading a chunk, it puts the chunk on its own thread-local free list. Producers in the next stage take from the free list before bumping into untouched arena space. Fresh memory then equals the peak live set rather than the sum over all stages.
- Why it applies: zeroing (8%) plus faults (12%) is about 0.57 core-seconds. 1.75 GB at the cited 8-11 GB/s per core works out to about 0.16-0.22 core-seconds of pure zeroing, which matches the measured 8%. Stage-1 output (690 MB + 123 MB) is dead once dedup has consumed it, so the 312 MB person-bucket scatter and later buffers can live in recycled chunks.
- Expected effect: with the planned 12-byte main records plus recycling, fresh memory could fall from 1.75 GB to about 0.4-0.7 GB, saving roughly 0.2-0.35 core-seconds (about 25-45 ms of wall time on 4C/8T). A second effect: non-temporal (NT) stores into recycled lines cost one DRAM write, while NT stores into freshly zeroed pages cost two (the kernel's zeroing write plus ours).
- Risks: chunk reuse must never hand out a chunk that is still being read (the stage barrier makes this safe). Fragmentation if chunk sizes vary. Reuse moves data across cores, which is harmless on a single socket.
- Evidence: zeroing is mandatory [mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html); zeroing throughput [Arora v3](https://patchew.org/linux/20250414034607.762653-1-ankur.a.arora@oracle.com); MADV_DONTNEED on private anonymous memory makes later accesses "zero-fill-on-demand", so never return memory to the kernel between stages [madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html).
- Test: `perf stat -e page-faults,task-clock`, `perf record -g` and look at the share of clear_page_erms / clear_huge_page / handle_mm_fault, plus VmHWM from /proc/self/status before and after.

**2. One shuffle of compact records, with lookups once per distinct ID (the planned redesign), plus dictionary encoding and late materialization**
- Mechanism: scatter 12-byte records once by raw user ID. Intern the raw ID per partition into a u32 (in a cache-resident table). Look up the person once per distinct ID (about 2M lookups instead of about 24.6M). Re-move only the 18.8% of rows whose person lives in another partition. Carry only the fields sessionization and aggregation need (u32 timestamp offset, local ID, a few attribute bits) and keep a row reference for anything needed only for the 2.2M output rows.
- Why it applies: 24.6M lookups into a 64 MB table cost about 1.57 GB of line fills (upper bound) and, at about 8 ns per miss per core, about 0.2 core-seconds even with perfect prefetching.
- Expected effect: about 0.15-0.2 core-seconds saved on lookups alone. The intermediate traffic table in the roofline section below estimates about 240 B/event today vs about 50-100 B/event planned.
- Risks: exactness. A dedup winner whose user ID differs from a losing copy's must still be handled, as in the planned side records. Timestamp offsets must have a proven range; fall back to wider records if the range check fails.
- Evidence: the 8 ns/key floor [PtrHash](https://arxiv.org/html/2502.15539v2); MLP limits [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf); radix partitioning cost grows with tuple width (doubling entry size slowed partitioning 1.31-1.69x) [Schuhknecht et al., PVLDB 2015](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf) (full text).
- Test: count LLC misses per stage (perf mem_load_retired.l3_miss, or the AMD equivalent on the test box) and bytes written per stage.

**3. Huge-page insurance for anonymous arenas: probe THP mode, 2 MB-align, call MADV_HUGEPAGE, fall back to MADV_COLLAPSE**
- Mechanism: read /sys/kernel/mm/transparent_hugepage/enabled and defrag (/sys is normally readable in a container). Align every big arena to 2 MB, because "memory may only be replaced by huge pages on hugepage-aligned boundaries". If the mode is [madvise], call madvise(MADV_HUGEPAGE). If it is [never] and the kernel is 6.1 or newer, touch one byte per 2 MB and call MADV_COLLAPSE on the arena slice from the owning worker thread.
- Why it applies: the scorer's THP mode is unknown and we measured +18% with THP off. Huge pages also make the exit teardown of 1.75 GB of anonymous memory cheap (about 875 PMDs instead of about 437K PTEs).
- Expected effect: 0 if the scorer runs THP "always" and huge pages are available. Up to about 15% of the score if it runs "madvise", "never", or "always" on fragmented memory that silently falls back to 4 KB pages.
- Risks: with defrag "madvise" (the default), MADV_HUGEPAGE regions get direct reclaim and compaction on allocation failure, which can stall faults. MADV_COLLAPSE is synchronous, copies into a freshly allocated huge page, "may enter direct reclaim and/or compaction", needs "at least one page" resident per region, and returns EINVAL on kernels older than 6.1. Never call MADV_POPULATE_WRITE before MADV_COLLAPSE: that would zero 4 KB pages and then copy them.
- Evidence: [transhuge doc](https://docs.kernel.org/admin-guide/mm/transhuge.html); [madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html). By default, multi-size THP is enabled only at PMD size (all other sizes "never"), so 64 KB mTHP cannot be counted on [transhuge doc](https://docs.kernel.org/admin-guide/mm/transhuge.html).
- Test: on the test box, switch the THP sysfs knob between never, madvise and always, and compare AnonHugePages in /proc/self/smaps_rollup and run time. In one scorer run, print the THP mode, `uname -r` and AnonHugePages to stderr.

**4. Input path: measure pread into small L2-resident per-thread buffers against mmap**
- Mechanism: each worker preads 256 KB-1 MB chunks of its file range into a reused buffer that stays in L2, then parses it. No file PTEs exist, so there are no file page faults and no exit teardown.
- Why it applies: exit spends 35 ms of serial wall time on about 485K 4 KB PTEs. Unmapping in parallel earlier failed because of TLB shootdowns.
- Expected effect: about 35 ms less serial wall time plus the file-fault CPU, against about 1.9 GB of kernel copy (about 0.16 core-seconds at about 12 GB/s per core). The net is uncertain, so this must be measured.
- Risks: io_uring is blocked by Docker's default seccomp, so use plain pread from each worker. Records that straddle chunk boundaries need carry-over logic. Bypassing the page cache with O_DIRECT would throw away the warm cache, so do not use it.
- Evidence: [Docker seccomp](https://docs.docker.com/engine/security/seccomp/). Large page-cache folios help large reads: ext4 large folios gave +77% (64K reads) and +82% (1M reads) on cached buffered reads, but only +1-2% for 4K reads (Xeon Gold 6240) [ext4 large-folio series](https://patchew.org/linux/20250512063319.3539411-1-yi.zhang@huaweicloud.com) (search excerpt). Use large pread sizes.
- Test: A/B on the test box with `perf stat` (page-faults, task-clock). Time the exit by taking a timestamp just before `_exit` and comparing it with the docker-run wall time.

**5. Fill barrier tails and serial phases with independent work (morsel-style task graph)**
- Mechanism: put link parsing, union-find and the person table, click parsing, bitmap building, and MADV_POPULATE_WRITE of the next stage's arena into the same task queue as event parsing. Split files into morsels of about 1-4 MB, and schedule per-partition tasks largest-first.
- Why it applies: the scatter-to-partition barrier is inherent, because every partition receives rows from every file. Idle threads at the barrier and in the serial start-up are free capacity.
- Expected effect: a few ms to about 20 ms per barrier. Total CPU is unchanged; wall time drops.
- Risks: running populate concurrently with the scatter competes for DRAM bandwidth.
- Evidence: morsel-driven scheduling averaged over 30x speedup on 32 cores (TPC-H/SSB), with morsels of "e.g., 100,000" tuples [Leis et al., SIGMOD 2014](https://db.in.tum.de/~leis/papers/morsels.pdf) (abstract/search excerpt). MADV_POPULATE_WRITE helps "already when only using a single thread", with negligible benefit when huge pages are in use [Hildenbrand RFC](https://lkml.iu.edu/hypermail/linux/kernel/2103.1/00856.html).
- Test: log per-thread idle time at each barrier.

**6. Software prefetching for what remains: group prefetching or relaxed-fusion staging, ~10-16 accesses in flight per core**
- Mechanism: batch-and-prefetch loops (Chen et al.'s group prefetching style) for the remaining DRAM-resident single-hop lookups (person table, link table). AMAC-style state machines only for variable-length chains (union-find walks, long probe sequences). No prefetching for L2-resident per-partition tables.
- Why it applies: the lookups are uniform and single-hop, which is the best case for GP.
- Expected effect: 2-4x on loops that stay DRAM-bound. Small overall once item 2 lands.
- Evidence: see key question 1.
- Test: sweep group size from 4 to 32 per thread, with 4 vs 8 threads.

**7. Keep per-row instruction count small inside fused DRAM-bound loops; fuse only compute-bound work**
- Mechanism: split "hash + prefetch" from "probe + consume" in vectorized style when a loop is DRAM-bound. Fuse parse with scatter (compute-bound) as today.
- Evidence: in Kersten et al. 2018, the vectorized engine beat the compiled one on hash-probe-heavy Q9 by 32%. The compiled engine beat it on compute-heavy Q1 by 74%. A cheaper CRC hash made the compiled engine up to 40% faster by "enabling more concurrent outstanding loads". — [Kersten et al., PVLDB 2018](https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf) (full text)

**8. Keep non-temporal stores for write-once scatter output; use regular stores for data re-read soon by the same core**
- Evidence: see key question 4. Ice Lake's automatic write-allocate evasion triggers only near bandwidth saturation (about 29-30 cores), so on a 4-core slice regular stores to fresh lines still pay the read-for-ownership (RFO) [Hager](https://blogs.fau.de/hager/?p=8997).

**9. SMT-aware stage configuration**
- Mechanism: size per-thread working sets to about half of L2 when both siblings run the same stage, and test 4 vs 8 threads for DRAM-bound stages. Read the topology from /sys/devices/system/cpu/cpu*/topology/thread_siblings_list and co-schedule a memory-bound task with a compute-bound one on sibling pairs.
- Evidence: see key question 5.

**10. Huge pages for the input page cache (gray area, host-dependent)**
- Mechanism: map the input with PROT_READ|PROT_EXEC plus MADV_HUGEPAGE or MADV_COLLAPSE. For file-backed non-tmpfs memory, MADV_HUGEPAGE requires "the file not be open for write and the mapping be executable" [madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html), which suggests the kernel needs CONFIG_READ_ONLY_THP_FOR_FS.
- Why it might pay: the collapsed huge pages stay in the host page cache, so the scorer's runs 2 and 3 (the median of 3 is scored) could map the input with about 950 PMDs instead of about 485K PTEs.
- Risks: the first run pays to copy 1.9 GB. The trick depends on host kernel config and mount options (a noexec mount blocks PROT_EXEC). It deliberately exploits state that survives across runs, which some may consider gaming. Treat it as an experiment only.

**11. Helper-thread prefetching on the SMT sibling (Ghost Threading)**
- Evidence: 1.33x geometric-mean speedup over baseline and 1.11x over plain parallelization on a modern Intel CPU [Guo et al., MICRO 2025](https://www.cl.cam.ac.uk/~tmj32/papers/docs/guo25-micro.pdf) (abstract/search excerpt).
- Ranked low: it is complex, and little DRAM-bound work remains after item 2.

**12. Process-separated parse workers or RUMA-style page rewiring**
- Process-separated workers: child processes would map and parse the files, so their teardown would run off the critical path, and munmap in a child would not send shootdown IPIs to the parent's CPUs. But shared outputs would live in shmem, where THP defaults to "never" for tmpfs [transhuge doc](https://docs.kernel.org/admin-guide/mm/transhuge.html), so expect 4 KB faults on about 0.6 GB of output. Likely a net loss.
- RUMA (rewired user-space memory) remaps physical pages with no OS changes [Schuhknecht et al., RUMA](https://bigdata.uni-saarland.de/publications/p768-schuhknecht.pdf) (search excerpt). It is unnecessary because chunked lists do not need contiguity, and mmap calls during parallel phases cost mmap_lock acquisitions and shootdowns.

**Not available in an unprivileged container:**
- MAP_HUGETLB: needs a hugetlb pool, and without CAP_IPC_LOCK fails with EPERM unless the caller is in hugetlb_shm_group [mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html).
- MAP_UNINITIALIZED: see above.
- io_uring: blocked by Docker seccomp.
- Sysctl and THP sysfs changes (per the assignment).
- Cache Allocation Technology via resctrl: my inference, not verified here, is that it needs a privileged mount.

### Gaps
- Unknown scorer facts that change the ranking: kernel version (MADV_POPULATE needs 5.14, MADV_COLLAPSE 6.1, per-VMA locks 6.4), THP enabled/defrag modes, the filesystem under the input (overlayfs over ext4 or xfs, and whether it uses large folios), exact CPU model, and whether vCPU pairs are true SMT siblings. Recommended: a calibration submission that prints `uname -r`, the THP files, AnonHugePages and FilePmdMapped from /proc/self/smaps_rollup, and getrusage minflt to stderr.
- No source found for the per-fault cost of file mmap faults (fault-around batch size) on current kernels. Measure locally instead.

## Key question 1: latency hiding for random lookups and scatters beyond prefetch-N-ahead (GP/SPP, AMAC, coroutines, helper threads; Rust fit)

### Takeaway
Group prefetching and software-pipelined prefetching already give 2.8-3.8x on uniform DRAM-bound probes. AMAC adds robustness for irregular chains (up to 2.3x over GP/SPP under skew). Coroutines match AMAC with code that stays close to the original, but in C++ they execute 4-5x the baseline instruction count. All of these lose to plain code when the data fits in cache. The engine's link lookups are uniform and single-hop, so its prefetch-64-ahead (SPP-like) is already the right class. The remaining lever is fewer lookups, then GP batches of about 10-16 in flight per core. In Rust, hand-written GP loops or AMAC state machines are the practical route; I found no measurements of async-based interleaving in Rust.

### Cited Findings
**AMAC (Kocberber, Falsafi, Grot, PVLDB 2015), full text**
- Corroborates Chen et al.: "Group Prefetching and Software-Pipelined Prefetching attain considerable single-thread speedups of 2.8x-3.8x over a baseline of a highly optimized no-prefetch hash join". — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- 2 GB join 2 GB, uniform keys: GP 2.8x, SPP 3.8x, AMAC 4.3x. With skewed R, GP and SPP fell to 1.4-2x and 1.2-2.2x. AMAC reached "up to 2.8x over the no-prefetch baseline and by up to 2.3x over the existing techniques". — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- Mechanism: AMAC keeps "the full state of each in-flight memory access separate" in a circular buffer, so one lookup finishing early does not waste stages. GP/SPP lose MLP on variable-length chains and early exits. — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- Instructions per tuple, LLC-resident table (2 MB join 2 GB): baseline 36, GP 90, SPP 67, AMAC 55. Cycles per tuple: 27, 37, 28, 22. "The baseline is faster than both GP and SPP by 32% on average". — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- More in-flight requests stop helping once "the limit of L1-D outstanding misses (i.e., 10 L1-D MSHRs) is reached". GP's best group size was 15 because GP is limited by instruction overhead. — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- Hardware: Xeon X5670 (Westmere, 6 cores/12 threads per socket, 12 MB L3) and SPARC T4. — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)

**Interleaving with coroutines (Psaropoulos et al., PVLDB 2017), full text**
- Binary search over arrays larger than the 25 MB LLC on a Xeon E5-2660 v3 (Haswell), in SAP HANA's dictionary lookups. GP performed best; CORO and AMAC "follow with decent speedups, in the ranges 2.0-2.4x and 1.8-2.3x for integers", and 1.4-2.1x and 1.2-1.9x for strings. — [Psaropoulos et al.](https://www.vldb.org/pvldb/vol11/p230-psaropoulos.pdf)
- Instruction overhead vs baseline: GP 1.8x, AMAC 4.4x, CORO 5.4x. Best group sizes: 10 for GP, 6 for AMAC and CORO. Main-memory latency 182 cycles. CPI rose from 0.9 to 6.3 when the dictionary grew from 1 MB to 2 GB. — [Psaropoulos et al.](https://www.vldb.org/pvldb/vol11/p230-psaropoulos.pdf)
- Group-size model: G >= T_target / (T_compute + T_switch) + 1. Interleaving pays only when T_stall >> T_switch. — [Psaropoulos et al.](https://www.vldb.org/pvldb/vol11/p230-psaropoulos.pdf)

**"Killer nanoseconds" (Jonathan et al., PVLDB 2018), full text**
- Hardware: 2x Xeon E5-2690 v4 (28 cores, 35 MB L3 per socket). Maximum improvement over naive for hash probes: GP 7.5x/7.8x, AMAC 8.4x/8.5x, Coro 8.2x/7.3x (Clang/MSVC). Binary search: 2.1-3.5x. Masstree with Coro: 3.2x. — [Jonathan et al.](https://www.vldb.org/pvldb/vol11/p1702-jonathan.pdf)
- With 0.5 MB of data (L3-resident), naive was up to 2.7x, 2.9x and 5.1x faster than GP, AMAC and Coro. In that case the best group size was about 3 for AMAC/Coro and about 12 for GP. — [Jonathan et al.](https://www.vldb.org/pvldb/vol11/p1702-jonathan.pdf)
- "Maximum interleaving performance is reached when there are > 10 outstanding prefetches, because multiple prefetches from L3 will complete while waiting for one prefetch from DRAM." — [Jonathan et al.](https://www.vldb.org/pvldb/vol11/p1702-jonathan.pdf)
- "Hyperthreading gives some of the benefits of coroutines, and vice versa, but combining both yields the best overall performance." Also: "the improvement can be overstated ... by disabling hyper-threading." — [Jonathan et al.](https://www.vldb.org/pvldb/vol11/p1702-jonathan.pdf)

**CoroBase (He, Lu, Wang, PVLDB 2020), full text**
- 48 cores / 96 hyperthreads, 6 memory channels per socket. Up to 2x faster than optimized sequential execution that already prefetches, and 2.5x faster than naive on read-only workloads. "With hyperthreading, the improvement becomes smaller across all variants". CoroBase was 1.3x (HT on) and 1.8x (HT off) faster than ERMIA. Optimal batch size: 8 (4-8, at most 10). — [CoroBase](https://www.vldb.org/pvldb/vol14/p431-he.pdf)

**Relaxed operator fusion (Menon et al., PVLDB 2017), full text**
- Prefetch group size: queries got faster up to 16 tuples, although the CPU (Xeon E5-2630 v4) supports 10 outstanding L1 references. — [ROF](https://www.vldb.org/pvldb/vol11/p1-menon.pdf)

**Helper threads on the SMT sibling**
- Zhou, Cieslewicz, Ross, Shah (VLDB 2005): a helper thread runs ahead using a "work-ahead set". 30-70% throughput gain over single-threaded code on Pentium 4 hyper-threading. — [Zhou et al.](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf) (search excerpt)
- Ghost Threading (Guo et al., MICRO 2025): software-only helper threads on idle SMT contexts, throttled through an instruction available on commercial processors. 1.33x geometric mean over baseline, 1.25x over state-of-the-art software prefetching, 1.11x over parallelization, on a modern Intel CPU; the benefits hold on a busy server. — [Ghost Threading](https://www.cl.cam.ac.uk/~tmj32/papers/docs/guo25-micro.pdf) (abstract/search excerpt)

**Other evidence**
- Cimple: coroutines yield at annotated long-latency operations, with static batches or dynamic refill. — [Cimple](https://ar5iv.arxiv.org/html/1807.01624) (search excerpt)
- CoroGraph reportedly degraded with more than two coroutines. — cited in [CoroAMU 2025](https://arxiv.org/html/2511.14990v1) (search excerpt; secondary)
- Rust: a Stack Overflow question asking whether C++20-style prefetch-and-yield works with async/await has no accepted answer. A commenter notes C++ coroutines are heap-allocated unless optimized, while Rust generators are stack-allocated. — [SO question](https://kiwix.lema.org/stackoverflow.com_en_all_2023-11/questions/72297210/prefetching-and-yielding-to-hide-cache-misses-in-rust) (search excerpt)
- PtrHash, a Rust crate, gets 8 ns/key streaming vs 12 ns/key in a loop using explicit prefetch over a stream of keys. That is evidence that the GP/SPP style works well in Rust. — [PtrHash](https://arxiv.org/html/2502.15539v2)

### Inferences
Mapping to the engine:
- Link-table lookups (64 MB, uniform, one cache line each) are the textbook GP/SPP case. The current prefetch-64-ahead is SPP-like and should already sit near the MLP ceiling of about 10-16 misses in flight per core. Going to AMAC or coroutines would mostly add instructions; the cited studies show 4.4-5.4x the baseline instruction count, against 1.8x for GP.
- The bigger win is cutting the count from about 24.6M to about 2M (ranked item 2). After that, latency hiding matters little.
- Per-partition dedup tables and per-bucket person tables sized to L2 should not be prefetched; AMAC's LLC-resident result shows prefetching costs about 32% there.
- Scatter writes to software write-combining (WC) buffers are L1/L2-resident, and the NT flushes never read memory, so there is no read latency to hide. Latency hiding does not apply to the scatter.

Rust fit:
- GP: a fixed batch loop of 8-16 iterations. First loop: compute hashes and call `core::arch::x86_64::_mm_prefetch` (stable). Second loop: probe. This is the simplest and, per the evidence, the fastest choice for uniform single-hop lookups.
- AMAC: an enum state machine in a fixed ring of N slots. Good for union-find `find` with path halving, or open-addressing probes when tables are DRAM-resident.
- Async-based interleaving: possible with a no-op waker and a custom round-robin poller. Futures are compiler-generated state machines that can sit in an array without heap allocation (per the SO comment). But Pin and poll overhead is real, and there is no measured evidence (gap). The C++ results suggest coroutines pay the highest instruction overhead.

Group size and SMT:
- Fill buffers are per core, so with both hyperthreads issuing, each should keep about 5-8 accesses in flight rather than 10-16.
- SMT itself provides latency hiding (Jonathan, CoroBase), so measure 4 threads with deep prefetch against 8 threads with shallow prefetch.
- Helper threads (Ghost Threading) are worth trying only if a DRAM-bound stage survives the redesign. Their 1.11x over plain parallelization is the relevant comparison, because every SMT context on the scorer is ours.

### Gaps
- The original Chen et al. ICDE 2004 / TODS 2007 numbers were not retrieved; only AMAC's corroboration (2.8-3.8x) is cited.
- No Cascade Lake, Ice Lake or Sapphire Rapids measurements of GP, AMAC or coroutines were found. The studies used Westmere, Haswell and Broadwell; the CoroBase CPU model was not extracted.
- No Rust measurements of coroutine or async interleaving were found.
- Ghost Threading's CPU model and per-workload numbers were not extracted.
- Fill-buffer counts on Cascade Lake and Ice Lake (often quoted as 12) were not verified in this session.

## Key question 2: pipeline fusion; minimum passes over event data and achievable bytes per event

### Takeaway
Fuse compute-bound steps tuple-at-a-time (data-centric compilation). Stage DRAM-bound steps in small L1/L2 batches so they can be prefetched or vectorized (relaxed operator fusion). The problem needs one pass over the input and one compact shuffle (write once, read once). The only extra traffic is proportional to duplicates and cross-partition persons. With 12-byte records, intermediate DRAM traffic can fall from about 240 B/event today to about 50-100 B/event, with a floor near 36-40 B/event; 8-byte records would bring the floor near 28 B/event.

### Cited Findings
- Data-centric code generation pushes tuples through produce/consume calls and "fuses all operators in a pipeline". Vectorization processes blocks through type-specialized primitives. — [Kersten et al. 2018](https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf) (full text)
- Compiled vs vectorized engines, same algorithms and data structures (i9-7900X Skylake-X):
  - "Vectorization is better at hiding cache miss latency, whereas data-centric compilation requires fewer CPU instructions."
  - Typer (compiled) 74% faster on Q1; Tectorwise (vectorized) 4% faster on Q3 and 32% faster on Q9 (hash-probe-bound).
  - Tectorwise executes up to 2.4x more instructions and has up to 3.3x more L1 misses.
  - A CRC hash made Typer up to 40% faster at larger scale factors by "enabling more concurrent outstanding loads".
  — [Kersten et al.](https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf)
- Relaxed operator fusion inserts staging points inside pipelines so SIMD and prefetching become possible. A prefetch group of 16 saturated MLP on a Xeon E5-2630 v4. Results: Q19 6x faster than Vector and 8x faster than HyPer; Q5 3.4x Vector and 1.1x HyPer; Q13 1.4x Vector; generally more than 2x over the non-ROF baseline. — [Menon et al.](https://www.vldb.org/pvldb/vol11/p1-menon.pdf) (full text)
- Morsel-driven parallelism: over 30x average speedup on 32 cores (TPC-H, SSB), morsels "e.g., 100,000" tuples. — [Leis et al.](https://db.in.tum.de/~leis/papers/morsels.pdf) (abstract/search excerpt)
- Radix partitioning (Xeon E5-2407, 100M 16-byte tuples):
  - Software-managed buffers sped partitioning up 2.25x at 64 partitions, 1.85x at 1024 (2.41 s to 1.30 s), and 1.20x at 16384. At 32 partitions the original won.
  - Performance degrades once buffers occupy about 2 MB in total.
  - Streaming stores added up to 1.36x at 16384 partitions, need small buffers (fewer than 8 lines; best is 1 line from 2048 partitions up), and add overhead at low fan-out.
  - Fill-state/destination micro-layouts added 1.52x at 16384 partitions.
  - With buffers plus streaming, "the 4KB pages do never pay off" vs 2 MB pages.
  - Doubling the entry size slowed partitioning only 1.31-1.69x.
  — [Schuhknecht et al., PVLDB 2015](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf) (full text)

### Inferences
Minimum passes, derived from the problem:
- Reading the 1.9 GB input once is unavoidable.
- There are two grouping keys (event_id for dedup, person for sessions) plus a person-keyed join with clicks. Partitioning by raw user ID at parse time lets dedup (through the side channel), person interning, sessionization, click attribution and aggregation all run inside one per-partition pass. That is one write and one read of a compact record per event.
- Unavoidable extras: (a) dedup side data, proportional to bitmap-prefilter positives (about 4% true duplicates plus false positives) if the prefilter gates the side records; (b) re-moving the 18.8% of rows whose person's partition differs.
- Alternative: parse links first (only 0.9M lines), run union-find, build a cache-resident filter of linked IDs, and scatter directly by person. That removes the 18.8% re-move but adds a per-event filter probe and a lookup for each linked-ID row. Roughly a wash; it depends on the fraction of events with linked IDs (gap).

Bytes per event (estimates; 21.6M events):

| Design | Writes | Zeroing | Reads | Random line fills | Approx. intermediate B/event |
|---|---|---|---|---|---|
| Current | 32 B + 16 B scatters (+ clicks) | about the same bytes again (fresh pages) | 32 B + 16 B | about 1.14 lookups x 64 B (upper bound) | ~240 |
| Planned, all side records, no recycling | 12 B + 16 B + 0.188 x 12 B | same | same | about 0.09 x 64 B | ~97 |
| Planned, bitmap-gated side records + recycled chunks | 12 B + about 1.6 B + 2.3 B | 12 B only | 12 B + about 1.6 B + 2.3 B | about 6 B | ~50 |
| Floor, 12-byte records | 12 B | 12 B | 12 B | — | ~36-40 |
| Floor, 8-byte records | 8 B | 8 B | 8 B | — | ~28 |

Fusion recommendations:
- Keep parse fused with scatter (it is compute-bound at about 1.3 GB/s per core).
- Inside the per-partition pass, fuse dedup filtering, interning, sorting or sessionizing, click attribution and aggregation, writing straight into per-partition Arrow column buffers. That avoids materializing sessions (about 2.2M rows).
- Use relaxed-fusion staging (batches of 16-64 rows) only around the residual DRAM lookups.
- Encode intermediates: u32 dictionary IDs, timestamps as u32 offsets after checking the range, attribute bits packed into the spare word. Use late materialization (row references) only for fields needed by output rows; random references back into the 1.9 GB input would themselves be cache misses.

### Gaps
- Neumann 2011 (PVLDB) and Boncz, Manegold and Kersten 1999 were not retrieved; their concepts are covered here through Kersten et al. and Schuhknecht et al.
- Unknown: whether duplicate copies can carry different user IDs, the fraction of events whose raw ID appears in links, and whether the input is sorted by time. Each affects the minimum-pass design.
- No source quantifies the CPU cost of Arrow builders for per-partition writing.

## Key question 3: memory management (page zeroing, populate, huge pages, input I/O, exit teardown)

### Takeaway
Zeroing cannot be turned off in a container; the only fix is to touch less fresh memory (recycle and shrink). MADV_POPULATE_WRITE (5.14+) moves faults into large in-kernel batches. It helps with 4 KB pages, barely with THP, and never removes zeroing. Huge pages should be forced where possible: MADV_HUGEPAGE when the mode is madvise, MADV_COLLAPSE when it is never. For the input, io_uring is blocked by Docker seccomp and O_DIRECT throws away the warm cache. The real choice is mmap (faults plus 35 ms teardown) vs pread into a reused small buffer (an extra copy). Large page-cache folios on new kernels shrink both fault and teardown costs, but that depends on the host.

### Cited Findings
- Anonymous memory is zero-initialized. MAP_UNINITIALIZED requires CONFIG_MMAP_ALLOW_UNINITIALIZED (embedded only). MAP_POPULATE prefaults page tables, causes read-ahead for file mappings, and "doesn't fail if the mapping cannot be populated". — [mmap(2)](https://man7.org/linux/man-pages/man2/mmap.2.html)
- madvise(2) — [madvise(2)](https://man7.org/linux/man-pages/man2/madvise.2.html):
  - MADV_POPULATE_READ/WRITE (since 5.14) "populate (prefault) page tables", and unlike MAP_POPULATE they report errors.
  - MADV_HUGEPAGE works only on hugepage-aligned boundaries. File-backed non-tmpfs memory additionally needs the file not open for write and an executable mapping.
  - MADV_COLLAPSE (since 6.1) is "best-effort synchronous", "independent of any sysfs" THP setting, copies onto a freshly allocated huge page, may reclaim or compact, and needs at least one resident page per region.
  - MADV_DONTNEED on private anonymous memory makes later accesses zero-fill-on-demand.
- Hildenbrand's MADV_POPULATE patch: "There is a performance benefit when using POPULATE_READ / POPULATE_WRITE already when only using a single thread". POPULATE_WRITE is faster than simple writes and read/write loops. The benefit is negligible with huge pages. — [LKML RFCv2](https://lkml.iu.edu/hypermail/linux/kernel/2103.1/00856.html) (search excerpt)
- Transhuge doc — [transhuge doc](https://docs.kernel.org/admin-guide/mm/transhuge.html):
  - "never" does not disable THP globally, because MADV_COLLAPSE ignores the settings.
  - Defrag "madvise" (the default) applies direct reclaim only to MADV_HUGEPAGE regions.
  - mTHP defaults: PMD size "inherit", all other sizes "never".
  - tmpfs/shmem THP defaults to "never".
  - "Currently THP only works for anonymous memory mappings and tmpfs/shmem."
  - PR_SET_THP_DISABLE exists and is inherited across fork and exec.
- Zeroing speed: Ice Lake 2 MB clear about 7.95 to 10.90 GB/s with multi-page clearing; AMD Milan 1 GB pages 16.5 to 42.8 GB/s. Reviewers questioned "when non-cached stores beat the cache". v11 (Jan 2026) replaced cache-optimized huge-page clearing with sequential clearing. — [Arora v3](https://patchew.org/linux/20250414034607.762653-1-ankur.a.arora@oracle.com); [v11](https://lkml.iu.edu/2601.0/06669.html) (search excerpts)
- Teardown batching for large folios: munmap about 20% faster (16 KiB), 31% (32 KiB), 58% (2 MiB), about 1% for 4 KiB. It also speeds up process teardown and MADV_DONTNEED. — [Hildenbrand zap series v3](https://lkml.iu.edu/hypermail/linux/kernel/2402.1/08491.html) (search excerpt)
- ext4 large folios for regular files were merged (Zhang Yi's 8-patch series). Cached buffered reads gained about 77% (64K) and 82% (1M), about 1-2% for 4K (Xeon Gold 6240). Write faults regressed (ext4_page_mkwrite). — [patchew](https://patchew.org/linux/20250512063319.3539411-1-yi.zhang@huaweicloud.com) (search excerpt)
- Per-VMA locks (merged in 6.4) let faults avoid mmap_lock. With large VMAs, threads faulting on the same VMA still contend on the VMA's rwsem. — [LWN 937943](https://lwn.net/Articles/937943); [LWN design article](https://lwn.net/SubscriberLink/906852/51870ed39bd87515/); [Wilcox note](https://www.infradead.org/~willy/linux/store-free-page-faults.html)
- Docker's default seccomp blocks io_uring (moby#46762). — [Docker docs](https://docs.docker.com/engine/security/seccomp/)
- RUMA rewires virtual-to-physical mappings in user space with no OS changes, for partitioning and merging. — [RUMA](https://bigdata.uni-saarland.de/publications/p768-schuhknecht.pdf) (search excerpt)
- Project facts: on WSL2 kernel 5.15, building many small Vecs on 8 threads did not help because of mmap_lock contention, while carving one allocation per worker did. — project memory note (internal)

### Inferences
Zeroing:
- At about 8-11 GB/s per core, every 100 MB of fresh memory costs about 9-12 ms of core time in clear_page alone. Recycle chunks across stages (ranked item 1) and keep all memory mapped until exit.
- Reuse per-partition hash tables across partitions; clearing a small table that is already in cache costs far less than a fresh fault.

Prefaulting:
- Useful mainly when THP is unavailable (4 KB faults): call MADV_POPULATE_WRITE on each worker's own arena slice, from that worker.
- To change wall time rather than CPU, run it in otherwise idle time (start-up, barrier tails).
- Idea, unverified: a SCHED_IDLE helper thread that prefaults the next stage's arena would only run when a CPU is idle. I did not verify whether unprivileged threads may switch to SCHED_IDLE.

Huge pages:
- Apply the decision procedure in ranked item 3, and check that std's allocator is not handing out unaligned big blocks: mmap the arenas yourself with 2 MB alignment.
- Even with THP "always", defrag defaults can silently fall back to 4 KB pages on a fragmented host. Read AnonHugePages to confirm.

Input:
- mmap costs: file faults (batched by fault-around, cost not sourced), about 485K PTEs mapped and then torn down (35 ms serial at exit), and TLB misses.
- pread into a reused 256 KB-1 MB buffer costs a 1.9 GB kernel copy (about 0.16 core-seconds at about 12 GB/s per core) but removes PTEs, faults and teardown. The parser then reads from L2.
- Because the parser runs at about 1.3 GB/s per core, far below DRAM speed, it is probably compute-bound, so the L2 benefit is small. The decision hinges on 35 ms of serial exit vs about 20-40 ms of parallel copy. Measure it.
- If large folios exist in the host page cache (ext4 merged 2025, XFS earlier), mmap faults and teardown get batched and mmap likely wins. Detect this through FilePmdMapped and minflt counts.

Exit teardown when the binary is PID 1:
- Teardown happens in whichever task drops the last mm reference, and docker run waits for PID 1.
- Workable options: don't create file PTEs (pread), make them cheap (large folios), or move the mapping into short-lived child processes whose exit overlaps later stages. That last option forces shmem outputs (4 KB pages by default) and is likely a net loss.
- Freeing memory explicitly before exit only adds work. Call `_exit` immediately after the Parquet files are flushed.

### Gaps
- Not retrieved: Crotty, Leis and Pavlo (CIDR 2022) on mmap and TLB shootdowns, and vmcache/exmap (SIGMOD 2023). vmcache's exmap needs a kernel module, which is unusable here (my recollection, not verified).
- Not found: the current default for fault_around_bytes and per-page costs of file faults; whether Arora's clearing series is merged and in which release; the exact kernel release for ext4 large folios (believed to be 6.16, unverified).
- Not verified: pid_namespaces(7) semantics (when init exits, the remaining processes in the namespace are killed and must be reaped).
- Not verified: whether MADV_COLLAPSE on a mostly empty 2 MB region zeroes in place without first faulting each 4 KB page. The man page says only that at least one page must be resident and that data is copied.

## Key question 4: bandwidth and roofline (per-core and per-hyperthread bandwidth, bytes per event for 0.3 s, streaming stores, write-allocate, cache partitioning)

### Takeaway
On Skylake-SP and Cascade Lake class Xeons, one core gets roughly 12 GB/s, limited by latency and concurrency rather than DRAM. Local DRAM latency is about 80 ns. Four cores therefore give at most about 40-48 GB/s if the shared socket is not saturated, and SMT siblings share their core's fill buffers. For a 0.3 s run, keep total DRAM traffic around 3-4 GB (input 1.9 GB plus at most about 1.5-2 GB of intermediates and zeroing, about 75-100 B/event) and random line misses at a few million. Today's estimate is about 7 GB and about 25M misses. NT stores remain right for write-once scatter output, because write-allocate evasion on Ice Lake triggers only near saturation.

### Cited Findings
- Skylake-SP Xeon Platinum 8176: "only 12 GB/s" available to a single thread, vs about 27.5 GB/s for one AMD Naples core. — [AnandTech](https://www.anandtech.com/show/11544/intel-skylake-ep-vs-amd-epyc-7000-cpu-battle-of-the-decade/12) (search excerpt)
- Cascade Lake Xeon Gold 6248 (2x20 cores, 24.75 MiB L3, measured at 1.6 GHz core / 2.4 GHz uncore): local L3 latency 21.6 ns (54 cycles), local RAM 80 ns (200 cycles), remote about 138-149 ns, L3 read bandwidth 11.3 B/cycle. "On a SNC, the RAM bandwidth can be saturated with eight cores." — [Velten et al., arXiv 2204.03290](https://ar5iv.labs.arxiv.org/html/2204.03290)
- McCalpin: single-thread bandwidth follows Little's law (concurrency / latency) once L2 prefetchers are disabled. Sapphire Rapids is limited by 48 outstanding misses against 130 ns latency. Intel's "single-core bandwidth ... is clearly continuing to fall behind" compute. — [McCalpin ISC23 slides](https://www.ixpug.org/images/docs/ISC23/McCalpin_SPR_BW_limits_2023-05-24_final.pdf); [McCalpin 2023 blog](https://sites.utexas.edu/jdm4372/2023/04/25/the-evolution-of-single-core-bandwidth-in-multicore-processors) (search excerpts; the blog page returned 404 when fetched)
- i9-7900X (Skylake-X, 10 cores): 58 GB/s memory bandwidth. — [Kersten et al., Table 4](https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf)
- Single-thread random access: hardware limit 7.4 ns per cache line, and 4 threads saturate DDR4 bandwidth (hardware model not captured). — [PtrHash](https://arxiv.org/html/2502.15539v2) (search excerpt)
- Non-temporal stores avoid the RFO read, cutting traffic by 1/3 for Copy/Scale and 1/4 for Add/Triad. On a Sandy Bridge EP Xeon, NT stores have higher occupancy than store misses that trigger the L2 prefetcher, so they slow STREAM when concurrency is the limit. — [McCalpin notes on streaming stores](https://sites.utexas.edu/jdm4372/2018/01/01/notes-on-non-temporal-aka-streaming-stores) (search excerpt; the page returned 404 when fetched)
- Ice Lake Xeon Platinum 8358 (32 cores): with few cores and standard stores, bus traffic is 50% above the reported copy bandwidth (write-allocate). SpecI2M write-allocate evasion reaches a ratio of 1.0 only at 29+ cores; on the 8368 it appears only beyond about 30 cores, near saturation. NT stores bypass the cache through write-combining buffers and are "a dependable choice". — [Hager blog](https://blogs.fau.de/hager/?p=8997)
- Streaming stores in radix partitioning help more as fan-out grows (up to 1.36x at 16384 partitions) and add overhead at low fan-out. — [Schuhknecht et al.](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf)
- Kernel zeroing about 8-11 GB/s per core on Ice Lake for 2 MB pages. — [Arora v3](https://patchew.org/linux/20250414034607.762653-1-ankur.a.arora@oracle.com)

### Inferences
Per-core and per-hyperthread bandwidth:
- About 12 GB/s per core (SKX/CLX), which implies about 15 lines in flight at 80 ns.
- When both siblings stream, each gets roughly half; the second thread adds about nothing on bandwidth- or MLP-bound code. Kersten's memory-bound Q6 gained only 3% from SMT (key question 5).
- Ice Lake and Sapphire Rapids raise per-core concurrency (SPR: 48 misses), so expect more per core there, but this was not measured for ICX (gap).

Roofline for 0.3 s on 4 cores (about 40-48 GB/s if the socket is not contended):
- Streaming the 1.9 GB input alone costs at least about 40-50 ms of wall time.
- Current intermediates (about 5.1 GB including zeroing and link-line fills, about 240 B/event) add about 110+ ms of memory time, which alone would be a third of a leader-level run.
- Planned intermediates (about 1.1-2.1 GB) add about 25-50 ms.
- Zeroing 1.75 GB costs about 0.16-0.22 core-seconds, about 40-55 ms wall on 4 cores, because SMT gives little for bandwidth-bound work. It needs to fall below about 0.5 GB.
- Random-miss budget: about 8 ns per miss per core, so about 2 ns aggregate across 4 cores. 24.6M misses cost about 50 ms of wall time; 2M cost about 4 ms.
- Rule of thumb for the 0.3 s target: at most about 100 B/event of total intermediate traffic, and at most about 0.25 random DRAM misses per event.

Streaming stores and write-allocate:
- Keep NT stores with full-line software WC buffers (4-line staging is consistent with Schuhknecht's "small buffers" advice) for scatter output that another core reads later.
- Use regular stores for data the same core re-reads soon (per-bucket sort keys in L2).
- On a 4-core slice, Ice Lake's SpecI2M will not trigger, so regular stores to cold lines pay the RFO.
- NT stores into freshly zeroed pages double the DRAM write traffic, which is another reason to recycle memory.

Cache partitioning:
- Intel Cache Allocation Technology needs resctrl and privileges (inference, unverified), so it is not an option.
- The working sets that matter (WC buffers of 256 KB per thread, per-partition hash tables) should be sized for L2: 1 MB per core on CLX, 1.25 MB on ICX, 2 MB on SPR (standard spec figures, not re-verified here).

### Gaps
- No per-core or per-hyperthread bandwidth numbers were found for Cascade Lake or Ice Lake specifically, and no numbers for cloud VMs where the socket is shared with other tenants.
- Travis Downs' posts and the relevant Intel optimization manual sections were not retrieved.
- Not found: whether the kernel's clear_page (rep stosb) avoids RFO on CLX/ICX, i.e. whether zeroing costs one or two memory transfers per line.

## Key question 5: thread scheduling for 4 cores x 2 SMT (stage overlap, work-stealing granularity, barrier tails, fewer threads per stage)

### Takeaway
SMT on Intel gives about +20-33% on hash-probe-heavy work, about +3-9% on bandwidth-bound work, and varies on compute-heavy code (+6% to +22% for Q1). Software prefetching and SMT draw on the same per-core fill buffers, so their gains do not add up. The scatter-to-dedup barrier is inherent to global partitioning; barrier tails should be filled with independent work (links, clicks, prefaulting) rather than by trying to stream partitions through. Use fine morsels and largest-first ordering, keep per-thread working sets at about half of L2 when siblings share a stage, and measure 4 vs 8 threads for DRAM- or L2-heavy stages.

### Cited Findings
SMT scaling on a 10-core Skylake-X (i9-7900X), TPC-H SF=100, Typer (compiled), 10 threads vs 20 threads (hyperthreads) — [Kersten et al., Table 3](https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf) (full text):

| Query | 10 threads | 20 threads | SMT gain |
|---|---|---|---|
| Q1 | 496 ms | 466 ms | +6% |
| Q6 (memory-bound) | 243 ms | 236 ms | +3% |
| Q3 | 1119 ms | 842 ms | +33% |
| Q9 | 3047 ms | 2525 ms | +21% |
| Q18 | 2099 ms | 1955 ms | +7% |

- Tectorwise (vectorized) gained +22% on Q1, +9% on Q6, +23% on Q3, +15% on Q9 and +20% on Q18. "Hyper-threading is effective at hiding some of the downsides of microarchitecturally sub-optimal code." — [Kersten et al.](https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf)
- AMAC on a Xeon X5670: the GP, SPP and AMAC throughputs "scale well up to four threads" but not to six physical cores (IPC 2x worse at six threads, about 4x more L1-D MSHR hits). The off-chip load queue (32 entries) is the bottleneck. The no-prefetch baseline used SMT to shrink a 2.5x gap to 80%. — [AMAC](https://www.vldb.org/pvldb/vol9/p252-kocberber.pdf)
- Coroutines plus hyperthreading together perform best. — [Jonathan et al.](https://www.vldb.org/pvldb/vol11/p1702-jonathan.pdf). Interleaving gain shrinks with HT (1.8x to 1.3x vs ERMIA). — [CoroBase](https://www.vldb.org/pvldb/vol14/p431-he.pdf)
- Helper threads on SMT siblings beat plain parallelization by 1.11x. — [Ghost Threading](https://www.cl.cam.ac.uk/~tmj32/papers/docs/guo25-micro.pdf) (abstract). 30-70% over single-threaded on Pentium 4 hyper-threading. — [Zhou et al. 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf) (search excerpt)
- Morsel-driven dispatch (elastic, about 100K-tuple morsels): over 30x on 32 cores. — [Leis et al.](https://db.in.tum.de/~leis/papers/morsels.pdf) (abstract/search excerpt)
- Partition buffers degrade once they total about 2 MB (cache capacity). — [Schuhknecht et al.](https://www.vldb.org/pvldb/vol8/p934-schuhknecht.pdf)

### Inferences
Stage overlap:
- Every partition needs every file's rows, so dedup cannot start on partial data.
- Overlap independent work instead: links parse and union-find, then building the person table during the event parse; click parse; bitmap or prefilter construction; MADV_POPULATE_WRITE of the next stage's arena (only if THP is absent); Parquet schema and buffer setup.
- One dispatcher queue with priorities removes most barrier idle time.

Granularity:
- Input morsels of 1-4 MB (about 500-2000 tasks) bound the parse tail to about 1-3 ms.
- For per-partition stages, sort tasks by size (largest first) behind the atomic counter, and split oversized partitions or persons, to bound tails without work-stealing deques.

SMT:
- Parsing (branchy, compute-bound) should use all 8 threads.
- For DRAM-latency stages, 4 threads with full-depth prefetch (about 10-16 in flight) may match 8 threads with shallow prefetch and leave siblings free. The scatter and dedup stages are L2-sensitive: two siblings x (256 KB WC staging + partition hash table) can exceed a 1 MB CLX L2, so use 2048 partitions or 2-line staging, or run 4 threads, and measure.
- Co-scheduling: read thread_siblings_list and pair a memory-bound task with a compute-bound task on each core (a cheap stand-in for Ghost Threading).
- Pin threads with sched_setaffinity inside the container's cpuset to avoid migrations.

Fewer threads can win when:
- (a) per-thread working sets overflow the shared L2/L1 under SMT;
- (b) the stage is limited by the core's fill buffers or the socket's off-chip queue, as in AMAC's results;
- (c) shared mmap_lock or VMA-lock traffic grows with thread count (kernels before 6.4).

### Gaps
- The scorer's vCPU-to-core mapping is unknown (are the 8 vCPUs really 4 sibling pairs?). Probe it from /sys inside the container.
- No direct measurements found of SMT effects on radix scatter with NT stores or on CSV tokenizing on Cascade Lake or Ice Lake.
- The morsel paper's tuned morsel size was not confirmed beyond the "e.g., 100,000" example.
