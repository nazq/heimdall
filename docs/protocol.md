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

| Byte   | Meaning                    |
|--------|----------------------------|
| `0x00` | Binary framing (active)    |
| `0x01` | Text/debug mode (reserved) |

The mode byte exists so that future versions can offer a human-readable text
protocol (mode `0x01`) where you could connect with `socat` or `netcat` and
interact without a custom client. Today only binary framing (`0x00`) is
implemented — the supervisor always sends `0x00`, and clients should assert
this value. Mode `0x01` is reserved for future use and is not handled by the
supervisor or any built-in client.

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

## Example: minimal Go client

A self-contained subscriber that connects to a heimdall session, prints pty
output to stdout, and exits with the child's exit code.

```go
package main

import (
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"os"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintf(os.Stderr, "usage: %s <socket-path>\n", os.Args[0])
		os.Exit(1)
	}

	conn, err := net.Dial("unix", os.Args[1])
	if err != nil {
		fmt.Fprintf(os.Stderr, "connect: %v\n", err)
		os.Exit(1)
	}
	defer conn.Close()

	// Read mode byte — must be 0x00 (binary framing).
	mode := make([]byte, 1)
	if _, err := io.ReadFull(conn, mode); err != nil {
		fmt.Fprintf(os.Stderr, "read mode byte: %v\n", err)
		os.Exit(1)
	}
	if mode[0] != 0x00 {
		fmt.Fprintf(os.Stderr, "unsupported mode: 0x%02x\n", mode[0])
		os.Exit(1)
	}

	// Send SUBSCRIBE frame: type=0x02, length=0.
	subscribe := []byte{0x02, 0x00, 0x00, 0x00, 0x00}
	if _, err := conn.Write(subscribe); err != nil {
		fmt.Fprintf(os.Stderr, "send subscribe: %v\n", err)
		os.Exit(1)
	}

	// Read frames until EXIT.
	header := make([]byte, 5)
	for {
		if _, err := io.ReadFull(conn, header); err != nil {
			fmt.Fprintf(os.Stderr, "read frame: %v\n", err)
			os.Exit(1)
		}
		msgType := header[0]
		length := binary.BigEndian.Uint32(header[1:5])

		payload := make([]byte, length)
		if length > 0 {
			if _, err := io.ReadFull(conn, payload); err != nil {
				fmt.Fprintf(os.Stderr, "read payload: %v\n", err)
				os.Exit(1)
			}
		}

		switch msgType {
		case 0x81: // OUTPUT — write pty data to stdout.
			os.Stdout.Write(payload)
		case 0x83: // EXIT — child exited, payload is i32 BE exit code.
			code := int32(binary.BigEndian.Uint32(payload))
			os.Exit(int(code))
		}
	}
}
```
