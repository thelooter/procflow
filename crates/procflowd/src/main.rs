use anyhow::{Context, Result};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixListener;

fn main() -> Result<()> {
    // Store: in-memory until the collector lands and there is something to
    // persist; the real file path + config come with it (ADR-0011).
    let store = procflowd::store::Store::open_in_memory()?;
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

    // eBPF collection is best-effort at this stage: without CAP_BPF +
    // CAP_PERFMON (ADR-0011) the daemon still serves stored history.
    let _collector = match procflowd::collector::start(std::time::Duration::from_secs(5)) {
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
    println!(
        "procflowd {} — schema v{}, ipc proto v{}, listening on {}",
        env!("CARGO_PKG_VERSION"),
        store.schema_version()?,
        procflow_ipc::PROTO_VERSION,
        socket.display(),
    );
    procflowd::server::serve(listener)
}
