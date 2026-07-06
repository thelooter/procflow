use clap::{Parser, Subcommand};

/// Per-process network traffic, tracked over time.
#[derive(Parser)]
#[command(name = "procflow", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Subcommands map ~1:1 to IPC verbs (ADR-0010). Flags land with the query layer.
#[derive(Subcommand)]
enum Command {
    /// Biggest talkers in a time window
    Top,
    /// One identity's history over time
    Series { identity_id: i64 },
    /// Browse/search identities
    List,
    /// Full identity detail
    Show { identity_id: i64 },
    /// Live-updating view
    Watch,
    /// Daemon status and versions
    Status,
}

fn main() {
    let _cli = Cli::parse();
    // Query layer lands with the daemon's IPC server; until then every verb
    // reports the same thing rather than pretending (ADR-0010: never a
    // silent empty result).
    eprintln!(
        "procflow: not implemented yet — the daemon and query layer are still being built \
         (will connect to {})",
        procflow_ipc::SOCKET_PATH
    );
    std::process::exit(1);
}
