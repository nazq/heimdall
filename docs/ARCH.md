# Architecture

Heimdall is a PTY session supervisor. It forks a child process inside a
pseudo-terminal, manages the process group lifecycle, and exposes a Unix
domain socket so any number of clients can observe or interact with the
session concurrently.

## Design principles

- **One process, one session.** Each `hm run` instance supervises exactly one
  child. Multiple sessions means multiple `hm` processes. No daemon, no
  multiplexer state to corrupt.
- **Fork before async.** The child is forked before the Tokio runtime starts.
  This satisfies the single-threaded requirement for safe `fork()` and keeps
  the async runtime free of pre-fork state.
- **Process groups, not PIDs.** The child calls `setsid()` making its PID the
  PGID. All signals (`SIGTERM`, `SIGKILL`) target `-pgid`, killing the entire
  tree — grandchildren included.
- **Clients are just socket connections.** The built-in `attach`, `status`,
  `ls`, and `kill` subcommands are thin clients over the same Unix socket
  protocol. Any program that speaks the binary framing protocol is a
  first-class client.

## Process lifecycle

```
hm run --id foo -- bash
│
├─ parse CLI args
├─ load config (heimdall.toml)
├─ check PID file (abort if session already running)
├─ openpty() — allocate master/slave pair
├─ fork()
│   ├─ [child] setsid, dup2 slave → stdio, set env, chdir, execvp
│   └─ [parent] close slave fd, write PID file
├─ start tokio runtime (single-threaded)
├─ bind Unix socket
├─ event loop:
│   ├─ pty master readable → push to scrollback + broadcast to subscribers
│   ├─ SIGCHLD → reap child, broadcast EXIT frame
│   └─ SIGTERM → kill process group, reap, broadcast EXIT frame
└─ cleanup: remove socket + PID file, exit with child's code
```

## Module map

```
src/
├── main.rs          CLI (clap), subcommands, supervisor event loop
├── config.rs        TOML config loading + resolution
├── pty.rs           fork/exec, pre-exec seam, process group signals
├── socket.rs        Unix socket server, per-client handler
├── protocol.rs      Binary framing: pack/unpack/read/write
├── broadcast.rs     Output fan-out, scrollback ring buffer
└── classify/
    ├── mod.rs       StateClassifier trait + factory
    ├── claude.rs    Sliding window classifier for Claude Code
    ├── simple.rs    Binary idle/active classifier
    └── none.rs      Null classifier (always idle)
```

## Data flow

```
                    ┌──────────┐
                    │  child   │
                    │ process  │
                    └────┬─────┘
                         │ pty slave (stdin/stdout/stderr)
                    ┌────┴─────┐
                    │pty master│
                    └────┬─────┘
                         │ read loop
                    ┌────┴─────┐
                    │broadcast │──→ state classifier
                    │          │──→ scrollback ring buffer
                    └────┬─────┘
                         │ tokio broadcast channel
              ┌──────────┼──────────┐
              ▼          ▼          ▼
         ┌────────┐ ┌────────┐ ┌────────┐
         │client 1│ │client 2│ │client N│
         └────────┘ └────────┘ └────────┘
              Unix socket connections
```

1. Pty output bytes are read from the master fd.
2. Each chunk is pushed to the `OutputState` which:
   - Records the timestamp for idle detection.
   - Feeds the byte count to the state classifier.
   - Appends to the scrollback ring buffer.
   - Broadcasts via a tokio channel to all subscribed clients.
3. New subscribers receive the full scrollback snapshot first, then live output.

## Pre-exec seam

The gap between `fork()` and `execvp()` is the most powerful boundary in the
supervisor. Everything set here is inherited by the child and its entire
process tree.

Currently used for:
- `setsid()` — new session and process group
- Pty slave wired to stdin/stdout/stderr
- Session ID env var (configurable name, default `HEIMDALL_SESSION_ID`)
- Extra env vars from config
- Working directory

Reserved for future use (not implemented until needed):
- `setrlimit` — per-session resource limits
- Cgroup assignment — memory/CPU isolation
- Seccomp filters — syscall restriction
- Namespace isolation — filesystem/network/PID
- Fd cleanup — close inherited fds the child shouldn't see

## Further reading

- [Protocol](protocol.md) — wire format, message types, connection lifecycle
- [Classifiers](classifiers.md) — state detection, the `StateClassifier` trait, writing custom classifiers
- [Configuration](configuration.md) — config resolution, all options, per-project and global config
