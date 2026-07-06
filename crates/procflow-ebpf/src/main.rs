// SPDX-License-Identifier: MIT OR Apache-2.0
//! procflow eBPF collector (ADR-0006/0007).
//!
//! The kernel counts payload bytes per tgid into `TRAFFIC`; userspace
//! resolves tgid → Identity. `PID_META` captures the fields that vanish with
//! a process on first sight; `NEW_PIDS` notifies the daemon so it can enrich
//! eagerly at activity time (the dead-PID race, ADR-0007).

#![no_std]
#![no_main]
#![allow(nonstandard_style, dead_code, unnecessary_transmutes)] // generated bindings

mod bindings;

use aya_ebpf::{
    helpers::{
        bpf_get_current_cgroup_id, bpf_get_current_comm, bpf_get_current_pid_tgid,
        bpf_get_current_uid_gid, bpf_probe_read_kernel,
    },
    macros::{fentry, fexit, map},
    maps::{HashMap, PerCpuHashMap, RingBuf},
    programs::{FEntryContext, FExitContext},
};
use bindings::sock;
use procflow_common::{PidMeta, TrafficKey, DIR_EGRESS, DIR_INGRESS, SCOPE_EXTERNAL, SCOPE_LOOPBACK};

// The BPF verifier requires a GPL-compatible license to call gpl_only
// helpers (bpf_probe_read_kernel & co). Per-object string, not project
// copyleft — see ADR-0012. Do NOT "fix" this to plain MIT.
#[no_mangle]
#[link_section = "license"]
pub static LICENSE: [u8; 13] = *b"Dual MIT/GPL\0";

/// Per-tgid payload byte counters (ADR-0006). Per-CPU: increments are
/// contention-free; the daemon sums across CPUs on read.
#[map]
static TRAFFIC: PerCpuHashMap<TrafficKey, u64> = PerCpuHashMap::with_max_entries(10240, 0);

/// First-sight metadata that dies with the process (ADR-0007).
#[map]
static PID_META: HashMap<u32, PidMeta> = HashMap::with_max_entries(10240, 0);

/// First-sight tgid notifications so the daemon enriches eagerly (ADR-0007).
#[map]
static NEW_PIDS: RingBuf = RingBuf::with_byte_size(64 * 1024, 0);

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;
const BPF_NOEXIST: u64 = 1;

// ---------------------------------------------------------------------------
// Hooks (ADR-0006). All run in process context, so bpf_get_current_pid_tgid()
// is the owning process — that is why ingress hooks tcp_cleanup_rbuf, not the
// softirq receive path.
// ---------------------------------------------------------------------------

#[fexit(function = "tcp_sendmsg")]
pub fn tcp_sendmsg(ctx: FExitContext) -> u32 {
    // tcp_sendmsg(sk, msg, size) -> int; return value is at arg index 3.
    let _ = sendmsg_exit(&ctx, 3);
    0
}

#[fentry(function = "tcp_cleanup_rbuf")]
pub fn tcp_cleanup_rbuf(ctx: FEntryContext) -> u32 {
    // tcp_cleanup_rbuf(sk, copied): app just drained `copied` bytes.
    let sk: *const sock = ctx.arg(0);
    let copied: i32 = ctx.arg(1);
    if copied > 0 {
        let _ = record(sk, DIR_INGRESS, copied as u64);
    }
    0
}

#[fexit(function = "udp_sendmsg")]
pub fn udp_sendmsg(ctx: FExitContext) -> u32 {
    // udp_sendmsg(sk, msg, len) -> int; return value at index 3.
    let _ = sendmsg_exit(&ctx, 3);
    0
}

#[fexit(function = "udpv6_sendmsg")]
pub fn udpv6_sendmsg(ctx: FExitContext) -> u32 {
    let _ = sendmsg_exit(&ctx, 3);
    0
}

#[fexit(function = "udp_recvmsg")]
pub fn udp_recvmsg(ctx: FExitContext) -> u32 {
    // udp_recvmsg(sk, msg, len, flags, addr_len) -> int on kernels >= 5.19
    // (the `noblock` arg was removed); return value at index 5.
    let _ = recvmsg_exit(&ctx, 5);
    0
}

