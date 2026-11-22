# Shopping Sessions: a custom Rust engine (no Polars). implementation.md describes it.
# The scorers are linux/amd64 (Intel Xeon). They build this file natively, so
# no FROM line pins a platform: `make build` builds for linux/amd64 anywhere.
FROM rust:1.98-slim-trixie AS build
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY crates/ crates/
COPY src/ src/
# Build switches for the scorer, one submission at a time (the scorer builds this file as
# it is, so a submission changes the default here):
# - CPU: the `target-cpu`. `cascadelake` is what the organizers name; `skylake-avx512` is
#   the same without VNNI.
# - EXTRA_RUSTFLAGS: more flags for the build. `-C llvm-args=-x86-branches-within-32B-boundaries`
#   pads the code so that no jump crosses or ends on a 32-byte boundary: on Skylake to
#   Cascade Lake Xeons the microcode fix for the jump erratum (SKX102) keeps such a jump out
#   of the decoded-instruction cache. No use on Ice Lake or later, nor on AMD.
ARG CPU=cascadelake
ARG EXTRA_RUSTFLAGS=""
# One static, non-relocatable binary: the C library is linked in, so the image below is the
# binary alone, and a start is an exec with no loader, no libraries to map and no relocations
# to apply (about 1 ms of a 17 ms run at 1M events, and what `docker run` adds on top).
# `--target` keeps these flags off the build's own helpers (proc macros cannot be static),
# and RUSTFLAGS replaces .cargo/config.toml's, so the CPU is named here again: cascadelake
# for linux/amd64 (AVX-512, AVX2, BMI2), as that file has it.
RUN target=$(rustc -vV | sed -n 's/^host: //p') \
 && case "$target" in x86_64-*) cpu="-C target-cpu=$CPU $EXTRA_RUSTFLAGS" ;; *) cpu="" ;; esac \
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

# Run switches for the scorer (8 pinned hyperthreads, 24 GB), one submission at a time.
# Unset, each is the binary's default.
# - SS_PIN=off|on|cores      a worker per allowed CPU (`types::par::pin`); `cores` orders
#                            them first threads of the cores first, for the three below.
# - SS_W_PARSE, SS_W_DEDUP, SS_W_SESSIONS=N   the workers that take that stage's tasks: 4
#                            with SS_PIN=cores is one a core, for a stage bound by memory.
# - SS_HUGE=advise|default|never   huge pages for the allocator's reservation (`region`).
# - SS_SPIN_US=N             how long an idle worker watches for the next stage before it
#                            sleeps (300): a watching thread slows its core's other thread.
# - SS_PART_BITS_OPAQUE=N    log2 of the event lists (11 at 20M events): two workers on
#                            one core share its 1 MB L2.
# - SS_PROBE=sleep           the machine as a number: the run takes 0.1 s times it longer
#                            (`probe::code` has the digits). The scorer shows no stderr.
# `scripts/variant.sh NAME SWITCH...` makes the branch `runner/NAME` with the line below
# rewritten and pushes it. SS_RUN is read by nothing: it keeps the line whole when every
# switch is at its default.
ENV SS_RUN=1 SS_PIN=on
CMD ["/app/shopping-sessions"]
