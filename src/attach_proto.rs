use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Message types for the attach protocol.
/// Minimal framing: 1 byte type + 4 byte big-endian length + payload.
pub const MSG_DATA: u8 = 0x01;
pub const MSG_RESIZE: u8 = 0x02;
pub const MSG_DETACH: u8 = 0x03;

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
    let mut payload = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut payload)?;
    }
    Ok((msg_type, payload))
}

pub fn resize_payload(rows: u16, cols: u16) -> [u8; 4] {
    let mut buf = [0u8; 4];
    buf[0..2].copy_from_slice(&rows.to_be_bytes());
    buf[2..4].copy_from_slice(&cols.to_be_bytes());
    buf
}

pub fn parse_resize(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    let rows = u16::from_be_bytes([payload[0], payload[1]]);
    let cols = u16::from_be_bytes([payload[2], payload[3]]);
    Some((rows, cols))
}

/// Compute the attach socket path for a session directory.
///
/// Unix domain sockets have a path length limit (104 bytes on macOS, 108 on Linux).
/// Session directories under temp dirs can easily exceed this limit. To stay safe,
/// we hash the session dir path and place the socket in the system temp directory.
///
/// The socket file is: `<tmp>/tendr-<hash>.sock`
/// A breadcrumb `a.sock.path` in the session dir records the socket location.
pub fn sock_path(session_dir: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};

    let dir_str = session_dir.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(dir_str.as_bytes());
    let hash = hasher.finalize();
    let short_hash: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();

    std::env::temp_dir().join(format!("tendr-{short_hash}.sock"))
}

/// Write a breadcrumb in the session dir pointing to the actual socket path.
pub fn write_sock_breadcrumb(session_dir: &Path, sock: &Path) {
    let breadcrumb = session_dir.join("a.sock.path");
    let _ = std::fs::write(&breadcrumb, sock.to_string_lossy().as_bytes());
}

/// Read the socket path from the session dir breadcrumb, falling back to inline path.
pub fn read_sock_path(session_dir: &Path) -> Option<PathBuf> {
    let breadcrumb = session_dir.join("a.sock.path");
    if let Ok(content) = std::fs::read_to_string(&breadcrumb) {
        let p = PathBuf::from(content.trim());
        if p.exists() {
            return Some(p);
        }
    }
    // Fallback: check inline socket (works when path is short enough)
    let inline = session_dir.join("a.sock");
    if inline.exists() {
        return Some(inline);
    }
    None
}
