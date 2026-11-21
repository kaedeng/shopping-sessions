# Linux profiling image, not submitted: the scorer image plus the profiling build, perf, strace,
# Python, objdump (for perf annotate), inferno (flamegraph SVGs) and rustfilt (perf cannot
# demangle Rust's v0 names). `make profile-image` builds it for linux/arm64, the VM's native
# platform: perf cannot see through Rosetta, so amd64 is not profiled here.
# scripts/profile_linux.sh runs it.
ARG BASE=shopping-sessions:arm64
FROM rust:1.98-slim-trixie AS build
RUN cargo install inferno rustfilt --locked --root /tools
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crates/ crates/
COPY src/ src/
# Line tables stay in the binary on Linux: perf cannot read a packed .dwp.
RUN CARGO_PROFILE_PROFILING_SPLIT_DEBUGINFO=off cargo build --profile profiling --features trace --locked

# The base keeps /app/shopping-sessions (the release build) and the image's GLIBC_TUNABLES.
FROM ${BASE}
RUN apt-get update && apt-get install -y --no-install-recommends linux-perf strace python3 binutils \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /tools/bin/ /usr/local/bin/
COPY --from=build /src/target/profiling/shopping-sessions /app/shopping-sessions-profiling
COPY scripts/bench.py /app/bench.py
