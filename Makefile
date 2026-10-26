# Sashiko Development and CI Tasks

.PHONY: help build fmt lint lint-local lint-local-cache test clean
.PHONY: check-pr check-all sob check-db-invariants repo

# Default target
.DEFAULT_GOAL := help

# List available commands
help:
	@echo "Available targets:"
	@echo ""
	@echo "  Development:"
	@echo "    build               - Build release binary"
	@echo "    fmt                 - Auto-format Rust code"
	@echo "    lint                - Run all linters (clippy, fmt --check, yamllint)"
	@echo "    lint-local          - Run clippy on local-review profile (no cache)"
	@echo "    lint-local-cache    - Run clippy on local-review profile with cache"
	@echo "    test                - Run unit and integration tests"
	@echo "    clean               - Remove build artifacts"
	@echo ""
	@echo "  CI Suites:"
	@echo "    check-pr            - Run fast diff-aware local checks (RANGE=HEAD~1..HEAD)"
	@echo "    check-all           - Run the complete check suite unconditionally"
	@echo "    check-db-invariants - Check database state anomalies"
	@echo ""
	@echo "  Utilities:"
	@echo "    sob                 - Check Signed-off-by tags (RANGE=HEAD~1..HEAD)"
	@echo "    repo                - Clone or update the reviewed tree (PROJECT=linux)"

# ── Development ──────────────────────────────────────────

# Build release binary
build:
	@cargo build --all-features --release

# Auto-format Rust code
fmt:
	@cargo fmt --all

# Run all linters (clippy, fmt --check, yamllint)
lint:
	@cargo clippy --all-targets --all-features -- -D warnings
	@cargo fmt --all -- --check
	@yamllint .

# Run clippy on local-review feature profile (without cache)
lint-local:
	@cargo clippy --all-targets --no-default-features -- -D warnings

# Run clippy on local-review feature profile with cache
lint-local-cache:
	@cargo clippy --all-targets --no-default-features --features cache -- -D warnings

# Run unit and integration tests
test:
	@cargo test --all-features

# Remove build artifacts
clean:
	@cargo clean

# ── CI Suites ────────────────────────────────────────────

# [Local PR Suite] Fast diff-aware local pre-flight check.
# Skips local cargo clippy/test when only non-code files (such as docs or
# prompts) are modified; GitHub Actions CI runs the full Rust test suite
# unconditionally on every pull request and push.
RANGE ?= HEAD~1..HEAD
check-pr: sob
	@set -e; \
	WORKTREE_CHANGED=$$(git diff --name-only HEAD); \
	RANGE_CHANGED=$$(git diff --name-only "$(RANGE)"); \
	CHANGED=$$(printf "%s\n%s\n" "$$WORKTREE_CHANGED" "$$RANGE_CHANGED" | sed '/^$$/d' | sort -u); \
	if [ -z "$$CHANGED" ]; then \
		echo "No changed files detected."; \
		exit 0; \
	fi; \
	if echo "$$CHANGED" | grep -qE '\.(yml|yaml)$$|^\.yamllint$$'; then \
		echo "Running yamllint..."; \
		yamllint .; \
	else \
		echo "No YAML changes detected; skipping yamllint."; \
	fi; \
	if echo "$$CHANGED" | grep -qE '^src/|^tests/|\.rs$$|^Cargo\.(toml|lock)$$|^rust-toolchain\.toml$$'; then \
		echo "Running Rust fmt, clippy, and tests..."; \
		cargo fmt --all -- --check; \
		cargo clippy --all-targets --all-features -- -D warnings; \
		cargo clippy --all-targets --no-default-features -- -D warnings; \
		cargo clippy --all-targets --no-default-features --features cache -- -D warnings; \
		cargo test --all-features; \
	else \
		echo "No Rust changes detected; skipping local cargo lint and test."; \
	fi

# Run the complete check suite unconditionally
check-all: sob lint lint-local lint-local-cache test check-db-invariants

# Check Signed-off-by tags (default: HEAD~1..HEAD)
sob:
	@./scripts/check-sob.sh "$(RANGE)"

# Clone or update the upstream tree a project reviews (default: linux).
# The explicit path keeps each project's tree in its own directory, whatever
# repository_path the local Settings.toml names.
PROJECT ?= linux
repo:
	@cargo run -q --bin sashiko -- --project "$(PROJECT)" repo sync --path "third_party/$(PROJECT)"

# Run lightweight database invariant checks
check-db-invariants:
	./scripts/check_invariants.sh sashiko.db
