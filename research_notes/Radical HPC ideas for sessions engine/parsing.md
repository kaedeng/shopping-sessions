# Extreme-speed CSV tokenizing and field decoding for the sessions engine's parse

Scope: per-row CPU of `crates/ingest` (mask tokenizer in `tok.rs`, per-row parse in `lib.rs::event_row`, timestamps in `ts.rs`, ID packing in `types/src/id.rs`). Baseline from the brief and the repo: ~1.44 s single-core parse for 1.9 GB (~54 ns, ~270 cycles per event row on the Zen 4 box); the `flatten` stores are ~17% of the parse; the parse is ~52% of all CPU samples ([x86.md](C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)). The scorer runs 8 vCPUs that are probably 4 cores x 2 SMT threads on an unknown AVX-512 Xeon (Cascade Lake or newer), so the number of instructions (uops) retired per row matters more than latency.

## Objective: a ranked list of techniques to cut parse CPU per row

### Takeaway
Our tokenizer (masks plus flatten) already costs about what simdjson's stage 1 would on our row shape (~70 cycles per row). Most of the remaining ~200 cycles go to per-field slicing and quote stripping, ID packing, the two timestamp parses and the scatter, so the biggest wins come from cutting instructions per field, not from a faster byte scan. The ranked plan:
1. One flattened list for commas and newlines, read 7 positions per row, with the field count checked once per block by `pext`.
2. Quote checking and stripping done with mask bits instead of byte loads.
3. SIMD decoding of fields in pairs: the two IDs together, and `ts` with `ingest_ts`.
4. A compress/permute row path that runs only when the CPU reports VBMI2 at runtime (Ice Lake or newer).
5. Width-aware AVX-512.

512-bit AVX-512 on a Cascade Lake scorer is probably a net loss, because of the license-1 clock drop.

