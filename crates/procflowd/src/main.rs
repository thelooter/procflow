mod store;

fn main() -> anyhow::Result<()> {
    // Scaffold: prove the schema opens and migrates against real DuckDB.
    // Daemon wiring (eBPF collector, IPC server, rollup job) lands next.
    let store = store::Store::open_in_memory()?;
    println!(
        "procflowd {} — schema v{} (ipc proto v{})",
        env!("CARGO_PKG_VERSION"),
        store.schema_version()?,
        procflow_ipc::PROTO_VERSION,
    );
    Ok(())
}
