# Shopping Sessions: Polars (Rust) starter

A Rust Polars skeleton, ported from the Python Polars starter, that reads the inputs and writes both output tables with the right schema. Both tables are empty, so the checker marks it incorrect until you implement `rules.md`. If this and `rules.md` disagree, `rules.md` wins.

- `src/main.rs` holds the pipeline. The TODO lists the steps in order.
- `Cargo.toml` pins `polars` 0.55 (features `lazy`, `parquet`, `dtype-date`). Add features as you need them (`strings`, `regex`, `is_in`, `rank`, ...).
- `Dockerfile` is a two-stage build: `rust:1.98-slim-trixie` compiles, `debian:trixie-slim` runs. Dependencies get their own layer, so editing `src/` doesn't recompile Polars.
- `run.sh` is the entry point. It points every temp and spill directory at `/scratch` and sets `POLARS_MAX_THREADS` from `NCPU`.
- `scripts/` and `Makefile` are the Python starter's, unchanged except for paths. `make native` and `make fixtures` read the 36 fixtures from `fixtures/`.

## Run and test

The test scripts need Python 3.10+ with `pip install polars==2.1.0`.

```sh
make native                          # cargo build --release, then all 36 fixtures without Docker
make fixtures                        # the same through the image, with scorer flags
make build run-dev PLATFORM=linux/arm64   # native image on Apple silicon; check amd64 before submitting
make check-dev                       # diff out/ against dev-answer/
```

Building amd64 on Apple silicon emulates rustc and compiling Polars takes a long time. Build time is never timed, only the run.

## Pitfalls

The Python starter's pitfalls apply (its CSV reader is not `rules.md`'s dialect; `\d` is not `[0-9]`; 24 GB, no swap). Rust-specific:

- **Spill directory.** Polars 0.55's streaming engine spills to `/var/tmp` by default, ignoring `POLARS_TEMP_DIR`; the root is read-only, so `run.sh` sets `POLARS_OOC_SPILL_DIR`.
- **Threads.** `POLARS_MAX_THREADS` is read once, when the pool first starts. Set it in `run.sh`, not mid-program.
- **CPU target.** The scorers have AVX-512, but `target-cpu=native` in the image would tune for the build host. Pin `-C target-cpu=cascadelake` for amd64 only if you want it.
