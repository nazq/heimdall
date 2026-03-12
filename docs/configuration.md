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

# State classifier: "claude", "simple", or "none".
# See docs/classifiers.md for details.
# Default: "claude"
classifier = "claude"

# Milliseconds of silence before the classifier transitions to Idle.
# Default: 3000
idle_threshold_ms = 3000

# Milliseconds a candidate state must persist before the classifier commits
# to the transition. Prevents rapid flickering on ambiguous output.
# Default: 200
debounce_ms = 200

# Extra environment variables injected into the child process.
# These are set in the pre-exec seam after fork(), so they're inherited by
# the child's entire process tree.
[[env]]
name = "MY_VAR"
value = "my_value"
```

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
