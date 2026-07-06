//! eBPF collector loader + drain pipeline (ADR-0006/0007).
//!
//! Loads the BPF object built by `scripts/build-ebpf.sh`, attaches the
//! socket-layer hooks, and runs the drain loop: eager enrichment from the
//! NEW_PIDS ring buffer, read-and-clear of the per-CPU TRAFFIC counters,
//! tgid → Identity resolution (live /proc, else PID_META fallback), a
//! current-minute accumulator, and flush of closed minutes to the store.
//!
//! Failure to start (no CAP_BPF/CAP_PERFMON, missing object, no BTF)
//! degrades gracefully: the daemon keeps serving IPC.

use crate::enrich;
use crate::store::Store;
use anyhow::{Context, Result};
use aya::maps::{HashMap as BpfHashMap, MapData, PerCpuHashMap, RingBuf};
use aya::programs::{FEntry, FExit};
use aya::{Btf, Ebpf};
use procflow_common::{PidMeta, TrafficKey, DIR_EGRESS, DIR_INGRESS, SCOPE_LOOPBACK};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
pub fn start(store: Arc<Mutex<Store>>, poll_interval: Duration) -> Result<Ebpf> {
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
    let drain = Drain {
        store,
        traffic: PerCpuHashMap::try_from(ebpf.take_map("TRAFFIC").context("TRAFFIC map missing")?)?,
        new_pids: RingBuf::try_from(ebpf.take_map("NEW_PIDS").context("NEW_PIDS map missing")?)?,
        pid_meta: BpfHashMap::try_from(ebpf.take_map("PID_META").context("PID_META map missing")?)?,
        cache: HashMap::new(),
        acc: HashMap::new(),
    };
    std::thread::Builder::new()
        .name("collector-drain".into())
        .spawn(move || drain.run(poll_interval))?;

    Ok(ebpf)
}

/// Accumulator key: (minute bucket epoch s, identity id, scope).
type AccKey = (i64, i64, u8);

struct Drain {
    store: Arc<Mutex<Store>>,
    traffic: PerCpuHashMap<MapData, TrafficKey, u64>,
    new_pids: RingBuf<MapData>,
    pid_meta: BpfHashMap<MapData, u32, PidMeta>,
    /// tgid → identity surrogate id (ADR-0007). Enrichment is once per tgid.
    cache: HashMap<u32, i64>,
    /// In-memory current-minute counters: flushed once the minute closes.
    acc: HashMap<AccKey, (u64, u64)>, // (ingress, egress)
}

impl Drain {
    fn run(mut self, poll_interval: Duration) -> ! {
        loop {
            if let Err(e) = self.tick() {
                eprintln!("collector: drain tick failed: {e:#}");
            }
            std::thread::sleep(poll_interval);
        }
    }

    fn tick(&mut self) -> Result<()> {
        self.drain_new_pids();
        self.drain_traffic()?;
        self.flush_closed_minutes()?;
        self.gc_dead_tgids();
        Ok(())
    }

    /// Eager enrichment (ADR-0007): resolve each first-sight tgid while its
    /// /proc entry is (almost always) still alive.
    fn drain_new_pids(&mut self) {
        let mut fresh = Vec::new();
        while let Some(item) = self.new_pids.next() {
            if item.len() >= 4 {
                fresh.push(u32::from_ne_bytes(item[..4].try_into().unwrap()));
            }
        }
        for tgid in fresh {
            if let Some(id) = self.resolve(tgid) {
                println!("collector: tgid {tgid} → identity {id}");
            }
        }
    }

    /// Read-and-clear the per-CPU counters and fold them into the
    /// current-minute accumulator.
    fn drain_traffic(&mut self) -> Result<()> {
        let keys: Vec<TrafficKey> = self.traffic.keys().filter_map(|k| k.ok()).collect();
        if keys.is_empty() {
            return Ok(());
        }
        let bucket = current_minute();
        for key in keys {
            let bytes: u64 = match self.traffic.get(&key, 0) {
                Ok(per_cpu) => per_cpu.iter().sum(),
                Err(_) => continue, // raced with kernel-side removal: skip
            };
            // Remove first: the kernel re-inserts on next traffic, and a
            // failed remove means we must not double-count.
            if self.traffic.remove(&key).is_err() || bytes == 0 {
                continue;
            }
            let Some(identity_id) = self.resolve(key.tgid) else { continue };
            let entry = self.acc.entry((bucket, identity_id, key.scope)).or_insert((0, 0));
            match key.dir {
                DIR_INGRESS => entry.0 += bytes,
                DIR_EGRESS => entry.1 += bytes,
                _ => {}
            }
        }
        Ok(())
    }

    /// Flush accumulator entries whose minute has closed (ADR-0005: only
    /// fully-closed buckets move; the in-progress minute stays in memory).
    fn flush_closed_minutes(&mut self) -> Result<()> {
        let open_bucket = current_minute();
        let closed: Vec<AccKey> =
            self.acc.keys().copied().filter(|(bucket, ..)| *bucket < open_bucket).collect();
        if closed.is_empty() {
            return Ok(());
        }
        let store = self.store.lock().expect("store mutex poisoned");
        for key in closed {
            let (bucket, identity_id, scope) = key;
            let (ingress, egress) = self.acc.remove(&key).unwrap();
            let scope_str = if scope == SCOPE_LOOPBACK { "loopback" } else { "external" };
            store
                .record_minute(bucket, identity_id, scope_str, ingress, egress)
                .with_context(|| format!("flushing minute {bucket} identity {identity_id}"))?;
        }
        Ok(())
    }

    /// tgid → identity id: cache, else live /proc, else kernel-captured
    /// PID_META (degraded Unresolved Identity), else the last-resort record.
    /// Never drops bytes (ADR-0007).
    fn resolve(&mut self, tgid: u32) -> Option<i64> {
        if let Some(id) = self.cache.get(&tgid) {
            return Some(*id);
        }
        let record = enrich::from_proc(tgid)
            .ok()
            .or_else(|| self.pid_meta.get(&tgid, 0).ok().map(|meta| enrich::from_pid_meta(&meta)))
            .unwrap_or_else(enrich::fully_unresolved);
        match self.store.lock().expect("store mutex poisoned").upsert_identity(&record) {
            Ok(id) => {
                self.cache.insert(tgid, id);
                Some(id)
            }
            Err(e) => {
                eprintln!("collector: identity upsert failed for tgid {tgid}: {e:#}");
                None
            }
        }
    }

    /// Evict kernel + userspace state for exited tgids whose counters have
    /// been drained (ADR-0007 map GC). Runs after drain, so nothing pending
    /// references them; a tgid that transmits again re-registers itself.
    fn gc_dead_tgids(&mut self) {
        let dead: Vec<u32> = self
            .pid_meta
            .keys()
            .filter_map(|k| k.ok())
            .filter(|tgid| !std::path::Path::new(&format!("/proc/{tgid}")).exists())
            .collect();
        for tgid in dead {
            let _ = self.pid_meta.remove(&tgid);
            self.cache.remove(&tgid);
        }
    }
}

/// Start of the current UTC minute, epoch seconds (wall clock, ADR-0003).
fn current_minute() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64;
    now - now % 60
}
