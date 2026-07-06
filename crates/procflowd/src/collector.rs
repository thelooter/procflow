//! eBPF collector loader + drain loop (ADR-0006/0007).
//!
//! Loads the BPF object built by `scripts/build-ebpf.sh`, attaches the
//! socket-layer hooks, and polls the kernel maps. Failure to start (no
//! CAP_BPF/CAP_PERFMON, missing object, no BTF) degrades gracefully: the
//! daemon keeps serving IPC so history remains queryable.
//!
//! Current drain is proof-of-life logging; folding into the minute tier via
//! the Identity enrichment pipeline (ADR-0007) is the next slice.

use anyhow::{Context, Result};
use aya::maps::{MapData, PerCpuHashMap, RingBuf};
use aya::programs::{FEntry, FExit};
use aya::{Btf, Ebpf};
use procflow_common::TrafficKey;
use std::path::PathBuf;
use std::time::Duration;

/// Env override for the BPF object path (development).
pub const BPF_OBJECT_ENV: &str = "PROCFLOW_BPF_OBJECT";

fn default_object_path() -> PathBuf {
    // Dev default: the build output of scripts/build-ebpf.sh, relative to the
    // repo. Packaging will install and pin a real path (ADR-0011).
    PathBuf::from("crates/procflow-ebpf/target/bpfel-unknown-none/release/procflow-ebpf")
}

pub fn object_path() -> PathBuf {
    std::env::var_os(BPF_OBJECT_ENV).map(Into::into).unwrap_or_else(default_object_path)
}

/// Load, attach, and start the drain thread. Returns the owning handle; drop
/// detaches everything.
pub fn start(poll_interval: Duration) -> Result<Ebpf> {
    let path = object_path();
    let mut ebpf = Ebpf::load_file(&path)
        .with_context(|| format!("loading BPF object {}", path.display()))?;
    let btf = Btf::from_sys_fs().context("reading kernel BTF from /sys/kernel/btf/vmlinux")?;

    // fentry hooks (ADR-0006)
    for name in ["tcp_cleanup_rbuf"] {
        let prog: &mut FEntry = ebpf
            .program_mut(name)
            .with_context(|| format!("program {name} missing from object"))?
            .try_into()?;
        prog.load(name, &btf).with_context(|| format!("loading fentry {name}"))?;
        prog.attach().with_context(|| format!("attaching fentry {name}"))?;
    }
    // fexit hooks (ADR-0006)
    for name in ["tcp_sendmsg", "udp_sendmsg", "udpv6_sendmsg", "udp_recvmsg", "udpv6_recvmsg"] {
        let prog: &mut FExit = ebpf
            .program_mut(name)
            .with_context(|| format!("program {name} missing from object"))?
            .try_into()?;
        prog.load(name, &btf).with_context(|| format!("loading fexit {name}"))?;
        prog.attach().with_context(|| format!("attaching fexit {name}"))?;
    }

    // Maps move into the drain thread; the program handles stay in `ebpf`.
    let traffic: PerCpuHashMap<MapData, TrafficKey, u64> =
        PerCpuHashMap::try_from(ebpf.take_map("TRAFFIC").context("TRAFFIC map missing")?)?;
    let mut new_pids: RingBuf<MapData> =
        RingBuf::try_from(ebpf.take_map("NEW_PIDS").context("NEW_PIDS map missing")?)?;

    std::thread::Builder::new().name("collector-drain".into()).spawn(move || loop {
        // Eager enrichment trigger (ADR-0007): consume first-sight tgids.
        while let Some(item) = new_pids.next() {
            if item.len() >= 4 {
                let tgid = u32::from_ne_bytes(item[..4].try_into().unwrap());
                // TODO(ADR-0007): eager /proc enrichment → Identity cache.
                println!("collector: new tgid {tgid}");
            }
        }
        // Poll-path proof of life: sum per-CPU counters and log them.
        // TODO(ADR-0006/0007): read-and-clear, resolve tgid → Identity, fold
        // into the minute accumulator, flush closed minutes to the store.
        for entry in traffic.iter() {
            if let Ok((key, per_cpu)) = entry {
                let total: u64 = per_cpu.iter().sum();
                println!(
                    "collector: tgid={} dir={} scope={} bytes={total}",
                    key.tgid, key.dir, key.scope
                );
            }
        }
        std::thread::sleep(poll_interval);
    })?;

    Ok(ebpf)
}
