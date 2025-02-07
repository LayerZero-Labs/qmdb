# Marks targets as phony to ensure commands are executed regardless of whether files with matching names exist
# This prevents conflicts with any files of the same names and ensures the commands always run.
.PHONY: install-deps build test

# Sets 'build' as the default target that will be executed when running 'make'
# without specifying a target. This means `make` and `make build` are equivalent.
.DEFAULT_GOAL := build

# List of packages to install
PACKAGES = g++ linux-libc-dev libclang-dev unzip libjemalloc-dev make

# Determines if sudo is needed based on the current user's UID
SUDO := $(if $(filter 0,$(shell id -u)),,$(word 1,$(MAKEFLAGS))sudo)

install-deps:
	@if ! $(SUDO) apt-get -s install $(PACKAGES) > /dev/null 2>&1; then \
		echo "Package database is outdated or missing. Running apt-get update..."; \
		$(SUDO) apt-get update; \
		$(SUDO) apt-get install -y $(PACKAGES); \
	else \
		$(SUDO) apt-get install -y $(PACKAGES); \
	fi

build: install-deps
	cargo build --release

test: install-deps
	cargo nextest run

format:
	cargo fmt --all

lint:
	cargo clippy --all -- -D warnings

clean:
	cargo clean