#[fexit(function = "udpv6_recvmsg")]
pub fn udpv6_recvmsg(ctx: FExitContext) -> u32 {
    let _ = recvmsg_exit(&ctx, 5);
    0
}

// ---------------------------------------------------------------------------

#[inline(always)]
fn sendmsg_exit(ctx: &FExitContext, ret_index: usize) -> Result<(), i64> {
    let sk: *const sock = ctx.arg(0);
    let ret: i64 = ctx.arg(ret_index);
    if ret > 0 {
        record(sk, DIR_EGRESS, ret as u64)?;
    }
    Ok(())
}

#[inline(always)]
fn recvmsg_exit(ctx: &FExitContext, ret_index: usize) -> Result<(), i64> {
    let sk: *const sock = ctx.arg(0);
    let ret: i64 = ctx.arg(ret_index);
    if ret > 0 {
        record(sk, DIR_INGRESS, ret as u64)?;
    }
    Ok(())
}

#[inline(always)]
fn record(sk: *const sock, dir: u8, bytes: u64) -> Result<(), i64> {
    let tgid = (bpf_get_current_pid_tgid() >> 32) as u32;
    note_pid(tgid);
    let key = TrafficKey::new(tgid, dir, scope_of(sk)?);
    unsafe {
        if let Some(count) = TRAFFIC.get_ptr_mut(&key) {
            *count += bytes;
        } else {
            // On map overflow the insert fails: bounded undercount (ADR-0006).
            let _ = TRAFFIC.insert(&key, &bytes, 0);
        }
    }
    Ok(())
}

/// First sight of a tgid: capture the fields that die with the process and
/// notify the daemon (ADR-0007). BPF_NOEXIST keeps the first capture.
#[inline(always)]
fn note_pid(tgid: u32) {
    if unsafe { PID_META.get(&tgid) }.is_some() {
        return;
    }
    let meta = PidMeta {
        cgroup_id: unsafe { bpf_get_current_cgroup_id() },
        uid: (bpf_get_current_uid_gid() & 0xffff_ffff) as u32,
        comm: bpf_get_current_comm().unwrap_or([0u8; 16]),
        _pad: [0; 4],
    };
    if PID_META.insert(&tgid, &meta, BPF_NOEXIST).is_ok() {
        // Losing a notification on a full ring is acceptable: the poll path
        // falls back to PID_META for a degraded Identity (ADR-0007).
        let _ = NEW_PIDS.output::<u32>(&tgid, 0);
    }
}

/// Scope classification from the socket (ADR-0006): loopback vs external.
#[inline(always)]
fn scope_of(sk: *const sock) -> Result<u8, i64> {
    unsafe {
        let family: u16 =
            bpf_probe_read_kernel(&(*sk).__sk_common.skc_family).map_err(|e| e)? as u16;
        match family {
            AF_INET => {
                let daddr: u32 = bpf_probe_read_kernel(
                    &(*sk).__sk_common.__bindgen_anon_1.__bindgen_anon_1.skc_daddr,
                )?;
                let saddr: u32 = bpf_probe_read_kernel(
                    &(*sk).__sk_common.__bindgen_anon_1.__bindgen_anon_1.skc_rcv_saddr,
                )?;
                // __be32: on little-endian the first octet is the low byte.
                if daddr & 0xff == 127 || (daddr != 0 && daddr == saddr) {
                    Ok(SCOPE_LOOPBACK)
                } else {
                    Ok(SCOPE_EXTERNAL)
                }
            }
            AF_INET6 => {
                let a: [u32; 4] = bpf_probe_read_kernel(
                    &(*sk).__sk_common.skc_v6_daddr.in6_u.u6_addr32,
                )?;
                let v6_loopback = a[0] == 0 && a[1] == 0 && a[2] == 0 && a[3] == u32::to_be(1);
                // ::ffff:127.x.y.z (v4-mapped loopback)
                let v4_mapped_loopback =
                    a[0] == 0 && a[1] == 0 && a[2] == u32::to_be(0x0000_ffff) && a[3] & 0xff == 127;
                if v6_loopback || v4_mapped_loopback {
                    Ok(SCOPE_LOOPBACK)
                } else {
                    Ok(SCOPE_EXTERNAL)
                }
            }
            // Unconnected/unknown: default external (ADR-0006).
            _ => Ok(SCOPE_EXTERNAL),
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
