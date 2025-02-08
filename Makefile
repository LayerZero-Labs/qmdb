PHONY_TARGETS := install-deps build build-debug build-release test format format-fix lint lint-fix run clean smart-prune-branches

# Marks targets as phony to ensure commands are executed regardless of whether files with matching names exist
# This prevents conflicts with any files of the same names and ensures the commands always run.
.PHONY: $(PHONY_TARGETS)

# Sets 'build' as the default target that will be executed when running 'make'
# without specifying a target. This means `make` and `make build` are equivalent.
.DEFAULT_GOAL := build

TARGETS := $(filter-out install-deps, $(PHONY_TARGETS))

# This rule establishes a dependency relationship where each target specified in the $(TARGETS) variable
# relies on the successful execution of the 'install-deps' target. 
$(TARGETS): install-deps

# Determines if sudo is needed based on the current user's UID
SUDO := $(if $(filter 0,$(shell id -u)),,$(word 1,$(MAKEFLAGS))sudo)

# Extra arguments passed to cargo commands
CARGO_ARGS ?=

install-deps: 

build: build-release

build-debug:
	cargo build --verbose

build-release:
	cargo build --release --verbose

check: install-deps
	cargo check --verbose 

test: install-deps
	cargo nextest run

test-coverage:
	cargo llvm-cov --lcov --output-path lcov.info

format:
	cargo fmt --all -- --check

format-fix:
	cargo fmt --all

lint:
	cargo clippy --all -- -D warnings

lint-fix:
	cargo clippy --all --fix --allow-dirty --allow-staged -- -D warnings

run:
	cargo run --release --verbose

clean:
	cargo clean

smart-prune-branches:
	@gh auth status >/dev/null 2>&1 || gh auth login
	@git branch | cut -c 3- | xargs -I {} gh pr view {} --json state,headRefName 2>/dev/null | jq -r 'select(.state == "MERGED") | .headRefName' | xargs -r -I :: git branch -D ::
