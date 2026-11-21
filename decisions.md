# Decisions

Tick a box once we're certain.

- [x] **CSV parser:** in-house (`memmap2` + `memchr` + `rayon`), not arrow-csv or the Polars reader
- [ ] **Hot path:** plain Rust over ~1 MB chunks cut at `\n` inside each file; fixed 32-byte records; an ID is a `u64` (up to 9 ASCII bytes, 7 bits each, order-preserving), longer or non-ASCII IDs interned on a slow path
- [ ] **Output:** one Parquet file per worker with arrow-rs (`parquet` crate `ArrowWriter`), uncompressed, no statistics, no dictionary; no Polars
- [ ] **Links:** union-find, the larger root linked under the smaller so the root is the person's name; built on one thread while the others parse; rows get their person after dedup, in one tight loop per partition against the read-only link table, prefetching 64 rows ahead
- [ ] **Event dedup:** scatter by `event_id` hash into 1,024–2,048 partitions inside the parse loop, dedup each with a small table, keep min `ingest_ts`; validity, including the late check, is one bit set at parse time, so an invalid kept copy is a tombstone
- [ ] **Click dedup:** skip it (credit = max, assists = set, copies identical)
- [ ] **Sessionize:** scatter survivors by person hash into ~1,024 buckets, `sort_unstable` each on packed `u64` (bucket-local person, ts); radix or vqsort only if profiled
- [ ] **Timestamps:** date-string lookup table from window start − 8 days to window end + 1 day, a miss drops the row; `ingest_ts` falls back to plain conversion; no calendar code on the hot path
- [ ] **SIMD:** mask-based field tokenizer from day one (AVX2 on the scorer, NEON for local runs), `memchr` only to cut chunks at `\n`; everything else only where a full-size profile points
- [ ] **Layout:** Cargo workspace, one crate per stage (shared types, ingest, links, dedup, sessions, output, the binary); the per-row parse loop stays inside one crate; every alternative in `alternatives.md` sits behind its own Cargo feature, features only add code, and an env var picks when a slot has several compiled in; LTO on so crate boundaries cost nothing
- [ ] **Parallelism:** 8 workers (from `NCPU`, never more: `--cpus 8` is a time quota), each owning its buffers and pulling tasks largest-first from a shared counter; every stage is data-parallel (file chunks, dedup partitions, person buckets, one output file per worker) with two barriers (after parse, after dedup); the union-find is the only serial job and runs beside the parse
