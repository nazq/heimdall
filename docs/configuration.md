# Configuration

Heimdall is zero-config by default. All settings have sensible defaults and
the binary works without any config file. Configuration is available for
tuning behaviour per-project or globally.

## Resolution order

Heimdall resolves config in this order:

1. **`--config <path>`** — explicit path passed on the CLI. Errors if the
   file doesn't exist.
2. **`./heimdall.toml`** — in the current working directory. This is the
   recommended approach for per-project config.
3. **`~/.config/heimdall/heimdall.toml`** — global user config.
4. **Built-in defaults** — if no config file is found.

The first file found wins. Files are not merged — if `./heimdall.toml`
exists, the global config is not read.

## Precedence

Within a single run, configuration follows a three-tier precedence:

**CLI flags > config file > built-in defaults**

For example, `hm run --idle-threshold-ms 5000` overrides whatever
`idle_threshold_ms` is set in the config file, which itself overrides the
built-in default of 3000.

## All options

```toml
# Directory for Unix sockets and PID files.
# Default: ~/.local/share/heimdall/sessions
socket_dir = "/path/to/sessions"

# Scrollback buffer size in bytes.
# Replayed to clients that connect after output has started.
# Default: 65536 (64 KB)
scrollback_bytes = 65536

# Environment variable name injected into the child process with the session ID.
# Default: HEIMDALL_SESSION_ID
session_env_var = "HEIMDALL_SESSION_ID"

# Signal the entire process group on kill/shutdown.
# When true (default), SIGTERM/SIGKILL reach all grandchild processes.
# Set to false to only signal the direct child.
kill_process_group = true

# Log file path for the supervisor.
# Default: <socket_dir>/<id>.log. Set to /dev/null to disable.
# log_file = "/var/log/heimdall/session.log"

# Log level for the supervisor (trace, debug, info, warn, error).
# RUST_LOG env var overrides this.
# Default: info
log_level = "info"

# Detach key byte for the attach client.
# Default: 28 (0x1C, Ctrl-\). Set to 0 to disable.
# detach_key = 28

# State classifier — string shorthand (built-in defaults):
classifier = "simple"    # or "claude" or "none"

# Or table form with per-classifier parameters:
# [classifier.simple]
# idle_threshold_ms = 3000

# [classifier.claude]
# idle_threshold_ms = 3000
# debounce_ms = 200

# [classifier.none]

# Extra environment variables injected into the child process.
# Set in the pre-exec seam after fork(), inherited by the child's entire tree.
[[env]]
name = "MY_VAR"
value = "my_value"
```

## Classifier parameters

Each classifier carries its own parameters. See
[classifiers.md](classifiers.md) for full details.

| Parameter | Classifiers | Default | Description |
|---|---|---|---|
| `idle_threshold_ms` | simple, claude | 3000 | Silence duration (ms) before idle |
| `debounce_ms` | claude | 200 | State transition debounce (ms) |

## CLI flags for `hm run`

| Flag | Overrides | Description |
|---|---|---|
| `--socket-dir` | `socket_dir` | Socket/PID directory |
| `--scrollback-bytes` | `scrollback_bytes` | Scrollback buffer size |
| `--session-env-var` | `session_env_var` | Env var name for session ID |
| `--kill-process-group` | `kill_process_group` | Process group signalling |
| `--classifier` | `classifier` | Classifier type (simple/claude/none) |
| `--idle-threshold-ms` | per-classifier | Idle detection threshold |
| `--debounce-ms` | per-classifier | State transition debounce |
| `--log-file` | `log_file` | Supervisor log file (default: `<socket_dir>/<id>.log`) |
| `--log-level` | `log_level` | Log level: trace, debug, info, warn, error |

## Per-project config

Drop a `heimdall.toml` in your project root. When you run `hm run` from that
directory, it picks up the local config automatically.

```
my-project/
├── heimdall.toml      ← picked up automatically
├── src/
└── ...
```

This is useful for setting per-project classifiers, env vars, or idle
thresholds. For example, a project using Claude Code might use the `claude`
classifier while a build server supervisor might use `simple`.

## Global config

For settings that apply to all sessions, use the global path:

```
~/.config/heimdall/heimdall.toml
```

## Environment variables

Heimdall injects the following env var into every child process:

| Variable                 | Description                          |
|--------------------------|--------------------------------------|
| `HEIMDALL_SESSION_ID`    | Session ID passed via `--id`         |

The variable name is configurable via `session_env_var`. Additional variables
can be injected via the `[[env]]` table.
