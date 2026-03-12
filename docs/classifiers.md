# Classifiers

Heimdall infers what the supervised process is doing by analysing pty output
byte patterns. This is done by a **state classifier** — a pluggable component
selected at runtime via config.

## The `StateClassifier` trait

```rust
pub trait StateClassifier: Send {
    fn record(&mut self, byte_count: usize, now_ms: u64);
    fn tick(&mut self, now_ms: u64);
    fn state(&self) -> ProcessState;
    fn state_ms(&self, now_ms: u64) -> u32;
    fn set_dead(&mut self, now_ms: u64);
    fn state_name(&self, state: u8) -> &'static str;
}
```

- **`record`** — called on every pty read with the chunk size and current
  timestamp. This is where pattern analysis happens.
- **`tick`** — called periodically (on status queries) without new output.
  Allows idle transitions to fire even when nothing is being written.
- **`state`** / **`state_ms`** — current state and how long it's been held.
- **`set_dead`** — forced transition when the child exits (SIGCHLD).
- **`state_name`** — maps a state byte to a human-readable string for CLI
  output.

## Process states

```
ProcessState::Idle      = 0x00   No output for >= idle threshold       (all)
ProcessState::Thinking  = 0x01   Spinner-like pattern: small bursts    (claude)
ProcessState::Streaming = 0x02   High-frequency variable-size output   (claude)
ProcessState::ToolUse   = 0x03   Large burst after a pause             (claude)
ProcessState::Active    = 0x04   Generic "producing output"            (simple)
ProcessState::Dead      = 0xFF   Child exited                          (all)
```

Each classifier uses a subset. The full enum is the distinct union of all
classifiers so that clients can handle any state byte regardless of which
classifier produced it. The `none` classifier always reports Idle.

## Built-in classifiers

### `claude` (default)

Full state machine tuned for Claude Code's terminal output patterns.

Uses a sliding window of the last 20 output events. For each new event it:

1. Checks silence duration against `idle_threshold_ms` (default 3000ms).
2. Looks for large bursts (>4KB) → ToolUse.
3. Looks for pause-then-burst patterns (>200ms gap, >1KB) → ToolUse.
4. Computes mean/stddev of recent burst sizes and inter-burst gaps:
   - Uniform small bursts (40–120 bytes) at regular intervals (30–200ms) → Thinking.
   - Variable bursts or high stddev at high frequency → Streaming.
5. Falls back to Thinking if there's recent output that doesn't match other
   patterns.

State transitions are **debounced** (`debounce_ms`, default 200ms) to prevent
rapid flickering. Idle transitions are instant since silence is unambiguous.

### `simple`

Binary idle/active classifier. Reports:
- **Idle** when silence exceeds the threshold.
- **Active** when there's been recent output.

No pattern analysis. Useful for processes where you only care whether
something is happening or not.

### `none`

Null classifier. Always reports Idle. Use when you only need pty supervision,
scrollback, and socket IPC — no state inference.

## Configuration

Set the classifier in `heimdall.toml`:

```toml
classifier = "claude"    # or "simple" or "none"
idle_threshold_ms = 3000
debounce_ms = 200
```

The `idle_threshold_ms` and `debounce_ms` values are passed to whichever
classifier is selected. The `none` classifier ignores both.
