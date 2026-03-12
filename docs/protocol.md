# Protocol

Heimdall uses a binary framing protocol over Unix domain sockets. The format
is designed for minimal overhead (5 bytes per frame) and straightforward
implementation in any language.

## Wire format

Every frame follows the same structure:

```
[type: u8][length: u32 BE][payload: <length> bytes]
```

- **type** — single byte identifying the message kind.
- **length** — 4-byte big-endian unsigned integer. Zero is valid (empty payload).
- **payload** — `length` bytes of data. Interpretation depends on `type`.

Total overhead per frame: 5 bytes.

## Connection handshake

On connect, the supervisor immediately writes a single **mode byte**:

| Byte   | Meaning        |
|--------|----------------|
| `0x00` | Binary framing |

The client must read this byte before sending any frames. This byte is not
framed — it's a raw single byte on the wire.

## Message types

### Client → Supervisor (0x01–0x7F)

| Type   | Name        | Payload                              |
|--------|-------------|--------------------------------------|
| `0x01` | `INPUT`     | Raw bytes to write to the pty master |
| `0x02` | `SUBSCRIBE` | Empty — switch to subscriber mode    |
| `0x03` | `STATUS`    | Empty — request status response      |
| `0x04` | `RESIZE`    | `[cols: u16 BE][rows: u16 BE]`       |
| `0x05` | `KILL`      | Empty — send SIGTERM to process group|

### Supervisor → Client (0x80–0xFF)

| Type   | Name          | Payload                                                         |
|--------|---------------|-----------------------------------------------------------------|
| `0x81` | `OUTPUT`      | Raw pty output bytes                                            |
| `0x82` | `STATUS_RESP` | See [Status payload](#status-payload)                           |
| `0x83` | `EXIT`        | `[code: i32 BE]`                                                |

## Status payload

The `STATUS_RESP` payload is 15 bytes:

```
[pid: u32 BE][idle_ms: u32 BE][alive: u8][state: u8][state_ms: u32 BE][reserved: u8]
```

| Offset | Size | Field      | Description                           |
|--------|------|------------|---------------------------------------|
| 0      | 4    | `pid`      | Child process PID                     |
| 4      | 4    | `idle_ms`  | Milliseconds since last pty output    |
| 8      | 1    | `alive`    | `1` if child is running, `0` if dead  |
| 9      | 1    | `state`    | Classifier state byte (see below)     |
| 10     | 4    | `state_ms` | Milliseconds in current state         |
| 14     | 1    | reserved   | Always `0x00`                         |

State bytes (distinct union of all classifier states):

| Byte   | State       | Classifiers        |
|--------|-------------|--------------------|
| `0x00` | Idle        | all                |
| `0x01` | Thinking    | `claude`           |
| `0x02` | Streaming   | `claude`           |
| `0x03` | ToolUse     | `claude`           |
| `0x04` | Active      | `simple`           |
| `0xFF` | Dead        | all                |

## Subscriber mode

When a client sends `SUBSCRIBE`, the connection transitions to subscriber
mode:

1. The supervisor replays the **scrollback buffer** as a series of `OUTPUT`
   frames.
2. Live pty output is streamed as `OUTPUT` frames in real time via a broadcast
   channel.
3. The client can continue sending `INPUT`, `RESIZE`, `KILL`, and `STATUS`
   frames while subscribed.
4. When the child exits, all subscribers receive an `EXIT` frame.

Subscribers that fall behind (slow readers) may have messages dropped. The
client receives no error — it simply misses some output. The scrollback
buffer ensures late joiners still get recent context.

## Implementing a client

Minimal client pseudocode:

```
connect to Unix socket
read 1 byte (mode byte, assert == 0x00)

# Request status
write frame(0x03, empty)
read frame → (0x82, status_payload)

# Or subscribe for output
write frame(0x02, empty)
loop:
    read frame → (type, payload)
    if type == 0x81: handle output
    if type == 0x83: handle exit, break
```

Any language with Unix socket support and the ability to read/write bytes can
be a heimdall client.
