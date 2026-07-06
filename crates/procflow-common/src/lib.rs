//! Types shared between the eBPF collector and the daemon (ADR-0006/0007).
//!
//! These are the BPF map ABI: `#[repr(C)]`, explicitly padded, identical on
//! both sides. Changing them is a breaking change to the collector.

#![no_std]

/// Direction of a counter (CONTEXT.md): kernel-side encoding.
pub const DIR_INGRESS: u8 = 0;
pub const DIR_EGRESS: u8 = 1;

/// Scope of a counter (CONTEXT.md): kernel-side encoding.
pub const SCOPE_EXTERNAL: u8 = 0;
pub const SCOPE_LOOPBACK: u8 = 1;

/// Key of the `TRAFFIC` per-CPU counter map (ADR-0006).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrafficKey {
    pub tgid: u32,
    pub dir: u8,
    pub scope: u8,
    pub _pad: [u8; 2],
}

impl TrafficKey {
    pub const fn new(tgid: u32, dir: u8, scope: u8) -> Self {
        Self { tgid, dir, scope, _pad: [0; 2] }
    }
}

/// Value of the `PID_META` map (ADR-0007): the cheap fields that vanish with
/// the process, captured in-kernel on first sight of a tgid.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PidMeta {
    pub cgroup_id: u64,
    pub uid: u32,
    pub comm: [u8; 16],
    pub _pad: [u8; 4],
}

#[cfg(feature = "user")]
mod user {
    unsafe impl aya::Pod for super::TrafficKey {}
    unsafe impl aya::Pod for super::PidMeta {}
}
