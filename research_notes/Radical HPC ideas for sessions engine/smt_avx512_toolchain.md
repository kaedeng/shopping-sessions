# SMT, AVX-512 and toolchain levers for the sessions engine on hyperthreaded Intel Xeon vCPUs

Scope: how to cut wall time of one run of the Rust engine (1.9 GB CSV, 21.6M event lines, 5.1M clicks, 0.9M links; parse 1.44 s + dedup/scatter 0.61 s + sessions 0.70 s + output 0.08 s of one-core CPU) on "8 pinned vCPUs (hyperthreads)" of an unknown AVX-512 Intel Xeon, scored by wall time, where score is roughly 0.05 s + (one-core CPU seconds)/3.6. Local evidence comes from the repo's `x86.md` (Zen 4 box under WSL2, no PMU) and the task brief; external evidence is cited inline. My estimates and syntheses sit under "Inferences" only.

Sanity check on the scorer model (my arithmetic, from the task brief): the leaders' 0.31-0.36 s implies about (0.31-0.05)x3.6 = 0.94 s to (0.36-0.05)x3.6 = 1.12 s of one-core CPU, against our 2.8 s. So the leaders do about 2.5-3x less CPU work. Everything in these notes (SMT, AVX-512, PGO and the like) is worth roughly 5-20% at most. Algorithmic work reduction, which other researchers cover, is the main lever. Each 10% of CPU removed is worth about 0.08 s of score.

---

## Key question 1: How does Hyper-Threading behave on Intel Xeons for mixed integer/SIMD/memory-bound code, which resources are shared or partitioned, and how should sibling threads be paired (helper threads, database SMT papers)?

### Takeaway
On Intel cores the two hyperthreads compete for the shared resources: execution ports, load ports, line fill buffers, L1D/L2. Partitioned resources are split in half when both threads run: the store buffer, and on Skylake the data TLBs. So the second thread typically adds about 10-30% throughput, more for latency-bound pointer-chasing code and little or nothing for code that already saturates ports or fill buffers. The database literature (Zhou et al. VLDB 2005) shows that cooperative designs beat naive "two independent workers per core" mainly by cutting cache interference. Siblings that split one input and share read-only structures, or a preloading helper, both qualify. A helper-thread design won at most 4-10% over naive parallelism even on a Pentium 4 with 276-cycle memory, so for this engine SMT-aware sizing of working sets matters more than helper threads.

