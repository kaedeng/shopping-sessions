# Sorting, grouping, sessionization and attribution algorithms for the sessions stage

Scope: the per-bucket sessions stage of the Rust engine (1024 person buckets, about 19K events, 650 persons, 5K clicks and 3.3K sessions per bucket; one-core cost about 0.70 s, the key sort about 45% of it). Every idea below is tied to that design: `crates/sessions/src/partitioned.rs` (intern, key build `local << ts_bits | ts`, `sort_std` = `sort_unstable`, one-subtraction walk), `purchase_side.rs` (purchases side list), `attr_sweep.rs` (two-pointer window, sparse set, one `assist` call per active campaign per session) and `agg_dense.rs` (dense `[day][campaign]` cells, person-day accumulator).

Source depth: the Polychroniou & Ross, Axtmann et al. (IPS4o/IPS2Ra), Leis et al. and Traub et al. (EDBT 2019) papers were read in full text, extracted with `pdftotext`. LearnedSort 2.0, positional popcount and the parallel-DBSCAN paper were read as arXiv/ar5iv HTML. Items marked *(abstract-level)* come only from abstracts or search snippets. Numbers marked *measured here* come from a scratch benchmark written for this note (described under the first key question).

## Ranked list of ideas

| Rank | Idea | Effect (measured or estimated) | Confidence |
|---|---|---|---|
| 1 | **Coarse-key sessionization**: sort on `(person, ts / W)` with any W ≤ 30:00.001 ms (2^20 or 1,800,000), never on full `ts`. A 21-bit key means 2 counting passes, then a branch-free per-group min/max/count sweep, then a walk over groups. | Measured here: 6.3–7.2 ns/row, vs 8.4–8.9 for LSD-11 + walk and 14.1–14.3 for `sort_unstable` + walk. That is about −50 to −55% against the current path and −17 to −28% against LSD-11. Scaled to the user's harness, about −5 ns/row vs current (≈ −100 ms one-core, ≈ 14% of the stage) and about −1.4 ns/row vs LSD-11. | High (exact; checked) |
| 1b | **Time-major variant**: one counting pass by bin over the whole bucket, then a per-person "open slice" state machine (Scotty-style slices with first/last timestamps), then a small scatter of the slices by person. | Measured here: 6.5–7.1 ns/row. A tie with idea 1; its state sweep (about 4 ns/row) is the bottleneck. | High (exact; checked) |
| 2 | **Dense person numbers from upstream**: remove the per-event hash intern and the per-click `find`. | Estimated 2–3 ns/row of intern plus about 15 ms of click lookups, so about 8–10% of the stage. Not measured. | Medium (touches links/partition stages) |
| 3 | **Lazy (flush-on-change) assist accounting**: add `k − since[c]` when a campaign leaves the window or the session day changes, instead of +1 per active campaign per session. | Fewer than 36.9M increments, down to about Σ over person-days of the active set. Removes the variable-trip loop from every session. Estimated 2–4% of the stage. | Medium (exact; ratio not measured) |
| 4 | **AVX-512 segmented min/max for the group sweep**: a boundary mask from adjacent-key compares, a log-step segmented min/max, and `vpcompress` of group records. | The sweep costs about 1.9–2.2 ns/row today (measured here). Could reach below 1 ns/row (estimate), about 3% of the stage. | Low–medium (unmeasured) |
| 5 | **Click side**: a bucket-wide LSD on `(local, ts)` for clicks instead of per-person comparator sorts; or, for persons with ≤16 clicks, AVX-512 brute-force windows (compare masks, a masked max for credit, and person-local campaign bits with one masked add per session). | Unknown share of the stage, estimated 2–8%. | Low (unmeasured) |
| 6 | **"Last occurrence" (DQUERY) decomposition of assists**: each click covers the disjoint interval `[t_i, min(next same-campaign click, t_i + 7d + 1))`, so assists are range counts with no distinctness check. | An alternative to 3; only pays when combined with per-day range counts. | Low |
| — | Not worth it here (see the key questions): LearnedSort; IPS2Ra/IPS4o; AVX-512 general sorts; adaptive sorts (Timsort/Powersort, patience); integer-sorting theory; DABA/FiBA; segment trees; positional-popcount bit-sliced campaign counters; one global time bucketing across all persons; a fused single-pass branch-free sweep (measured slower). | — | — |

