IMAGE ?= shopping-sessions:latest
CPUS  ?= 8
MEM   ?= 24g
PLATFORM ?= linux/amd64
DEV   ?= dev
ANSWER ?= dev-answer
export IMAGE CPUS MEM PLATFORM

.PHONY: build native test fixtures fixtures-native dev run-dev check-dev

## Build the image for the scorer's platform
build:
	docker buildx build --platform $(PLATFORM) --load -t $(IMAGE) .

## Build the binary for this machine
native:
	cargo build --release

## Unit tests
test:
	cargo test --release

## Run the image on all 36 fixtures and diff against the expected tables
fixtures: build
	python3 scripts/run_fixtures.py fixtures -- scripts/docker_run.sh

## Run the native binary on all 36 fixtures
fixtures-native: native
	python3 scripts/run_fixtures.py fixtures -- target/release/solver

## Make the 1M-event dev set with the published generator (needs `generate` on PATH)
dev:
	generate --seed 1 --events 1000000 --clicks 250000 --output $(DEV)

## Run on the dev set with scorer flags and time it
run-dev: build
	rm -rf out && time scripts/docker_run.sh $(DEV) out

## Compare the dev output with the published answer (dev-answer.zip, unzipped)
check-dev:
	python3 scripts/compare.py out $(ANSWER)
