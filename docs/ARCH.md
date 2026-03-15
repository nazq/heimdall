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
  This satisfies the single-threaded requirement for safe
  [`fork()`](https://docs.rs/nix/latest/nix/unistd/fn.fork.html) and keeps
  the async runtime free of pre-fork state.
- **Process groups, not PIDs.** The child calls `setsid()` making its PID the
  PGID. All signals (`SIGTERM`, `SIGKILL`) target `-pgid`, killing the entire
  tree — grandchildren included. This behavior is configurable via
  `kill_process_group` in `heimdall.toml` (default: `true`). When disabled,
  signals target only the direct child PID.
- **Clients are just socket connections.** The built-in `attach`, `status`,
  `ls`, and `kill` subcommands are thin clients over the same
  [Unix domain socket](https://man7.org/linux/man-pages/man7/unix.7.html)
  protocol. Any program that speaks the binary framing protocol is a
  first-class client.

## Process lifecycle

When you run `hm run --id foo -- bash`, the supervisor walks through these
steps in order. Understanding the sequence matters because each step depends
on the previous one, and the fork/exec boundary is where the child's
environment is permanently set.

```
hm run --id foo -- bash
│
├─ parse CLI args
├─ load config (heimdall.toml)
├─ flock PID file (abort if locked or PIDs alive)
├─ write supervisor PID (line 1)
├─ openpty() — allocate master/slave pair
├─ fork()
│   ├─ [child] setsid, dup2 slave → stdio, set env, chdir, execvp
│   └─ [parent] close slave fd, write child PID (line 2)
├─ start tokio runtime (single-threaded)
├─ bind Unix socket
├─ event loop:
│   ├─ pty master readable → push to scrollback + broadcast to subscribers
│   ├─ SIGCHLD → reap child, broadcast EXIT frame
│   └─ SIGTERM → kill process group, reap, broadcast EXIT frame
└─ cleanup: remove socket + PID file, exit with child's code
```

### What each step does

1. **Parse CLI args** — Clap extracts the session ID, the command to run, and
   any flags. Nothing interesting happens here.

2. **Load config** — Reads `heimdall.toml` (CWD, then `~/.config/heimdall/`,
   or `--config`). Config controls socket directory, scrollback size,
   classifier selection (with per-classifier parameters), process group
   kill behaviour, and extra environment variables. CLI flags override
   config file values.

3. **Check PID file** — Each session writes a two-line PID file
   (`<supervisor_pid>\n<child_pid>`) protected by an `flock`. If the lock
   is held by another process, the supervisor aborts with diagnostics
   (holder's PID, uptime, command line from `/proc`). If the lock is
   available but the file contains PIDs that are still alive, the
   supervisor aborts to prevent two supervisors fighting over the same
   socket and process group. Stale PID files (dead processes) are
   overwritten.

4. **openpty()** — Allocates a pseudo-terminal pair: a *master* fd and a
   *slave* fd. The master side is what the supervisor reads/writes. The slave
   side becomes the child's terminal — its stdin, stdout, and stderr all
   point to it, so the child thinks it's running in a real terminal.

5. **fork()** — Splits the process into parent and child. This happens
   *before* the async runtime starts, because `fork()` in a multi-threaded
   process is unsafe (locks held by other threads become permanently locked
   in the child).

6. **[child] setsid** — The child calls `setsid()` to create a new session
   and process group. This detaches it from the parent's terminal and makes
   the child's PID the process group leader. All processes spawned by the
   child inherit this group, so the supervisor can signal the entire tree
   at once with `kill(-pgid, signal)`.

7. **[child] dup2 slave to stdio** — The child uses `dup2()` to replace its
   stdin (fd 0), stdout (fd 1), and stderr (fd 2) with the slave fd. After
   this, anything the child prints goes through the PTY, and anything the
   supervisor writes to the master fd appears as the child's input.

8. **[child] set env, chdir, execvp** — Environment variables are set
   (including the session ID), the working directory is changed if
   configured, and `execvp()` replaces the child process image with the
   requested command. From this point, the child IS the command (e.g. bash).

9. **[parent] close slave fd, write PID file** — The parent doesn't need the
   slave side (only the child uses it). Closing it ensures the PTY sends EOF
   properly when the child exits. The supervisor PID is written to line 1
   of the PID file before fork; the child PID is appended to line 2 after
   fork. Both are protected by the flock acquired at step 3.

10. **Start Tokio runtime** — A single-threaded async runtime starts. It's
    single-threaded because the fork already happened — there's no need for
    multiple OS threads, and it keeps the supervisor lightweight.

11. **Bind Unix socket** — The socket is created at
    `<socket_dir>/<session_id>.sock`. Clients connect here to subscribe to
    output, send input, or query status.

12. **Event loop** — The supervisor multiplexes three concerns: reading PTY
    output (and fanning it out to subscribers), catching `SIGCHLD` (child
    exited), and catching `SIGTERM` (someone asked the supervisor to stop).

13. **Cleanup** — After the child exits (or the supervisor is told to stop),
    the socket and PID file are removed, and the supervisor exits with the
    child's exit code.

## Run modes and the two-process model

`hm run` has two modes that determine the process architecture:

### `hm run --id foo -- bash` (default: launch and attach)

This spawns **two independent processes**:

1. The original process re-execs itself as `hm run --detach --id foo -- bash`,
   which starts the supervisor in the background.
2. Once the supervisor's socket appears, the original process becomes a pure
   **attach client** — connecting to the socket, setting up raw terminal mode,
   and running the terminal passthrough loop.

```
hm run --id foo -- bash
│
├─ spawn "hm run --detach --id foo -- bash" (background)
│   └─ supervisor process (owns pty, binds socket, event loop)
│
├─ poll for socket to appear
│
└─ attach client (raw mode, status bar, select loop)
    └─ connects to supervisor via Unix socket
```

The supervisor calls `setsid()` before starting, placing itself in a new
process session. This is the key isolation boundary: the supervisor and its
child belong to a different session from the user's terminal.

If you close the terminal window (which sends `SIGHUP` to all processes in
the terminal's session), only the attach client dies. The supervisor and its
child keep running. You can reattach later with `hm attach foo`.

### `hm run --detach --id foo -- bash` (headless)

A single process: the supervisor runs in the foreground with no terminal UI.
Used when something else manages the lifecycle — a web dashboard, systemd,
CI, or scripts that interact via the socket API.

### `hm attach foo` (reconnect)

Connects to an already-running supervisor. Identical to the attach phase of
the default mode — same terminal passthrough, same status bar, same signals.
Multiple clients can attach to the same session simultaneously.

This is the same basic mechanism that `tmux` and `screen` use, but without
the multiplexer complexity. Each session is one supervisor process, and
clients are just socket connections.

## Session ID environment variable

The supervised child process (and all its descendants) receive an environment
variable containing the session ID. By default this variable is named
`HEIMDALL_SESSION_ID`, but the name is configurable in `heimdall.toml`.

This serves as the identity mechanism for the entire process tree. Hooks,
plugins, and child processes can read this variable to determine which
Heimdall session they belong to. For example, a post-exit hook script can
use `$HEIMDALL_SESSION_ID` to report results to the correct session, or a
long-running child can use it for logging and metrics attribution.

## Module map

```
src/
├── main.rs          Entry point: CLI parse, config merge, dispatch
├── cli.rs           Clap structs, RunArgs, SessionParams, config merge
├── supervisor.rs    Fork child, PID lock, event loop, cleanup guard
├── attach.rs        launch_and_attach, terminal passthrough, signal handling
├── commands.rs      Status, list, kill subcommands
├── terminal.rs      ANSI consts, status bar rendering, termios guard
├── config.rs        TOML config loading + resolution
├── pty.rs           fork/exec, pre-exec seam, process group signals
├── socket.rs        Unix socket server, per-client handler
├── protocol.rs      Binary framing: pack/unpack/read/write
├── pidfile.rs       PID file abstraction: read, write, liveness checks
├── util.rs          Shared helpers: session_socket, with_runtime
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
                    │broadcast │──> state classifier
                    │          │──> scrollback ring buffer
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

The gap between `fork()` and `execvp()` is the most key boundary in the
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
