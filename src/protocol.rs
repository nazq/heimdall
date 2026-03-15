//! Binary framing protocol for heimdall.
//!
//! Wire format: `[type: u8][len: u32 BE][payload: len bytes]`
//!
//! Client → Supervisor: 0x01–0x7F
//! Supervisor → Client: 0x80–0xFF

use bytes::{Bytes, BytesMut};
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// -- Client → Supervisor message types --

/// Write raw bytes to the pty master.
pub const INPUT: u8 = 0x01;
/// Subscribe to live pty output (scrollback + stream).
pub const SUBSCRIBE: u8 = 0x02;
/// Request a one-shot status response.
pub const STATUS: u8 = 0x03;
/// Resize the pty. Payload: `[cols: u16 BE][rows: u16 BE]` (4 bytes).
pub const RESIZE: u8 = 0x04;
/// Send SIGTERM to the child, then SIGKILL after timeout.
pub const KILL: u8 = 0x05;

// -- Supervisor → Client message types --

/// Raw pty output bytes. Sent to subscribed clients.
pub const OUTPUT: u8 = 0x81;
/// Status response. Payload (15 bytes):
/// `[pid: u32 BE][idle_ms: u32 BE][alive: u8][state: u8][state_ms: u32 BE]`.
pub const STATUS_RESP: u8 = 0x82;
/// Child process exited. Payload: `[code: i32 BE]` (4 bytes). Broadcast to all.
pub const EXIT: u8 = 0x83;

// -- Mode byte (first byte on connection) --

/// Binary framing mode.
pub const MODE_BINARY: u8 = 0x00;

/// Pack a frame into bytes: `[type][len BE][payload]`.
pub fn pack_frame(msg_type: u8, payload: &[u8]) -> Bytes {
    let len = payload.len() as u32;
    let mut buf = BytesMut::with_capacity(5 + payload.len());
    buf.extend_from_slice(&[msg_type]);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    buf.freeze()
}

/// Pack a status response payload (15 bytes).
pub fn pack_status(pid: u32, idle_ms: u32, alive: bool, state: u8, state_ms: u32) -> Bytes {
    let mut payload = [0u8; 15];
    payload[0..4].copy_from_slice(&pid.to_be_bytes());
    payload[4..8].copy_from_slice(&idle_ms.to_be_bytes());
    payload[8] = u8::from(alive);
    payload[9] = state;
    payload[10..14].copy_from_slice(&state_ms.to_be_bytes());
    // payload[14] reserved (zero).
    pack_frame(STATUS_RESP, &payload)
}

/// Pack a resize payload: `[cols: u16 BE][rows: u16 BE]`.
pub fn pack_resize(cols: u16, rows: u16) -> [u8; 4] {
    let mut payload = [0u8; 4];
    payload[0..2].copy_from_slice(&cols.to_be_bytes());
    payload[2..4].copy_from_slice(&rows.to_be_bytes());
    payload
}

/// Pack an exit notification payload.
pub fn pack_exit(code: i32) -> Bytes {
    pack_frame(EXIT, &code.to_be_bytes())
}

/// Parse a RESIZE frame payload into `(cols, rows)`.
///
/// Payload must be exactly 4 bytes per the protocol spec.
pub fn parse_resize(payload: &[u8]) -> io::Result<(u16, u16)> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "RESIZE payload must be exactly 4 bytes, got {}",
                payload.len()
            ),
        ));
    }
    let cols = u16::from_be_bytes([payload[0], payload[1]]);
    let rows = u16::from_be_bytes([payload[2], payload[3]]);
    Ok((cols, rows))
}

