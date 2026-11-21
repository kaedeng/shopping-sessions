# Correctness checks in a container, for machines without Python and Polars (x86.md): the release
# binary from an image, plus Polars and the two check scripts. scripts/check_docker.sh builds and
# runs it. Same Debian release as the scorer image, so the binary's glibc matches.
ARG BASE=shopping-sessions:latest
FROM ${BASE} AS bin

FROM python:3.12-slim-trixie
RUN pip install --no-cache-dir polars==2.1.0
COPY --from=bin /app/shopping-sessions /app/shopping-sessions
ENV GLIBC_TUNABLES=glibc.malloc.hugetlb=1:glibc.malloc.top_pad=268435456:glibc.malloc.mmap_threshold=33554432
COPY scripts/run_fixtures.py scripts/compare.py /app/scripts/
WORKDIR /app
