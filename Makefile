.PHONY: help build test fmt fmt-check lint doc check deny bench bench-check cross coverage coverage-report clean all

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

# Files listed in `coverage-pending.txt` are the only `src/` code allowed below 100%.
COVERAGE_IGNORE = $$(scripts/coverage-pending.sh regex)

coverage:  ## Enforce 100% line, function and region coverage of src/ (needs cargo-llvm-cov)
	scripts/test-coverage-pending.sh
	scripts/coverage-pending.sh validate
	@set -eu; \
	tmp=$$(mktemp -d); trap 'rm -rf "$$tmp"' EXIT; \
	cargo llvm-cov clean --workspace; \
	cargo llvm-cov nextest --all-features --no-report; \
	cargo llvm-cov report --json --summary-only --output-path "$$tmp/coverage.json"; \
	scripts/coverage-pending.sh stale "$$tmp/coverage.json"; \
	if git rev-parse --verify --quiet origin/main >/dev/null; then scripts/coverage-pending.sh shrink origin/main; fi; \
	ignore="$(COVERAGE_IGNORE)"; \
	cargo llvm-cov report --show-missing-lines \
		--fail-under-lines 100 --fail-under-functions 100 --fail-under-regions 100 \
		$${ignore:+--ignore-filename-regex "$$ignore"}

coverage-report:  ## Write lcov and HTML reports to coverage/ from the last `make coverage` run
	mkdir -p coverage
	ignore="$(COVERAGE_IGNORE)"; \
	cargo llvm-cov report --lcov --output-path coverage/lcov.info $${ignore:+--ignore-filename-regex "$$ignore"}; \
	cargo llvm-cov report --html --output-dir coverage $${ignore:+--ignore-filename-regex "$$ignore"}

clean:  ## Clean artifacts
	cargo clean

all: fmt-check lint check test doc bench-check deny coverage  ## Run the full gate (what CI runs)
