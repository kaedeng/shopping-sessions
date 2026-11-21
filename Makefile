IMAGE ?= shopping-sessions:latest
CPUS  ?= 8
MEM   ?= 24g
# The scorer's platform. PLATFORM=linux/arm64 builds and runs natively on an Apple-silicon Mac, for fast local loops.
PLATFORM ?= linux/amd64
DEV   ?= dev
# The 36 edge-case fixtures, one folder per case (input/ and expected/).
FIXTURES ?= fixtures
export IMAGE CPUS MEM PLATFORM

.PHONY: build fixtures native dev run-dev check-dev profiling samply systrace-mac trace bench half linux-data profile-image

## Build the image for the scorer's platform, linux/amd64 (emulated on Apple silicon)
build:
	docker buildx build --platform $(PLATFORM) --load -t $(IMAGE) .

## Run the image on all 36 edge-case fixtures and diff against the expected tables
fixtures: build
	python3 scripts/run_fixtures.py $(FIXTURES) -- scripts/docker_run.sh

## Fast loop without Docker: build natively and run all 36 fixtures
native:
	cargo build --release
	python3 scripts/run_fixtures.py $(FIXTURES) -- target/release/shopping-sessions

## Make the 1M-event dev set with the published generator (needs `generate` on PATH)
GENERATE ?= generate
dev:
	$(GENERATE) --seed 1 --events 1000000 --clicks 250000 --output $(DEV)

## Run on the dev set with scorer flags and time it
run-dev: build
	rm -rf out && time scripts/docker_run.sh $(DEV) out

## Compare your dev output with the published dev answer (dev-answer.zip, unzipped): make check-dev ANSWER=dev-answer
ANSWER ?= dev-answer
check-dev:
	python3 scripts/compare.py out $(ANSWER)

# Benchmarking and profiling (README has the walkthrough). DATA is the set a native run reads.
DATA  ?= full
RUNS  ?= 5
BENCH ?= target/release/shopping-sessions
VOLUME ?= ss-half
PROFILE_IMAGE ?= shopping-sessions:profile
export VOLUME PROFILE_IMAGE

## Release code plus symbols, line tables and `trace`: target/profiling/shopping-sessions
profiling:
	cargo build --profile profiling --features trace

## CPU flamegraph on the Mac (samply); opens the Firefox Profiler when the run ends
samply: profiling
	rm -rf out && DATA_DIR=$(DATA) OUTPUT_DIR=out samply record -r 4000 target/profiling/shopping-sessions

## Every syscall and page fault of one run on the Mac (Instruments System Trace), summarized
systrace-mac: profiling
	rm -rf out prof/mac.trace && mkdir -p prof
	xctrace record --template 'System Trace' --output prof/mac.trace --env DATA_DIR=$(DATA) --env OUTPUT_DIR=out --launch -- target/profiling/shopping-sessions
	python3 scripts/xctrace_summary.py prof/mac.trace | tee prof/mac-systrace.txt

## Per-task timeline, one track per worker: open prof/trace.json in ui.perfetto.dev
trace: profiling
	rm -rf out && mkdir -p prof && SS_TRACE=prof/trace.json SS_TIMING=1 DATA_DIR=$(DATA) OUTPUT_DIR=out target/profiling/shopping-sessions

## Variants in turns, median per stage: make bench BENCH="target/release/shopping-sessions 'NCPU=4 target/release/shopping-sessions'"
bench:
	cargo build --release
	python3 scripts/bench.py -n $(RUNS) --data $(DATA) $(BENCH)

## The 30M-event set for the Linux VM, which cannot hold the full one beside its page cache
half:
	$(GENERATE) --seed 2 --events 30000000 --clicks 7500000 --output half

## Copy half/ into the Docker volume $(VOLUME); a bind mount would read through virtiofs
linux-data:
	docker volume create $(VOLUME)
	docker run --rm -v $(VOLUME):/data -v $(CURDIR)/half:/src:ro debian:trixie-slim sh -c 'rm -rf /data/* && cp -a /src/. /data/'

## The arm64 scorer image plus perf, strace, Python and the profiling build
profile-image:
	$(MAKE) build PLATFORM=linux/arm64 IMAGE=shopping-sessions:arm64
	docker buildx build --platform linux/arm64 --load --build-arg BASE=shopping-sessions:arm64 -f scripts/profile.Dockerfile -t $(PROFILE_IMAGE) .

## In the Linux VM: linux-flame, linux-annotate, linux-faults, linux-syscalls, linux-trace, linux-bench ARGS="...", linux-shell
linux-%:
	scripts/profile_linux.sh $* $(ARGS)