### 1. Coarse-key sessionization (top idea)
- **Mechanism.** Choose a bin width W with W − 1 ≤ GAP, so W ≤ 1,800,001 ms; 2^20 ms = 17.5 min, or 1,800,000 ms. Any two rows in one bin are then at most 30:00.000 apart and always share a session, so the order inside a bin is irrelevant. A break can only fall between two consecutive occupied bins of a person, and it falls there exactly when `min(next bin) − max(previous bin) > GAP`. This is the "maximum gap" pigeonhole argument and the DBSCAN grid-cell argument (cells with diameter ≤ ε are internally connected), applied in 1-D.
- **Pipeline.**
  1. The intern pass also computes `bin = (ts − event_lo) / W` and two histograms (bins, locals). It writes an 8-byte item `((local << bb | bin) << 32) | ts`.
  2. Run a stable counting scatter by bin (about 1,731 buckets at 2^20, 1,008 at 30 min), then one by local (about 650).
  3. A branch-free sweep keeps a running group min/max/count and always writes the open group's 16-byte record, advancing the index when the key changes.
  4. Walk the groups (about 0.19–0.21 per row) with the same gap test.
- **Facts the rest of the engine needs are preserved.**
  - Session start is the min of its first group, which gives the day and the attribution start.
  - Event count is a sum, and revenue and per-category revenue are sums, so order does not matter.
  - Sessions still come out per person in increasing start, which is what `Day` and `Attr` require.
- **Purchases.** Purchases can merge by the coarse group key, because every row of a (person, bin) group lies in one session. The item also has about 23 free bits, which could carry a purchase index and remove the separate side-list merge.
- **Why it applies here.**
  - Keys drop from about 41–42 bits (local + 31–32 ts bits) to about 20–21 bits, so 4 LSD passes become 2.
  - Clustering helps: 0.207 groups per row at 2^20 and 0.186 at 30 min (measured here), so the walk touches about one-fifth of the items.
  - Heavy persons become linear-time; there is no n log n for the 400K-event person.
- **Measured (single thread, this box).**
  - Phase breakdown at 2^20 bins (ns/row): build with 2 histograms 1.72; two scatters +2.1; group sweep +1.9; group walk +0.8. Total 6.60.
  - The branchless sweep beat a branchy one by about 0.36 ns/row.
  - Fusing the group sweep and the session walk into one branch-free pass was slower (7.7–8.1 ns/row): the select chain is latency-bound.
  - Skipping the zero-fill of the group buffer made no difference.
- **Risks and pitfalls.**
  - W must be ≤ GAP + 1. Use 2^20, or the exact constant 1,800,000 (the compiler turns that division into a multiply-shift).
  - Compute bin and person bit widths from `ctx` (`ts_end − event_lo`, person count); never hard-code 21 days.
  - Bins are relative to `event_lo` (`ts ≥ event_lo` already holds by contract).
  - The group walk's `min − prev_max` must be taken only within a person; compare the person first or use the 7-day-offset trick.
  - Purchases must use the same group key.
  - With 30-min bins aligned to UTC midnight, `day = first_bin / 48` comes for free (optional).
- **How to test.**
  - Bit-for-bit output equality with the current implementation on the dev set.
  - Adversarial generator cases: gaps of exactly 30:00.000 and 30:00.001 on both sides of bin edges and of UTC midnight; equal timestamps; one-row persons; the whale.
  - Time it in the user's harness (`sessions_bench`) next to LSD-11.

### 1b. Time-major counting pass plus per-person slices
- **Mechanism.**
  - Counting-sort the bucket once by bin only, then sweep in time order.
  - Each person keeps one open slice (bin, min, max, count). When a row of that person arrives in a new bin, the old slice is emitted, branch-free with an always-write. A person's slices are emitted in bin order.
  - A stable scatter of the slices (about 0.2 per row) by person, then the same group walk.
- **Why.** It answers the brief's question "could a global time-bucketing of all events replace per-person sorting?" Yes, inside a person bucket; there is no second pass over rows.
- **Measured.** 6.48–7.05 ns/row (best runs; 30-min bins 6.48). Phases: bin histogram + scatter 1.6; per-person state sweep +4.0, which is load–select–store chains through a 650-entry table plus the always-write group store; group scatter +0.5; walk +0.8.
- **Risk.** Same exactness rules as idea 1. It is slower than idea 1 when one person dominates a bin, because of the serial dependency through that person's state.
- **When to prefer.** When an extra 8-byte-per-row buffer matters, or once a register-cached "same person as last row" fast path cuts the state chain. That fast path is untested.

### 2. Dense person numbers from upstream (removes the intern hash)
- **Mechanism.**
  - The links/partition stage already maps every event's user to its person. If it emits a dense person number laid out so that each bucket's persons form a contiguous range (for example, persons numbered in person-hash order), then `local = person − bucket_base` and no hash probe is needed.
  - The record can shrink from 16 bytes to 8–12. Clicks get the same mapping, so `find` goes away.
  - The output keeps a `local → Id` table per bucket.
