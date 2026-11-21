# Shopping Sessions: custom Rust engine

Reads the inputs under `$DATA_DIR` and writes `user_daily/` and `campaign_daily/` as Parquet under `$OUTPUT_DIR`, following `rules.md`. If this and `rules.md` disagree, `rules.md` wins.

- `decisions.md` holds the design pick per slot, `alternatives.md` every option and measurement, `implementation.md` how the picks are built: the crates, the feature and env var of every slot, the record layouts, the limits.
- `x86.md`: how the M4 Mac, the x86 test box and the scorer's Xeon differ, what was measured on x86, and the procedure for timing there. Read it before deciding anything on Mac numbers alone.
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

Without local Polars or `make` (Windows, Git Bash), Docker does it all:

```sh
IMAGE=shopping-sessions:latest sh scripts/check_docker.sh   # the 36 fixtures and dev/ against dev-answer/, in a container with Polars
py -3 scripts/bench_docker.py -n 6 shopping-sessions:base shopping-sessions:latest   # scorer flags, a fresh container per run
```

`.gitattributes` keeps `*.sh` and the Dockerfiles LF and the `*.csv` and `*.json` files byte-exact on any `core.autocrlf`. A CRLF `run.sh` breaks the image's entry point, and a converted `\r` breaks the fixtures; `x86.md` has the repair for an older clone.

## Benchmarking an alternative

Every alternative is a Cargo feature that only adds code, and each slot with more than one compiled in has an env var that picks at start (the table is in `implementation.md`):

```sh
cargo build --release --features tok-scalar
SS_TOK=scalar SS_TIMING=1 DATA_DIR=full OUTPUT_DIR=out target/release/shopping-sessions
```

## Benchmarking and profiling

`make profiling` builds `target/profiling/shopping-sessions`: the release build plus symbols, line tables and the `trace` feature. It times the same as release. Native targets read `DATA` (default `full`).

| What | Mac (16 KB pages) | Linux VM (4 KB pages, glibc, like the scorer's kernel) |
| --- | --- | --- |
| Variants in turns, median per stage, CPU time, page faults, peak RSS | `make bench BENCH="A 'SS_X=y B'"` | `make linux-bench ARGS="/app/shopping-sessions 'NCPU=4 /app/shopping-sessions'"` |
| CPU flamegraph | `make samply`: Firefox Profiler with flame graph, call tree, thread tracks, assembly | `make linux-flame`: `prof/linux-cpu.svg` and the top functions in `prof/linux-cpu.txt` |
| Hot instructions | samply's assembly view (double-click a function) | `make linux-annotate` after `make linux-flame`: `prof/linux-annotate.txt`, hottest function first |
| Syscalls | `make systrace-mac` (Instruments System Trace, no sudo) | `make linux-syscalls`: `strace -f -c` and `perf stat` |
| Page faults by code | `make systrace-mac` | `make linux-faults`: `prof/linux-faults.svg` |
| Per-task timeline, a track per worker | `make trace`, then open `prof/trace.json` in ui.perfetto.dev | `make linux-trace` |
| Cold page cache | `python3 scripts/bench.py --prepare 'sudo purge' target/release/shopping-sessions` | `make linux-bench ARGS="--prepare 'echo 3 > /proc/sys/vm/drop_caches' /app/shopping-sessions"` |

On an x86 machine (the Ryzen box: Windows, Docker Desktop on WSL2), `x86.md` has the equivalents:

- variants in turns with `scripts/bench_docker.py`, against the 20M-event volume `ss-p20`;
- `scripts/profile_linux.sh` with `PLATFORM=linux/amd64`, which profiles the scorer's AVX2 code (timer sampling only; WSL2 has no PMU);
- `scripts/trace_summary.py` for per-stage gaps in an `SS_TRACE` timeline.

The Linux targets run in OrbStack (`open -ga OrbStack`); stop it (`orbctl stop`) before timing natively at full size, since the VM's memory can push the Mac into compressing pages. One-time setup: `make half GENERATE=generator/target/release/generate` (30M events; the VM's 12.6 GB cannot hold the full set beside its page cache), `make linux-data` (copies it into the volume `ss-half`), `make profile-image` (`scripts/profile.Dockerfile`: the arm64 scorer image plus perf, strace and the profiling build; rebuild it after a code change). The VM is arm64 with no hardware counters, so it profiles the NEON path by timer sampling, not the scorer's AVX2 code.

Where the methods come from: the alternating runs with a change column are DataFusion's `bench.sh compare` and DuckDB's regression runner; cold and warm runs are ClickBench's; the per-task timeline plays the part of Polars' `profile()` and DuckDB's operator profiling; the `profiling` Cargo profile is the one DataFusion and Polars keep. To compare against an older build, keep a copy of its binary (`cp target/release/shopping-sessions target/base`) and pass both to `make bench`.

Not set up: hardware counters (cycles, cache misses), since neither VM (OrbStack, WSL2) has them and the Mac has them only through Instruments' CPU Counters template; instruction counting under Valgrind as Polars' CodSpeed runs do; criterion micro-benchmarks, whose place the `examples/*bench.rs` of each crate take.
