use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use procflow_ipc::v1::{request, response, Hello, Request, Response};
use procflow_ipc::{read_msg, write_msg, PROTO_VERSION};
use std::os::unix::net::UnixStream;

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

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Status => status(),
        _ => {
            // Query layer lands next; report honestly rather than pretending
            // (ADR-0010: never a silent empty result).
            eprintln!("procflow: not implemented yet — the query layer is still being built");
            std::process::exit(1);
        }
    }
}

/// One request over a fresh connection (ADR-0008).
fn request(body: request::Body) -> Result<Response> {
    let path = procflow_ipc::socket_path();
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "procflow daemon not running (cannot connect to {})",
            path.display()
        )
    })?;
    write_msg(&mut stream, &Request { proto: PROTO_VERSION, id: 1, body: Some(body) })?;
    Ok(read_msg(&mut stream)?)
}

fn status() -> Result<()> {
    let resp = request(request::Body::Hello(Hello {}))?;
    match resp.body {
        Some(response::Body::HelloOk(ok)) => {
            println!("daemon:   procflowd {}", ok.daemon_version);
            println!("protocol: v{}..=v{} (client v{})", ok.proto_min, ok.proto_max, PROTO_VERSION);
            println!("socket:   {}", procflow_ipc::socket_path().display());
            Ok(())
        }
        Some(response::Body::Error(e)) => bail!("daemon error: {} ({})", e.message, e.code),
        other => bail!("unexpected response: {other:?}"),
    }
}