- **Effect.** Estimate only: per-event hashing plus a linear-probe loop costs about 8–10 cycles, i.e. 19.56M × 2.5–3 ns ≈ 50–60 ms, plus about 15 ms for 5.1M click lookups. Not measured.
- **Risks.** It is a cross-stage change, and bucket load balance must be kept (number persons in hash order).

### 3. Lazy assist accounting per person-day
- **Mechanism.**
  - Keep `k` = sessions so far in the current day. Each active campaign holds `since[c]`, the session index at which it was activated, reset at each day change.
  - When campaign c drops out of the window: `assisted[day][c] += k − since[c]`.
  - On a session day change: flush every active campaign into the old day, then reset `since` and `k`.
  - At person end: flush.
  - Activation sets `since[c] = k`.
  - Process the day change before the window update for the new session.
- **Effect.**
  - Increments drop from Σ over sessions of |active| (36.9M) to roughly Σ over person-days of |active| plus deactivations. The global average is 3.40M sessions / 2.24M person-days = 1.52, so a uniform spread would cut about one-third; heavy persons with many sessions per day cut much more.
  - It also removes the per-session loop over the active set: one variable-trip loop exit per session, about 3.4M mispredict-prone exits.
  - The day row is 2,000 × 4 B = 8 KB and lives in L1, so increments are cheap. The estimated gain is about 15–25 ms one-core.
- **Test.** Count Σ over sessions of |active| against the number of distinct (person, day, campaign) triples on the dev set; compare outputs bit for bit.

### 4. AVX-512 segmented reduction for the group sweep
- **Mechanism.**
  - For 8 rows of 64-bit items per 512-bit register (or 16 lanes of separated key/ts arrays), build a mask where `key[i] != key[i−1]`.
  - Run a log-step segmented min and max: 3–4 masked permute + min steps that do not cross the boundary mask.
  - `vpcompressd` the lanes that end a segment into the group array.
  - Carry the open segment across registers.
- **Why.** The scalar branch-free sweep costs about 1.9–2.2 ns/row (measured here) and is now the largest single phase after the two scatters.
- **Risks.**
  - Complexity, and AVX-512 frequency behaviour on the target Xeon.
  - Rust AVX-512 intrinsic availability on the chosen toolchain must be verified.
  - Unmeasured.

### 5. Click ordering and small-person attribution without sorts
- **Bucket-wide click LSD.**
  - Mechanism: replace per-person `sort_unstable_by(click_order)` with one bucket-wide LSD on `(local:10 | ts:32)` plus a click index, then a rare tie fix-up pass for equal `ts` by `click_id` (as `sort_clicks` already does for long runs).
  - Effect (estimate): about 4 passes × 1.2 ns over 5.1M clicks, ≈ 25 ms, against small comparator sorts for 667K persons (unmeasured).
- **Brute force for persons with ≤16 clicks.**
  - Credit: broadcast the session start, run two `vpcmpud` for `[s − 7d, s]`, then a masked max over a precomputed `(ts, reverse click_id rank)` key.
  - Assists: OR-reduce the person-local campaign bits of the in-window lanes, then one masked add into a 16-lane per-day counter vector. Flush it at day change, which ties in with idea 3.
  - This removes the click sort and the two-pointer loops (and their mispredicts) for most persons. Unmeasured.
  - Risk: exact `click_id` tie order; persons above 16 clicks need the old path.

### 6. Last-occurrence decomposition of assists (DQUERY idea)
- **Mechanism.**
  - Campaign c is active at s exactly when the latest click of c at or before s is at or after s − 7d.
  - Click i is the latest such click for sessions with start in `[t_i, next_i)`, where `next_i` is the next click of the same (person, campaign).
  - So click i alone covers starts in `[t_i, min(next_i, t_i + 7d + 1))`. These intervals are disjoint per campaign, so assists = Σ over clicks of #sessions in the interval, split by day.
- **Use.** Lets assists be computed as per-click range counts over a person's sorted session starts, with no distinctness check. It is no fewer operations than idea 3 unless sessions per day are many. Unmeasured.

## Q1. Sorting faster than LSD radix: learned/CDF sorts, MSD/IPS2Ra, write-combining, AVX-512 sorts, key compression, adaptive sorts

### Takeaway
The fastest "sort" here sorts fewer bits. Session rules never need order inside a ≤30:00.001 ms bin, so the key drops from about 41 to about 21 bits and from 4 LSD passes to 2. Measured here, that runs about 25% faster than LSD-11 and about 2.1× faster than `sort_unstable`, including the walk. The published fast general sorts (LearnedSort, IPS2Ra, SkaSort) report their wins on arrays far larger than an L2-resident 19K-row bucket, and none of them can exploit "order only to 30-minute granularity".

