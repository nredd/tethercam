.PHONY: help build test fmt fmt-check lint doc check deny bench bench-check cross clean all

help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

build:  ## Build the project
	cargo build --all-features

test:  ## Run tests (nextest + doctests)
	cargo nextest run --all-features
	cargo test --doc --all-features

fmt:  ## Format code
	cargo fmt --all

fmt-check:  ## Check formatting without writing
	cargo fmt --all -- --check

lint:  ## Run clippy (pedantic, warnings are errors)
	cargo clippy --all-targets --all-features -- -D warnings

doc:  ## Build documentation (warnings are errors)
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

check:  ## Fast compile check
	cargo check --all-targets --all-features

deny:  ## Audit dependencies (advisories, licenses, sources)
	cargo deny check

bench:  ## Run benchmarks
	cargo bench --all-features

bench-check:  ## Compile benchmarks and run each once as a test
	cargo bench --all-features -- --test

cross:  ## Compile-check the embedded Linux targets
	cargo check --all-features --target aarch64-unknown-linux-gnu
	cargo check --all-features --target armv7-unknown-linux-gnueabihf

clean:  ## Clean artifacts
	cargo clean

all: fmt-check lint check test doc bench-check deny  ## Run the full gate (what CI runs)
