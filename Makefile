# Sashiko Development and CI Tasks

.PHONY: help build fmt lint test clean
.PHONY: check-pr check-integration check-all sob integration-test

# Default target
.DEFAULT_GOAL := help

# List available commands
help:
	@echo "Available targets:"
	@echo ""
	@echo "  Development:"
	@echo "    build             - Build release binary"
	@echo "    fmt               - Auto-format Rust code"
	@echo "    lint              - Run all linters (clippy, fmt --check, yamllint)"
	@echo "    test              - Run unit tests"
	@echo "    clean             - Remove build artifacts"
	@echo ""
	@echo "  CI Suites:"
	@echo "    check-pr          - Run all PR checks (SOB, Lint, Unit Tests)"
	@echo "    check-integration - Run integration tests (server + API)"
	@echo "    check-all         - Run the complete check suite (PR + Integration)"
	@echo ""
	@echo "  Utilities:"
	@echo "    sob               - Check Signed-off-by tags (RANGE=HEAD~1..HEAD)"

# ── Development ──────────────────────────────────────────

# Build release binary
build:
	@cargo build --release

# Auto-format Rust code
fmt:
	@cargo fmt --all

# Run all linters (clippy, fmt --check, yamllint)
lint:
	@cargo clippy --all-targets --all-features --release -- -D warnings
	@cargo fmt --all -- --check
	-@yamllint .

# Run unit tests
test:
	@cargo test --release

# Remove build artifacts
clean:
	@cargo clean

# ── CI Suites ────────────────────────────────────────────

# [PR Suite] Run all checks required for a Pull Request (SOB, Lint, Unit Tests)
check-pr: sob lint test

# [Integration Suite] Run #[ignore]-tagged integration tests (server + API)
check-integration: integration-test

# Run the complete check suite (PR + Integration)
check-all: check-pr check-integration

# Check Signed-off-by tags (default: HEAD~1..HEAD)
RANGE ?= HEAD~1..HEAD
sob:
	-@./scripts/check-sob.sh "$(RANGE)"

# Run #[ignore]-tagged integration tests (spins up real HTTP servers)
integration-test:
	@cargo test --release --test integration_tests -- --ignored
