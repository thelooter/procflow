//! Shared IPC protocol for procflow (ADR-0008).
//!
//! Wire format: `SOCK_STREAM` Unix socket at [`SOCKET_PATH`], u32-LE
//! length-prefixed frames, one encoded protobuf message per frame.

/// Generated protobuf types, version 1 of the protocol.
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/procflow.v1.rs"));
}

/// Default daemon socket path (ADR-0008/0011).
pub const SOCKET_PATH: &str = "/run/procflow/procflow.sock";

/// Current protocol version. Bumped on breaking changes only; additive
/// changes ride protobuf field numbers (ADR-0008).
pub const PROTO_VERSION: u32 = 1;

/// Upper bound on a single frame's payload, as a sanity check against
/// corrupt length prefixes. Results are tiny (ADR-0001); 16 MiB is generous.
pub const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Environment variable overriding the socket path (development; the packaged
/// daemon uses [`SOCKET_PATH`] via its systemd RuntimeDirectory, ADR-0011).
pub const SOCKET_ENV: &str = "PROCFLOW_SOCKET";

/// Resolve the socket path: `$PROCFLOW_SOCKET` if set, else [`SOCKET_PATH`].
pub fn socket_path() -> std::path::PathBuf {
    std::env::var_os(SOCKET_ENV)
        .map(Into::into)
        .unwrap_or_else(|| SOCKET_PATH.into())
}

use prost::Message;
use std::io::{self, Read, Write};

/// Write one length-prefixed frame: u32-LE byte length, then the encoded
/// message (ADR-0008).
pub fn write_msg<M: Message>(w: &mut impl Write, msg: &M) -> io::Result<()> {
    let len = msg.encoded_len();
    if len > MAX_FRAME_LEN as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds MAX_FRAME_LEN"));
    }
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    msg.encode(&mut buf).expect("Vec<u8> encode is infallible");
    w.write_all(&buf)?;
    w.flush()
}

/// Read one length-prefixed frame and decode it.
pub fn read_msg<M: Message + Default>(r: &mut impl Read) -> io::Result<M> {
    let mut len_bytes = [0u8; 4];
    r.read_exact(&mut len_bytes)?;
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds MAX_FRAME_LEN"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    M::decode(buf.as_slice()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
