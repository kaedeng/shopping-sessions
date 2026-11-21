# Shopping Sessions: custom Rust engine

Reads the inputs under `$DATA_DIR` and writes `user_daily/` and `campaign_daily/` as Parquet under `$OUTPUT_DIR`, following `rules.md`. If this and `rules.md` disagree, `rules.md` wins.

- `decisions.md` holds the design pick per slot, `alternatives.md` every option and measurement, `implementation.md` how the picks are built: the crates, the feature and env var of every slot, the record layouts, the limits.
- `src/main.rs` is the pipeline: three parallel stages (parse, dedup and person scatter, sessions and output). Each stage's code is a crate under `crates/`.
- `Dockerfile` is a two-stage build: `rust:1.98-slim-trixie` compiles, `debian:trixie-slim` runs. `.cargo/config.toml` compiles amd64 for `x86-64-v3` (AVX2, BMI2).
- `run.sh` is the entry point. The binary needs no temp directory and takes its thread count from `NCPU`.
- `generator/` writes datasets; `prototypes/` are the throwaway benchmarks behind `alternatives.md`.

## Run and test

The test scripts need Python 3.10+ with `pip install polars==2.1.0`.

```sh
make native                          # cargo build --release, then all 36 fixtures without Docker
make fixtures                        # the same through the image, with scorer flags
make build run-dev PLATFORM=linux/arm64   # native image on Apple silicon; check amd64 before submitting
make check-dev                       # diff out/ against dev-answer/
SS_TIMING=1 DATA_DIR=dev OUTPUT_DIR=out target/release/shopping-sessions   # per-stage wall time
```

`make dev` needs `generate` on the PATH: `cargo build --release` in `generator/`.

## Benchmarking an alternative

Every alternative is a Cargo feature that only adds code, and each slot with more than one compiled in has an env var that picks at start (the table is in `implementation.md`):

```sh
cargo build --release --features tok-scalar
SS_TOK=scalar SS_TIMING=1 DATA_DIR=full OUTPUT_DIR=out target/release/shopping-sessions
```
