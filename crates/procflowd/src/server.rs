//! IPC server (ADR-0008): one request per connection over a Unix socket.
//!
//! Snapshot verbs get one `Ok` (or `Error`) and the connection closes; `Watch`
//! will hold its connection and stream `Chunk`s once the query layer exists.

use anyhow::Result;
use procflow_ipc::v1::{request, response, Error, ErrorCode, HelloOk, Request, Response};
use procflow_ipc::{read_msg, write_msg, PROTO_VERSION};
use std::os::unix::net::{UnixListener, UnixStream};

/// Accept loop: thread per connection. Connections are short-lived
/// (one request each), so threads are cheap and honest here.
pub fn serve(listener: UnixListener) -> Result<()> {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(move || {
                    if let Err(e) = handle(stream) {
                        eprintln!("procflowd: connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("procflowd: accept error: {e}"),
        }
    }
    Ok(())
}

fn handle(mut stream: UnixStream) -> Result<()> {
    let req: Request = read_msg(&mut stream)?;
    let resp = respond(&req);
    write_msg(&mut stream, &resp)?;
    Ok(())
}

/// Pure request→response mapping, separated from I/O for testability.
pub fn respond(req: &Request) -> Response {
    let error = |code: ErrorCode, message: &str| Response {
        id: req.id,
        body: Some(response::Body::Error(Error {
            code: code as i32,
            message: message.to_string(),
        })),
    };

    if req.proto != PROTO_VERSION {
        return error(
            ErrorCode::UnsupportedProtocol,
            &format!(
                "daemon supports protocol {PROTO_VERSION}..={PROTO_VERSION}, client sent {}",
                req.proto
            ),
        );
    }

    match &req.body {
        Some(request::Body::Hello(_)) => Response {
            id: req.id,
            body: Some(response::Body::HelloOk(HelloOk {
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                proto_min: PROTO_VERSION,
                proto_max: PROTO_VERSION,
            })),
        },
        Some(_) => error(
            ErrorCode::Unimplemented,
            "query layer not implemented yet — only Hello is served",
        ),
        None => error(ErrorCode::BadRequest, "request has no body"),
    }
}