### Cited Findings
**Shared vs partitioned resources (Intel)**
- Intel: "the store data buffer on a physical core is statically partitioned across the active threads on that core". With two active threads, half the entries serve each thread. When a thread sleeps, its entries can become usable by the other thread. — [Intel, Deep Dive: Analysis of Microarchitectural Data Sampling](https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/intel-analysis-microarchitectural-data-sampling.html)
- Intel: "Fill buffers are shared between threads on the same physical core without any partitioning", and "Load ports are shared between threads on the same physical core." The document gives no entry counts and does not cover L1D, ROB or uop cache. — [Intel MDS deep dive](https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/intel-analysis-microarchitectural-data-sampling.html)
- Measured on Skylake: the store buffer has 56 entries. Merely enabling HT does not shrink it, but running any code (even store-free code) on the sibling halves the capacity available to the first thread. This confirms static partitioning. — [Fallout paper (arXiv 1905.12701)](https://arxiv.org/pdf/1905.12701)
- An Intel-forum post citing Table 2-5 of Intel's optimization manual says that on Skylake the ITLB is dynamically partitioned while "all other levels of TLBs are fixed partitioned". This is secondary, a user quoting the manual; I could not open the thread (404) and saw it only via search. — [Intel community forum, "Hyper-threading on Skylake"](https://community.intel.com/t5/Intel-Moderncode-for-Parallel/Hyper-threading-on-Skylake/m-p/1142414)
- In the original (Pentium 4) Hyper-Threading design, the ROB entries and load/store buffers were statically divided in half when both threads were active. — [Pentium 4 HT analysis (U. Toronto reading)](https://www.eecg.utoronto.ca/~steffan/carg/readings/initial_smt_p4.pdf)
- SMT threads "share many of the execution resources, including the memory bus and caches", so SMT is intrinsically slower than two physical CPUs, and competing threads "may even show a decrease in performance relative to a single thread". — [Zhou, Cieslewicz, Ross, Shah, VLDB 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf)

**How much the second hyperthread adds**
- Phoronix (2026, Xeon 678X "Granite Rapids-WS"): geometric-mean gain from HT about 11%. Blender ran at 1.26-1.29x and code compilation at about 1.1x, while some workloads (easyWave, an OpenVINO face-detection test) were slower with HT on. HT also typically raised latency. — [Phoronix, Hyper Threading on Xeon 678X](https://www.phoronix.com/review/intel-xeon-678x-ht)
- Zhou et al. (Pentium 4 3.4 GHz with HT; 8 KB L1D, 512 KB L2, L2 miss 276 cycles): SMT techniques gave "a 30% to 70% improvement in throughput over single threaded implementations" on in-memory DB operations. Hash join throughput rose up to 48% (two independent parallel joins), 50% (bi-threaded join) and 55% (work-ahead-set join). CSB+-tree traversal rose up to 69% (bi-threaded) and 68% (work-ahead set). — [Zhou et al. VLDB 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf)
- Same paper: with column-wise layout and clustered (sequential, hardware-prefetchable) access, all threading techniques gave only "similar, modest throughput improvements". — [Zhou et al. VLDB 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf)
- Local measurement, Zen 4 not Intel: running two copies of the image at once (two threads per core) took one run from 472 to 736 ms. — task brief

**Cooperative vs naive pairing; helper threads**
- Zhou et al.: naive parallel operators "suffer elevated cache-misses per thread because they are not designed to be cooperative: there is cross-thread cache pollution". The "bi-threaded" operator splits one input between the siblings, interleaved, with separate output buffers, so write-write contention on shared lines is avoided. — [Zhou et al. VLDB 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf)
- Work-ahead set (a helper thread doing explicit loads of addresses the main thread posts): backward preloading with spin-wait was up to 33% faster than single-threaded for index traversal and 41% for unclustered probes, and cut main-thread cache misses by up to 97%. It beat naive parallel operators by only up to 10% (CSB+ tree) and 4.3% (hash join). — [Zhou et al. VLDB 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf)
- When different operators ran on the two siblings (hash join + CSB+ traversal), bi-threading and the work-ahead set beat naive parallelism by 26% and 29%, "primarily due to reduced conflicts in the L2 Cache". Operators whose instruction footprint exceeds the instruction cache may lose more to i-cache contention. — [Zhou et al. VLDB 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf)
- Pitfalls in the same paper:
  - Replacing the helper's explicit loads with prefetch instructions, or using staged single-thread prefetching, performed "comparable to the single-threaded implementation" on P4, where prefetches are dropped on TLB misses.
  - Locks or mutexes for main/helper coordination cost more than the helper gains.
  - Sibling-shared structures caused costly memory-ordering machine clears (MOMC). A work-ahead set of 64 entries was too small; they used 128.
  - Spin-waiting fills the shared execution units, and PAUSE releases resources to the sibling. — [Zhou et al. VLDB 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf)
- Kim et al. (CGO 2004; Intel + U. Maryland) ran prefetching helper threads on a real Pentium 4 HT and saw gains on selected benchmarks. Some HT structures are hard-partitioned rather than shared as in research SMT models, which makes contention hard to predict, and helpers need lightweight synchronization to switch on and off at fine grain. — [Kim, UMD dissertation abstract](https://drum.lib.umd.edu/bitstream/1903/1957/1/umi-umd-1907.pdf)

**Local engine facts that interact with SMT** ([repo x86.md](C:\Users\Shawn\Main\Clone\shopping-sessions\x86.md))
- Per worker, the parse's 1,024 partition tails (64 KB) plus `Vec` headers (24 KB) do not fit a 32 KB L1D. 16% of `parse_with` samples land right after the `EventRec` store into a partition, and 3% on the load of that `Vec`'s len/cap.
- Partition count costs a lot (`parse_bench`):
  - 1 worker: 1.95 GB/s at 2 partitions, 1.37 GB/s at 1,024.
  - 8 workers: 11.3 GB/s at 2 partitions, 7.2 GB/s at 1,024. The 1,024-way scatter is "about a third of the parse".
- In the dedup stage, 24.8% of samples sit on the 16-byte `PersonEvent` store into one of 1,024 buckets, and 9.8% stall right after a prefetch into the link table, presumably with the line fill buffers full.
- Workers come from `std::thread::scope` per stage, with tasks handed out by a shared `AtomicUsize` (`crates/types/src/par.rs`). There is no pinning and no spinning.

### Inferences
- **Why our scatter phases are the most SMT-sensitive code we have.** Each hyperthread effectively gets half the store buffer (Intel, Fallout), probably half the DTLB/STLB on Skylake-class cores (forum citing manual), and shares L1D and fill buffers with its sibling (Intel). On a 4C/8T scorer, two scatter workers per core then hold 2 x (64 KB staging + 24 KB headers) = ~176 KB of hot scatter state per core, against a 32 KB (Skylake/Cascade Lake) or 48 KB (Ice Lake/Sapphire Rapids) L1D. Those L1D sizes are published specs I did not re-verify here. Both threads also fight for one pool of fill buffers for refills and for the `_mm256_stream_si256` write-combining. The dedup stage's prefetch stall on full fill buffers would get worse in the same way. So the SMT yield of the scatter-heavy phases on Intel may be near zero or negative. Our Zen 4 numbers (8 workers on 16 logical CPUs, probably spread across 8 physical cores) would not show this.
- **The scorer model fits "4 cores x 2 threads".** 4 cores x ~1.2 SMT yield x ~0.75 per-core speed of a Xeon relative to a 5 GHz Zen 4 gives about 3.6, the fitted divisor. Eight full cores would need each Xeon core at ~45% of a Zen 4 core, which is implausible. Both multipliers are my assumptions, but the fit supports the 4C/8T reading.
- **Translate "bi-threaded" to this engine.** Siblings should cooperate on one working set rather than each bring its own:
  - (a) Assign scatter partitions, or dedup buckets, to cores rather than to threads, so the two siblings work on disjoint halves of the same input but the same set of open partitions. Partition ownership per core needs atomics or a per-core merge, so treat this as an experiment, not a default.
  - (b) The cleaner variant is the per-core pipeline in idea 9 of KQ6: one sibling tokenizes/parses, which is compute- and branch-bound; the other does the 1,024-way scatter, which is store- and fill-buffer-bound. Each core then holds one set of staging buffers instead of two, and the two threads use different execution resources. This answers the brief's "pair a compute-bound parse thread with a memory-bound scatter thread" directly.
- **A helper thread per core ("4 heavy + 4 light") is unlikely to pay here.** Even on a P4 with 276-cycle misses and useless software prefetch, the work-ahead set beat naive parallelism by only 4.3-10%. Our lookups already carry software prefetching (`lookup-prefetch`), and the helper would consume issue slots and the same shared fill buffers that bound the prefetching. With the score tracking total core occupancy, symmetric workers with SMT-aware sizing look better. The exception would be a phase that is truly latency-bound with idle issue slots and no in-thread prefetching.
- **Spin-waits are poison under SMT.** The current `par` (scoped threads + join + atomic counter) is SMT-friendly: dynamic task claiming absorbs the speed differences between threads whose sibling is busy or idle, and nothing spins. Any future barrier or ring buffer between siblings should spin briefly with `std::hint::spin_loop()` (PAUSE), then block.
- **Serial sections run at full single-core speed** as long as the sibling is idle, so they are relatively cheaper under SMT than parallel phases. Parallel phases get only about 1.1-1.3x per core from the second thread.

### Gaps
- No primary Intel source retrieved for Skylake-SP / Ice Lake-SP / Sapphire Rapids partitioning of the ROB, load buffer, scheduler, uop cache (DSB) or IDQ. Only P4-era statements, a secondary forum citation of the Skylake TLB table, and the Intel MDS doc (store buffer, fill buffers, load ports) were found.
- Fill-buffer counts per core for each Xeon generation not found in a primary source.
- No modern (2015-2026) database paper on SMT-aware operator design found in this session. Only Zhou et al. 2005 and Kim et al. 2004 (both Pentium 4) were found; Collins et al. speculative precomputation was not retrieved.
- No measured Intel-vs-AMD SMT-yield comparison retrieved (the Phoronix GNR-WS vs Zen 5 article returned 403). So it is unknown how far our Zen 4 co-run (472 to 736 ms) over- or under-states Intel's SMT contention.

---

## Key question 2: What do AVX-512 frequency licenses do on Skylake-SP/Cascade Lake vs Ice Lake-SP vs Sapphire Rapids, does a runtime-dispatched AVX-512BW/VBMI2 tokenizer pay off, and what happens to the sibling hyperthread?

### Takeaway
On Skylake-SP/Cascade Lake, even light 512-bit integer instructions move the whole core to a lower frequency license: 3.2 to 2.8 GHz (-12.5%) on a Skylake-SP Xeon W-2104, held for ~0.65 ms after the last such instruction. Ports 0 and 1 also fuse for 512-bit work. On Ice Lake the penalty is about 0-3%, and on Sapphire/Emerald Rapids about 0-2%. AVX-512 kernels for structural-character classification and bitset "flattening" give 1.5-3.3x on those kernels and +60% on whole-parser throughput (simdjson, Tiger Lake). The right design is therefore runtime dispatch:
- 512-bit kernels only where `avx512vbmi2` is present (Ice Lake and newer);
- AVX-512VL at 256-bit width, which avoids the license drop, on Skylake/Cascade Lake;
- AVX2 otherwise.

### Cited Findings
**Skylake-SP class (Skylake-X/W, Skylake-SP, Cascade Lake share the license scheme)**
- Xeon W-2104 (Skylake-SP family) license levels:
  - frequencies: L0 3.2 GHz, L1 2.8 GHz, L2 2.4 GHz;
  - a 512-bit light integer `vpor` dropped the core to L1 (2.8 GHz);
  - 256-bit `vpor` caused no frequency change, only a voltage transition.
  - Timing: the frequency-transition halt is ~10-11 us; voltage-only transitions are ~8-20 us during which dispatch is throttled about 4x; the core stays at 2.8 GHz for ~650 us after a single 512-bit instruction (relaxation ~680 us after the last one).
  - The throttling also slows 256-bit and scalar ALU instructions while it lasts.
  - Sibling-thread effects were not tested. — [Travis Downs, Gathering Intel on Intel AVX-512 Transitions](https://travisdowns.github.io/blog/2020/01/17/avxfreq1.html)
- Xeon Silver 4116 (Skylake-SP): base 2.1 GHz non-AVX, AVX-512 1.8 GHz with one core and 1.4 GHz with all cores (WikiChip figures quoted by Cloudflare). Real NGINX tests:
  - 10% ChaCha20-Poly1305 (AVX-512) requests cut total throughput by 5.5%, and 20% cut it by more than 7%;
  - all requests on ChaCha cost 10%;
  - the AVX-512 code used 2.5% of CPU time with all requests on ChaCha (under 0.3% at 10%), yet it slowed everything else on the server. — [Cloudflare, On the dangers of Intel's frequency scaling](https://blog.cloudflare.com/on-the-dangers-of-intels-frequency-scaling/)
- Skylake-SP executes 512-bit uops with "Port 0/1 fused to a single 512b execution unit" plus a 512-bit unit on port 5. A search summary of that thread says the claim that port 1 is "shut down" is not supported; it is fusion. Parts with one 512-bit FMA unit include Xeon Bronze 3000, Silver 4000 and Gold 5000 (with exceptions). — [Intel community, State of AVX-512 on Skylake-X](https://community.intel.com/t5/Intel-ISA-Extensions/State-of-AVX-512-on-Skylake-X/m-p/1143065)
- LLVM's 2017 RFC on prefer-256/128 cited exactly this: the port 0/1 vector ALUs on Skylake Server are 256 bits wide (512-bit needs both), plus the frequency drop. A Sept 2019 change made `-mprefer-vector-width=256` the default for Skylake-AVX512 and later Intel CPUs (opt back in with `-mprefer-vector-width=512` or `-mattr=-prefer-256-bit`). — [llvm-dev RFC (2017)](https://lists.llvm.org/pipermail/llvm-dev/2017-November/119081.html); [llvm-commits Sept 2019](https://lists.llvm.org/pipermail/llvm-commits/Week-of-Mon-20190909/693481.html)

**Ice Lake, Rocket Lake, Sapphire/Emerald Rapids**
- Ice Lake client (i5-1035G4): light and heavy 512-bit instructions drop the clock only with 1 active core, from 3.7 to 3.6 GHz (100 MHz, ~2.7%); otherwise "there is zero license-based downclocking". Licenses depend purely on width. Voltage-only transitions last about 2x as long as on SKX. Rocket Lake i9-11900K showed no license-based downclocking at all. Ice Lake Xeons were not tested. — [Travis Downs, Ice Lake AVX-512 Downclocking](https://travisdowns.github.io/blog/2020/08/19/icl-avx512-freq.html)
- Ice Lake-SP (3rd Gen Xeon): at launch Intel said it had made improvements to minimize the frequency impact. WikiChip reports that "AVX-512 light" runs at the AVX2 base/turbo frequencies, with no reduction on recent chips. — [WikiChip Fuse, 3rd Gen Ice Lake Xeon launch](https://fuse.wikichip.org/?p=4734)
- Intel's 3rd Gen Xeon crypto whitepaper: during crypto instructions the core "may be reduced" to AVX2 or AVX-512 base frequencies, and recovery may take milliseconds. An NGINX RSA-2k test with 75.81% of cycles in AVX-512 ran at 1.96 GHz against 2.19 GHz for the software version. — [Intel, Cryptography processing with 3rd Gen Xeon Scalable (2021)](https://intel.com/content/dam/www/central-libraries/us/en/documents/cryptography-processing-with-3rd-gen-intel-xeon-scalable-processors-19-may-2021.pdf)
- Sapphire Rapids: Chips and Cheese measured clocks up to 3.8 GHz with 512-bit vectors and found no fixed frequency offsets for 512-bit, unlike early AVX-512 chips. — [Chips and Cheese, A peek at Sapphire Rapids](https://chipsandcheese.com/p/a-peek-at-sapphire-rapids); [Chips and Cheese, Golden Cove hits servers](https://chipsandcheese.com/i/138977299/loadstore-unit)
- Phoronix: Sapphire Rapids has "reduced penalties from engaging AVX-512", and for some AVX-512 instructions there is no longer any measurable impact. Emerald Rapids Xeon Platinum 8592+ peaked at 2.95 GHz with AVX-512 against 3.01 GHz without (-2%). — [Phoronix forum, Genoa vs SPR & Ice Lake AVX-512](https://www.phoronix.com/forums/forum/phoronix/latest-phoronix-articles/1368380-avx-512-performance-comparison-amd-genoa-vs-intel-sapphire-rapids-ice-lake); [Phoronix, Emerald Rapids AVX-512](https://mail.phoronix.com/review/intel-5th-gen-xeon-avx512/5)

**What AVX-512 buys for parsing-like kernels**
- Bitset decoding (turning a bitmask into a list of positions, our "flatten" step):
  - On Skylake-X, an AVX-512 (compress-based) version used 2.4 cycles per value against 4.4 for the tzcnt loop: 45% fewer cycles but only 33% less time. It had no branch mispredictions.
  - On Tiger Lake, a VBMI2-era version took 0.29 ns per value against 0.95 ns for the basic loop, 0.74 for the simdjson-style unrolled loop and 0.57 for the first AVX-512 version.
  - Source: Lemire's blog, May 2022, read via a mirror. — [Lemire, Fast bitset decoding using AVX-512 (mirror)](https://eklausmeier.neocities.org/lemire/blog/2022/05-06-fast-bitset-decoding-using-intel-avx-512); [Lemire, Faster bitset decoding using AVX-512 (mirror)](https://eklausmeier.neocities.org/lemire/blog/2022/05-10-faster-bitset-decoding-using-intel-avx-512)
- simdjson's AVX-512 kernel (Intel-contributed, shipped in simdjson 2.0): ~60% higher throughput on Tiger Lake with GCC 11, 7.4 GB/s vs 4.6 GB/s for the 256-bit kernel. Lemire notes that Rocket Lake and Ice Lake "have done away with this systematic frequency throttling". — [Lemire, Parsing JSON faster with Intel AVX-512 (mirror)](https://eklausmeier.neocities.org/lemire/blog/2022/05-25-parsing-json-faster-with-intel-avx-512); [Phoronix forum on simdjson AVX-512](https://www.phoronix.com/forums/forum/hardware/processors-memory/1325679-intel-avx-512-a-big-win-for-json-parsing-performance)
- Rust 1.89 (Aug 2025) stabilized the `avx512*` target features and many AVX-512 intrinsics. The AVX-512 FP16 intrinsics followed in 1.94. — [Rust 1.89.0 announcement](https://blog.rust-lang.org/2025/08/07/Rust-1.89.0/); [rust-lang/rust#138940](https://github.com/rust-lang/rust/pull/138940); [Phoronix, Rust 1.94](https://phoronix.com/news/rust-1.94-released)
- Local facts: the tokenizer uses AVX2 cmpeq/movemask over 64-byte blocks. The index pass's `flatten` stores (`tok.rs:156`) are 8.7% of all samples, about 17% of the parse; the per-row field work is the rest. — [repo x86.md](C:\Users\Shawn\Main\Clone\shopping-sessions\x86.md)

### Inferences
- **The Skylake-X bitset numbers already show the license tax.** Cycles fell 45% but time only 33%: 0.55/0.67 implies about 18% lower effective frequency during the AVX-512 run. Expect the same on a Skylake-SP/Cascade Lake scorer: the faster kernel loses a third of its gain to clocks, and so does everything else on that core.
- **The sibling pays too (inference; nobody measured it in what I found).** The license and clock belong to the physical core and the p0/p1 fusion is a core-wide port resource. When one hyperthread runs 512-bit code on Skylake/Cascade Lake, the sibling also drops to the L1 frequency for ~0.65 ms after the last 512-bit uop and loses port-1 vector throughput while fused uops issue. Cloudflare's server-wide slowdown from a small AVX-512 share fits this. In our engine all 8 threads run the same phase, so a 512-bit tokenizer on SKX/CLX would put the whole parse phase, about half the run, on L1 clocks: a -10 to -15% frequency tax on everything in that phase, including the scalar field parsing.
- **Estimated gain on Ice Lake / Sapphire Rapids (rough).**
  - Classification in AVX2 takes 2x `vpcmpeqb` + 2x `vpmovmskb` + shift/or per character class per 64 bytes. AVX-512BW needs 1 compare straight into a mask register and 1 `kmovq`, about 2x fewer uops for this step.
  - A VBMI2 `vpcompressb` flatten (bytes 0..63 compressed by the mask, widened and stored, pointer advanced by popcnt) removes the branchy tzcnt/blsr loop. Lemire's 1.7-3.3x on that kernel, applied to a 17%-of-parse flatten, saves ~7-11% of the parse.
  - Total: maybe 3-6% of all CPU on ICX/SPR, more if the field parsing is vectorized too.
  - Under SMT the uop savings count double: fewer uops leave issue slots for the sibling's scalar work.
- **Dispatch design** (ideas only; no copied code):
  - (1) Pick once at startup with `std::arch::is_x86_feature_detected!`. Put the whole hot chunk loop (tokenize + flatten + field parse) inside a `#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vbmi2,bmi2")]` function, so nothing crosses a non-inlinable feature boundary per block; make the parse loop generic over a tokenizer trait and monomorphize it once per ISA.
  - (2) Use `avx512vbmi2` as the proxy for "Ice Lake or newer, small license penalty". Skylake-SP, Cascade Lake and Cooper Lake lack VBMI2; Ice Lake-SP, Sapphire/Emerald/Granite Rapids have it. That is my understanding of the ISA tables; check `/proc/cpuinfo` on a test VM.
  - (3) On AVX-512 parts without VBMI2, use AVX-512VL+BW at 256-bit width: `vpcmpeqb k, ymm`, `vpternlogd`, `vpcompressd ymm`, ymm16-31. Travis showed 256-bit light instructions do not change license on SKX, and the Ice Lake measurements show licenses depend only on width.
  - (4) Masked loads (`_mm512_maskz_loadu_epi8`) can replace scalar tail loops at chunk ends.
- **Local Zen 4 testing cannot validate this.** Zen 4 has AVX-512 incl. VBMI2 but no Intel-style license drops (widely reported, not verified here), so a 512-bit kernel that wins locally may lose on a Skylake/Cascade Lake scorer. Gate the 512-bit path per CPU, not per local benchmark.

### Gaps
- No measured per-license frequency table for Ice Lake-SP or Sapphire Rapids under light 512-bit integer load, only Intel/WikiChip statements and Chips and Cheese / Phoronix observations.
- No source measured the AVX-512 effect on the sibling hyperthread specifically.
- No measured AVX-512 result for CSV tokenizing specifically (only JSON and bitset decoding).
- The exact scorer CPU is unknown. If it is Cascade Lake (the x86.md assumption), the 512-bit path should stay off.

---

## Key question 3: What do PGO and BOLT buy for Rust, and how do we do them inside a Docker build (training, cargo-pgo, instrumented BOLT, risks)?

### Takeaway
On x86, PGO has measured gains of 7-38% on Rust XML parsers (Ryzen 5900X) and 10-16% wall time for rustc. Some microbenchmarks regress 0.5-2%, and our own M4 test showed nothing. BOLT gives up to 8% on top of PGO+LTO for large server binaries, and about 3.7% cycles on LLVM. Both can run entirely inside the Docker build with instrumentation and no `perf`: the generator makes training data at build time. The real risks are mechanical: rustflags precedence silently dropping `target-cpu`, a profile that is never written, and code paths that never ran during training. For a small, hand-tuned binary, expect PGO about 0-8% and BOLT about 0-3%.

### Cited Findings
**Measured gains**
- roxmltree issue (Ryzen 9 5900X, turbo off, Fedora 39, rustc 1.78, cargo-pgo, pinned with `taskset -c 0`), Release vs PGO:
  - roxmltree: huge -7.4%, large -12.2%, medium -9.5%, tiny -12.3% time;
  - other XML crates: quick-xml huge -25.5%, xmltree -33.2%, xml-rs -38.1%;
  - two descendant-iteration microbenchmarks were 0.5% and 2.0% slower;
  - no BOLT numbers. — [roxmltree issue #118](https://github.com/RazrFalcon/roxmltree/issues/118)
- rustc PGO (2020, Ryzen 1700X): ~5% fewer instructions, but 10-16% lower wall time on most real-world builds. The author guesses better i-cache use and branch prediction explain the gap, unverified. PGO "almost tripled" build times. — [Inside Rust: Exploring PGO for the Rust compiler](https://blog.rust-lang.org/inside-rust/2020/11/11/exploring-pgo-for-the-rust-compiler/)
- Later rustc build-config work (perf.rust-lang.org, instructions unless noted):
  - incremental PGO training changes: -0.34% to -2.48% mean;
  - BOLT on `libLLVM.so`: cycles -3.67% mean (-6.60% max), max RSS -3.97% mean;
  - a BOLT attempt on rustc itself had "lackluster" results;
  - BOLT used instrumentation because LBR was unavailable on CI and on AMD;
  - instrumented BOLT crashed nondeterministically until LLVM 15 fixes;
  - overlap between training and benchmark sets was called a "self-fulfilling prophecy". — [Kobzol, Speeding up rustc without changing its code (2022)](https://kobzol.github.io/rust/rustc/2022/10/27/speeding-rustc-without-changing-its-code.html)
- BOLT paper (Panchenko, Auler, Nell, Ottoni): up to 8.0% speedup for data-center apps "on top of profile-guided function reordering and LTO"; up to 20.4% for GCC/Clang on top of FDO+LTO, and 52.1% without them. — [BOLT, arXiv 1807.06735](https://arxiv.org/abs/1807.06735)
- Database data points: ScyllaDB claims up to 50% higher throughput with PGO; separately "5-7% ..., sometimes up to 10%" for Enterprise builds (private conversation quoted by the list). Kernel PGO "does not bring huge results" for Redis/PostgreSQL. PGO is usually applied after LTO and complements it. — [awesome-pgo](https://github.com/zamazan4ik/awesome-pgo)

**Mechanics**
- rustc workflow: build with `-Cprofile-generate=/abs/dir`, run representative inputs, `llvm-profdata merge -o merged.profdata dir`, rebuild with `-Cprofile-use=merged.profdata`. "All rustc flags must match". Use absolute paths and `--target` so build scripts are not instrumented. `llvm-profdata` comes from `rustup component add llvm-tools-preview` and is not put on PATH. `-Cllvm-args=-pgo-warn-missing-function` warns about functions without profile data. Sampling PGO needs `perf record -b` (LBR) and an unstable `-Z` flag. — [rustc book: Profile-guided Optimization](https://doc.rust-lang.org/rustc/profile-guided-optimization.html)
- cargo-pgo:
  - PGO: `cargo pgo build`, run, `cargo pgo optimize`.
  - BOLT: `cargo pgo bolt build [--with-pgo]`, run `<bin>-bolt-instrumented`, `cargo pgo bolt optimize [--with-pgo]`.
  - Needs `llvm-bolt` and `merge-fdata`, from a self-built LLVM or a GitHub release archive (LLVM 16+). The README recommends Docker for BOLT.
  - "Do not strip symbols ... when using BOLT".
  - Put custom rustflags in `[target.<triple>]`, not `[build]`, "otherwise your flags are overridden".
  - BOLT/Docker support is "experimental"; PGO profiles only help on similar workloads, otherwise the binary "may be slower". — [cargo-pgo README](https://github.com/Kobzol/cargo-pgo)
- BOLT:
  - supports x86-64 and AArch64 ELF;
  - needs an unstripped symbol table and, for maximum gains, linking with `--emit-relocs`/`-q`;
  - instrumentation: `llvm-bolt <exe> -instrument -o <exe.inst>`, run the workload, and the profile appears at `/tmp/prof.fdata`;
  - recommended optimize flags: `-reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions -split-all-cold -split-eh -dyno-stats`;
  - stale profiles reduce gains;
  - without LBR data the gains are lower;
  - the README gives no speedup numbers. — [BOLT README](https://github.com/llvm/llvm-project/blob/main/bolt/README.md)
- Cargo uses exactly one rustflags source: `CARGO_ENCODED_RUSTFLAGS`, then `RUSTFLAGS`, then joined `target.<triple>.rustflags`, then `build.rustflags`. They are mutually exclusive, not merged. — [Cargo reference: build.rustflags](https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags)
- Local: `.cargo/config.toml` sets `-C target-cpu=x86-64-v3` under `[target.x86_64-unknown-linux-gnu]`, and "tok-mask and the ID packer pick their fast paths from these at compile time". The `fast-exit` slot ends with `std::process::exit(0)` (`src/main.rs:157`). PGO on an Apple M4 gave no gain (task brief).

### Inferences
- **Why x86 may differ from the M4 null result.** x86-64 has 16 GPRs against AArch64's 31, so PGO-guided spill placement and inlining in the hot per-row parse matter more. x86's decoder/uop-cache front end and branch layout respond to hot/cold splitting. The rustc case shows wall-time gains about 2-3x larger than the instruction-count gains. Still, our hot code is a few hand-written loops with intrinsics, unlike the XML crates that gained 25-38%, so I would budget 0-8% and test.
- **Pitfall 1: rustflags precedence can strip `target-cpu=x86-64-v3`.** Following the rustc book literally (`RUSTFLAGS="-Cprofile-generate=..." cargo build`) makes Cargo ignore the `[target]` rustflags. The binary then silently becomes baseline x86-64, and the compile-time-selected AVX2/BMI2 paths (tok-mask, the pext packer) vanish. Always pass `-C target-cpu=x86-64-v3` (or the chosen CPU) in the same RUSTFLAGS for both builds, or add the PGO flags with `--config 'target.x86_64-unknown-linux-gnu.rustflags=[...]'`. Verify by disassembling for `vpcmpeqb`/`pext`.
- **Pitfall 2: the profile must actually be written.** The LLVM profile runtime and BOLT's instrumentation runtime write at normal process exit. Our `std::process::exit(0)` goes through libc `exit()`, so exit handlers run and the profile should be written. A future "faster" exit via `libc::_exit`/`exit_group` would silently produce no profile, or `/tmp/prof.fdata` for BOLT. Make the training run fail loudly if the `.profraw`/`.fdata` file is missing.
- **Pitfall 3: never-executed paths become "cold".** If the build host differs from the scorer's CPU, runtime-dispatched kernels (an AVX-512 path, say) never run during training and PGO treats them as cold. Options:
  - (a) train on a machine with the same ISA and commit the `.profdata` as a build input, pinned to the exact `rust:1.98` image so the profile is not stale;
  - (b) train each kernel variant separately (env switch) and merge; you cannot run AVX-512 code on a non-AVX-512 build host;
  - (c) keep the dispatched kernels out of PGO's reach (separate crate built without profile-use). Option (c) is my own suggestion; check that it works.
- **Docker build recipe (outline):**
  - Build stage:
    1. `rustup component add llvm-tools-preview`.
    2. Build the generator (it is excluded from the workspace) and generate a full-size training set: same 20M-event shape and the dev set's distribution, a different seed.
    3. Instrumented build: `RUSTFLAGS="-C target-cpu=... -Cprofile-generate=/pgo"`, plus `--target x86_64-unknown-linux-gnu`. This changes the output path, so fix the `COPY`.
    4. Run with `DATA_DIR`/`OUTPUT_DIR`/`NCPU` set, merge, rebuild with `-Cprofile-use`.
    5. Optionally add `-C link-arg=-Wl,--emit-relocs`, then BOLT: instrument, run, optimize.
  - Instrumented multithreaded counters are racy but good enough.
  - Build time is not scored, but a 20M-event generation plus an instrumented run adds minutes. The scorer's build limits are unknown.
- **What BOLT can and cannot do here.** BOLT's wins come from i-cache/iTLB layout in large, front-end-bound binaries (data-center apps, compilers). Our binary is small with a few hot loops, so after PGO expect 0-3%. Do it only after PGO is in and measured, and only if it is cheap to keep. BOLT also adds the risk of an LLVM-version-specific bug in the shipped binary.

### Gaps
- No measured PGO or BOLT numbers for a CSV/columnar ingest engine on Intel Xeon found. The parser numbers are Ryzen and XML.
- Whether `llvm-bolt` is packaged in Debian trixie was not checked (cargo-pgo points to LLVM 16+ release archives).
- The current LLVM behavior for never-executed functions under `-Cprofile-use` (optsize/cold placement, any `-pgo-cold-func-opt` knob) was not verified.

---

## Key question 4: Which other toolchain/runtime levers matter: target-cpu and feature flags, prefer-256-bit, allocators, static/musl, huge pages, codegen pitfalls, container mitigations?

### Takeaway
`x86-64-v4` is safe on every AVX-512 Xeon. It measured -3.7% on Zen 4 but is untested on Intel, and LLVM prefers 256-bit vectors on Intel AVX-512 targets to avoid the license drops. Ship it behind a startup CPU check, never as the only binary. Allocator swaps are exhausted: mimalloc already lost to tuned glibc. The remaining memory lever is touching less fresh memory, by reusing dead buffers and shrinking records, since page zeroing was 6.9% of samples. One container lever is easy to miss: on kernels before 5.16, seccomp-filtered processes could get Speculative Store Bypass Disable forced on, a 2-8% tax that hits store-heavy code. We cannot turn it off, but we can detect it.

### Cited Findings
- Intel AVX-512 parts default to 256-bit preferred vector width in LLVM since 2019, because of the frequency drop and the 256-bit-wide p0/p1 units. A D-forum anecdote saw LDC targeting `x86-64-v4` emit 256-bit sequences in one case and a 512-bit move in another. I found no documentation of x86-64-v4's tuning. A 2023 LLVM change added an `allow-light-256-bit` tuning flag. — [llvm-commits Sept 2019](https://lists.llvm.org/pipermail/llvm-commits/Week-of-Mon-20190909/693481.html); [D forum thread](https://forum.dlang.org/post/dknjcjmdrtiaotsbayed@forum.dlang.org); [llvm-commits 2023, light AVX](https://lists.llvm.org/pipermail/llvm-commits/Week-of-Mon-20230123/1120239.html)
- Measured on Zen 4: `target-cpu=x86-64-v4` -3.7%, `cascadelake` +0.7% (noise); mimalloc slower than tuned glibc. — task brief
- Local x86 facts ([repo x86.md](C:\Users\Shawn\Main\Clone\shopping-sessions\x86.md)):
  - kernel page zeroing (`clear_page_erms`) was 6.9% of samples;
  - building many `Vec`s on 8 threads lost to `mmap_lock` contention on kernel 5.15 (`mprotect` 1.37 ms per call on 8 threads vs 31 us on one);
  - one allocation per worker and record type (`types::parts::Parts`) cut the init stages to 2-3.5 ms and the scorer-like run from 742 to 597 ms;
  - the glibc tunables (hugetlb=1, top_pad=256 MB, mmap_threshold=32 MB) gave 13-14% with THP on and 4% with THP off (Dockerfile comment).
- Container mitigations:
  - Linux 5.16 changed the defaults for `spec_store_bypass_disable` and `spectre_v2_user` from "seccomp" to "prctl", so seccomp-filtered processes no longer get the mitigations forced on. — [seccomp pull for v5.16-rc1](https://lore.kernel.org/lkml/163581426904.14115.15371811525127876559.pr-tracker-bot@kernel.org/T/); [Kees Cook RFC](https://lore.kernel.org/bpf/202110041214.564BF23@keescook/T/)
  - Canonical (bug LP #1980160, requested by Intel): a cloud workload team saw a performance hit in non-privileged containers because default container seccomp turned on the SSB mitigation, tested on Ice Lake and Sapphire Rapids servers. The fix was proposed for Ubuntu's 5.15 kernel (Jammy). — [Ubuntu kernel-team SRU](https://lists.snapcraft.io/archives/kernel-team/2022-October/133708.html)
  - Conflicting claim: the upstream patch says the change was largely moot for big seccomp users because runc/crun already set `SECCOMP_FILTER_FLAG_SPEC_ALLOW` by default. A Canonical reviewer NACKed the backport over cross-hyperthread MDS exposure. — [Kees Cook RFC](https://lore.kernel.org/bpf/202110041214.564BF23@keescook/T/); [Ubuntu NACK](https://lists.ubuntu.com/archives/kernel-team/2022-October/133710.html)
  - Per-process control goes through `prctl(PR_SET_SPECULATION_CTRL, PR_SPEC_STORE_BYPASS, ...)`. — [kernel docs, Speculation Control](https://kernel.org/doc/html/v5.16/userspace-api/spec_ctrl.html)
- SSBD cost: Intel reported about 2-8% (SPECint_rate n-copy ~2%, 1-copy ~8%; servers ~3% n-copy, ~8% 1-copy). Red Hat explains the mitigation disables the memory-disambiguation optimization, so the impact depends on the workload. — [Packt summary of Intel's figures](https://www.packtpub.com/en-lu/learning/tech-news/intels-spectre-variant-4-patch-impacts-cpu-performance); [Red Hat, CVE-2018-3639](https://access.redhat.com/security/vulnerabilities/ssbd)
- In our WSL2 container, `/proc` showed `Speculation_Store_Bypass: vulnerable` and `SpeculationIndirectBranch: conditional force disabled`. Whether the scorer mitigates is unverified. — [repo x86.md](C:\Users\Shawn\Main\Clone\shopping-sessions\x86.md)

### Inferences
- **target-cpu strategy (safest with the most upside): one image, several binaries, chosen in `run.sh`.** Build `ss-v3` (current), `ss-v4` (`-C target-cpu=x86-64-v4`, or `skylake-avx512`/`cascadelake` tuning) and optionally `ss-icx` (`icelake-server` or `sapphirerapids`, with the 512-bit kernels compiled in statically). `run.sh` picks one from `/proc/cpuinfo` flags and `exec`s it, so the binary stays PID 1 and there is no runtime dispatch cost. A missing flag falls back to v3, so a hypervisor that hides AVX-512 cannot cause SIGILL. Expected: -2% to -4% if the Zen 4 v4 result carries over. Check every AVX-512 binary for unexpected `zmm` use (`objdump -d | grep -c zmm`), and force `+prefer-256-bit` for the SKX/CLX binary if any appears. Rust may warn about the non-standard feature name; check the flag actually reaches LLVM.
- **Allocators are done.** Since mimalloc lost and the big buffers are already carved from one mapping per worker, jemalloc or a bump allocator would at best save the remaining glibc metadata and syscall overhead (~0-2%). The bigger target is the kernel's zeroing of fresh pages (6.9%):
  - (a) reuse dead stage buffers, e.g. parse partitions after the dedup has consumed them, for later stages instead of mapping new memory;
  - (b) shrink records (fewer bytes written means fewer pages zeroed and less scatter traffic);
  - (c) optionally prefault each worker's next arena in parallel with `madvise(MADV_POPULATE_WRITE)` (Linux 5.14+; falls back cleanly with EINVAL). The kernel facts are from memory, not fetched here.
  - Under SMT, page zeroing is store-bandwidth work that competes with the sibling's scatter for fill buffers, one more reason to touch less fresh memory.
- **Static linking/musl.** Static glibc saves at most the dynamic loader's ~1 ms. musl would likely lose (slower malloc and string routines), and it is not worth testing ahead of anything else in this list. These are general-knowledge claims, unmeasured here.
- **Rust codegen pitfalls visible in our profile.**
  - (1) `Vec` len/cap reloads after each partition store (3% of parse samples): writes through `*mut T` may alias the header, so LLVM reloads it. Keep cursors in plain locals or a cursor array, which Parts may already do.
  - (2) Bounds checks in the per-field loop: use `chunks_exact`, iterators or one up-front length assert.
  - (3) Mark rare paths (malformed rows, quoted fields, spill paths) `#[cold]`/`#[inline(never)]`; PGO subsumes most of this.
  - (4) Avoid `dyn`/function pointers and large `match` jump tables in hot loops. Indirect branches are the code most exposed to `spectre_v2_user`/STIBP when a container kernel forces it.
- **SSBD, if present on the scorer, hits exactly our pattern.** We store a record through a computed partition cursor, then load the next cursor from memory. With memory disambiguation off, younger loads cannot run past older stores whose addresses are still unknown. If the probe in KQ5 shows `thread force mitigated`, keeping cursors in registers and batching scatter loads ahead of the stores matters more. A forced mitigation cannot be undone from inside the process.

### Gaps
- The x86-64-v4 tuning model in current LLVM (SKX tuning with prefer-256-bit, or generic) was not verified from `X86.td`.
- No measurements found on glibc static vs dynamic startup, or musl vs glibc for this kind of engine.
- Whether the scorer's kernel is older than 5.16, and whether its runtime sets `SECCOMP_FILTER_FLAG_SPEC_ALLOW`, is unknown, as is its THP mode.

---

## Key question 5: How can we measure or emulate the scorer locally, and which metric best predicts the score?

### Takeaway
No local setup reproduces Intel SMT contention or AVX-512 license behavior: the Zen 4 box differs in SMT design and has no license drops, and WSL2 exposes no PMU. The best-predicting setup is the actual image on a rented 8-vCPU (4C/8T) AVX-512 Intel VM. Locally, use three tiers:
1. single-worker per-stage CPU time for micro-optimizations;
2. a sibling-pinned 8-vCPU cpuset for SMT-sensitive changes;
3. the scorer-like fresh-container-after-idle run for final checks.

Add a startup stderr probe so the scorer itself tells us its CPU model, topology and mitigations.

### Cited Findings
- WSL2 VM exposes no PMU. `--cpus 8` over 16 logical CPUs throttled 3 periods (30 ms) over 4 runs, about 2% of a run. A run after ~10 s idle is ~180 ms slower than back-to-back. The `docker run` wall adds ~0.45 s on that box. — [repo x86.md](C:\Users\Shawn\Main\Clone\shopping-sessions\x86.md)
- Two image copies co-running: 472 to 736 ms per run. Fit of two official scores: score ~= 0.05 s + (one-core CPU s)/3.6. — task brief
- Run-to-run frequency effects: AVX-512 license changes on SKX persist ~0.65 ms after the last wide instruction, and transitions halt the core ~10 us. — [Travis Downs, AVX-512 transitions](https://travisdowns.github.io/blog/2020/01/17/avxfreq1.html)
- zamazan4ik pins PGO benchmarks to one core with `taskset -c 0` for repeatability. — [roxmltree issue #118](https://github.com/RazrFalcon/roxmltree/issues/118)

### Inferences
- **Metric tiers:**
  - (1) *Work metric*: per-stage CPU time with `NCPU=1`, pinned to one core. It is low-noise and tracks the "one-core CPU seconds" in the score fit, so use it for instruction-level changes (PGO, flags, codegen). It hides SMT, L2/L3 sharing and bandwidth effects.
  - (2) *SMT metric*: wall time with `docker run --cpuset-cpus=<4 sibling pairs> --cpus 8`, which emulates 4C/8T exactly. It also removes the 16-CPU quota throttling the scorer will not have. Use it for fan-out, AVX-512 width, thread placement and pipelining. On WSL2, verify that the vCPUs reported as siblings (`/sys/devices/system/cpu/cpu*/topology/thread_siblings_list`) really share a core. Pin two integer busy-loops to a claimed sibling pair and to a non-sibling pair: on a real sibling pair each loop should slow down a lot, on a non-sibling pair not at all. Hyper-V's scheduler may not keep the guest's sibling pairs on one physical core; that is my unverified assumption.
  - (3) *Scorer metric*: fresh container, `--idle 12`, median of 3. Co-running two copies is a cruder SMT proxy that also doubles memory-bandwidth demand, which the scorer would not see.
- **An Intel VM is the decisive test bed.** Run the image unchanged with `--cpus 8` on an Intel AVX-512 instance with 8 vCPUs = 4 cores x 2 threads, ideally one of each: Skylake/Cascade Lake, Ice Lake, Sapphire Rapids. Confirm topology with `lscpu`. Measure:
  - wall time;
  - per-stage timings;
  - if the VM exposes counters, `perf stat -e cycles,instructions,ref-cycles` per thread: cycles/ref-cycles reveals license downclocking, instructions/cycles gives per-thread IPC under SMT.
  - This is the only way to see Intel SMT yields and AVX-512 license effects. Cloud instance details are my assumption; verify each instance's topology.
- **Calibration probe** (stderr, a few microseconds, behind an env var if the scorer's stderr is not visible). Print:
  - CPU model name and flags (`avx512bw`, `avx512vbmi2`, `avx512vnni`);
  - `nproc`, `sched_getaffinity` and the sibling lists;
  - the THP mode (`/sys/kernel/mm/transparent_hugepage/enabled`);
  - the kernel release;
  - the `Speculation_Store_Bypass` and `SpeculationIndirectBranch` lines of `/proc/self/status`;
  - the cgroup `cpu.max`.
  - One scorer run then settles most of the unknowns in these notes: CPU generation, 4C/8T or not, SSBD and THP.
- **What predicts the score best.** The fit says total one-core CPU work, and the sanity check (4 x ~1.2 x ~0.75 = 3.6) says the divisor already contains Intel SMT and per-core speed. So rank candidate changes by tier-1 CPU reduction. Use tier 2 to catch changes whose cost depends on SMT (scatter fan-out, AVX-512, buffers). Use tier 3 only to confirm.

### Gaps
- Whether Docker Desktop/WSL2 on Windows 10 21H2 keeps guest sibling vCPUs on one physical core is unknown; the busy-loop test settles it.
- Whether the scorer shows stderr, and whether it is a VM or bare metal, is unknown. A calibration submission is the only way to learn.

---

## Key question 6 (objective): Ranked techniques for this engine on hyperthreaded AVX-512 Xeons

### Takeaway
Rank by expected gain x confidence / effort. First get Intel ground truth: the probe and an Intel 4C/8T VM. Next come the cheap measured levers: SMT-aware scatter sizing (the knob exists already), PGO done right, and multi-binary target-cpu dispatch. Then VBMI2-gated AVX-512 flatten and classification. Speculative SMT designs (the per-core parse/scatter pipeline, helper threads) come last. Altogether these are worth maybe 10-25% of CPU, about 0.08-0.2 s of score. They will not close the 2.5-3x work gap to the leaders on their own.

### Cited Findings
- Evidence used for the ranking is cited under KQ1-KQ5:
  - SMT resource sharing: [Intel MDS](https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/intel-analysis-microarchitectural-data-sampling.html), [Fallout](https://arxiv.org/pdf/1905.12701);
  - SMT pairing results: [Zhou et al. 2005](https://www.vldb.org/conf/2005/papers/p49-zhou.pdf);
  - SMT yield: [Phoronix GNR-WS HT](https://www.phoronix.com/review/intel-xeon-678x-ht);
  - AVX-512 licenses: [Travis Downs SKX](https://travisdowns.github.io/blog/2020/01/17/avxfreq1.html), [Travis Downs ICL](https://travisdowns.github.io/blog/2020/08/19/icl-avx512-freq.html), [Cloudflare](https://blog.cloudflare.com/on-the-dangers-of-intels-frequency-scaling/);
  - AVX-512 kernels: [Lemire bitset decoding](https://eklausmeier.neocities.org/lemire/blog/2022/05-10-faster-bitset-decoding-using-intel-avx-512), [Lemire simdjson AVX-512](https://eklausmeier.neocities.org/lemire/blog/2022/05-25-parsing-json-faster-with-intel-avx-512);
  - PGO: [roxmltree PGO](https://github.com/RazrFalcon/roxmltree/issues/118), [rustc PGO](https://blog.rust-lang.org/inside-rust/2020/11/11/exploring-pgo-for-the-rust-compiler/);
  - BOLT: [BOLT paper](https://arxiv.org/abs/1807.06735), [BOLT README](https://github.com/llvm/llvm-project/blob/main/bolt/README.md);
  - mechanics: [rustc PGO docs](https://doc.rust-lang.org/rustc/profile-guided-optimization.html), [Cargo rustflags precedence](https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags);
  - container mitigations: [Ubuntu SRU on seccomp/SSBD](https://lists.snapcraft.io/archives/kernel-team/2022-October/133708.html);
  - local profile: [repo x86.md](C:\Users\Shawn\Main\Clone\shopping-sessions\x86.md).

### Inferences
Ranked list. Gains are my estimates of one-core CPU unless stated. Score delta is about 0.8 x the CPU fraction of 0.84 s.

**0. Prerequisite: Intel ground truth (probe + Intel 4C/8T VM).**
- *Mechanism*: every decision below (512-bit or not, fan-out, PGO coverage, SSBD) depends on the CPU generation and topology, which the Zen 4/WSL2 box cannot show.
- *Applies*: the scorer CPU could be Cascade Lake, Ice Lake or Sapphire Rapids, and the wrong choice flips the sign of the AVX-512 idea.
- *Expected*: no direct gain; prevents shipping regressions.
- *Risk*: VM noise. Use the tier-3 protocol and medians.
- *Test*: KQ5 probe in one submission; same image on 1-3 Intel instance types.

**1. SMT-aware fan-out and buffer budgets for both scatters.**
- *Mechanism*: under SMT each thread gets about half the store buffer and (on Skylake) half the DTLB/STLB, and shares L1D and fill buffers with its sibling. So size per-thread hot state for half a core:
  - parse scatter: 1,024 x 64 B staging plus headers is 88 KB per thread, 176 KB per core;
  - candidates: a fan-out of 128-256 per pass (8-16 KB staging), plus a second in-L2 split in the dedup stage if the buckets get too big;
  - apply the same to the 1,024 person buckets (16-byte `PersonEvent`).
- *Applies*: the scatter is about 1/3 of the parse on 8 workers. Partition count already swings throughput 1.37 to 1.95 GB/s per worker on Zen 4 without SMT pairing.
- *Expected*: 0-10% total on Intel 4C/8T.
- *Risk*: fewer partitions means bigger per-bucket hash tables. Keep two siblings' tables within ~1/2 to 2/3 of L2 (1 MB SKX/CLX, 1.25 MB ICX, 2 MB SPR; published specs, not re-verified). A second pass costs one more read+write of ~0.7 GB of records.
- *Also test*: plain stores vs `_mm256_stream_si256` under SMT. Both siblings' write-combining drains share the fill buffers, so `scatter-wc`'s win may shrink on Intel.
- *Test*: sweep `SS_PART_BITS` 7-10 under the tier-2 sibling cpuset and on the Intel VM. Compare `NCPU=4` (one per core) with `NCPU=8` per stage: a stage where 4 ties 8 has about zero SMT yield.

**2. PGO in the Docker build, done safely.**
- *Mechanism*: profile-driven block layout, inlining and spill placement for the branchy per-row parse, the sessions sweep and the sort.
- *Applies*: x86 is register-poorer than M4, and there are measured x86 Rust parser gains of 7-38% (roxmltree, quick-xml, xml-rs) and 10-16% for rustc.
- *Expected*: 0-8% (a hand-tuned engine sits at the low end; M4 showed 0).
- *Risks* (details in KQ3):
  - (a) RUSTFLAGS overriding `[target]` rustflags drops x86-64-v3 and the compile-time AVX2 paths;
  - (b) a training run must exit through libc `exit()`, which it does today;
  - (c) dispatched kernels never run in training and become cold. Commit a `.profdata` trained on matching hardware, pinned to the `rust:1.98` image.
  - (d) a training/target mismatch is low risk with the generator, but use full size.
- *Test*: tier-1 per-stage CPU, then tier 3. Check that the binary still contains AVX2 (`vpcmpeqb`/`vpmovmskb`) and BMI2 (`pext`).

**3. Multi-binary target-cpu dispatch in `run.sh` (v3 / v4 / icx).**
- *Mechanism*: the compiler may use EVEX encodings, mask registers, `vpternlog`, 32 vector registers and AVX-512VL ops, while LLVM's Intel tuning keeps auto-vectorization at 256 bits. `exec` keeps PID 1.
- *Applies*: x86-64-v4 measured -3.7% on Zen 4, and every AVX-512 Xeon supports v4.
- *Expected*: 0-4%.
- *Risk*: an unexpected `zmm` in the SKX/CLX binary (check with objdump, or force prefer-256-bit), and SIGILL if a hypervisor hides AVX-512, which the flags check prevents.
- *Test*: on the Intel VM, run each binary and check `cycles/ref-cycles` if counters exist.

**4. VBMI2-gated 512-bit tokenizer and flatten; AVX-512VL-256 variant for SKX/CLX.**
- *Mechanism*:
  - classification: one compare straight into a mask register per character class per 64 bytes instead of 2 compares + 2 movemasks;
  - flatten: `vpcompressb` of offsets 0..63 replaces the tzcnt/blsr loop;
  - masked loads for chunk tails;
  - everything is inside one `#[target_feature]` function, selected once with `is_x86_feature_detected!` (AVX-512 intrinsics are stable since Rust 1.89).
- *Applies*: flatten is ~17% of the parse, and AVX-512 bitset decoding is 1.7-3.3x faster than tzcnt loops (Lemire).
- *Expected*: 3-6% total on Ice Lake/Sapphire Rapids, 1-3% for the 256-bit variant on Cascade Lake.
- *Risks*:
  - the license drop on SKX/CLX (-12.5% core frequency incl. the sibling, ~0.65 ms hysteresis) if 512-bit code runs there, hence the VBMI2 gate;
  - quote/escape edge cases and chunk-boundary bugs; differential-test against `tok-scalar`.
- *Test*: `parse_bench` per ISA on the Intel VM, then tier 3. Zen 4 results do not transfer (no license drops).

**5. Touch less fresh memory.**
- *Mechanism*: reuse arenas across stages, shrink records, optionally prefault next-stage arenas in parallel with `MADV_POPULATE_WRITE`.
- *Applies*: `clear_page_erms` was 6.9% of samples. Zeroing is store-bandwidth work that competes with the sibling's scatter for fill buffers.
- *Expected*: 2-5%.
- *Risk*: lifetime bugs when reusing buffers; prefaulting moves the cost rather than removing it.
- *Test*: tier 1 + `perf stat` faults and sys time.

**6. Sibling-aware thread pinning and per-stage thread count.**
- *Mechanism*:
  - read sibling lists, `sched_setaffinity` each worker (unprivileged within the cpuset);
  - in phases with low SMT yield, run one thread per core, which gives it the full store buffer and TLB; the Intel MDS doc says entries return to the active thread;
  - in branchy/latency-bound phases (sort, sessions), run 8;
  - give serial work a core with an idle sibling.
- *Applies*: stages already spawn scoped threads and claim tasks from an atomic counter, so placement is easy to add.
- *Expected*: 0-3% (more if a VM hides topology and the guest scheduler places threads badly).
- *Risk*: a cpuset that is not 4C/8T, or wrong topology info. Fall back to unpinned.
- *Test*: tier 2 and Intel VM, per-stage.

**7. BOLT (instrumented) on top of PGO.**
- *Mechanism*: post-link block and function reordering plus hot/cold splitting. It needs `-Wl,--emit-relocs`, an unstripped binary and an `llvm-bolt` 16+ archive in the build stage; the profile is written at exit to `/tmp/prof.fdata`.
- *Applies*: weakly. BOLT's 8-20% wins are for large front-end-bound binaries, and BOLT on LLVM gave 3.7% cycles.
- *Expected*: 0-3%.
- *Risk*: BOLT bugs (instrumentation crashes were seen before LLVM 15) and a version-specific binary.
- *Test*: tier 1 + output equality.

**8. Detect and design around forced SSBD.**
- *Mechanism*: log `Speculation_Store_Bypass`. If it reads force-mitigated, prefer scatter code that does not reload cursors right after stores: registers, or batched index computation before the stores.
- *Applies*: SSBD costs 2-8% (Intel), mostly store-heavy code; container seccomp triggered it on pre-5.16 kernels on ICX/SPR (Canonical). The upstream patch claims runc/crun opt out, so this is a conflicting picture.
- *Expected*: 0% if absent, a few % recovered if present.
- *Risk*: none for the probe.
- *Test*: probe first, then microbenchmark under `prctl(PR_SET_SPECULATION_CTRL, PR_SPEC_STORE_BYPASS, PR_SPEC_DISABLE)` locally to measure sensitivity.

**9. Per-core SMT pipeline: parse sibling + scatter sibling (crazy but legit).**
- *Mechanism*:
  - on each core, thread A tokenizes and parses fields into an L2-resident ring of small row batches;
  - thread B, the sibling, drains them into the 1,024 partitions;
  - this gives complementary port usage (SIMD/branch vs store/fill buffers) and one set of staging buffers per core instead of two;
  - B also takes other memory-bound work (clicks/links scatter) when idle.
- *Applies*: this is the brief's "pair compute-bound with memory-bound" made concrete for our biggest stage. Zhou et al. found cooperative sibling designs beat naive parallelism by 26-29% when L2 conflicts dominated.
- *Expected*: speculative, -5% to +15% on the parse.
- *Risks*:
  - load imbalance (the scatter is ~1/3 of parse work);
  - memory-ordering machine clears on shared ring lines (Zhou needed large distances: work-ahead set ≥128);
  - complexity.
  - Hand off in multi-KB batches, publish an index once per batch, and block rather than spin.
- *Test*: prototype behind a feature flag; evaluate only on the Intel VM.

**10. Helper-thread preloading (work-ahead set) for dedup/link lookups: not recommended.**
- *Mechanism*: the sibling issues explicit loads for addresses the main thread posts.
- *Evidence*: only +4.3% (hash join) to +10% (CSB+ tree) over naive parallelism on a P4 with 276-cycle memory and dropped prefetches. Our code already prefetches, and the helper would compete for the very fill buffers that bound the prefetching.
- *Expected*: <=0.
- *Test*: only if idea 6 shows a phase with near-zero SMT yield and idle issue slots.

**11. Allocator swaps, musl, static linking: skip.**
- mimalloc already lost to tuned glibc, the buffers are already arenas, and static/musl changes are ≤1 ms or negative.

### Gaps
- All gain estimates above are mine, from the cited kernel-level numbers and our sample shares. None has been measured on an Intel Xeon. The x86 profile itself comes from a Zen 4 under WSL2 without hardware counters.
- The relative size of SMT contention on Intel versus our Zen 4 co-run is unknown until we have an Intel 4C/8T run.
- No source was found on what the leading public repo's AVX-512 paths do. The brief only reports that it uses PGO and runtime-selected AVX-512.
