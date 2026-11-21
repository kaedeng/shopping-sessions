# Shopping Sessions: a custom Rust engine (no Polars). implementation.md describes it.
# The scorers are linux/amd64 (Intel Xeon). They build this file natively, so
# no FROM line pins a platform: `make build` builds for linux/amd64 anywhere.
FROM rust:1.98-slim-trixie AS build
WORKDIR /src

# .cargo/config.toml sets target-cpu=x86-64-v3 for amd64 (AVX2, BMI2).
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crates/ crates/
COPY src/ src/
RUN cargo build --release --locked

# Same Debian release as the build stage, so the binary's glibc matches.
FROM debian:trixie-slim

# The scorer runs the container as uid 1000; give that uid a name and a home.
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app

COPY --from=build /src/target/release/shopping-sessions /app/shopping-sessions
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh

# glibc malloc: back large buffers with 2 MB pages where the host allows it, grow the heap in
# 256 MB steps, and take everything under 32 MB from the heap instead of its own mmap.
# 13 to 14% off the wall time with transparent huge pages on, 4% with them off (alternatives.md).
ENV GLIBC_TUNABLES=glibc.malloc.hugetlb=1:glibc.malloc.top_pad=268435456:glibc.malloc.mmap_threshold=33554432

# At run time the root filesystem is read-only, the user is 1000:1000, there is
# no network, /data is read-only, and /output and /scratch are the only writable
# places. The binary writes only under /output.
WORKDIR /app
CMD ["/app/run.sh"]
