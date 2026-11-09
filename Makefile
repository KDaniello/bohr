.DEFAULT_GOAL := help

CARGO ?= cargo
# rustfmt and cargo-fuzz need nightly; dated, because an undated one reformats untouched code the
# day rustfmt changes. CI reads the date from this line (.github/actions/toolchain).
NIGHTLY ?= +nightly-2026-10-16
WORKSPACE_FLAGS := --workspace --all-features
# Targets that execute code use these instead of WORKSPACE_FLAGS: --all-features would switch on
# every chaos injection at once, and the determinism suite is built to go red under any one of
# them (docs/TESTING.md 7.4). Only scripts/mutation-harness.sh enables them, one at a time.
# Static analysis keeps --all-features so the injections themselves stay compiled and linted.
RUN_FLAGS := --workspace
BIN := target/release/bohr
# The compiler the SDK crates promise to build on, read from the crate rather than restated.
SDK_MSRV := $(shell sed -n 's/^rust-version = "\(.*\)"/\1/p' sdk/rust/Cargo.toml)

# Guests are built from committed source with pinned flags, never committed as binaries.
GUEST_SRC := $(wildcard testdata/guests/*.c testdata/guests/*.rs)

##@ Build

.PHONY: build
build: ## Build the release binary
	$(CARGO) build --release --bin bohr

.PHONY: build-debug
build-debug: ## Build the debug binary
	$(CARGO) build --bin bohr

.PHONY: install
install: ## Install bohr into $(CARGO_HOME)/bin
	$(CARGO) install --path crates/bohr-cli --force --locked

.PHONY: build-musl
build-musl: ## Static build, the artefact referenced in the documentation
	$(CARGO) build --release --target x86_64-unknown-linux-musl --bin bohr

.PHONY: guests
guests: ## Build the test guest programs
	$(MAKE) -C testdata/guests

.PHONY: example-split-brain
example-split-brain: ## Build the raft-rs cluster scenario (docs/ROADMAP.md M2 gate)
	scripts/build-example.sh split-brain

.PHONY: probe
probe: ## Build the probe guest statically, where bohr looks for it (docs/API.md 5.9)
	scripts/build-probe.sh

.PHONY: example-chain-toy
example-chain-toy: ## Build the seeded blockchain defects (docs/TESTING.md 8)
	scripts/build-example.sh chain-toy

.PHONY: example-raft-toy
example-raft-toy: ## Build the seeded consensus defect (docs/TESTING.md 8)
	scripts/build-example.sh raft-toy

.PHONY: test-hunting
test-hunting: build-debug example-raft-toy example-chain-toy probe ## Does the search still reach the seeded defects (8)
	$(CARGO) test --test hunting --test subjects -- --ignored --nocapture

##@ Test

.PHONY: test-fast
test-fast: ## L0-L2: unit, property, integration. No Linux required. Under a minute.
	$(CARGO) nextest run $(RUN_FLAGS) --exclude bohr-backend-ptrace

.PHONY: test-linux
test-linux: guests ## L3-L4: the real ptrace backend against real guests
	$(CARGO) nextest run $(RUN_FLAGS) --test determinism

.PHONY: test-det
test-det: guests ## The determinism gates only (docs/TESTING.md 7)
	$(CARGO) nextest run --test determinism

.PHONY: test-portability
test-portability: build ## Produce bundles here and replay them, as two machines would
	$(CARGO) nextest run $(RUN_FLAGS) --test portability

.PHONY: test-doc
test-doc: ## Documentation tests
	$(CARGO) test --doc $(RUN_FLAGS)

.PHONY: test
test: test-fast test-linux test-doc ## Everything this host can run

.PHONY: bdd
bdd: guests ## The cucumber feature files (docs/TESTING.md 5)
	$(CARGO) test --test bdd

.PHONY: test-sdk
test-sdk: ## SDK conformance across every language with a driver (docs/TESTING.md 6)
	$(CARGO) build -p conformance-driver
	dotnet build -c Debug -o sdk/conformance/drivers/csharp/out sdk/conformance/drivers/csharp
	cd sdk/conformance/drivers/go && go build -o out/conformance-driver .
	python3 sdk/conformance/check.py --all

.PHONY: msrv
msrv: ## The SDK crates build on the oldest compiler they promise (their rust-version)
	$(CARGO) +$(SDK_MSRV) check -p bohr-sdk -p bohr-sdk-proto --all-targets

.PHONY: test-mutation
test-mutation: ## Assert every chaos injection breaks the determinism suite (docs/TESTING.md 7.4)
	scripts/mutation-harness.sh

.PHONY: gate
gate: build guests ## Every milestone gate, measured on this host
	scripts/gate.sh

.PHONY: policy-comparison
policy-comparison: build guests ## Which scheduler reaches the seeded defects (docs/TESTING.md 8)
	scripts/policy-comparison.sh

##@ Quality

.PHONY: fmt
fmt: ## Format with nightly rustfmt
	$(CARGO) $(NIGHTLY) fmt --all

.PHONY: fmt-check
fmt-check:
	$(CARGO) $(NIGHTLY) fmt --all --check

.PHONY: clippy
clippy:
	$(CARGO) clippy $(WORKSPACE_FLAGS) --lib --bins --examples --tests --benches -- -D warnings

.PHONY: deny
deny:
	$(CARGO) deny check

.PHONY: typos
typos:
	typos

.PHONY: lint-rules
lint-rules: ## The mechanical architecture rules (docs/ARCHITECTURE.md 8, rule 14)
	scripts/lint-rules.sh

.PHONY: lint-python
lint-python: ## ruff and strict mypy over the SDK, the conformance checker and the scripts
	python3 -m ruff check sdk scripts
	python3 -m mypy

.PHONY: lint-workflows
lint-workflows: ## Every workflow file must parse, or its jobs silently never run
	python3 scripts/check-workflows.py

.PHONY: lint-go
lint-go: ## gofmt and vet over the Go SDK and its conformance driver
	@out=$$(gofmt -l sdk/go sdk/conformance/drivers/go); if [ -n "$$out" ]; then echo "not gofmt clean:"; echo "$$out"; exit 1; fi
	cd sdk/go && go vet ./...
	cd sdk/conformance/drivers/go && go vet ./...

.PHONY: lint
lint: fmt-check clippy deny typos lint-rules lint-python lint-workflows ## Everything CI checks for style and architecture

.PHONY: fix
fix: ## Apply clippy suggestions, then format
	$(CARGO) clippy $(WORKSPACE_FLAGS) --fix --allow-dirty --allow-staged
	$(MAKE) fmt

##@ Benchmarks and fuzzing

.PHONY: golden
golden: build guests ## The golden guests: every part of bohr, its speed, memory and determinism (17)
	target/release/bohr check golden -j 2

.PHONY: bench-slowdown
bench-slowdown: build guests ## Slowdown versus native against benches/baseline.toml
	scripts/bench-slowdown.sh

# Which target `make fuzz` runs, and where its seeds and its grown corpus live.
F := fuzz_$(or $(T),bundle)

# The host triple, stated rather than inferred: the sanitiser libfuzzer needs cannot link
# against a static libc, so a musl default would fail the build rather than the fuzzing.
FUZZ_HOST := $(shell rustc -vV | sed -n 's/^host: //p')

.PHONY: fuzz
fuzz: ## Run one fuzz target: make fuzz T=bundle S=60
	@mkdir -p fuzz/corpus/$(F)
	$(CARGO) $(NIGHTLY) fuzz run --target $(FUZZ_HOST) $(F) fuzz/corpus/$(F) fuzz/seeds/$(F) -- -max_total_time=$(or $(S),60)

.PHONY: fuzz-seed
fuzz-seed: ## Rewrite the committed fuzz seeds from the current formats
	$(CARGO) run $(RUN_FLAGS) -p bohr-testkit --example fuzz-seed

##@ Documentation

.PHONY: docs
docs: ## Build and open the rustdoc
	$(CARGO) doc $(WORKSPACE_FLAGS) --no-deps --open

.PHONY: docs-check
docs-check: ## Every relative link in the repository resolves
	scripts/check-docs.sh

##@ Release

.PHONY: pr
pr: lint test-fast test test-doc bdd docs-check ## Run before opening a pull request
	@echo "ready"

.PHONY: clean
clean:
	$(CARGO) clean
	$(MAKE) -C testdata/guests clean

##@ Help

.PHONY: help
help:
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n"} \
	  /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2 } \
	  /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) }' $(MAKEFILE_LIST)
