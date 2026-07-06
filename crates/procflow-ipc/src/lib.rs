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