/// Parse an EXIT frame payload into the exit code.
///
/// Payload must be exactly 4 bytes per the protocol spec.
pub fn parse_exit_code(payload: &[u8]) -> io::Result<i32> {
    if payload.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "EXIT payload must be exactly 4 bytes, got {}",
                payload.len()
            ),
        ));
    }
    Ok(i32::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

/// Maximum frame payload size (1 MB). Frames larger than this are rejected
/// to prevent OOM from malicious or buggy clients.
pub const MAX_FRAME_SIZE: usize = 1 << 20;

/// Read one frame from an async reader. Returns `(type, payload)`.
///
/// Rejects frames with payload larger than [`MAX_FRAME_SIZE`].
pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> io::Result<(u8, Bytes)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header).await?;
    // First byte is the message type.
    let msg_type = header[0];
    // Length of the payload in bytes (big-endian).
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len == 0 {
        return Ok((msg_type, Bytes::new()));
    }
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload {len} bytes exceeds maximum {MAX_FRAME_SIZE}"),
        ));
    }
    // allocate a buffer and read the payload, we already know the length from the header, `len`
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok((msg_type, Bytes::from(payload)))
}

/// Write one frame to an async writer.
///
/// Writes the 5-byte header and payload separately rather than allocating
/// a combined buffer via `pack_frame`. Tokio's buffered writers coalesce
/// these into a single syscall.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg_type: u8,
    payload: &[u8],
) -> io::Result<()> {
    let len = payload.len() as u32;
    let mut header = [0u8; 5];
    header[0] = msg_type;
    header[1..5].copy_from_slice(&len.to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await
}

// -- Client session --

use std::path::Path;
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Client-side session: connected socket with mode-byte handshake complete.
///
/// For simple request-response commands, use [`send`](Self::send) and
/// [`recv`](Self::recv). For long-lived connections (subscribe + select loop),
/// access the raw `reader` and `writer` fields directly.
pub struct Session {
    pub reader: BufReader<OwnedReadHalf>,
    pub writer: OwnedWriteHalf,
}

impl Session {
    /// Connect to a session's Unix socket and perform the mode-byte handshake.
    pub async fn connect(socket_path: &Path) -> io::Result<Self> {
        let stream = tokio::net::UnixStream::connect(socket_path).await?;
        let (read_half, writer) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let mut mode = [0u8; 1];
        reader.read_exact(&mut mode).await?;
        if mode[0] != MODE_BINARY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "expected binary mode (0x{MODE_BINARY:02x}), got 0x{:02x}",
                    mode[0]
                ),
            ));
        }

        Ok(Self { reader, writer })
    }

    /// Send a frame to the supervisor.
    pub async fn send(&mut self, msg_type: u8, payload: &[u8]) -> io::Result<()> {
        write_frame(&mut self.writer, msg_type, payload).await
    }

    /// Read one frame from the supervisor.
    pub async fn recv(&mut self) -> io::Result<(u8, Bytes)> {
        read_frame(&mut self.reader).await
    }

    /// Send a RESIZE frame.
    pub async fn send_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.send(RESIZE, &pack_resize(cols, rows)).await
    }

    /// Send a SUBSCRIBE frame to start receiving pty output.
    pub async fn subscribe(&mut self) -> io::Result<()> {
        self.send(SUBSCRIBE, &[]).await
    }

    /// Request a status response.
    pub async fn send_status(&mut self) -> io::Result<()> {
        self.send(STATUS, &[]).await
    }

    /// Send STATUS and parse the response into a [`StatusResponse`].
    pub async fn recv_status(&mut self) -> io::Result<StatusResponse> {
        self.send_status().await?;
        let (msg_type, payload) = self.recv().await?;
        if msg_type != STATUS_RESP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected STATUS_RESP (0x{STATUS_RESP:02x}), got 0x{msg_type:02x}"),
            ));
        }
        StatusResponse::parse(&payload)
    }

    /// Send a KILL frame to terminate the session.
    pub async fn send_kill(&mut self) -> io::Result<()> {
        self.send(KILL, &[]).await
    }
}

