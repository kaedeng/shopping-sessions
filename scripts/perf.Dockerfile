# x86 profiling image, not submitted: the release build with line tables (the scorer's flags:
# static, cascadelake), beside perf 6.x, objdump and addr2line on Debian trixie. Hardware
# counters work in a privileged container on the x86 box (x86.md, "Profiling").
#   docker buildx build --platform linux/amd64 --load -f scripts/perf.Dockerfile -t shopping-sessions:perf .
#   docker run --rm --privileged -v ss-p20v5:/data:ro shopping-sessions:perf perf stat -e cycles,instructions /app/shopping-sessions
FROM rust:1.98-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crates/ crates/
COPY src/ src/
ARG EXTRA_RUSTFLAGS=""
RUN target=$(rustc -vV | sed -n 's/^host: //p') \
 && RUSTFLAGS="-C target-cpu=cascadelake -C target-feature=+crt-static -C relocation-model=static $EXTRA_RUSTFLAGS" \
    CARGO_PROFILE_PROFILING_SPLIT_DEBUGINFO=off \
    cargo build --profile profiling --locked --target "$target" \
 && cp "target/$target/profiling/shopping-sessions" /shopping-sessions

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends linux-perf binutils python3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /shopping-sessions /app/shopping-sessions
COPY scripts/bench.py /app/bench.py
ENV DATA_DIR=/data OUTPUT_DIR=/tmp/out NCPU=8
WORKDIR /prof
