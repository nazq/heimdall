<div align="center">

# Heimdall

**PTY session supervisor with teeth.**

Fork. Watch. Control. From anywhere.

[![CI](https://github.com/nazq/heimdall/actions/workflows/ci.yml/badge.svg)](https://github.com/nazq/heimdall/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/nazq/heimdall/graph/badge.svg)](https://codecov.io/gh/nazq/heimdall)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://www.rust-lang.org/)

</div>

---

Heimdall supervises a process inside a pseudo-terminal, owns the entire
process group lifecycle, and exposes a Unix socket so any number of clients
can attach, observe, query status, or send input — concurrently, from
anywhere.

It was built to solve a specific problem: supervising long-running AI coding
agents (Claude Code sessions) that need to be monitored, attached to from
multiple terminals, and cleanly killed — including all their grandchild
processes. But there's nothing AI-specific in the core. If it runs in a
terminal, Heimdall can supervise it.

## Why not just tmux?

tmux is a terminal multiplexer. Heimdall is a process supervisor. Different
tools, different jobs.

| Capability | tmux | screen | zellij | Heimdall |
|---|---|---|---|---|
| Terminal multiplexer (splits, tabs) | Yes | Yes | Yes | No |
| PTY supervision (fork, own, reap) | Side effect | Side effect | Side effect | Core purpose |
| Process group kill (`kill -pgid`) | No | No | No | Yes |
| Multi-client attach (concurrent) | One at a time | One at a time | One at a time | Unlimited |
| Binary socket protocol (5-byte frames) | No | No | No | Yes |
| Scrollback replay for late joiners | Per-pane buffer | Per-window | Per-pane | Ring buffer, streamed on subscribe |
| Process state classification | No | No | No | Pluggable (idle/thinking/streaming/tool_use) |
| Structured status queries | No | No | No | Binary STATUS frame with PID, idle time, state |
| Pre-exec seam (env, workdir, future: cgroups) | Limited | Limited | No | Full control of fork/exec boundary |
| Config per project | `.tmux.conf` | `.screenrc` | `config.kdl` | `./heimdall.toml` |
| Zero dependencies at runtime | Needs server | Needs server | Needs server | Single static binary |
| Grandchild cleanup on kill | No | No | No | Yes (`setsid` + `-pgid`) |

Heimdall doesn't replace tmux — it replaces the part of tmux you were
misusing as a process supervisor.

## Quick start

### Download a release (recommended)

Grab the latest binary for your platform from
[**Releases**](https://github.com/nazq/heimdall/releases/latest),
or copy the one-liner for your system:

**Linux x86_64:**
```bash
curl -fsSL https://github.com/nazq/heimdall/releases/latest/download/heimdall-x86_64-unknown-linux-gnu.tar.gz | tar xz -C ~/.local/bin --strip-components=1
```

**Linux ARM64:**
```bash
curl -fsSL https://github.com/nazq/heimdall/releases/latest/download/heimdall-aarch64-unknown-linux-gnu.tar.gz | tar xz -C ~/.local/bin --strip-components=1
```

**macOS (Apple Silicon):**
```bash
curl -fsSL https://github.com/nazq/heimdall/releases/latest/download/heimdall-aarch64-apple-darwin.tar.gz | tar xz -C ~/.local/bin --strip-components=1
```

**macOS (Intel):**
```bash
curl -fsSL https://github.com/nazq/heimdall/releases/latest/download/heimdall-x86_64-apple-darwin.tar.gz | tar xz -C ~/.local/bin --strip-components=1
```

| Platform | Target |
|---|---|
| Linux x86_64 | `heimdall-*-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `heimdall-*-aarch64-unknown-linux-gnu.tar.gz` |
| macOS x86_64 | `heimdall-*-x86_64-apple-darwin.tar.gz` |
| macOS ARM64 (Apple Silicon) | `heimdall-*-aarch64-apple-darwin.tar.gz` |

### Install from source

```bash
cargo install --git https://github.com/nazq/heimdall
```

### Or build locally

```bash
git clone https://github.com/nazq/heimdall
cd heimdall
cargo build --release
cp target/release/hm ~/.local/bin/
```

The binary is called `hm`.

### Run a supervised session

```bash
# Supervise any command
hm run --id my-session -- bash
hm run --id build -- make -j$(nproc)
hm run --id agent -- claude

# From another terminal
hm attach my-session        # full terminal passthrough (Ctrl-\ to detach)
hm status my-session        # structured status query
hm ls                       # list active sessions
hm kill my-session          # SIGTERM to entire process group, SIGKILL after 5s
```

### Configure (optional)

Drop a `heimdall.toml` in your project directory, or at
`~/.config/heimdall/heimdall.toml` for global defaults:

```toml
classifier = "claude"          # "claude", "simple", or "none"
idle_threshold_ms = 3000
scrollback_bytes = 65536

[[env]]
name = "MY_API_KEY"
value = "sk-..."
```

See [`heimdall.example.toml`](heimdall.example.toml) for all options.

## How it works

```
hm run --id foo -- bash
│
├─ fork() before async runtime (single-threaded safety)
│   ├─ child: setsid → new process group, pty slave → stdio, exec
│   └─ parent: own master fd, write PID file
├─ tokio event loop (single-threaded)
│   ├─ pty read → scrollback + broadcast to subscribers
│   ├─ SIGCHLD → reap, broadcast EXIT
│   └─ SIGTERM → kill(-pgid), reap, broadcast EXIT
└─ cleanup: remove socket + PID, exit with child's code
```

Clients connect via Unix socket at `~/.local/share/heimdall/sessions/<id>.sock`.
The binary framing protocol is 5 bytes overhead per message — trivial to
implement in virtually any language.

<details>
<summary>Sorry Java-nauts and friends, here are some links to socket writing in your langs...</summary>

| Language | Unix Socket Support |
|---|---|
| Java | [`UnixDomainSocketAddress`](https://docs.oracle.com/en/java/javase/16/docs/api/java.base/java/net/UnixDomainSocketAddress.html) (Java 16+) |
| C# / .NET | [`UnixDomainSocketEndPoint`](https://learn.microsoft.com/en-us/dotnet/api/system.net.sockets.unixdomainsocketendpoint) (.NET 5+) |
| Erlang/Elixir | [`:gen_tcp` with `{:local, path}`](https://www.erlang.org/doc/man/gen_tcp.html) |
| Dart | [`RawSocket`](https://api.dart.dev/stable/dart-io/RawSocket-class.html) |

It builds character.

</details>

## Comparison with other tools

| Tool | What it does | How Heimdall differs |
|---|---|---|
| **tmux / screen / zellij** | Terminal multiplexing with session persistence | Heimdall is a supervisor, not a multiplexer. No splits, no tabs — just process ownership, group lifecycle, and a programmatic socket API. |
| **supervisord** | Daemon process manager (config-driven, many processes) | Heimdall supervises one process per instance with a pty. Supervisord has no pty, no attach, no scrollback. |
| **systemd** | System/service manager | Heimdall is user-space, per-session, interactive. Systemd services are headless. |
| **dtach / abduco** | Minimal detach/reattach for a single program | Close in spirit but no socket protocol, no multi-client, no state classification, no process group kill. |
| **script** | Record terminal session to file | Capture only. No attach, no IPC, no lifecycle management. |
| **expect / empty** | Scriptable terminal interaction | Automation tools, not supervisors. No persistent sessions, no multi-client. |
| **nohup / disown** | Survive terminal hangup (SIGHUP immunity) | Fire-and-forget. No reattach, no output access, no lifecycle control. Heimdall keeps the session alive *and* accessible. |
| **reptyr / neercs** | Steal/migrate a running process into a pty | Process migration, not supervision. Heimdall owns the process from birth. |

## Documentation

| Document | Description |
|---|---|
| [Architecture](docs/ARCH.md) | Design principles, process lifecycle, module map, data flow |
| [Protocol](docs/protocol.md) | Wire format, message types, status payload, subscriber mode |
| [Classifiers](docs/classifiers.md) | State detection, `StateClassifier` trait, built-in classifiers |
| [Configuration](docs/configuration.md) | Config resolution, all options, per-project and global config |
| [Example config](heimdall.example.toml) | Annotated config file with all options |

## Building

```bash
just check       # clippy + fmt + tests
just release      # optimised binary
just install      # copy to ~/.local/bin
just cov          # test coverage (requires cargo-llvm-cov)
just doctor       # verify toolchain
```

## License

MIT
