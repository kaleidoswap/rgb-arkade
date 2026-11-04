# rgb-arkade: common tasks. See README.md.
SCENARIO ?= offchain-transfer

.PHONY: help setup setup-cponly setup-master setup-emulator build regtest-up regtest-up-fast regtest-up-cponly regtest-up-master regtest-down app scenario test test-all test-full bench patches-check list

help:
	@echo "make setup        clone pinned upstreams, apply patches, build arkd:rgb, tools/arkdb and the demo"
	@echo "make setup-cponly build arkd:rgb-cponly (arkd master + only the checkpoint OP_RETURN), after make setup"
	@echo "make setup-master build arkd:rgb-master (arkd master + checkpoint, forfeit and data OP_RETURN), after make setup"
	@echo "make setup-emulator build emulator:rgb (patched arkade-os/emulator) and tools/emucov"
	@echo "make build        build the demo (cargo build in demo/)"
	@echo "make regtest-up   start the regtest stack (arkd:rgb + bitcoind + electrs + pools mirror)"
	@echo "make regtest-up-fast  restart the stack clean with exit delays in blocks (quick suite in minutes, no time-scaled scenarios)"
	@echo "make regtest-up-cponly  restart the stack clean on arkd:rgb-cponly, for the checkpoint-* scenarios"
	@echo "make regtest-up-master  restart the stack clean on arkd:rgb-master"
	@echo "make regtest-down stop the regtest stack"
	@echo "make app          start the regtest stack if needed and serve the prototype app on :8080 (PORT=...)"
	@echo "make list         list scenarios"
	@echo "make scenario SCENARIO=<name> [ARGS=...]  run one scenario"
	@echo "make test         quick regression suite, full patch set (no time-scaled scenarios; ~20 min on the fast profile)"
	@echo "make test-all     quick + time-scaled scenarios, seconds profile (~3 h)"
	@echo "make test-full    every lane (full set, checkpoint-only, stock), restarting the stack clean per profile"
	@echo "make bench        payload size / validation time benchmarks"
	@echo "make patches-check  git apply --check every patch against its pinned upstream checkout"

setup:
	scripts/setup.sh

setup-cponly:
	scripts/setup-cponly.sh

setup-master:
	scripts/setup-master.sh

setup-emulator:
	scripts/setup-emulator.sh

build:
	cd demo && cargo build

regtest-up:
	scripts/regtest-up.sh

regtest-up-fast:
	REGTEST_PROFILE=fast scripts/regtest-up.sh clean

regtest-up-cponly:
	REGTEST_PROFILE=cponly scripts/regtest-up.sh clean

regtest-up-master:
	REGTEST_PROFILE=master scripts/regtest-up.sh clean

regtest-down:
	cd arkade-regtest && node regtest.mjs stop; docker rm -f rgbarkade-pools >/dev/null 2>&1 || true

app:
	scripts/app.sh $(PORT)

list: build
	$${CARGO_TARGET_DIR:-demo/target}/debug/ark-rgb-demo list

scenario: build
	scripts/run-scenario.sh $(SCENARIO) $(ARGS)

test: build
	scripts/run-all.sh quick

test-all: build
	scripts/run-all.sh all

test-full: build
	scripts/run-all.sh full

bench: build
	scripts/bench.sh

patches-check:
	scripts/patches-check.sh
