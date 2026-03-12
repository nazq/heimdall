# Heimdall — Project Rules

## What is it

PTY session supervisor. Forks a child process in a pty, manages its lifecycle via process groups, and exposes a Unix socket for multi-client IPC with binary framing.

## Quick Commands

```bash
cargo build                    # dev build
cargo test                     # run tests
cargo clippy -- -W clippy::all # lint
cargo build --release          # optimised binary at target/release/hm
```

## Architecture

- **Binary name:** `hm`
- **Config:** `./heimdall.toml` (CWD first), then `~/.config/heimdall/heimdall.toml`, or `--config <path>`
- **Socket dir:** `~/.local/share/heimdall/sessions/` (default)
- **Binary framing:** `[type: u8][len: u32 BE][payload]` — 5-byte overhead per frame
- **Process lifecycle:** fork before tokio, setsid for new process group, kill(-pgid) for cleanup
- **State classifier:** pluggable via config — `claude` (full state machine), `simple` (idle/active), `none`
- **Scrollback:** ring buffer with configurable max bytes, replayed to late-joining subscribers

## Module Map

- `main.rs` — CLI (clap), subcommands: run, attach, status, ls, kill
- `config.rs` — TOML config loading with serde
- `pty.rs` — fork/exec, pre-exec seam, process group signals
- `socket.rs` — Unix socket server, per-client handler, subscribe mode
- `protocol.rs` — binary framing, pack/unpack helpers
- `broadcast.rs` — output fan-out, scrollback ring buffer
- `classify/` — StateClassifier trait + implementations (claude, simple, none)

## Conventions

- Rust 2024 edition
- clippy clean with `-W clippy::all`
- Single-threaded tokio runtime (fork safety)
- All signal handling via process groups (kill -pgid), not individual PIDs
