# Contributing to Heimdall

## Prerequisites

- Rust stable toolchain (rustup recommended)
- [just](https://github.com/casey/just) command runner
- [uv](https://docs.astral.sh/uv/) (for Python attach tests)
- Optional: `cargo-llvm-cov` for coverage reports

Run `just doctor` to verify your environment.

## Setup

```bash
git clone https://github.com/nazq/heimdall.git
cd heimdall
cargo build
uv sync          # install Python test deps (pexpect, pytest)
just doctor
```

## Just targets

| Target             | Description                                      |
|--------------------|--------------------------------------------------|
| `just check`       | Run all quality checks (clippy + fmt + tests)    |
| `just test`        | Run unit and Rust integration tests              |
| `just test-attach` | Run Python attach tests (requires `uv sync`)     |
| `just test-all`    | Full test suite (Rust + Python)                  |
| `just fmt`         | Format code                                      |
| `just fmt-check`   | Check formatting without modifying files         |
| `just clippy`      | Lint with clippy                                 |
| `just build`       | Debug build                                      |
| `just release`     | Release build                                    |
| `just install`     | Build release and install `hm` to `~/.local/bin` |
| `just cov`         | Generate coverage report (requires cargo-llvm-cov)|

## Running locally

```bash
# Start a supervised session
just run my-session bash

# Attach to it from another terminal
just attach my-session

# List running sessions
just ls

# Check session status
just status my-session

# Kill a session
just kill my-session
```

## Test expectations

All PRs must pass `just check` (clippy + format check + full test suite).

### Testing philosophy

Tests exist to prove the system works, not to prove the code compiles. Every
test must satisfy three criteria:

1. **Setup is correct** — the test creates the right preconditions and waits
   for them (e.g. socket appears before connecting).
2. **The operation runs** — the test actually exercises the code path it claims
   to test, not a happy-path shortcut.
3. **All invariants are asserted** — don't assert one field when the response
   has five. If a STATUS_RESP has pid, idle_ms, alive, state, and state_ms,
   assert the ones that have known-good values. Skipping fields hides bugs.

### Wire-level protocol tests

The protocol is documented in [`docs/protocol.md`](docs/protocol.md). Protocol
tests come in two flavours, and both are required:

- **Round-trip tests** — pack through `pack_*`, parse through `read_frame`,
  verify fields match. These catch regressions but have a blind spot: if pack
  and parse have the same bug (e.g. both swap two fields), the test passes
  while the wire format is silently wrong.
- **Golden byte tests** — assert that a known input produces an exact byte
  sequence. These pin the wire format to the documented spec and catch
  symmetric pack/parse bugs that round-trip tests cannot.

When adding a new frame type or modifying a payload layout, add both.

### Integration tests

- **Rust** (`tests/integration.rs`) — spawn real `hm` processes, connect over
  Unix sockets, send/receive protocol frames, assert responses byte-by-byte.
  These test the supervisor end-to-end without mocks.
- **Python** (`tests/test_attach.py`) — use pexpect over real PTYs to verify
  the terminal UX: alt screen, status bar, detach, signal forwarding, resize.
  Run via `just test-attach` (requires `uv sync`).

Both suites use temp directories for socket isolation and clean up processes
in fixtures/teardown.

## Commit style

Conventional commits. One logical change per commit.

```
feat: add scrollback size config option
fix: handle SIGCHLD race on rapid child exit
deps: bump nix to 0.29
ci: add aarch64-linux to release matrix
docs: clarify process group signaling in ARCH.md
```

## Code style

- `cargo fmt` -- all code must be formatted.
- `cargo clippy -- -W clippy::all` -- no warnings allowed.

## Documentation

See `docs/` for detailed documentation:

- [Architecture](docs/ARCH.md) -- design principles, process lifecycle, data flow
- [Protocol](docs/protocol.md) -- wire format and message types
- [Classifiers](docs/classifiers.md) -- state detection and custom classifiers
- [Configuration](docs/configuration.md) -- all config options