/// Parsed status response from the supervisor.
#[derive(Debug, Clone)]
pub struct StatusResponse {
    pub pid: u32,
    pub idle_ms: u32,
    pub alive: bool,
    pub state: u8,
    pub state_ms: u32,
}

impl StatusResponse {
    /// Parse from a STATUS_RESP payload (minimum 14 bytes).
    fn parse(payload: &[u8]) -> io::Result<Self> {
        if payload.len() < 14 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "STATUS_RESP payload too short: {} bytes (need 14)",
                    payload.len()
                ),
            ));
        }
        let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let idle_ms = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let alive = payload[8] != 0;
        let state = payload[9];
        let state_ms = u32::from_be_bytes([payload[10], payload[11], payload[12], payload[13]]);
        Ok(Self {
            pid,
            idle_ms,
            alive,
            state,
            state_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn round_trip_frame() {
        let frame = pack_frame(INPUT, b"hello");
        let mut cursor = Cursor::new(frame.to_vec());
        let (msg_type, payload) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(msg_type, INPUT);
        assert_eq!(payload.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn round_trip_empty_payload() {
        let frame = pack_frame(STATUS, b"");
        let mut cursor = Cursor::new(frame.to_vec());
        let (msg_type, payload) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(msg_type, STATUS);
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn round_trip_exit() {
        let frame = pack_exit(42);
        let mut cursor = Cursor::new(frame.to_vec());
        let (msg_type, payload) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(msg_type, EXIT);
        let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        assert_eq!(code, 42);
    }

    /// Issue #7: pack_status must produce exactly 20 bytes (5 header + 15 payload).
    #[test]
    fn pack_status_exact_size() {
        let frame = pack_status(1, 0, true, 0x00, 0);
        assert_eq!(frame.len(), 20, "pack_status must be exactly 20 bytes");
    }

    /// Oversized frame is rejected, not allocated.
    #[tokio::test]
    async fn oversized_frame_rejected() {
        let mut header = [0u8; 5];
        header[0] = OUTPUT;
        header[1..5].copy_from_slice(&u32::MAX.to_be_bytes());

        let mut cursor = Cursor::new(header.to_vec());
        let result = read_frame(&mut cursor).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("exceeds maximum"),
            "error should mention size limit: {err}"
        );
    }

    /// Frame at exactly MAX_FRAME_SIZE is accepted.
    #[tokio::test]
    async fn frame_at_max_size_accepted() {
        let payload = vec![0xAB; MAX_FRAME_SIZE];
        let frame = pack_frame(OUTPUT, &payload);
        let mut cursor = Cursor::new(frame.to_vec());
        let (msg_type, data) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(msg_type, OUTPUT);
        assert_eq!(data.len(), MAX_FRAME_SIZE);
    }

    /// Frame one byte over MAX_FRAME_SIZE is rejected.
    #[tokio::test]
    async fn frame_one_over_max_rejected() {
        let len = (MAX_FRAME_SIZE + 1) as u32;
        let mut header = [0u8; 5];
        header[0] = OUTPUT;
        header[1..5].copy_from_slice(&len.to_be_bytes());

        let mut cursor = Cursor::new(header.to_vec());
        let result = read_frame(&mut cursor).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    /// Issue #11: status response round-trips correctly for every ProcessState byte,
    /// including Active (0x04) which was previously missing.
    #[tokio::test]
    async fn round_trip_status_all_state_bytes() {
        let states: &[(u8, &str)] = &[
            (0x00, "Idle"),
            (0x01, "Thinking"),
            (0x02, "Streaming"),
            (0x03, "ToolUse"),
            (0x04, "Active"),
            (0xFF, "Dead"),
        ];

        for &(state_byte, label) in states {
            let frame = pack_status(999, 42, true, state_byte, 1234);
            let mut cursor = Cursor::new(frame.to_vec());
            let (msg_type, payload) = read_frame(&mut cursor).await.unwrap();

            assert_eq!(msg_type, STATUS_RESP, "wrong type for state {label}");
            assert_eq!(payload.len(), 15, "wrong payload len for state {label}");

            let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let idle_ms = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
            let alive = payload[8] != 0;
            let state = payload[9];
            let state_ms = u32::from_be_bytes([payload[10], payload[11], payload[12], payload[13]]);

            assert_eq!(pid, 999, "pid mismatch for state {label}");
            assert_eq!(idle_ms, 42, "idle_ms mismatch for state {label}");
            assert!(alive, "alive mismatch for state {label}");
            assert_eq!(state, state_byte, "state byte mismatch for {label}");
            assert_eq!(state_ms, 1234, "state_ms mismatch for state {label}");
        }
    }

    /// Round-trip status with alive=false.
    #[tokio::test]
    async fn round_trip_status_dead_process() {
        let frame = pack_status(42, 9999, false, 0xFF, 500);
        let mut cursor = Cursor::new(frame.to_vec());
        let (msg_type, payload) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(msg_type, STATUS_RESP);
        assert_eq!(payload[8], 0, "alive byte should be 0 for dead process");
    }

    /// Round-trip exit with negative code.
    #[tokio::test]
    async fn round_trip_exit_negative() {
        let frame = pack_exit(-1);
        let mut cursor = Cursor::new(frame.to_vec());
        let (_msg_type, payload) = read_frame(&mut cursor).await.unwrap();
        let code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        assert_eq!(code, -1);
    }

    /// Truncated frame header (< 5 bytes) yields an error, not a panic.
    #[tokio::test]
    async fn truncated_header_errors() {
        let mut cursor = Cursor::new(vec![0x01, 0x00, 0x00]); // 3 bytes, need 5
        let result = read_frame(&mut cursor).await;
        assert!(result.is_err());
    }

    /// Truncated payload (header says 10, only 3 bytes follow) yields an error.
    #[tokio::test]
    async fn truncated_payload_errors() {
        let mut buf = vec![INPUT];
        buf.extend_from_slice(&10u32.to_be_bytes()); // claims 10 bytes
        buf.extend_from_slice(b"abc"); // only 3
        let mut cursor = Cursor::new(buf);
        let result = read_frame(&mut cursor).await;
        assert!(result.is_err());
    }

    /// Empty reader yields an error on read_frame.
    #[tokio::test]
    async fn empty_reader_errors() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_frame(&mut cursor).await;
        assert!(result.is_err());
    }

    /// pack_frame with large payload produces correct length field.
    #[test]
    fn pack_frame_large_payload() {
        let payload = vec![0xAB; 100_000];
        let frame = pack_frame(OUTPUT, &payload);
        assert_eq!(frame.len(), 5 + 100_000);
        let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        assert_eq!(len, 100_000);
    }

    /// pack_exit with signal code (128 + signal) round-trips.
    #[tokio::test]
    async fn round_trip_exit_signal() {
        let code = 128 + 9; // SIGKILL
        let frame = pack_exit(code);
        let mut cursor = Cursor::new(frame.to_vec());
        let (msg_type, payload) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(msg_type, EXIT);
        let parsed = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        assert_eq!(parsed, 137);
    }

    /// Two frames back-to-back in a single buffer parse correctly.
    #[tokio::test]
    async fn sequential_frames_parse() {
        let f1 = pack_frame(INPUT, b"hello");
        let f2 = pack_frame(OUTPUT, b"world");
        let mut buf = f1.to_vec();
        buf.extend_from_slice(&f2);
        let mut cursor = Cursor::new(buf);

        let (t1, p1) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(t1, INPUT);
        assert_eq!(p1.as_ref(), b"hello");

        let (t2, p2) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(t2, OUTPUT);
        assert_eq!(p2.as_ref(), b"world");
    }

    /// Status with all-zero fields round-trips.
    #[tokio::test]
    async fn round_trip_status_zeros() {
        let frame = pack_status(0, 0, false, 0x00, 0);
        let mut cursor = Cursor::new(frame.to_vec());
        let (msg_type, payload) = read_frame(&mut cursor).await.unwrap();
        assert_eq!(msg_type, STATUS_RESP);
        assert_eq!(payload.len(), 15);
        assert!(payload.iter().all(|&b| b == 0));
    }

    /// Status with max u32 values round-trips.
    #[tokio::test]
    async fn round_trip_status_max_values() {
        let frame = pack_status(u32::MAX, u32::MAX, true, 0xFF, u32::MAX);
        let mut cursor = Cursor::new(frame.to_vec());
        let (_, payload) = read_frame(&mut cursor).await.unwrap();
        let pid = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let idle = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let state_ms = u32::from_be_bytes([payload[10], payload[11], payload[12], payload[13]]);
        assert_eq!(pid, u32::MAX);
        assert_eq!(idle, u32::MAX);
        assert_eq!(state_ms, u32::MAX);
    }

    /// Reserved byte 14 in status payload must always be zero.
    /// If someone writes to it, wire compatibility breaks silently.
    #[test]
    fn pack_status_reserved_byte_is_zero() {
        let frame = pack_status(12345, 999, true, 0x02, 500);
        // Frame layout: [type:1][len:4][payload:15] = 20 bytes
        // payload[14] is frame byte offset 5+14 = 19
        assert_eq!(frame[19], 0, "reserved byte 14 must be zero");

        // Also check with extreme values.
        let frame2 = pack_status(u32::MAX, u32::MAX, true, 0xFF, u32::MAX);
        assert_eq!(frame2[19], 0, "reserved byte stays zero with max values");
    }

    /// Empty payload frame is exactly 5 bytes (header only).
    #[test]
    fn pack_frame_empty_payload_is_five_bytes() {
        let frame = pack_frame(STATUS, b"");
        assert_eq!(frame.len(), 5);
        assert_eq!(frame[0], STATUS);
        assert_eq!(&frame[1..5], &[0, 0, 0, 0]);
    }

    // -- Wire-level golden byte tests --
    // These assert exact byte sequences to catch symmetric pack/parse bugs.
    // If both sides have the same field-swap bug, round-trip tests pass
    // but the wire format is silently wrong.

    /// pack_frame produces exact wire bytes for a known input.
    #[test]
    fn pack_frame_golden_bytes() {
        let frame = pack_frame(INPUT, b"\xDE\xAD");
        // [type=0x01][len=0x00000002][payload=0xDEAD]
        assert_eq!(frame.as_ref(), &[0x01, 0x00, 0x00, 0x00, 0x02, 0xDE, 0xAD]);
    }

    /// pack_exit produces exact 9-byte wire format.
    #[test]
    fn pack_exit_golden_bytes() {
        let frame = pack_exit(137); // 128 + SIGKILL(9)
        // [type=0x83][len=0x00000004][code=0x00000089]
        assert_eq!(
            frame.as_ref(),
            &[0x83, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x89]
        );
    }

    /// pack_exit with negative code produces correct two's complement bytes.
    #[test]
    fn pack_exit_negative_golden_bytes() {
        let frame = pack_exit(-1);
        // [type=0x83][len=0x00000004][code=0xFFFFFFFF]
        assert_eq!(
            frame.as_ref(),
            &[0x83, 0x00, 0x00, 0x00, 0x04, 0xFF, 0xFF, 0xFF, 0xFF]
        );
    }

    /// pack_resize produces exact 4-byte payload layout.
    #[test]
    fn pack_resize_golden_bytes() {
        let payload = pack_resize(120, 40);
        // [cols=0x0078][rows=0x0028]
        assert_eq!(payload, [0x00, 0x78, 0x00, 0x28]);
    }

    /// pack_status produces exact 20-byte wire format with every field at a known offset.
    #[test]
    fn pack_status_golden_bytes() {
        let frame = pack_status(
            0x0000_1234, // pid
            0x0000_0042, // idle_ms = 66
            true,        // alive
            0x02,        // state = streaming
            0x0000_04D2, // state_ms = 1234
        );
        assert_eq!(frame.len(), 20);
        #[rustfmt::skip]
        let expected: [u8; 20] = [
            0x82,                         // type = STATUS_RESP
            0x00, 0x00, 0x00, 0x0F,       // len = 15
            0x00, 0x00, 0x12, 0x34,       // pid
            0x00, 0x00, 0x00, 0x42,       // idle_ms
            0x01,                         // alive
            0x02,                         // state
            0x00, 0x00, 0x04, 0xD2,       // state_ms
            0x00,                         // reserved
        ];
        assert_eq!(frame.as_ref(), &expected);
    }

    /// parse_resize against hand-constructed bytes (not from pack_resize).
    #[test]
    fn parse_resize_from_raw_bytes() {
        // 80 cols = 0x0050, 24 rows = 0x0018
        let raw = [0x00, 0x50, 0x00, 0x18];
        let (cols, rows) = parse_resize(&raw).unwrap();
        assert_eq!(cols, 80);
        assert_eq!(rows, 24);
    }

    /// parse_exit_code against hand-constructed bytes (not from pack_exit).
    #[test]
    fn parse_exit_code_from_raw_bytes() {
        // exit code 42 = 0x0000002A
        let raw = [0x00, 0x00, 0x00, 0x2A];
        let code = parse_exit_code(&raw).unwrap();
        assert_eq!(code, 42);
    }

    /// parse_exit_code with signal death (negative via two's complement).
    #[test]
    fn parse_exit_code_negative_from_raw_bytes() {
        // -9 in two's complement = 0xFFFFFFF7
        let raw = [0xFF, 0xFF, 0xFF, 0xF7];
        let code = parse_exit_code(&raw).unwrap();
        assert_eq!(code, -9);
    }

    /// parse_resize rejects short payload.
    #[test]
    fn parse_resize_rejects_short() {
        assert!(parse_resize(&[0x00, 0x50]).is_err());
        assert!(parse_resize(&[]).is_err());
    }

    /// parse_resize rejects oversized payload (spec says exactly 4).
    #[test]
    fn parse_resize_rejects_oversized() {
        assert!(parse_resize(&[0x00, 0x50, 0x00, 0x18, 0xFF]).is_err());
    }

    /// parse_exit_code rejects short payload.
    #[test]
    fn parse_exit_code_rejects_short() {
        assert!(parse_exit_code(&[0x00, 0x00]).is_err());
        assert!(parse_exit_code(&[]).is_err());
    }

    /// parse_exit_code rejects oversized payload (spec says exactly 4).
    #[test]
    fn parse_exit_code_rejects_oversized() {
        assert!(parse_exit_code(&[0x00, 0x00, 0x00, 0x2A, 0xFF]).is_err());
    }

    /// Dead process golden bytes: alive=0, state=0xFF at correct offsets.
    #[test]
    fn pack_status_dead_golden_bytes() {
        let frame = pack_status(
            0x0000_002A, // pid = 42
            0x0000_270F, // idle_ms = 9999
            false,       // alive = dead
            0xFF,        // state = Dead
            0x0000_01F4, // state_ms = 500
        );
        #[rustfmt::skip]
        let expected: [u8; 20] = [
            0x82,                         // type = STATUS_RESP
            0x00, 0x00, 0x00, 0x0F,       // len = 15
            0x00, 0x00, 0x00, 0x2A,       // pid = 42
            0x00, 0x00, 0x27, 0x0F,       // idle_ms = 9999
            0x00,                         // alive = false
            0xFF,                         // state = Dead
            0x00, 0x00, 0x01, 0xF4,       // state_ms = 500
            0x00,                         // reserved
        ];
        assert_eq!(frame.as_ref(), &expected);
    }
}