### Cited Findings
- **Measured here, coarse key vs exact sorts.** Single thread, AMD Ryzen 7 7800X3D (Zen 4), Windows 10, rustc 1.92 with `-C target-cpu=native`. Synthetic data: 20,602,916 rows, 1,024 buckets (median 18,729 rows, max 118,000), 667,000 persons (lognormal body with median 14, plus a 1% Pareto tail ≥256 capped at 400K), 3,209,043 sessions. Best of 7–9 rounds, range over 3 runs, in ns/row:
  - copy only: 0.34–0.37
  - walk on already-sorted keys: 1.08–1.23
  - `sort_unstable` + walk: 14.08–14.31
  - LSD 11-bit (4 passes) + walk: 8.41–8.93
  - coarse LSD with 2^20 bins: 6.60–6.99
  - coarse LSD with 30-min bins: 6.34–7.15
  — [measured here: scratch benchmark `coarsebench/src/main.rs`](C:/Users/Shawn/AppData/Local/Temp/claude/e2243e7e7423ffa4/059307ec-51b9-42e4-965d-09c915346a0e/scratchpad/coarsebench/src/main.rs)
- **Measured here, exactness.** Every variant produced the same session count and the same order-independent checksum of (person, session start, event count) over all 3.2M sessions. A further 200,000 random tiny buckets also matched the `sort_unstable` reference: 1–4 persons, offsets on k·30 min + {0, 1, 2} ms, on 2^20 and 30-min bin edges ±1, and equal timestamps. Hand cases: exactly 30:00.000 joins; 30:00.001 splits. — [measured here](C:/Users/Shawn/AppData/Local/Temp/claude/e2243e7e7423ffa4/059307ec-51b9-42e4-965d-09c915346a0e/scratchpad/coarsebench/src/main.rs)
- **Measured here, coarse LSD phases.** Build with 2 histograms 1.72; two counting scatters about 2.1 (about 1.05 per pass); branch-free group sweep about 1.9; group walk about 0.8 ns/row. Groups per row: 0.207 at 2^20 bins, 0.186 at 30-min bins. — [measured here](C:/Users/Shawn/AppData/Local/Temp/claude/e2243e7e7423ffa4/059307ec-51b9-42e4-965d-09c915346a0e/scratchpad/coarsebench/src/main.rs)
- **Radix passes depend on key bits.** The number of radix-sort passes "is dependent on the number of bits required to encode the distinct values in the array", which is why order-preserving compression speeds up radix sort. — [Polychroniou & Ross, SIGMOD 2014](https://www.cs.columbia.edu/~orestis/sigmod14I.pdf)
- **Partitioning fanout and buffering (Polychroniou & Ross).** On 4× Xeon E5-4620 (Sandy Bridge, 2.2 GHz, 32 KB L1D, 256 KB L2): unbuffered partitioning running out of cache has its best fanout at 5–6 bits. Buffered (software write-combining) variants raise the best fanout to 10–12 bits when non-in-place. They also state that "using out-of-cache variants for small cache-resident array sizes incurs unnecessary overheads." — [Polychroniou & Ross, SIGMOD 2014](https://www.cs.columbia.edu/~orestis/sigmod14I.pdf)
- **MSB vs LSB (Polychroniou & Ross).** Their MSB radix sort stops early: once a part fits in cache it makes n/4 to n/8 partitions and insertion-sorts 4–8 items. For 32-bit keys, MSB is 10–20% slower than the fastest LSB. For 64-bit keys, MSB is faster than LSB "since it stops partition passes earlier". — [Polychroniou & Ross, SIGMOD 2014](https://www.cs.columbia.edu/~orestis/sigmod14I.pdf)
- **IPS2Ra vs SkaSort.** Sequential I1S2Ra (the in-place radix variant of IPS4o) is on average 2.06, 2.13, 1.12 and 0.82 times as fast as SkaSort for n ≥ 2^15 (the relation symbol was garbled in the PDF text; read as ≥) on the paper's machines A1x16, A1x64 (AMD EPYC Rome 7702P), I4x20 (4× Xeon Gold 6138) and I2x16 (2× Xeon E5-2683 v4). On the Intel machines the advantage is small or negative. SkaSort incurs 1.25× more cache misses and 1.52× more branch mispredictions. — [Axtmann et al., arXiv 2009.13569](https://arxiv.org/pdf/2009.13569)
- **IPS2Ra vs IPS4o; Timsort.** I1S2Ra beats I1S4o by 1.10–1.52× at the largest sizes. Timsort is "in most cases even more than a factor of three" slower than I1S4o; it wins only on almost-sorted input. — [Axtmann et al., arXiv 2009.13569](https://arxiv.org/pdf/2009.13569)
- **LearnedSort 2.0 design.** It samples 1% of keys to train a two-layer CDF model with 1,000 leaves. It makes two 1,000-way partition passes, then a model-based counting sort and an insertion-sort fix-up. — [Kristo, Vaidya, Kraska 2021 (ar5iv)](https://ar5iv.arxiv.org/html/2107.03290)
- **LearnedSort 2.0 numbers.** On a Xeon Gold 6150 it averages 4.78× faster than the original on high-duplicate data and 1.60× on low-duplicate data. On Normal data from 1M to 1B doubles it is 65% faster than I1S4o and 95% faster than Radix Sort on average. Below cache size it can be up to 45% slower than the original. The 2020 version was about 30% faster than I1S4o, 49% faster than Radix Sort and 238% faster than `std::sort`. — [Kristo, Vaidya, Kraska 2021 (ar5iv)](https://ar5iv.arxiv.org/html/2107.03290)
- **BlockQuicksort.** Removing data-dependent branches from partitioning gave an 80% speedup over GCC `std::sort` on random integers. The pdqsort paper reports 50–80% from the same technique for small integers. — [Edelkamp & Weiß, ESA 2016 (abstract-level)](https://arxiv.org/abs/1604.06697v2); [Peters, pdqsort (abstract-level)](https://arxiv.org/pdf/2106.05123)

### Inferences
- **Projected gain on the user's harness.** Here, LSD-11 takes 0.57 of the time of `sort_unstable` (sort time only, after subtracting copy and walk). The user measured 4.9–5.1 against 8.2–9.0, i.e. 0.58, so the machines scale alike and this box runs about 1.45× slower. Scaling gives:
  - coarse key ≈ 4.3 ns/row for sort + walk;
  - current `sort_unstable` path ≈ 9.3;
  - LSD-11 path ≈ 5.7.
  That is about −5 ns/row (≈ −100 ms one-core, ≈ 14% of the 0.70 s stage) against the current code, and about −1.4 ns/row against an LSD-11 switch. If the in-pipeline sort really costs about 16 ns/row (45% of 0.70 s / 19.56M), the gain against the current code is larger.
- **Bin width.** 2^20 bins (11 bits) and 30-min bins (10 bits) perform alike. 30-min bins give 10% fewer groups, and 7 days is exactly 336 of them, which could later help a coarse click window.
- **Learned, IPS2Ra, SkaSort and AVX-512 sorts.** All are built to produce exact order on large arrays. Exact order is not needed here, and a 2-pass counting sort over 19K items is already near the cost of two streaming passes, so I expect no gain. vqsort, x86-simd-sort, Bramas and Gueron–Krasnov were already checked by the user; I found nothing that changes this.
- **Software write-combining.** It is not needed inside the stage: bucket arrays are L2-resident (19K × 8 B ≈ 150 KB), which Polychroniou & Ross say makes the buffered variants pure overhead.
- **Adaptive sorts.** Timsort, Powersort and patience sort gain only from runs. The input is uniformly shuffled, so they have nothing to use.
- **Integer-sorting theory.** Han, Thorup and packed sorting have no practical edge at n ≈ 2^14 to 2^15 with 21-bit keys, where counting sort is already linear with a tiny constant.

### Gaps
- None of these numbers come from the scoring hardware (Intel Xeon, AVX-512, 4 cores × 2 SMT). The gain on Intel and under 8 threads sharing L1/L2 per physical core needs re-measuring in the user's harness and in Docker.
- I found no published measurements of LearnedSort or IPS2Ra at n ≈ 2^14 to 2^15 for 64-bit keys. IPS4o's plots start around 2^8–2^12, but I did not extract per-size numbers.
- My benchmark reuses the same data each round; Lemire notes that predictors can learn repeated data. The arrays are 20M rows, which is probably far too long to memorise, but this was not verified.

## Q2. Avoiding the sort entirely: per-person bins, bitmaps, two-level bins, minute-slot hashing, global time bucketing

### Takeaway
Exact sessionization needs only (person, bin) grouping, where a bin is at most 30:00.001 ms wide, plus each group's min and max. Done batch-wide by counting sorts, it beats both the per-person bin maps and the hash-table binning the user already tried. A time-major bucket-wide pass with per-person slices is also exact and about equally fast. A truly global (all-persons) time bucketing would lose locality.

### Cited Findings
- **Maximum gap.** The linear-time maximum-gap method takes "the max of some bucket and the min of the next nonempty bucket". The result is attributed to Gonzalez 1975 via Preparata & Shamos p. 261; that attribution is secondhand. — [Eppstein, 11011110 blog (abstract-level)](https://11011110.github.io/blog/2007/06/18/cute-problem-related.html)
- **Grid-based DBSCAN cells.** The cells have side ε/√d. "All points inside a cell are within a distance of ε from each other, and will belong to the same cluster in the end." Sorting points by cell ID with a comparison sort "is not work-efficient", so the authors semisort, grouping without ordering. Tested on 2× Xeon Platinum 8124M. — [Wang, Gu, Shun, SIGMOD 2020 (ar5iv)](https://ar5iv.labs.arxiv.org/html/1912.06255); [Gan & Tao, SIGMOD 2015 (abstract-level)](https://khoury.northeastern.edu/home/vip/teach/DMcourse/2_cluster_EM_mixt/notes_slides/sigmod15-dbscan.pdf)
- **Measured here, time-major.** One counting pass by bin over the whole bucket, per-person open slices, then a scatter of the slices by person: 6.48–7.05 ns/row, exact. Its per-person state sweep alone costs about 4.0 ns/row. — [measured here](C:/Users/Shawn/AppData/Local/Temp/claude/e2243e7e7423ffa4/059307ec-51b9-42e4-965d-09c915346a0e/scratchpad/coarsebench/src/main.rs)
- **Earlier prototype.** The repo's prototype `sessions_binned` already tried a per-bucket open-addressing hash table of (person, 30-min bin) holding min/max/count, followed by a sort of the bins. — [prototypes/sortbench/src/main.rs](../../prototypes/sortbench/src/main.rs)

### Inferences
- **Why this beats the earlier binning attempts.** Per-person bin maps (the user's 6.1 ns/row for persons with ≥256 events) and hash binning do a random read-modify-write and a probe or branch per row. Counting scatters are streaming and branch-free, and they amortise setup across the bucket's roughly 650 persons. Tiny persons get no per-person setup at all.
- **Two-level bins and occupancy bitmaps.** Neither is needed. With bins of at most 30 min, non-adjacent occupied bins always split, and adjacent ones need one min/max compare. A bitmap only replaces the second scatter for very dense persons.
- **Global time bucketing across all 667K persons.** The per-person state would be about 667K × 16 B ≈ 10.7 MB of random accesses (out of L2), which would undo the bucket locality that makes everything else cheap. Keep time-major ordering inside person buckets only.
- **Prefer the two-pass coarse LSD (idea 1).** Its sweep keeps the running group in registers. The time-major state machine chains each person's state through memory, which costs about 4 ns/row. Take time-major only if the state chain can be shortened, for example with a "same person as the previous row" register fast path.

### Gaps
- Not measured: a version with 23 free bits per item carrying purchase indices (which folds the purchase merge in), and a register fast path for the time-major sweep.
- I did not measure the "always-write vs branch" choice under SMT, where a sibling thread can hide misprediction stalls.

## Q3. Stream-processing window/session research (Scotty, Cutty, Flink/Beam sessions, DABA/FiBA) applied to batch

### Takeaway
Stream slicing already contains the key fact: session windows can be built from slices that keep only partial aggregates plus first/last timestamps, even with out-of-order input, provided the aggregates are commutative. The bins of idea 1 are exactly such slices. DABA/FiBA solve non-invertible sliding aggregates, which this problem does not need: counts are invertible and window ends are monotone, so the existing two-pointer is already amortised O(1).

### Cited Findings
- **Slice metadata and merge cost.** A slice stores t_start, t_end, t_first and t_last (its first and last tuple timestamps). A merge sets `t_end(A) ← t_end(B)` and `a ← a ⊕ b`, which costs constant time for sum, min and avg. — [Traub et al., EDBT 2019](https://www-live.dfki.de/fileadmin/user_upload/import/10263_traub-efficient-window-aggregation-with-general-stream-slicing-edbt-2019.pdf)
- **Out-of-order streams and sessions.** With out-of-order streams, tuples must be kept only when the aggregate is non-commutative, or when the window is context-aware and not a session window. "Session windows are an exception, because they are context aware, but never require recomputing aggregates." General stream slicing "outperforms alternative concepts by up to one order of magnitude". — [Traub et al., EDBT 2019](https://www-live.dfki.de/fileadmin/user_upload/import/10263_traub-efficient-window-aggregation-with-general-stream-slicing-edbt-2019.pdf)
- **Scotty (ICDE 2018 / TODS).** Slicing for session windows depends only on the session window with the smallest gap. The open-source Scotty supports tumbling, sliding and session windows. — [Traub et al., Scotty ICDE 2018 (abstract-level)](https://www.dfki.de/fileadmin/user_upload/import/9719_traub-scotty-icde-2018.pdf); [Scotty TODS record (abstract-level)](https://research.tudelft.nl/en/publications/scotty-general-and-efficient-open-source-window-aggregation-for-s)
- **DABA.** Worst-case O(1) per insert, evict and query for FIFO windows, with operators that need only be associative (not invertible or commutative). FIFO in-order windows only. — [Tangwongsan, Hirzel, Schneider, arXiv 2009.13768 (abstract-level)](https://arxiv.org/abs/2009.13768v1)

### Inferences
- **Slices map onto bins.** A (person, bin) group is a slice with t_first and t_last; a session is a run of slices joined by "gap ≤ 30 min". All the outputs are commutative sums (counts, revenue, per-category sums), so no tuple order inside a slice is needed. This is the stream-processing justification for idea 1. The batch version simply orders the slices by counting sort, not by a slice manager.
- **FiBA, DABA and segment trees.** The attribution window has monotone ends, and its per-campaign counts are invertible. The latest click is always `clicks[hi − 1]` after the ascending sort. So these structures add nothing. Cutty and Flink/Beam session merging are per-key versions of slice merging; I found nothing extra that applies.

### Gaps
- I did not read Cutty (Carbone et al. 2016), the FiBA full text, or the Flink/Beam session-merging code; they are not needed for the conclusion.

## Q4. Attribution: latest click in [s − 7d, s] and per-campaign assists with fewer operations

### Takeaway
The credit (latest click) is already O(1) per session. The assist work can shrink by moving from per-session to per-person-day accounting (lazy "since" counters) or to disjoint per-click coverage intervals (the DQUERY last-occurrence trick). SIMD brute force with person-local campaign bitmasks could remove the click sort and the window loops for the many persons with few clicks. Dense bit-sliced counters over 2,000 campaigns look like a loss, because active sets average about 10.8 of 2,000 campaigns, about 0.5% density.

### Cited Findings
- **DQUERY last-occurrence trick.** In distinct-count queries ("DQUERY"), only the rightmost occurrence of each value is counted. An offline sweep keeps one active mark per value at its latest position, and a range sum counts the distinct values: O((N + Q) log N). — [CSES Distinct Values Queries notes (abstract-level)](https://mtuann.github.io/competitive-programming-cpp/practice/ladders/data-structures/offline-tricks/distinctvaluesqueries/); [Codeforces blog 120313 (abstract-level)](https://codeforces.com/blog/entry/120313)
- **Positional popcount.** Carry-save adders built from two `vpternlogd` per 512-bit step reach 0.36–0.47 cycles per 16-bit word at 256 MB (14–18 GB/s) on an i3-8121U (Cannon Lake). At 8 KB they need 1.2–1.5 cycles per word, and the method pays only from a few thousand words up. With one-hot words the per-bit sums are a frequency histogram. — [Klarqvist, Muła, Lemire (ar5iv)](https://ar5iv.arxiv.org/html/1911.02696); follow-up [Clausecker, Lemire, Schintke 2024 (abstract-level)](https://arxiv.org/abs/2412.16370)
- **Code fact.** The engine's `assists(day)` closure increments a 2,000-entry u32 row per call, one call per active campaign per session (about 8 KB per day row). — [crates/sessions/src/agg_dense.rs](../../crates/sessions/src/agg_dense.rs); [crates/sessions/src/attr_sweep.rs](../../crates/sessions/src/attr_sweep.rs)

### Inferences
- **Lazy per-person-day accounting (idea 3)** is exact and simple. The work becomes about the number of (person, day, campaign) triples, with deactivations amortised over clicks, instead of 36.9M (session, campaign) pairs. With 1.52 sessions per person-day on average, expect a cut of at least about 34% if assists were spread evenly; heavy persons with many sessions per day give more. Because the day row is L1-resident, the gain is mostly the removed per-session loop and its exit mispredict: an estimated 15–25 ms one-core.
- **Last-occurrence form (idea 6).** Campaign c assists session s iff c's latest click at or before s is at or after s − 7d. So click i covers starts in `[t_i, min(next_same_campaign_i, t_i + 7d + 1))`. These intervals are disjoint per (person, campaign), so assists(c, d) is a sum of range counts over the person's sorted starts, with no distinctness check. It suits batching or SIMD, but its increment count is no lower than idea 3.
- **Bit-sliced counters.** The CSA/positional-popcount approach does fixed work for all 2,000 lanes per session to count about 11 set bits, and per the paper it only wins beyond about 8 KB of dense input. Expect a loss, except perhaps for a few extreme persons with dense active sets.
- **AVX-512 small-person path (idea 5).**
  - Person-local campaign numbering: most persons have few clicks (5.1M clicks / 667K persons ≈ 7.6 on average).
  - Per session: two compares give the in-window mask; a masked max gives the credit; an OR-reduce gives the active local-campaign mask, then one masked vector add into a per-day 16-lane counter, flushed at day end.
  - That is about 20 branch-free instructions per session, with no click sort and no two-pointer loop.
  - Pitfall: equal-`ts` ties must order by `click_id` bytes. Precompute a per-person rank key (ts, reverse-id rank) so that one max is exact.

### Gaps
- I found no published measurements of these attribution variants. The current per-session attribution cost was not profiled; the stage share estimates above are mine. Instrument `attr.session`, `counters.*` and `sort_clicks` before building any of them.
- I did not find a specific Codeforces/AtCoder problem that matches "distinct colours among points in a time window, counted per query day" more closely than DQUERY.

## Q5. Exploiting skew; how real engines handle per-partition sorting

### Takeaway
Real window operators do what this engine already does: hash-partition into about 1,000 groups, then sort each group. Their own data shows the sort dominates and gets worse with larger partitions. The coarse-key method makes the per-row cost almost independent of person size, so heavy persons stop being special. The remaining skew issue is scheduling the whale bucket early, not choosing its algorithm.

### Cited Findings
- **Window operator phases.** The HyPer window operator partitions into 1,024 thread-local hash groups, then sorts each group by the partition and order keys. In theory the hash approach is O(n), against O(n log n) for sort-only. — [Leis et al., PVLDB 2015](https://www.vldb.org/pvldb/vol8/p1058-leis.pdf)
- **Where the time goes.** Most query time is in sorting and partitioning. More partitions mean higher throughput, because sorting larger partitions is "significantly more expensive ... due to both asymptotic and caching reasons". 64-byte tuples run 1.6× slower than 16-byte tuples. Hardware: Core i7-3930K, 6 cores / 12 threads, 3.2 GHz. — [Leis et al., PVLDB 2015](https://www.vldb.org/pvldb/vol8/p1058-leis.pdf)
- **Skew.** With 50% of tuples in the largest partition, parallelism across partitions alone gave a 1.9× speedup. Adding parallelism within partitions for large groups gave 5.9×. — [Leis et al., PVLDB 2015](https://www.vldb.org/pvldb/vol8/p1058-leis.pdf)
- **Branch mispredictions (Lemire).** On Skylake, a loop writing only the odd values of a random array cost about 15 cycles per value, against 3.3 for writing all values. A misprediction costs over 10 cycles. Predictors learn repeated data (48% → about 2% misses), which makes small benchmarks misleading. — [Lemire 2019 (blog mirror; abstract-level)](https://eklausmeier.neocities.org/lemire/blog/2019/10-15-mispredicted-branches-can-multiply-your-running-times); [Lemire 2019 follow-up (abstract-level)](https://eklausmeier.neocities.org/lemire/blog/2019/10-16-benchmarking-is-hard-processors-learn-to-predict-branches)
- **Measured here, branchless vs branchy.** A branch-free always-write group sweep beat a branchy one by about 0.36 ns/row (6.99 vs 7.35 total). A fully fused branch-free single pass was slower (7.7–8.1) because of its dependency chain. — [measured here](C:/Users/Shawn/AppData/Local/Temp/claude/e2243e7e7423ffa4/059307ec-51b9-42e4-965d-09c915346a0e/scratchpad/coarsebench/src/main.rs)

### Inferences
- **Skew no longer needs separate paths.** With counting sorts on coarse keys, a 400K-event person costs about the same per row as a 3-event person. Tiny-vs-heavy code paths, SIMD lanes of tiny persons, and insertion sort ≤32 become unnecessary for sessionization. A 400K-row whale bucket costs about 3 ms, against roughly 17 ms of work per thread (19.56M rows / 8 threads × about 7 ns), so it only needs to be scheduled first.
- **Skew still matters for attribution.** The ≤16-click SIMD path, and lazy accounting that helps most for persons with many sessions per day, are where size-specific code still pays.
- **Keep the 1,024-bucket partitioning.** Leis et al.'s numbers support many small partitions. The sessions stage also benefits from the 8-byte items of idea 1, which halve the per-thread L2 footprint, relevant when two SMT threads share a core's L2.
- **Split rather than fuse.** Branch-free code pays when it breaks a random branch (about 21% of rows start a new group), but long select chains become latency-bound. Separate passes that each stream through memory were the better shape on this box.

### Gaps
- I did not obtain DuckDB or Umbra window-operator internals or numbers (for example, segment-tree use and radix sort on normalised keys).
- I did not measure the dense-ID upstream change (idea 2). Its size estimate (≈ 50–75 ms one-core) is mine and should be checked by timing `intern` and `gather_clicks` alone in `sessions_bench`.
