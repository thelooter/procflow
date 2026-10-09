use anyhow::{Context, Result};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::time::Duration;

/// How often the collector reads the kernel counters: the resolution of the
/// live view. Becomes config-driven with ADR-0011's config.toml.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How often closed buckets are rolled up and old rows pruned (ADR-0005).
const ROLLUP_INTERVAL: Duration = Duration::from_secs(60);

fn main() -> Result<()> {
    // PROCFLOW_DB overrides for dev; default is the packaged path (ADR-0011).
    // In-memory ONLY when explicitly requested (":memory:") — counters are
    // history, silently losing them on restart would be a lie.
    let db_path = std::env::var_os("PROCFLOW_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/var/lib/procflow/procflow.duckdb".into());
    let store = if db_path.as_os_str() == ":memory:" {
        procflowd::store::Store::open_in_memory()?
    } else {
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating state directory {}", dir.display()))?;
        }
        procflowd::store::Store::open(&db_path)
            .with_context(|| format!("opening store {}", db_path.display()))?
    };
    let schema_version = store.schema_version()?;
    let store = Arc::new(std::sync::Mutex::new(store));
    let socket = procflow_ipc::socket_path();

    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating socket directory {}", dir.display()))?;
    }
    // Remove a stale socket from a previous run (bind fails on an existing path).
    match std::fs::symlink_metadata(&socket) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(&socket)?,
        Ok(_) => anyhow::bail!("{} exists and is not a socket — refusing to remove it", socket.display()),
        Err(_) => {}
    }

    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    // Any local user may connect; what each one sees is decided per
    // connection from its peer uid (ADR-0009).
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("opening {} to local users", socket.display()))?;

    // eBPF collection is best-effort at this stage: without CAP_BPF +
    // CAP_PERFMON (ADR-0011) the daemon still serves stored history.
    let hub = Arc::new(procflowd::live::Hub::default());
    let collector = match procflowd::collector::start(store.clone(), hub.clone(), POLL_INTERVAL) {
        Ok(handle) => {
            println!("procflowd: eBPF collector attached");
            Some(handle)
        }
        Err(e) => {
            eprintln!("procflowd: collector disabled: {e:#}");
            eprintln!("procflowd: (needs CAP_BPF + CAP_PERFMON and the BPF object — see ADR-0011)");
            None
        }
    };

    // The first pass heals whatever closed while the daemon was down.
    let rollup_store = store.clone();
    std::thread::Builder::new().name("rollup".into()).spawn(move || loop {
        let result = rollup_store.lock().expect("store mutex poisoned").rollup(procflowd::now_s());
        if let Err(e) = result {
            eprintln!("procflowd: rollup failed: {e:#}");
        }
        std::thread::sleep(ROLLUP_INTERVAL);
    })?;
    println!(
        "procflowd {} — schema v{schema_version}, ipc proto v{}, listening on {}",
        env!("CARGO_PKG_VERSION"),
        procflow_ipc::PROTO_VERSION,
        socket.display(),
    );
    let live = collector.is_some().then_some(hub);
    Arc::new(procflowd::server::Server { store, live, demo: false }).serve(listener)
}
