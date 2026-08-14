# Auto-documented Makefile:
# http://marmelab.com/blog/2016/02/29/auto-documented-makefile.html
#
# Two families of targets:
#   * cargo-native (build/test/fmt/clippy/…) — fast local iteration.
#   * Dagger (ci/dist/release/…) — the same containerized pipeline CI runs, so a
#     failing PR reproduces locally. The Dagger module lives in `.dagger/`.

.PHONY: help build build-release test fmt fmt-check clippy lint check run install clean \
	contracts-check freshness-check check-tapes-pins bump-harnesses ci lint-ci test-ci dist \
	nightly release upload-install-script

CARGO_TEST_FLAGS ?=

# The commit stamped into binaries built through Dagger. Passed explicitly
# because the Dagger module builds from a source directory with no `.git` in it,
# so a build cannot work this out for itself the way a local `cargo build` can.
COMMIT ?= $(shell git rev-parse HEAD 2>/dev/null)

help:	## Print available targets
	@awk 'BEGIN {FS = ":.*##"; printf "Targets:\n"} /^[a-zA-Z_-]+:.*##/ {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

# --- cargo-native (local) -----------------------------------------------------

build:	## Build all crates (debug)
	cargo build --workspace

build-release:	## Build the release binary with the local toolchain
	cargo build --workspace --release

test:	## Run all workspace tests
	cargo test --workspace $(CARGO_TEST_FLAGS)

fmt:	## Format all sources
	cargo fmt --all

fmt-check:	## Verify formatting without modifying
	cargo fmt --all -- --check

clippy:	## Run clippy with workspace-wide deny warnings
	cargo clippy --workspace --all-targets -- -D warnings

lint: fmt-check clippy	## Run all lint checks (fmt + clippy)

check: build clippy test	## Build + lint + test

contracts-check:	## Verify the vendored tapes ingest contract against its recorded fingerprint (and a tapes checkout, when present)
	./scripts/contracts-check.sh

freshness-check:	## Verify each vendored fixture corpus against the upstream commit its SOURCE.md pins
	./scripts/fixture-freshness-check.sh

check-tapes-pins:	## Verify the tapes crates pins agree with each other and name a revision on the upstream default branch
	./scripts/check-tapes-pins.sh

# The whole repin, so that humans and the upstream bump-consumers workflow run
# the same command rather than two descriptions of one procedure that drift.
# REV is required; see scripts/bump-harnesses.sh for what it does and why it no
# longer touches flake.nix.
bump-harnesses:	## Re-point every tapes crates git dep at REV (e.g. `make bump-harnesses REV=<sha>`)
	./scripts/bump-harnesses.sh $(REV)

run:	## Run the tapesctl CLI (e.g. `make run ARGS="version"`)
	cargo run -p tapesctl -- $(ARGS)

install:	## Install the tapesctl binary into $(HOME)/.local/bin
	cargo install --path crates/tapesctl --root $(HOME)/.local

clean:	## Remove build artifacts
	cargo clean

# --- Dagger CI / release ------------------------------------------------------
# These reproduce CI locally. `dist` and the bucket ops need a container engine;
# the release/nightly/upload targets read bucket creds from the environment.

ci: lint-ci test-ci	## Run the PR gates through Dagger (lint + test)

lint-ci:	## cargo fmt --check + clippy via Dagger
	dagger call lint

test-ci:	## cargo test --workspace via Dagger
	dagger call test

dist:	## Cross-compile all release targets via Dagger into ./build
	dagger call build-release --commit=$(COMMIT) export --path ./build

nightly:	## Build and upload nightly artifacts to the release bucket
	dagger call nightly \
		--commit=$(COMMIT) \
		--endpoint=env://BUCKET_ENDPOINT \
		--bucket=env://BUCKET_NAME \
		--access-key-id=env://BUCKET_ACCESS_KEY_ID \
		--secret-access-key=env://BUCKET_SECRET_ACCESS_KEY

release:	## Build and upload release artifacts + install script to the bucket
	dagger call release-latest \
		--version=$(VERSION) \
		--commit=$(COMMIT) \
		--endpoint=env://BUCKET_ENDPOINT \
		--bucket=env://BUCKET_NAME \
		--access-key-id=env://BUCKET_ACCESS_KEY_ID \
		--secret-access-key=env://BUCKET_SECRET_ACCESS_KEY

upload-install-script:	## Republish the install script without cutting a release
	dagger call upload-install-sh \
		--endpoint=env://BUCKET_ENDPOINT \
		--bucket=env://BUCKET_NAME \
		--access-key-id=env://BUCKET_ACCESS_KEY_ID \
		--secret-access-key=env://BUCKET_SECRET_ACCESS_KEY

.DEFAULT_GOAL := help
