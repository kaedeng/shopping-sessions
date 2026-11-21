# Shopping Sessions: a custom Rust engine (no Polars). implementation.md describes it.
# The scorers are linux/amd64 (Intel Xeon). They build this file natively, so
# no FROM line pins a platform: `make build` builds for linux/amd64 anywhere.
FROM rust:1.98-slim-trixie AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crates/ crates/
COPY src/ src/
# One static, non-relocatable binary: the C library is linked in, so the image below is the
# binary alone, and a start is an exec with no loader, no libraries to map and no relocations
# to apply (about 1 ms of a 17 ms run at 1M events, and what `docker run` adds on top).
# `--target` keeps these flags off the build's own helpers (proc macros cannot be static),
# and RUSTFLAGS replaces .cargo/config.toml's, so the CPU is named here again: cascadelake
# for linux/amd64 (AVX-512, AVX2, BMI2), as that file has it.
RUN target=$(rustc -vV | sed -n 's/^host: //p') \
 && case "$target" in x86_64-*) cpu="-C target-cpu=cascadelake" ;; *) cpu="" ;; esac \
 && RUSTFLAGS="$cpu -C target-feature=+crt-static -C relocation-model=static" \
    cargo build --release --locked --target "$target" \
 && cp "target/$target/release/shopping-sessions" /shopping-sessions

# Nothing but the binary. It reads DATA_DIR, OUTPUT_DIR and NCPU itself, allocates through its
# own allocator (src/region.rs; no GLIBC_TUNABLES to set) and needs no shell, no temp
# directory and no user database.
FROM scratch
COPY --from=build /shopping-sessions /app/shopping-sessions

# At run time the root filesystem is read-only, the user is 1000:1000, there is
# no network, /data is read-only, and /output and /scratch are the only writable
# places. The binary writes only under /output.
USER 1000:1000
WORKDIR /app
CMD ["/app/shopping-sessions"]
