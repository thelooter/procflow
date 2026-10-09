//! An unprivileged stand-in for the daemon: the real IPC server over an
//! in-memory store, fed invented traffic instead of the eBPF collector.
//! For trying the CLI without CAP_BPF:
//!
//!     cargo run -p procflowd --example demo
//!     PROCFLOW_SOCKET=/tmp/procflow-demo.sock cargo run -p procflow
//!
//! Nothing here is measured. Every byte count is made up.

use anyhow::{Context, Result};
use procflow_ipc::v1::Scope;
use procflowd::enrich::IdentityRecord;
use procflowd::live::{Delta, Hub, Tick};
use procflowd::server::Server;
use procflowd::store::Store;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One invented process: what it is, and roughly how much it moves.
struct Talker {
    comm: &'static str,
    cmdline: &'static str,
    /// Under the home directory; empty for none.
    project: &'static str,
    scope: Scope,
    /// Typical bytes per second, in and out.
    rate: (u64, u64),
    /// One poll in this many is busy; the rest are nearly idle.
    burst: u64,
    /// Owned by root, so only root sees it (ADR-0009).
    root: bool,
}

#[rustfmt::skip] // one talker per line reads as a table
const TALKERS: [Talker; 9] = [
    Talker { comm: "firefox", cmdline: "firefox", project: "", scope: Scope::External, rate: (1_800_000, 90_000), burst: 2, root: false },
    Talker { comm: "node", cmdline: "node server.js --port 3000", project: "code/storefront", scope: Scope::External, rate: (40_000, 310_000), burst: 3, root: false },
    Talker { comm: "node", cmdline: "node server.js --port 3000", project: "code/storefront", scope: Scope::Loopback, rate: (600_000, 600_000), burst: 2, root: false },
    Talker { comm: "postgres", cmdline: "postgres -D data", project: "code/storefront", scope: Scope::Loopback, rate: (500_000, 700_000), burst: 2, root: false },
    Talker { comm: "cargo", cmdline: "cargo build", project: "code/procflow", scope: Scope::External, rate: (4_000_000, 20_000), burst: 30, root: false },
    Talker { comm: "git", cmdline: "git fetch origin", project: "code/procflow", scope: Scope::External, rate: (900_000, 8_000), burst: 45, root: false },
    Talker { comm: "spotify", cmdline: "spotify", project: "", scope: Scope::External, rate: (160_000, 4_000), burst: 1, root: false },
    Talker { comm: "curl", cmdline: "", project: "", scope: Scope::External, rate: (250_000, 1_000), burst: 20, root: false },
    Talker { comm: "sshd", cmdline: "sshd: backup", project: "", scope: Scope::External, rate: (30_000, 2_500_000), burst: 10, root: true },
];

/// xorshift: enough randomness for invented numbers, no dependency.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Bytes for `seconds` of a talker: busy one poll in `burst`.
    fn bytes(&mut self, talker: &Talker, seconds: u64) -> (u64, u64) {
        let busy = self.next().is_multiple_of(talker.burst);
        let jitter = 50 + self.next() % 100; // percent
        let scale = if busy { jitter } else { jitter / 25 };
        (
            talker.rate.0 * seconds * scale / 100,
            talker.rate.1 * seconds * scale / 100,
        )
    }
}

fn scope_name(scope: Scope) -> &'static str {
    if scope == Scope::Loopback {
        "loopback"
    } else {
        "external"
    }
}

fn main() -> Result<()> {
    let socket = std::env::var_os(procflow_ipc::SOCKET_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/tmp/procflow-demo.sock".into());
    let uid = std::fs::metadata("/proc/self")?.uid();
    let user = std::env::var("USER").unwrap_or_else(|_| "demo".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/demo".into());

    let store = Store::open_in_memory()?;
    let ids: Vec<i64> = TALKERS
        .iter()
        .map(|talker| {
            // curl stands in for a process that exited before /proc was read.
            let unresolved = talker.cmdline.is_empty();
            store.upsert_identity(&IdentityRecord {
                uid: if talker.root { 0 } else { uid },
                unit_or_cgroup: "/user.slice/app.slice".into(),
                exe: if unresolved {
                    "<unresolved>".into()
                } else {
                    format!("/usr/bin/{}", talker.comm)
                },
                project_root: match (unresolved, talker.project) {
                    (true, _) => "<unresolved>".into(),
                    (false, "") => "<none>".into(),
                    (false, project) => format!("{home}/{project}"),
                },
                normalized_cmdline: if unresolved {
                    format!("<unresolved:{}>", talker.comm)
                } else {
                    talker.cmdline.into()
                },
                comm: talker.comm.into(),
                raw_cmdline: talker.cmdline.into(),
                username: Some(if talker.root {
                    "root".into()
                } else {
                    user.clone()
                }),
            })
        })
        .collect::<Result<_>>()?;

    // Five weeks of history: half an hour's worth every twelve hours, then
    // every minute of the last three quarters of an hour. The rollup fills
    // the coarser tiers from it. Kept small so the daemon is up within a few
    // seconds.
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    let now = procflowd::now_s();
    let minute = now - now % 60;
    let sparse = (1..35 * 2).map(|step| (minute - step * 12 * 3600, 1800));
    let minutely = (1..45).map(|minutes_ago| (minute - minutes_ago * 60, 60));
    for (bucket, seconds) in sparse.chain(minutely) {
        for (talker, id) in TALKERS.iter().zip(&ids) {
            let (ingress, egress) = random.bytes(talker, seconds);
            store.record_minute(bucket, *id, scope_name(talker.scope), ingress, egress)?;
        }
    }
    store.rollup(now)?;

    let store = Arc::new(Mutex::new(store));
    let hub = Arc::new(Hub::default());
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    println!(
        "procflowd demo: invented traffic, listening on {}",
        socket.display()
    );
    println!(
        "point the CLI at it:  PROCFLOW_SOCKET={} procflow",
        socket.display()
    );

    // Stands in for the collector: a tick a second, also written to the store.
    let (feed, feed_store) = (hub.clone(), store.clone());
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = procflowd::now_s();
        let store = feed_store.lock().expect("store mutex poisoned");
        let mut deltas = Vec::new();
        for (talker, id) in TALKERS.iter().zip(&ids) {
            let (ingress_bytes, egress_bytes) = random.bytes(talker, 1);
            if let Err(e) = store.record_minute(
                now - now % 60,
                *id,
                scope_name(talker.scope),
                ingress_bytes,
                egress_bytes,
            ) {
                eprintln!("demo: {e:#}");
            }
            deltas.push(Delta {
                identity_id: *id,
                scope: talker.scope,
                ingress_bytes,
                egress_bytes,
            });
        }
        feed.publish(Tick {
            at_unix_ms: now * 1000,
            interval_ms: 1000,
            deltas,
        });
    });

    Arc::new(Server {
        store,
        live: Some(hub),
        demo: true,
    })
    .serve(listener)
}
