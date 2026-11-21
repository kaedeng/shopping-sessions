# Shopping Sessions starter: Polars (Rust) skeleton.
# The scorers are linux/amd64 (Intel Xeon). They build this file natively, so
# no FROM line pins a platform: `make build` builds for linux/amd64 anywhere.
FROM rust:1.98-slim-trixie AS build
WORKDIR /src

# Build the dependencies once, in their own layer, so editing src/ doesn't recompile Polars.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked && rm -rf src

COPY src/ src/
# touch: COPY keeps the old mtime, which cargo would mistake for the dummy main.rs.
RUN touch src/main.rs && cargo build --release --locked

# Same Debian release as the build stage, so the binary's glibc matches.
FROM debian:trixie-slim

# The scorer runs the container as uid 1000; give that uid a name and a home.
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app

COPY --from=build /src/target/release/shopping-sessions /app/shopping-sessions
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh

# At run time the root filesystem is read-only, the user is 1000:1000, there is
# no network, /data is read-only, and /output and /scratch are the only writable
# places. run.sh points every temp and cache directory at /scratch.
WORKDIR /app
CMD ["/app/run.sh"]
