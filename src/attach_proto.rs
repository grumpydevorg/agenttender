use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::recording::Geometry;

/// Message types for the attach protocol.
/// Minimal framing: 1 byte type + 4 byte big-endian length + payload.
pub const MSG_DATA: u8 = 0x01;
pub const MSG_RESIZE: u8 = 0x02;
pub const MSG_DETACH: u8 = 0x03;
/// Client → sidecar, first message of every connection: `[version, mode]`.
/// A connection that does not open with a valid hello is closed without
/// gaining control.
pub const MSG_HELLO: u8 = 0x04;
/// Sidecar → client: control granted. Payload: the controller epoch (u64 BE).
pub const MSG_ACCEPTED: u8 = 0x05;
/// Sidecar → client: control refused. Payload: UTF-8 reason. The sidecar then
/// closes the connection.
pub const MSG_REJECTED: u8 = 0x06;
/// Sidecar → client: another client took over. Payload: the new epoch (u64 BE).
/// The sidecar then shuts the connection down.
pub const MSG_RETIRED: u8 = 0x07;

/// The attach protocol version carried in [`MSG_HELLO`].
pub const PROTOCOL_VERSION: u8 = 1;
/// [`MSG_HELLO`] mode: take control only if nobody holds it.
pub const MODE_ATTACH: u8 = 1;
/// [`MSG_HELLO`] mode: take control, retiring any current controller.
pub const MODE_TAKEOVER: u8 = 2;
/// [`MSG_HELLO`] mode: an agent push. Claims control only if nobody holds it,
/// streams input as [`MSG_DATA`], ends it with [`MSG_DETACH`], and receives a
/// [`MSG_INPUT_DONE`] outcome. No output is sent to a push connection.
pub const MODE_PUSH: u8 = 3;

/// Sidecar → push client: the push's outcome, then the connection closes.
/// Payload: `[status u8][accepted u64 BE][received u64 BE]`, where `accepted`
/// counts bytes written to the PTY and `received` counts bytes the sidecar read.
pub const MSG_INPUT_DONE: u8 = 0x08;
/// [`MSG_INPUT_DONE`] status: every received byte was written.
pub const INPUT_WRITTEN: u8 = 0;
/// [`MSG_INPUT_DONE`] status: control was taken over; the rest was dropped.
pub const INPUT_REVOKED: u8 = 1;
/// [`MSG_INPUT_DONE`] status: the PTY stopped accepting input.
pub const INPUT_CLOSED: u8 = 2;

/// Encode a [`MSG_INPUT_DONE`] payload.
#[must_use]
pub fn input_done_payload(status: u8, accepted: u64, received: u64) -> [u8; 17] {
    let mut buf = [0u8; 17];
    buf[0] = status;
    buf[1..9].copy_from_slice(&accepted.to_be_bytes());
    buf[9..17].copy_from_slice(&received.to_be_bytes());
    buf
}

/// Decode a [`MSG_INPUT_DONE`] payload into `(status, accepted, received)`.
#[must_use]
pub fn parse_input_done(payload: &[u8]) -> Option<(u8, u64, u64)> {
    let bytes: &[u8; 17] = payload.try_into().ok()?;
    let accepted = u64::from_be_bytes(bytes[1..9].try_into().ok()?);
    let received = u64::from_be_bytes(bytes[9..17].try_into().ok()?);
    Some((bytes[0], accepted, received))
}

/// How long the sidecar waits for a connection's hello.
pub const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Largest payload any attach frame may carry. [`read_msg`] rejects a larger
/// length before allocating for it.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

pub fn write_msg(w: &mut impl Write, msg_type: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    w.write_all(&[msg_type])?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_msg(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let msg_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("attach frame declares {len} bytes; the limit is {MAX_FRAME_PAYLOAD}"),
        ));
    }
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok((msg_type, payload))
}

/// Encode a [`MSG_RESIZE`] payload: `[rows u16 BE][cols u16 BE]`.
#[must_use]
pub fn resize_payload(size: Geometry) -> [u8; 4] {
    let mut buf = [0u8; 4];
    buf[0..2].copy_from_slice(&size.rows().to_be_bytes());
    buf[2..4].copy_from_slice(&size.cols().to_be_bytes());
    buf
}

/// Decode a [`MSG_RESIZE`] payload. `None` if it is short or either dimension
/// is zero: no terminal program can draw into such a size, and it cannot be
/// recorded, so it is refused here rather than at the PTY.
#[must_use]
pub fn parse_resize(payload: &[u8]) -> Option<Geometry> {
    if payload.len() < 4 {
        return None;
    }
    let rows = u16::from_be_bytes([payload[0], payload[1]]);
    let cols = u16::from_be_bytes([payload[2], payload[3]]);
    Geometry::new(rows, cols)
}

/// Read the attach socket path from the session's breadcrumb (`a.sock.path`),
/// which the sidecar publishes atomically after binding its private socket.
/// There is no fallback location: a session without a breadcrumb has no
/// attachable listener.
pub fn read_sock_path(session_dir: &Path) -> Option<PathBuf> {
    read_breadcrumb(session_dir).filter(|path| path.exists())
}

/// The socket path the session's breadcrumb names, whether or not it exists.
pub fn read_breadcrumb(session_dir: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(session_dir.join("a.sock.path")).ok()?;
    Some(PathBuf::from(content.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(msg_type: u8, declared_len: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![msg_type];
        out.extend_from_slice(&declared_len.to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_resize_round_trips_and_a_zero_dimension_is_refused_at_parse() {
        let size = Geometry::new(40, 120).unwrap();
        assert_eq!(parse_resize(&resize_payload(size)), Some(size));
        assert_eq!(parse_resize(&[0, 0, 0, 100]), None, "zero rows");
        assert_eq!(parse_resize(&[0, 24, 0, 0]), None, "zero columns");
        assert_eq!(parse_resize(&[0, 24, 0]), None, "short");
    }

    #[test]
    fn a_frame_at_the_payload_limit_is_read() {
        let payload = vec![7u8; MAX_FRAME_PAYLOAD];
        let bytes = frame(MSG_DATA, MAX_FRAME_PAYLOAD as u32, &payload);
        let (msg_type, got) = read_msg(&mut bytes.as_slice()).unwrap();
        assert_eq!(msg_type, MSG_DATA);
        assert_eq!(got.len(), MAX_FRAME_PAYLOAD);
    }

    #[test]
    fn an_oversized_frame_is_rejected_before_allocation() {
        // A 4 GiB declared length with no payload behind it: reading must fail
        // on the length alone, not try to allocate or wait for the bytes.
        let bytes = frame(MSG_DATA, u32::MAX, &[]);
        let err = read_msg(&mut bytes.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let just_over = frame(MSG_DATA, MAX_FRAME_PAYLOAD as u32 + 1, &[]);
        let err = read_msg(&mut just_over.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
