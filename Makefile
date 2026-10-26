# rgb-arkade: common tasks. See README.md.
SCENARIO ?= offchain-transfer

.PHONY: help setup build regtest-up regtest-down scenario test test-all bench patches-check list

help:
	@echo "make setup        clone pinned upstreams, apply patches, build arkd:rgb, tools/arkdb and the demo"
	@echo "make build        build the demo (cargo build in demo/)"
	@echo "make regtest-up   start the regtest stack (arkd:rgb + bitcoind + electrs + pools mirror)"
	@echo "make regtest-down stop the regtest stack"
	@echo "make list         list scenarios"
	@echo "make scenario SCENARIO=<name> [ARGS=...]  run one scenario"
	@echo "make test         quick regression suite (no time-scaled scenarios, ~1 h 30)"
	@echo "make test-all     full regression suite (~3 h)"
	@echo "make bench        payload size / validation time benchmarks"
	@echo "make patches-check  git apply --check every patch against its pinned upstream checkout"

setup:
	scripts/setup.sh

build:
	cd demo && cargo build

regtest-up:
	scripts/regtest-up.sh

regtest-down:
	cd arkade-regtest && node regtest.mjs stop; docker rm -f rgbarkade-pools >/dev/null 2>&1 || true

list: build
	$${CARGO_TARGET_DIR:-demo/target}/debug/ark-rgb-demo list

scenario: build
	scripts/run-scenario.sh $(SCENARIO) $(ARGS)

test: build
	scripts/run-all.sh quick

test-all: build
	scripts/run-all.sh

bench: build
	scripts/bench.sh

patches-check:
	scripts/patches-check.sh
