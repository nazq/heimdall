# Heimdall — PTY session supervisor
# Usage: just <recipe>

# Default: list recipes
default:
    @just --list

# ─── Build ─────────────────────────────────────────────────────────

# Build (debug)
build:
    cargo build

# Build (release)
release:
    cargo build --release

# Install to ~/.local/bin
install: release
    cp target/release/hm ~/.local/bin/hm
    @echo "hm installed to ~/.local/bin/hm"

# ─── Quality ───────────────────────────────────────────────────────

# Run all checks (clippy + fmt check + full test suite)
check: clippy fmt-check test-all

# Run unit + Rust integration tests
test *args:
    cargo test {{ args }}

# Run Python attach tests
test-attach: build
    uv run pytest tests/ -v

# Run full integration suite (Rust + Python attach tests)
test-all: test test-attach

# Run tests with coverage (requires cargo-llvm-cov + uv)
# Mirrors the CI coverage workflow exactly.
cov:
    #!/usr/bin/env bash
    set -euo pipefail
    source <(cargo llvm-cov show-env --sh)
    cargo llvm-cov clean --workspace
    cargo build
    cargo test
    HM_BIN="$(pwd)/target/debug/hm" uv run pytest tests/ -v
    cargo llvm-cov report --lcov --output-path lcov.info
    cargo llvm-cov report --summary-only

# Lint with clippy
clippy:
    cargo clippy -- -W clippy::all

# Check formatting
fmt-check:
    cargo fmt -- --check

# Format code
fmt:
    cargo fmt

# ─── Run ───────────────────────────────────────────────────────────

# Launch a supervised session
run id +cmd:
    cargo run -- run --id {{ id }} -- {{ cmd }}

# Attach to a session
attach id:
    cargo run -- attach {{ id }}

# List sessions
ls:
    cargo run -- ls

# Session status
status id:
    cargo run -- status {{ id }}

# Kill a session
kill id:
    cargo run -- kill {{ id }}

# ─── Dev ───────────────────────────────────────────────────────────

# Check dependencies
doctor:
    @echo "Checking dependencies..."
    @which cargo >/dev/null 2>&1 && echo "  cargo: $(cargo --version)" || echo "  cargo: MISSING"
    @which just >/dev/null 2>&1 && echo "  just: $(just --version)" || echo "  just: MISSING"
    @which uv >/dev/null 2>&1 && echo "  uv: $(uv --version)" || echo "  uv: MISSING (needed for Python attach tests)"
    @which cargo-llvm-cov >/dev/null 2>&1 && echo "  cargo-llvm-cov: ok" || echo "  cargo-llvm-cov: MISSING (optional, for coverage)"
    @echo "Done."

# Clean build artifacts
clean:
    cargo clean
