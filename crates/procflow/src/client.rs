//! The client half of the IPC protocol (ADR-0008): a fresh connection per
//! request.

use anyhow::{anyhow, bail, Context, Result};
use procflow_ipc::v1::{request, response, Chunk, Hello, HelloOk, Request, Response, Rows, Watch};
use procflow_ipc::{read_msg, write_msg, PROTO_VERSION};
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// How long a snapshot verb may take before the daemon counts as hung.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Open a connection and send one request on it.
fn send(body: request::Body) -> Result<UnixStream> {
    let path = procflow_ipc::socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| match e.kind() {
        // ADR-0010: a down daemon is a clear error, never an empty result.
        ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
            anyhow!(
                "procflow daemon not running (cannot connect to {})",
                path.display()
            )
        }
        _ => anyhow!(e).context(format!(
            "connecting to the procflow daemon at {}",
            path.display()
        )),
    })?;
    write_msg(
        &mut stream,
        &Request {
            proto: PROTO_VERSION,
            id: 1,
            body: Some(body),
        },
    )?;
    Ok(stream)
}

fn daemon_error(e: procflow_ipc::v1::Error) -> anyhow::Error {
    anyhow!("{}", e.message)
}

pub fn hello() -> Result<HelloOk> {
    let mut stream = send(request::Body::Hello(Hello {}))?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    match read_msg::<Response>(&mut stream)
        .context("reading the daemon's reply")?
        .body
    {
        Some(response::Body::HelloOk(ok)) => Ok(ok),
        Some(response::Body::Error(e)) => Err(daemon_error(e)),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Run one snapshot verb.
pub fn rows(body: request::Body) -> Result<Rows> {
    let mut stream = send(body)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    match read_msg::<Response>(&mut stream)
        .context("reading the daemon's reply")?
        .body
    {
        Some(response::Body::Ok(rows)) => Ok(rows),
        Some(response::Body::Error(e)) => Err(daemon_error(e)),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// A live `Watch` stream. Dropping it hangs up, which ends the stream.
pub struct WatchStream(UnixStream);

pub fn watch(watch: Watch) -> Result<WatchStream> {
    send(request::Body::Watch(watch)).map(WatchStream)
}

impl Iterator for WatchStream {
    type Item = Result<Chunk>;

    /// The next chunk; `None` once the daemon ends the stream.
    fn next(&mut self) -> Option<Self::Item> {
        match read_msg::<Response>(&mut self.0).map(|r| r.body) {
            Ok(Some(response::Body::Chunk(chunk))) => Some(Ok(chunk)),
            Ok(Some(response::Body::End(_))) => None,
            Ok(Some(response::Body::Error(e))) => Some(Err(daemon_error(e))),
            Ok(other) => Some(Err(anyhow!("unexpected response: {other:?}"))),
            Err(e) => Some(Err(anyhow!(e).context("the daemon closed the live stream"))),
        }
    }
}