### Cited Findings
- The engine's tokenizer works in two passes per 8 KB batch. First it builds AVX2 masks, writes 8 unconditional comma-position stores and 2 newline stores per 64-byte block, and saves a 24-byte `Block` record per block. Then, per row, it looks up the block, counts commas and quotes with masked popcounts, and for each of the 7 fields loads a comma position, loads the first and last bytes, compares them with `"`, and strips them ([tok.rs](C:/Users/Shawn/Main/Clone/shopping-sessions/crates/ingest/src/tok.rs)).
- The `flatten` stores are 8.7% of all samples, about 17% of the parse. The scorer-class x86 core has 16 integer registers, against 31 on the M4 ([x86.md](C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)).
- Measured on the M4: `ts` costs 4.2 ns and `ingest_ts` 1.9 ns per row, out of ~22 ns for the whole event row. A branch-free "compute every form and select" timestamp parse was 10% slower than branches on the M4 ([alternatives.md](C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).
- simdjson's Skylake cost model:
  - stage 1 = 1.7 cycles per structural character + 0.62 cycles per byte;
  - stage 2 = 19 cycles per float + 8.7 per structural + 0.31 per byte;
  - 8.3 instructions per byte on average;
  - flattening writes 8 indexes unconditionally per 64-bit word ([Langdale & Lemire, simdjson paper](https://arxiv.org/html/1902.08318v7)).
- csveee (Ellmann & Neumann, PVLDB 19(11), 2026; Rust; AMD EPYC 9745, Zen 5):
  - Single-threaded record counting runs at 1.8 GB/s in quoted mode and 2.1 GB/s unquoted.
  - Its "core" SIMD phase costs about 1 cycle per byte, nearly independent of the data.
  - Index construction rises to about 0.5 cycles per byte for files with 5 to 10 fields per 64-byte vector.
  - For CSV's sparse masks (about 7 bits per vector), a simple scalar set-bit extraction loop beat simdjson's unrolled loop.
  - Converting all 16 TPC-H lineitem columns cut throughput 4.7x (188 to 40 GB/s, multi-threaded on the EPYC server), while converting one integer per record barely mattered (186 GB/s).
  - Source: [csveee paper](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf).
- Ragnar Groot Koerkamp's 1BRC write-up (Intel i7-10750H):
  - Interleaving 4 independent line streams in one thread raised IPC from 2.20 to 2.95, with steps going 1.23 s → 1.19 s → 1.11 s → 1.05 s.
  - After the ILP work, hyperthreading stopped helping: 6 threads took 0.90 s with HT off, and 12 threads 0.92 s with HT on.
  - Source: [curiouscoding.nl 1BRC post](https://curiouscoding.nl/posts/1brc/).
- On Skylake-SP (Xeon W-2104), a light 512-bit integer instruction (`vpor zmm`) drops the core from 3.2 GHz (L0) to 2.8 GHz (L1). The 256-bit `vpor ymm` does not change the frequency. A frequency transition halts the core for ~10–11 µs, and the clock returns ~680 µs after the last wide instruction ([Travis Downs, AVX-512 transitions](https://travisdowns.github.io/blog/2020/01/17/avxfreq1.html)).
- On an Ice Lake client (i5-1035G4), light 512-bit code dropped only from 3.7 to 3.6 GHz, at 1 active core, and not at all at 2–4 cores ([Travis Downs, Ice Lake](https://travisdowns.github.io/blog/2020/08/19/icl-avx512-freq.html)).
- AVX-512 extensions by generation: Cascade Lake has F/CD/VL/DQ/BW/VNNI only. Ice Lake adds VBMI, VBMI2 (`vpcompressb`), BITALG, VPOPCNTDQ, VPCLMULQDQ and GFNI, and Sapphire Rapids keeps them. Throttling, mostly on pre-Ice Lake Intel, is triggered by 512-bit width; AVX-512VL at 128/256 bits does not trigger it ([Wikipedia: AVX-512](https://en.wikipedia.org/wiki/AVX-512)).

### Inferences
**Rough per-row instruction budget of the current event path.** This is my own count from reading the code. It has not been measured: the WSL2 box exposes no PMU ([x86.md](C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)).

| Part | What it does | Approx. uops/row |
| --- | --- | --- |
| Index pass | 3 class masks: 2 loads, 6 cmpeq and 6 movemask per block (~20) | ~28 |
| | `Block` store and quote popcount (~5 per block) | ~7 |
| | Comma flatten (~36 per block) | ~50 |
| | Newline flatten (~12 per block) | ~16 |
| Row slicing (`rows()`) | Block lookup, 2 masked popcounts, count check | ~15 |
| | Per field: position load, 2 byte loads, quote compares, strip (~13 × 7) | ~90 |
| | Likely stack spills of the 7 `Field`s (16 integer registers) | ~15–30 |
| Field decode (`event_row`) | 2 × `pack_at` | ~60 |
| | `ts` | ~45 |
| | `ingest_ts` | ~40 |
| | Event type, validity, record, hash | ~30 |
| | Scatter push | ~15 |
| **Total** | | **~400 uops/row**, about IPC 1.5 at 270 cycles |

Applying simdjson's Skylake model to an event row predicts the tokenizer cost. A row has ~10 structural characters (6 commas, 1 newline, ~2.8 quotes) and 89 bytes, so 1.7×10 + 0.62×89 ≈ 72 cycles per row, close to my ~100 uops for the index pass. The tokenizer is therefore not unusually slow. Slicing plus field decode, roughly 75% of the uops, is where the big cuts are.

**Ranked techniques.** Effect estimates are fractions of parse CPU and assume cycles scale with uops. That is plausible under SMT, where the sibling thread fills latency bubbles but not issue slots. Every estimate needs measurement.

1. **One flattened list for commas and newlines, read 7 positions per row, field count validated once per block with `pext` (all CPUs).**
   - *Mechanism.* Flatten `D = C | N` into a single position list, as u16 since the batch is 8 KB, instead of separate comma and newline lists. Check every row in a block at once the way csveee does: `P = pext(N, D)` should repeat the pattern `0000001` (period 7), selected by rotation from a small table ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)). A well-formed block lets rows read positions `7r..7r+7` at a fixed stride. This is the untested `tok-index` row in alternatives.md.
   - *Why it applies.* It drops the newline flatten (2 stores per block), the `Block` array and the per-row masked popcounts. Mispredictions stay at zero because the stride is fixed.
   - *Expected effect.* −6 to −10%.
   - *Risks.* A malformed row (wrong comma count, about 1 row in 2,000, so ~4.5% of 92-row batches) breaks the stride. Keep an exact per-row resync path that falls back to the current code for that block. Positions must stay exact across batch and block boundaries.
   - *Test.* Run `parse_bench` on one core before and after. Fuzz against `tok::Scalar` with random comma counts per line, and with lines crossing batch boundaries.

2. **Quote validation and stripping from mask bits, not byte loads (all CPUs).**
   - *Mechanism.* Per block, take `QS = Q & (D<<1 | carry)`, quotes at a field's first byte, and `QE = Q & (D>>1 | next)`, quotes at a field's last byte. A line is well-formed exactly when three conditions hold:
     - (a) `Q & ~(QS|QE) == 0`;
     - (b) no 1-byte field is a lone quote: `Q & FS & FL == 0`;
     - (c) each field's start-quote bit equals its end-quote bit. Check (c) for all fields at once by compressing both onto delimiter order, `pext(QS>>1, D)` and `pext(QE<<1, D)`, and comparing one against the other shifted by one delimiter.
   - Then strip each field by adding a bit taken from those `pext` results (shift and AND), or from a bit-test on the in-register `Q`, instead of two byte loads and compares.
   - This is a variant of csveee's B/E masks ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)). It deliberately avoids the PCLMUL prefix-XOR: one malformed line with an odd number of quotes would flip the parity for every later line in the batch unless the parity is reset at each newline.
   - *Expected effect.* −5 to −10%, since about 6–8 uops per field go away.
   - *Risks.* Off-by-one errors in the carries across blocks. Fields of length 0, 1 and 2 (`""` is a valid empty field). Must match the rules exactly.
   - *Test.* An exhaustive small-alphabet fuzzer (`a`, `"`, `,`, `\n`) against the Scalar oracle, plus fixture E20 (`\r` is data).

3. **Decode fields in pairs with SIMD: two IDs per register; `ts` and `ingest_ts` in one 256-bit register for the ISO-Z case (all CPUs; AVX2 is enough).**
   - *IDs.* Load 16 bytes at each ID start into the two halves of a ymm. Mask by length (iota < len). Validate with one `movemask` (high bit set or zero byte). Byte-reverse with `vpshufb`. Pack 7-bit bytes with `vpmaddubsw` (weights 128, 1) and `vpmaddwd` (1<<14, 1), plus one shift/OR. This is the same pack-by-multiply-add idea Muła and Lemire use for 6-bit base64 (`maddubs` 0x01400140, `madd` 0x00011000; ~0.25 cycles per byte for AVX2 decoding against ~1.8 for Chrome's scalar code, on Skylake; [Muła & Lemire](https://ar5iv.labs.arxiv.org/html/1704.00605)).
   - *Timestamps.* Validate the digit and separator template of both stamps with one compare chain, and combine digit pairs with one `maddubs`. Keep the date-table lookup scalar.
   - *Expected effect.* −8 to −15%: IDs ~60 → ~20 uops; ts plus ingest ~85 → ~45 uops in the 58% ISO-Z case.
   - *Risks.* For short inputs the SIMD setup can cost more than it saves: the base64 paper reports little gain below ~100 bytes ([Muła & Lemire](https://ar5iv.labs.arxiv.org/html/1704.00605)). SIMD timestamp parsing is not automatically cheaper than our table method: Lemire's SSE parser takes 65 instructions (7.9 ns) per stamp ([Lemire 2023](https://lemire.me/blog/2023/07/01/parsing-time-stamps-faster-with-simd-instructions/)), against our 1.9–4.2 ns on the M4. Measure each piece separately.
   - *Test.* Microbenchmarks per decoder; property tests against `pack()` and `parse_f1`.

4. **VBMI2 row path, only when `is_x86_feature_detected!("avx512vbmi2")` (Ice Lake, Sapphire Rapids and later; also the Zen 4 dev box).**
   - *Flatten.* Replace it with `vpcompressb` of an iota vector under the delimiter mask, then widen and store. That is 2 uops on port 5 with throughput 2 cycles on Ice Lake through Emerald Rapids ([uops.info](https://uops.info/html-instr/VPCOMPRESSB_ZMM_K_ZMM.html)). Lemire measured 0.29 ns per value against 0.74 for the simdjson-style unrolled loop on Tiger Lake ([Lemire](https://lemire.me/blog/2022/05/10/faster-bitset-decoding-using-intel-avx-512/)).
   - *More radical.* Remove the quote bytes of each validated block with `vpcompressb`, carry the delimiter masks along with `pext(C, ~Q)`, and parse a de-quoted copy kept in L1. Per-field quote logic then disappears. Optionally gather each row's fields into fixed slots with `vpermi2b`, and fetch quote bits per field start with `vpshufbitqmb` (BITALG).
   - *Expected effect.* −10 to −30% on those CPUs, and 0 on Cascade Lake. Compress-based minify in simdjson's 512-bit kernel ran 12 GB/s against 4.3 GB/s for its 256-bit kernel ([Lemire](https://lemire.me/blog/2022/05/25/parsing-json-faster-with-intel-avx-512/)).
   - *Risks.* Two code paths to keep correct. On Zen 4, compress with a memory destination is about 10x slower than compressing into a register ([Go runtime commit](https://remotebranch.eu/Stowage/go/commit/041f564b3e)): always compress into a register and then store, or the local benchmarks will mislead.
   - *Test.* The same fuzzers, run under both dispatch paths, comparing outputs byte for byte.

5. **Width-aware AVX-512.**
   - On any AVX-512 Intel CPU, use EVEX at 256 bits (AVX-512BW/VL): compares straight into k-registers, `vpternlog`, masked loads that remove the need for padding, `vpcompressd ymm`, and 32 vector registers. Use 512-bit vectors only when VBMI2 is present, which means Ice Lake or newer, where the frequency penalty is small ([Travis Downs](https://travisdowns.github.io/blog/2020/08/19/icl-avx512-freq.html); [Lemire](https://lemire.me/blog/2022/05/25/parsing-json-faster-with-intel-avx-512/)).
   - On Cascade Lake, 512-bit light integer code drops the whole parse phase to L1. That was −12.5% on the W-2104, more than the ~3–5% of uops it would save in the mask phase ([Travis Downs](https://travisdowns.github.io/blog/2020/01/17/avxfreq1.html)).
   - *Expected effect.* −2 to −5%.
6. **Handle the three timestamp forms as homogeneous batches.**
   - *Mechanism.* Classify the forms for a batch of rows, then run the Z, offset and epoch parsers each over its own selection vector, the "refill" idea of [Lang et al.](https://15721.courses.cs.cmu.edu/spring2023/papers/08-vectorization/lang-vldbj2020.pdf).
   - *Why.* With forms i.i.d. at 58/10/32, even an ideal predictor mispredicts the byte-10 branch 32% of the time and the Z/offset branch 10/68 of the time: ≈0.42 mispredictions per row (my arithmetic). The M4 showed branch-free selection is worse, which leaves selection vectors as the alternative.
   - *Expected effect.* −2 to −4%.
7. **Struct-of-arrays staging so the ID packing runs across rows** (4 rows per ymm, or 8 per zmm on Ice Lake+, using variable shifts `vpsrlvq` to mask lengths and the shift/OR "squeeze" already written as the non-BMI2 fallback in `id.rs`). This beats plain multi-cursor interleaving on SMT, because it removes instructions rather than only overlapping latency. *Expected effect.* −3 to −8% if technique 3 has not already been done.
8. **Multi-cursor interleaving (2–4 rows in flight per thread).** In 1BRC it raised IPC by about 34% (2.20 → 2.95), but once it was in place HT gave nothing extra ([1BRC](https://curiouscoding.nl/posts/1brc/)). *Expected effect* on 4C/8T: 0 to −5%. Lowest priority.
9. **Not worth it here:**
   - Shape-specialized fast paths: only ~10% of rows match a fixed template (see the fixed-shape section below).
   - Sparser-style raw filtering: most rows pass validation, so there is little to filter.
   - Perfect hashing for product → category: only 3% of rows look it up.
   - Lazily skipping `ingest_ts`: it is needed for every row, by dedup and by the late check.

### Gaps
- No uop or instruction counts were measured, because the x86 box has no PMU under WSL2. The budget table above is a code-reading estimate. Measure on bare-metal Linux, or on cloud VMs with PMU access (one Cascade Lake, one Ice Lake or newer), or approximate with static analyzers such as llvm-mca or uiCA for `cascadelake` and `icelake-server`. Those tools were not evaluated in this research.
- The scorer's exact Xeon model is unknown, so the sign of the AVX-512 (512-bit) effect is unknown. I found no published license-1 frequency table for the cloud Cascade Lake SKUs this scorer might use.
- Unverified hypothesis: Skylake-derived cores (incl. Cascade Lake) sustain one store per cycle and Ice Lake two. If so, the store-heavy flatten (~14 stores per row), the `Field` spills and the scatter compete for one port per core on Cascade Lake, shared by both SMT threads. No source was fetched for the store-port counts.
- The record scatter (32-byte records into 1,024 partitions) is outside this note's scope, though it counts toward parse CPU.

## Q1: What do the fastest published CSV/JSON tokenizers do per byte and per row, and which tricks are we missing?

### Takeaway
The state of the art for tokenizing alone is ~0.3–1.5 cycles per byte, depending on delimiter density and features. Field conversion, not tokenizing, dominates realistic parsers (csveee: 4.7x slower with full type conversion). Tricks we do not yet use:
- csveee's field-count check by `pext` against a rotation table, once per block;
- quote-aware begin/end masks;
- a plain extraction loop for sparse masks;
- simdzone's "expect the common layout and call the specific parser directly" fast path;
- reading into reused buffers instead of mmap, which csveee measured (at 128 cores).

### Cited Findings
- **simdjson** (Skylake, 3.4 GHz):
  - Stage 1 classifies 64-byte blocks with two `vpshufb` nibble lookups, finds quote regions with one PCLMULQDQ prefix-XOR per block (7-cycle latency, 1 per cycle on Skylake), and flattens with popcnt/tzcnt/blsr, writing 8 indexes unconditionally.
  - Model: stage 1 = 1.7S + 0.62B cycles; total = 19F + 11S + 0.92B (S = structural characters, F = floats, B = bytes).
  - 8.3 instructions per byte on average; 0.94–3.2 GB/s across files.
  - Source: [simdjson paper](https://arxiv.org/html/1902.08318v7).
- **csveee** (AMD EPYC 9745, 128 cores, Zen 5, 2.4/3.7 GHz):
  - Masks built per 64-byte vector:
    - B (field beginnings): `D<<1`, moved past an opening quote;
    - E (field ends): delimiter or closing quote;
    - R (characters to remove);
    - where `M = prefix_xor(Q)` (PCLMUL, 5-cycle latency on Zen 5, once per 64 bytes) and `D = (C|N) & ~M`.
  - Field count validated with `P = pext(T, D)`, where T is the record terminators outside quotes (`N & ~M`, called R in the paper's Figure 6, not to be confused with the remove mask), against a compile-time table of rotations of the expected pattern, selected by popcount.
  - Simple scalar extraction beats simdjson's unrolled loop for ~7 bits per vector.
  - Single thread: 1.8 GB/s quoted mode, 2.1 GB/s unquoted, counting records only.
  - Cycles per byte: core ~1; index ~0.5 for 5–10 fields per vector; field processing "essentially free" without escapes.
  - mmap partitioned across threads peaked at 34 GB/s against 183 GB/s with `read` into reused thread buffers, and MAP_POPULATE made mmap slower.
  - Source: [csveee paper](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf).
- **Sep (.NET)**, single-threaded low-level parse (50,000 rows, 29 MB, so ~580 B per row):
  - 21,384.9 MB/s (27.3 ns per row) on a Ryzen 9 9950X (5.7 GHz boost).
  - The tokenizer loads blocks, compares for `\n`, `\r`, `"` and the separator, ORs the results into one bitmask, skips blocks whose combined mask is zero, and writes column-end indices plus per-row column counts.
  - The best variant loads 512 bits, narrows to 256 bits with `vpmovuswb`, then uses 256-bit compares and `vpmovmskb`, reaching 21,597.7 MB/s.
  - Source: [Sep 0.10.0](https://nietras.com/2025/05/09/sep-0-10-0/).
- **simdzone** (DNS zone files, SIMD indexing à la simdjson):
  - In-memory parsing at "nearly a gigabyte per second", at least 3x faster than Knot DNS and about 10x faster than NSD's old parser ([simdzone paper](https://arxiv.org/abs/2411.12035)).
  - Design: it indexes on the fly instead of indexing the whole input up front. It stores token lengths so the parser never rescans for delimiters. Once the record type is known, it expects the common layout and calls the specific field parsers directly. The fast path is inlined and the slow path moved out of line ([simdzone design notes](https://simdzone.docs.nlnetlabs.nl/en/latest/design_notes.html)).
- **Mison** pushes projections and filters into the parser. It predicts the logical position of queried fields from patterns seen earlier in the dataset, maps them to physical positions with SIMD-built structural bit indexes, and claims "one order of magnitude" over existing JSON parsers ([Mison, VLDB 2017](https://www.microsoft.com/en-us/research/publication/mison-fast-json-parser-data-analytics/)).
- **Sparser** applies cheap SIMD "raw filters" (false positives allowed, never false negatives) before parsing, for up to 22x over state-of-the-art parsers and up to 9x end-to-end in Spark, best on highly selective queries ([Sparser paper](https://people.eecs.berkeley.edu/~matei/papers/2018/vldb_sparser.pdf); [DAWN blog](https://dawn.cs.stanford.edu/2018/08/07/sparser)).
- The **simdcsv** README is a plan, not a benchmark: an AVX2 simdjson-style design with a PCLMULQDQ quote pass and a "bits-to-indexes transform", and no performance numbers ([simdcsv](https://github.com/geofflangdale/simdcsv)).
- The **zsv** README claims "the world's fastest CSV parser" and describes a fast parser with "branchless prefix-XOR carry propagation" (NEON, AVX2, SSE2), but gives no GB/s figures there ([zsv](https://github.com/liquidaty/zsv)). csveee excluded zsv because its parallel mode miscounted rows on quoted input ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)).

### Inferences
- Per byte, our index pass is about at simdjson's level: ~72 cycles per row predicted by its model, against our ~0.8 cycles per byte estimated from the 17% flatten share plus the mask work. Sep's 0.27 cycles per byte (5.7 GHz ÷ 21.4 GB/s) is on rows ~6.5x longer than ours (~580 vs 89 bytes), and its delimiter density was not found, so it does not transfer directly. What does transfer is its trick of skipping blocks whose combined mask is zero, which does not apply here: every 64-byte block of event data holds delimiters.
- Tricks to adopt, in order:
  - csveee's block-level `pext` check of the field-count pattern, which removes per-row comma counting and enables fixed-stride rows;
  - csveee's plain extraction loop, as an A/B against our unrolled `flatten::<8>`, since an average block holds only ~4.3 commas;
  - simdzone's "inline fast path for the common layout, out-of-line slow path", which we already do for timestamps (`ts::slow`) and could extend to the whole malformed-row handling.
- csveee's mmap result (34 vs 183 GB/s at 128 cores) suggests testing per-worker `pread` into a reused 1 MB buffer. At 8 threads the gap is probably much smaller. Weigh the copy cost (~1.9 GB of memcpy) against fault-around, page-table teardown and the 0.36 s of system time seen in `perf stat` ([x86.md](C:/Users/Shawn/Main/Clone/shopping-sessions/x86.md)).

### Gaps
- No published per-byte numbers were found for simdcsv or zsv. Their READMEs carry none, and zsv's benchmark page was not fetched.
- I found no source for "Zebra/SIMD-friendly parsing" as a named technique.
- FishStore, ParPaRaw and Instant Loading numbers were not fetched; the team already rejected speculative chunking.
- Mison's per-field speculation numbers come from its abstract only.

## Q2: AVX-512-specific tricks on Intel, and the frequency-license risk on Cascade Lake vs Ice Lake/Sapphire Rapids

### Takeaway
The large AVX-512 wins for parsing come from VBMI2 `vpcompressb` (positions, de-quoting, minify-style compaction) and VBMI permutes. Cascade Lake has neither, and there 512-bit light ops cost an L1 frequency license (about −12.5% on a Skylake-SP part). The safe policy: AVX-512BW/VL at 256 bits on Cascade Lake, full 512-bit VBMI2 kernels only when runtime detection finds Ice Lake or newer, register-form compress only (Zen 4 trap), and k-masks moved to general registers once.

### Cited Findings
- **Bitset decoding (flatten).**
  - Skylake-X, GCC: basic 9.3 instructions and 4.4 cycles per value; simdjson-style unrolled 9.9 and 3.6; AVX-512 `vpcompressd` (four 16-bit masks per 64-bit word, `_mm512_mask_compressstoreu_epi32`) 6.2 and 2.4. That is 45% fewer cycles than basic. The 256-bit variant roughly doubles the instruction count ([Lemire, 2022-05-06](https://lemire.me/blog/2022/05/06/fast-bitset-decoding-using-intel-avx-512/)).
  - Tiger Lake, GCC 9, ~10% bit density: basic 0.95 ns per value; unrolled 0.74; `vpcompressd` 0.57; VBMI2 `vpcompressb` of bytes 0..63, widened with `vpmovzxbd`, 0.29 ([Lemire, 2022-05-10](https://lemire.me/blog/2022/05/10/faster-bitset-decoding-using-intel-avx-512/)).
- **`VPCOMPRESSB zmm{k}, zmm`** ([uops.info](https://uops.info/html-instr/VPCOMPRESSB_ZMM_K_ZMM.html)):

  | CPU | Uops | Latency | Throughput (cycles) |
  | --- | --- | --- | --- |
  | Ice Lake, Tiger Lake, Rocket Lake, Alder Lake-P, Emerald Rapids | 2 (2×p5) | 3/6 | 2.00 |
  | Zen 4 | 2 | | 1.33 (measured) |
  | Zen 5 | 2 | | 1.00 |

- **Zen 4 trap.** On AMD Genoa/Zen 4, `VPCOMPRESSQ` with a memory destination costs about an order of magnitude more than a register destination, and Zen 5 fixes it ([Go runtime commit](https://remotebranch.eu/Stowage/go/commit/041f564b3e)). Lemire's post "AVX-512 gotcha: avoid compressing words to memory with AMD Zen 4 processors" says the same trick works well on recent Intel parts ([mirror of Lemire's post](https://broadcastchannel-c4q.pages.dev/posts/31366)).
- **simdjson 2.0's 512-bit kernel against its 256-bit kernel:** `find_field` 7.4 vs 4.6 GB/s (Tiger Lake, GCC 11); minify 12 vs 4.3 GB/s; twitter.json DOM parse 3.6 vs 2.8 GB/s (~30%). The post says Ice Lake and Rocket Lake "have done away with this systematic frequency throttling", and that `vpternlog` and `vpermt2b` were not yet used ([Lemire, 2022-05-25](https://lemire.me/blog/2022/05/25/parsing-json-faster-with-intel-avx-512/)).
- **Frequency licenses.**
  - Skylake-SP, Xeon W-2104: L0 3.2 GHz, L1 2.8, L2 2.4. `vpor zmm` goes to L1; `vpor ymm` stays at L0. A transition halts the core ~10–11 µs and relaxes ~680 µs after the last wide instruction. The first 256- or 512-bit instruction after ~680 µs idle runs ~9–10 µs at IPC ~0.25 ([Travis Downs](https://travisdowns.github.io/blog/2020/01/17/avxfreq1.html)).
  - On SKX the license classes are: scalar/128 at L0; 256-bit light L0, heavy L1; 512-bit light L1, heavy L2 ([Travis Downs, Ice Lake post](https://travisdowns.github.io/blog/2020/08/19/icl-avx512-freq.html)).
  - Ice Lake client (i5-1035G4): licenses depend only on width, and 512-bit cost only 100 MHz at 1 active core. Rocket Lake showed no license downclocking ([Travis Downs, Ice Lake post](https://travisdowns.github.io/blog/2020/08/19/icl-avx512-freq.html)).
- **Extension availability.** Cascade Lake: F/CD/VL/DQ/BW/VNNI, so no VBMI, VBMI2 or BITALG. Ice Lake and Sapphire Rapids add VBMI, VBMI2, BITALG, VPOPCNTDQ, GFNI and VPCLMULQDQ. Zen 4 and Zen 5 have VBMI, VBMI2 and BITALG. The 128/256-bit forms (AVX-512VL) do not trigger throttling, and gcc and clang default to 256-bit vectors for Intel targets ([Wikipedia](https://en.wikipedia.org/wiki/AVX-512)).
- **Sep's experience.** In .NET 9, which has no mask-register support, each compare became `vpcmpeqb` + `vpmovm2b` and later `vpmovb2m` + `kmovq`. Its AVX-512 parser ran 19,944 MB/s, against 20,599 for AVX2 and 21,598 for the AVX-512-to-256 parser ([Sep 0.10.0](https://nietras.com/2025/05/09/sep-0-10-0/)).
- **PCLMULQDQ prefix-XOR** costs one instruction per 64 bytes, with 7-cycle latency and 1 per cycle on Skylake ([simdjson paper](https://arxiv.org/html/1902.08318v7)), and 5-cycle latency on Zen 5 ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)).
- **Our own data.** The repo measured `target-cpu=x86-64-v4` at −3.7% total, within the noise, on the Zen 4 box. AVX-512 intrinsics are stable since Rust 1.89 ([alternatives.md](C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).

### Inferences
- **Cascade Lake.** Our current flatten costs ~6.5 cycles per position (17% of ~270 cycles over 7 positions per row, measured on Zen 4), well above Lemire's 3.6 cycles per value for the same unrolled method on Skylake-X. Our blocks are sparse (~4.3 commas), so about half of the 8 stores are wasted. A 512-bit `vpcompressd` flatten would save ~1.2 cycles per value against unrolled, ~8 cycles per row, but would put the whole parse at L1 (−12.5% on the W-2104 example). That probably loses overall. `vpcompressd ymm` (8 lanes per op, 8 ops per 64-bit mask) avoids the license but roughly doubles the instructions, per Lemire's note on the 256-bit variant. Not attractive.
- **Ice Lake, Sapphire Rapids and later.** VBMI2 `vpcompressb` positions (0.29 vs 0.74 ns per value) would cut the flatten by more than half. Compress-based de-quoting enables a parse path with no per-field quote logic. Light-512 throttling is negligible on these parts.
- **Runtime dispatch.** Read `is_x86_feature_detected!("avx512vbmi2")` once per process and select a monomorphized `parse_fn`. The repo already picks `parse_fn` once (`Ingest::open`).
- **Mask moves.** Rust's AVX-512 intrinsics return `__mmask64` directly (`vpcmpeqb k` → `kmovq r64`, 2 uops per class per 64 bytes, against 6 for AVX2's two cmpeq/movemask pairs plus shift/OR). Sep's .NET penalty should not apply, but check the generated assembly for `vpmovm2b`/`vpmovb2m` round trips.
- **Testing trap.** The Zen 4 dev box runs AVX-512 without license downclocking and has fast register compress. Local wins will therefore overstate the gain on Cascade Lake and understate the Ice Lake compress cost (2 cycles vs 1.33 throughput). Never use the memory-destination compress form, or Zen 4 numbers will be ~10x worse than Intel's.

### Gaps
- No measured license frequencies for Ice Lake server or Sapphire Rapids Xeons were found, only Ice Lake client and an assertion that ICX transitions are fast.
- No published per-row CSV numbers on Cascade Lake with and without AVX-512 were found.
- Skylake-SP's 512-bit port layout (ports 0+1 fused), which caps 512-bit integer throughput, was not verified; the WikiChip fetch failed.

## Q3: Can we avoid flattening positions, for example with a fixed-shape fast path, shape detection, or skip-ahead?

### Takeaway
Fixed-shape speculation is a poor fit for this data. Random per-field quoting (20%), the timestamp forms (58/10/32) and ID-length variation leave only ~10% of rows on any single byte template, so a template branch would mispredict most of the time. The productive form of "avoid flattening" is shape-agnostic straight-line code: a fixed stride of 7 positions per row from one unified delimiter list, validated per block by `pext`. Or, more radically, no position list at all: comma offsets taken from in-register masks per row. Optionally, VBMI2 compress replaces the stores entirely.

### Cited Findings
- csveee fixes the field count N at compile time, and finds that this enables cheap index and record construction and gives the speculative oracle more power ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)).
- csveee checks the field count of all records in a 64-byte vector at once with one `pext` and a rotation-table XOR. For 64 or more fields it uses popcnt/tzcnt ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)).
- simdzone expects the predictable layout once the record type is known, falls back to a slow path otherwise, and calls the specific field parsers directly instead of a generic descriptor-driven parser ([simdzone design notes](https://simdzone.docs.nlnetlabs.nl/en/latest/design_notes.html)).
- Mison speculates on field positions from patterns observed in the dataset ([Mison](https://www.microsoft.com/en-us/research/publication/mison-fast-json-parser-data-analytics/)).
- simdjson flattens so that stage 2 can walk a tape of structural positions later, and expects at least one branch miss per word when more than 8 bits are set ([simdjson paper](https://arxiv.org/html/1902.08318v7)).
- In 1BRC, a branch-free fixed-width scan (one 32-byte compare always finds the next `;` and `\n`, since lines are ≤33 bytes) cut branch misses from 150M to 4M and instructions from 96.4G to 87.6G, but did not speed the run up: IPC fell from 2.72 to 2.49 ([1BRC](https://curiouscoding.nl/posts/1brc/)).
- Pandey, Bender and Johnson's machine-word select (k-th set bit) takes 4 x86 instructions, two of them introduced in Haswell, i.e. PDEP plus TZCNT, and is 2–4x faster than earlier word-level select ([Pandey et al.](https://arxiv.org/abs/1706.00990)).

### Inferences
- **Template hit rate**, from the data facts, assuming uniform 40-bit ID values:
  - A letter plus 8 base36 digits (9 bytes) has probability (2^40 − 36^7)/2^40 ≈ 92.9%; 7 digits ≈ 6.9%.
  - P(no quoted field among 7) = 0.8^7 ≈ 21%.
  - P(both IDs 9 bytes) ≈ 86%; P(ISO-Z `ts`) = 58%.
  - So ≈10% of event rows match the commonest byte layout, and ≈50% even after de-quoting. A shape branch would be close to the worst case for prediction. Rule it out, or use it only as a branch-free "same offsets" path.
- **Fixed stride from a unified list (technique 1 above).** Flatten `D = C|N` into one u16 list. Validate the period-7 newline pattern per block with `pext(N, D)`. Rows then start at `7r`. This removes the separate newline flatten (`W=2`), the `Block` array, the two masked popcounts per row, and the block lookup. A malformed row breaks the stride, so resync from that block with the current per-row code, which is exact.
- **Row-direct, no position memory (more radical).** Keep per-block C/Q masks in registers. For each row, form a 128-bit view starting at the row's first byte with `shrd`/u128 shifts over the stored block masks, and take the 6 comma offsets with a `blsr`/`tzcnt` chain (2 uops per comma) or with independent `tzcnt(pdep(1<<k, m))` (2 uops per comma, but parallel). This avoids ~50 store uops per row, at a cost of ~20 uops of window construction.
  - Pitfall: with 89-byte rows, a row ending early in a block often starts two blocks back (P ≈ 25/64 ≈ 39% for an 89-byte row). The window must span 3 blocks, or be built from the row start, to stay branch-free.
  - Lines longer than the window (allowed: leading zeros in epoch-ms or price can make a valid line arbitrarily long) need the exact scalar fallback.
- **Skip-ahead on ID length.** Predicting comma positions from typical lengths saves nothing once the masks exist; the masks already give exact positions for free. Skip-ahead only pays when masks are not computed, as in 1BRC's per-line scan.

### Gaps
- No public measurement of a CSV parser that consumes rows straight from register masks with no stored index. csveee and Sep both store offsets.
- The win from fixed-stride rows over our current per-row counting is untested in the repo (`tok-index` is marked untested in [alternatives.md](C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).

## Q4: Faster timestamp and integer parsing (SWAR/SIMD; three timestamp forms; validating 8+ digits at once)

### Takeaway
Our SWAR plus date-table design is already near the published frontier. Lemire's SIMD timestamp parser spends 65 instructions (7.9 ns) per simpler 14-digit stamp, and his AVX-512 integer parser 57 instructions (17 cycles) per integer, both above our ~1.9–4.2 ns on the M4. The remaining gains are:
- parsing `ts` and `ingest_ts` together in one vector for the 58% ISO-Z case;
- handling the three forms without mispredictions (selection vectors);
- validating both 8-digit SWAR words of an epoch-ms in one 128-bit compare.

### Cited Findings
- **Lemire's SIMD timestamp parser** (SSE; Ice Lake server; GCC 12), for `%Y%m%d%H%M%S`:
  - Loads 16 bytes and subtracts `'0'`; checks per-digit limits with a saturating subtract (`_mm_subs_epu8`); combines digit pairs with `_mm_maddubs_epi16` (weights 10, 1); checks month ≤12, day ≤31, hour ≤23 and minutes/seconds ≤59 with `_mm_subs_epu16`; finishes the epoch in scalar C.
  - 65 instructions and 7.9 ns per stamp, against 700 instructions and 46 ns for `strptime`.
  - No days-per-month or leap-year check in the SIMD part.
  - A reader suggested XOR instead of subtract and checking the BCD-like limits before `maddubs`.
  - Source: [Lemire 2023](https://lemire.me/blog/2023/07/01/parsing-time-stamps-faster-with-simd-instructions/).
- **Lemire's AVX-512 integer parser** (digit span known): a masked load right-justifies the digits; a masked subtract and compare against 9 into a k-mask validates them; a `maddubs` → pack → `maddubs` → `madd` cascade converts. Throughput 1.8 GB/s, 57 instructions and 17 cycles per number, against 0.8 GB/s, 128 instructions and 39 cycles for `std::from_chars` (Ice Lake server, GCC 12); 2.3 GB/s inlined ([Lemire 2023](https://lemire.me/blog/2023/09/22/parsing-integers-quickly-with-avx-512/)).
- **simdjson** checks 8 digits with a SWAR test (`0x3333…` comparison) and converts with ~7 SIMD instructions, ≥17 cycles of latency. Stage 2 costs 17–19 cycles per float ([simdjson paper](https://arxiv.org/html/1902.08318v7)).
- **1BRC** number parsing: a single-multiplication digit combine (multiplier `1 + (10<<16) + (100<<24)`) ran 1.48 s; a PDEP-based parse 1.42 s; branchless parsing cut branch misses from 440M to 140M ([1BRC](https://curiouscoding.nl/posts/1brc/)).
- **Our current code** ([ts.rs](C:/Users/Shawn/Main/Clone/shopping-sessions/crates/ingest/src/ts.rs)):
  - A perfect-hashed date table settles `YYYY-MM-DDT` with two 8-byte compares.
  - `HH:MM:SS` is checked in one SWAR word with carry-trick limits.
  - Offsets go through a second SWAR word.
  - Epoch-ms (13 digits, no leading zero) takes two overlapping 8-digit SWAR checks and parses; everything else goes to an out-of-line slow path.
  - The form is picked by branching on byte 10, then on length.
  - Measured on the M4: 4.2 ns for `ts` and 1.9 ns for `ingest_ts` per row ([alternatives.md](C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).

### Inferences
- **Branch cost.** With forms i.i.d. at 58/10/32, branching costs ≥0.42 mispredictions per row. Two options remove most of them without the 10% branch-free penalty seen on the M4:
  - (a) Classify the form for a batch of rows (one 32- or 64-byte compare of byte 10 across staged rows, or the field lengths), then run three homogeneous loops over selection vectors ([Lang et al.](https://15721.courses.cs.cmu.edu/spring2023/papers/08-vectorization/lang-vldbj2020.pdf) report up to 30–34% from this "refill" idea in compiled query pipelines).
  - (b) Order the checks so the 58% form falls through straight-line code.
- **Pair parsing.** For an ISO-Z `ts` (58%) and the always-ISO-Z `ingest_ts`, load both 20-byte fields into one ymm. Validate all digits and the `-`, `:`, `T`, `Z` positions against a two-stamp template with one `vpsubb` (or XOR), one saturating-subtract limit check and one separator compare. Do one `vpmaddubsw` for all two-digit groups. Extract the two date words for the scalar table lookups. This saves roughly 40% of the two parses' uops in that case (my estimate).
- **Epoch-ms.** Validate the two overlapping 8-byte words in one SSE compare (16 bytes at once) instead of two SWAR `is8` calls. A small gain (32% of rows).
- **Price parsing** runs only for purchases (3%) and is already SWAR. Leave it.
- **Don't replace the date table** with calendar arithmetic: the table is one multiply and one load, and Lemire's SIMD parser still finishes in scalar code.

### Gaps
- Wojciech Muła's "SIMD parsing of integer sequences" article (shuffle patterns indexed by a separator mask) could not be fetched (TLS certificate error on 0x80.pl), and its repo README has no numbers.
- I found no published SIMD parser for RFC 3339 with offsets and validation that reports ns per stamp on Intel server parts. The `rfc3339-fast` crate has an SSE backend but no fetched numbers.

## Q5: Interleaving rows/streams in one thread for ILP, and SoA staging so each phase runs as a tight loop

### Takeaway
Interleaving raises IPC on dedicated cores, but on SMT the sibling thread already fills those slots. In 1BRC, HT gave nothing once ILP was added. On the 4C/8T scorer, prefer restructurings that remove instructions (SoA staging that enables SIMD across rows, fewer stores and spills) over those that only overlap latency.

### Cited Findings
- 4-stream interleaving in 1BRC: IPC 2.20 → 2.95 across the steps, runtime 1.23 s → 1.05 s, instructions about constant (62.7G → 63.0G). Splitting data into 2–4 chunks without interleaving the instructions "didn't work out immediately" ([1BRC](https://curiouscoding.nl/posts/1brc/)).
- Hyperthreading in the same post:
  - Early version: 12 threads with HT reached IPC 1.33 over 54.3G cycles, against 6 threads without HT at IPC 2.39 over 30.4G cycles, for the same 72.5G instructions; HT made that version ~6–10% faster.
  - After ILP work: 6 threads 0.90 s (HT off) vs 12 threads 0.92 s (HT on).
  - Source: [1BRC](https://curiouscoding.nl/posts/1brc/).
- csveee notes that hyperthreads gave "diminishing or even slightly negative returns" beyond 128 cores, partly because memory bandwidth became the limit ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)).
- Refill and compaction of active lanes with AVX-512 compress/expand gave up to 30% (geospatial join) and up to 34% (TPC-H Q1-style scan) in compiled pipelines ([Lang et al., VLDB J 2020](https://15721.courses.cs.cmu.edu/spring2023/papers/08-vectorization/lang-vldbj2020.pdf)).
- The repo already lists 2–4 cursors per worker as untested, citing a 1BRC entry that went from 1.34 to 1.23 s ([alternatives.md](C:/Users/Shawn/Main/Clone/shopping-sessions/alternatives.md)).

### Inferences
- **Expected effect** of multi-cursor interleaving on the scorer: 0 to −5% of parse CPU, since total work counts and both SMT threads parse at the same time. The cost: more live registers on an x86 core with only 16 GPRs, so more spills.
- **SoA staging is worth it when it unlocks SIMD across rows.** For example:
  - Stage (ptr, len) of event_id and user_id for 8–16 rows and pack 4 IDs per ymm, 8 per zmm on Ice Lake+, with `vpsrlvq` masking plus the shift/OR squeeze from `id.rs` (works on AVX2).
  - Classify timestamp forms for 16 rows at once and run homogeneous loops.
  - Each staged entry costs a store and a load (~2 uops), so the SIMD phase must save more than that per field.
- **A cheaper first step than interleaving:** reduce register pressure in `rows()` → `event_row` so the 7 `Field` structs (112 bytes) are not spilled, by consuming fields as they are produced instead of materializing `[Field; 7]`.

### Gaps
- No direct measurement of row interleaving under SMT for a parser with heavy field work (as opposed to 1BRC's lighter per-line work).
- The scorer's SMT topology (4×2 vs 8×1) is unconfirmed.

## Q6: Lazy and partial parsing, and raw filtering

### Takeaway
Little to gain. Nearly every event row survives validation (malformed ~0.05%, plus some invalid or late rows), so Sparser-style raw filtering has no selectivity to exploit. Dedup comes before validation, so every row needs `event_id` and `ingest_ts`, and nearly every kept row needs every other field. The code already parses `product_id`/`price` lazily, only for the 3% purchases. The remaining "skip work" levers are cheaper validity checks and not building per-row structures nobody uses.

### Cited Findings
- Sparser's raw filters gain most on highly selective queries: up to 22x over parsers like Mison, up to 9x end-to-end ([Sparser](https://people.eecs.berkeley.edu/~matei/papers/2018/vldb_sparser.pdf); [DAWN blog](https://dawn.cs.stanford.edu/2018/08/07/sparser)).
- Mison's speedups come from pushing projections and filters into the parser ([Mison](https://www.microsoft.com/en-us/research/publication/mison-fast-json-parser-data-analytics/)).
- csveee: one integer conversion per record changes throughput by ~1% (188 → 186 GB/s); converting all 16 columns makes it 4.7x slower ([csveee](https://www.vldb.org/pvldb/vol19/p3579-ellmann.pdf)). Per-field conversion work dominates once tokenizing is fast.
- simdjson On-Demand materializes values only when accessed. The abstract reports "superior performance in multiple benchmarks" without numbers ([Keiser & Lemire](https://arxiv.org/abs/2312.17149)).
- Our parse already decodes `price_cents` and looks up the product category only when `event_type == purchase`, and keeps invalid rows with a cleared `VALID` bit so dedup still sees them ([lib.rs](C:/Users/Shawn/Main/Clone/shopping-sessions/crates/ingest/src/lib.rs)).

### Inferences
- **Fields a kept row can skip:** only `product_id` and `price` for views and carts (97%), and those are already reduced to a length check on `product_id`. Parsing "after dedup only for the kept copies" would save at most the duplicate share of rows (~7% if the 21.6M lines hold 20M unique events), and would cost a second pass to re-find the fields. Not worth it.
- **Mask-level raw checks that are worth having:** the field-count and quote validity checks of technique 1 and technique 2 settle well-formedness for a whole block in a few mask operations. That is the CSV analogue of Sparser's "cheap filter first, exact work only on suspects": exact per-row checks then run only in the ~4.5% of batches that contain a malformed row.
- **Lazy `ingest_ts`** is not possible: the late check (`ingest − ts ≤ 24h`) and the min-ingest dedup need it for every row. Making it cheaper (pair parsing with `ts`, technique 3) is the lever.

### Gaps
- The exact duplicate rate and the invalid or late share of the test set were not available, so the upper bound for lazy validation is an estimate.

## Q7: Hashing and packing IDs straight from SIMD registers; perfect hashing of product IDs

### Takeaway
Packing the two 9-byte IDs of each row in SIMD (multiply-add 7-bit packing, as in SIMD base64) or across rows with SoA is the main ID lever. It could cut ~60 uops per row to ~20. Perfect hashing (PtrHash and similar) is irrelevant for products, since only 3% of rows look one up. It matters only where every row does a lookup, which is outside the parse.

### Cited Findings
- **The current `pack_at`** ([id.rs](C:/Users/Shawn/Main/Clone/shopping-sessions/crates/types/src/id.rs)):
  - Loads 8 bytes plus a conditional 9th, masks by length, rejects high-bit or zero bytes, and goes out of line (interning) for IDs over 9 bytes.
  - Byte-swaps, then `pext(x, 0x7F7F…)` keeps 7 bits per byte, preserving byte order.
  - A non-BMI2 shift/OR "squeeze" fallback exists.
- **Muła & Lemire's AVX2 base64 decoder** packs 6-bit values with `vpmaddubsw` (0x01400140), `vpmaddwd` (0x00011000), `vpshufb` and `vpermd`, at ~0.25 cycles per byte against ~1.8 for Chrome's decoder (i7-6700 Skylake), with little gain below ~100 bytes ([Muła & Lemire](https://ar5iv.labs.arxiv.org/html/1704.00605)). The AVX-512 version encodes and decodes "at nearly the speed of a memory copy" with several times fewer instructions ([Muła & Lemire, AVX-512](https://arxiv.org/abs/1910.05109)).
- **1BRC hashing:** inline `u64` keys and FxHashMap took the run from 61 s to 41 s. Before PtrHash, hash lookups were ~35% of runtime; after, the lookup was still ~20% ([1BRC](https://curiouscoding.nl/posts/1brc/)).
- **Rows per type:** purchases are 3% of event rows, and only they look up a product category; clicks look up one of 2K campaigns (brief; [lib.rs](C:/Users/Shawn/Main/Clone/shopping-sessions/crates/ingest/src/lib.rs)).

### Inferences
- **SIMD pair packing**, all CPUs:
  - Load 16 bytes at each ID start into the two halves of a ymm (`vinserti128` from memory).
  - Mask by length with `vpcmpgtb(iota, len)`; check for no high bit and no zero byte with one `vpmovmskb`.
  - Reverse bytes with `vpshufb`; pack with `vpmaddubsw(·, 0x01 0x80 …)` → 14-bit, then `vpmaddwd(·, 1 | 1<<14)` → 28-bit, then one 64-bit shift/OR → 56 bits; add the 9th byte with a shift.
  - Extract 2× u64 (`vmovq`/`vpextrq`).
  - About 14–20 uops for both IDs against ~60 now. It must reproduce `pack_at` bit for bit, including the order-preserving layout and the interning fallback (`len > 9`, high-bit or zero bytes). Test with a property test against `pack()` over random byte strings of length 0–16.
- **On Ice Lake+, `vpmultishiftqb` (VBMI)** could replace the multiply-add cascade (its use in base64 is suggested by the AVX-512 paper's abstract, not verified here). Bounded gain once the AVX2 version exists.
- **Product → category:** at ~650K lookups per run (3% of 21.6M), even a 20 ns miss costs ~13 ms of CPU in total. Not worth a perfect hash. For the campaign lookup (2K entries, 5.1M clicks), make sure the table stays L1/L2-resident (2K × 16 B ≈ 32 KB).
- **Hashing for partitions:** `hash(id) >> shift` on the packed u64 costs ~3 uops. Hashing raw bytes before packing gains nothing, because the packed key is needed anyway for the reversible, order-preserving ID.

### Gaps
- No published benchmark of SIMD 7-bit packing for short identifiers; the base64 papers cover streams, not 9-byte keys.
- PtrHash's own paper was not fetched; its relevance here is low.
