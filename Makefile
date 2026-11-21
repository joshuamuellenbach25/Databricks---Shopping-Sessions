IMAGE ?= shopping-sessions:latest
CPUS  ?= 8
MEM   ?= 24g
PLATFORM ?= linux/amd64
DEV   ?= dev
ANSWER ?= dev-answer
export IMAGE CPUS MEM PLATFORM

.PHONY: build native test fixtures fixtures-native dev run-dev check-dev

build:
	docker buildx build --platform $(PLATFORM) --load -t $(IMAGE) .

native:
	cargo build --release

test:
	cargo test --release

fixtures: build
	python3 scripts/run_fixtures.py fixtures -- scripts/docker_run.sh

fixtures-native: native
	python3 scripts/run_fixtures.py fixtures -- target/release/solver

dev:
	generate --seed 1 --events 1000000 --clicks 250000 --output $(DEV)

run-dev: build
	rm -rf out && time scripts/docker_run.sh $(DEV) out

check-dev:
	python3 scripts/compare.py out $(ANSWER)
