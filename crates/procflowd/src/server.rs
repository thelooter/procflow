//! IPC server (ADR-0008): one request per connection over a Unix socket.
//!
//! Snapshot verbs get one `Ok` (or `Error`) and the connection closes;
//! `Watch` holds its connection and streams one `Chunk` per collector poll.
//! Every request is answered under the caller's [`Visibility`] (ADR-0009).

use crate::live::{Event, Hub, Tick};
use crate::query::{group_key, QueryError, Visibility};
use crate::store::Store;
use anyhow::Result;
use procflow_ipc::v1::{
    request, response, Chunk, CounterRow, End, Error, ErrorCode, GroupBy, HelloOk, Identity,
    Request, Response, Rows, Watch,
};
use procflow_ipc::{read_msg, scope_matches, write_msg, PROTO_VERSION};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a client may take to send its request, or to accept a frame.
/// Bounds the threads a stalled peer can hold.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Server {
    pub store: Arc<Mutex<Store>>,
    /// The collector's live feed; `None` while the collector is not running.
    pub live: Option<Arc<Hub>>,
}

impl Server {
    /// Accept loop: thread per connection. Snapshot connections are
    /// short-lived and a `Watch` blocks on its feed, so threads are cheap
    /// and honest here.
    pub fn serve(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let server = self.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = server.handle(stream) {
                            eprintln!("procflowd: connection error: {e}");
                        }
                    });
                }
                Err(e) => eprintln!("procflowd: accept error: {e}"),
            }
        }
        Ok(())
    }

    fn handle(&self, mut stream: UnixStream) -> Result<()> {
        let vis = Visibility::for_peer(peer_uid(&stream)?);
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let req: Request = read_msg(&mut stream)?;
        match &req.body {
            Some(request::Body::Watch(watch)) if req.proto == PROTO_VERSION => {
                self.watch(stream, req.id, watch, vis)
            }
            _ => Ok(write_msg(&mut stream, &self.respond(&req, vis))?),
        }
    }

    /// Request→response mapping for the snapshot verbs, separated from I/O
    /// for testability.
    pub fn respond(&self, req: &Request, vis: Visibility) -> Response {
        if req.proto != PROTO_VERSION {
            return error(
                req.id,
                ErrorCode::UnsupportedProtocol,
                format!(
                    "daemon supports protocol {PROTO_VERSION}..={PROTO_VERSION}, client sent {}",
                    req.proto
                ),
            );
        }
        let store = || self.store.lock().expect("store mutex poisoned");
        let now = crate::now_s();
        let result = match &req.body {
            Some(request::Body::Hello(_)) => {
                return Response {
                    id: req.id,
                    body: Some(response::Body::HelloOk(HelloOk {
                        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                        proto_min: PROTO_VERSION,
                        proto_max: PROTO_VERSION,
                        collector_active: self.live.is_some(),
                    })),
                }
            }
            Some(request::Body::TopIdentities(q)) => store().top(vis, q, now),
            Some(request::Body::Series(q)) => store().series(vis, q, now),
            Some(request::Body::ListIdentities(q)) => store().list_identities(vis, q),
            Some(request::Body::Resolve(q)) => {
                store().resolve(vis, q.identity_id).map(|identity| Rows {
                    identities: vec![identity],
                    ..Default::default()
                })
            }
            Some(request::Body::Watch(_) | request::Body::Cancel(_)) => Err(
                QueryError::BadRequest("no stream on this connection".into()),
            ),
            None => Err(QueryError::BadRequest("request has no body".into())),
        };
        match result {
            Ok(rows) => Response {
                id: req.id,
                body: Some(response::Body::Ok(rows)),
            },
            Err(e) => {
                if let QueryError::Internal(e) = &e {
                    eprintln!("procflowd: query failed: {e:#}");
                }
                error(req.id, e.code(), e.to_string())
            }
        }
    }

    /// Stream one `Chunk` per collector poll until the client hangs up or
    /// sends `Cancel` (ADR-0008).
    fn watch(&self, mut stream: UnixStream, id: u64, watch: &Watch, vis: Visibility) -> Result<()> {
        let Some(hub) = &self.live else {
            let message = "the collector is not running, so there is no live traffic to watch";
            return Ok(write_msg(
                &mut stream,
                &error(id, ErrorCode::Unavailable, message.into()),
            )?);
        };
        let (stop, events) = hub.subscribe();

        // The request side of the connection is idle from here on: a frame
        // on it is a Cancel, and an error means the client is gone.
        let mut requests = stream.try_clone()?;
        requests.set_read_timeout(None)?;
        std::thread::spawn(move || {
            while let Ok(req) = read_msg::<Request>(&mut requests) {
                if matches!(req.body, Some(request::Body::Cancel(_))) {
                    break;
                }
            }
            let _ = stop.send(Event::Stop);
        });

        let mut view = WatchView::new(watch, vis);
        for event in events {
            let response = match event {
                Event::Tick(tick) => match view.chunk(&self.store, &tick) {
                    Ok(chunk) => Response {
                        id,
                        body: Some(response::Body::Chunk(chunk)),
                    },
                    Err(e) => error(id, e.code(), e.to_string()),
                },
                Event::Stop => Response {
                    id,
                    body: Some(response::Body::End(End {})),
                },
            };
            let streaming = matches!(response.body, Some(response::Body::Chunk(_)));
            if write_msg(&mut stream, &response).is_err() || !streaming {
                break;
            }
        }
        // Unblocks the request reader if it is still waiting.
        let _ = stream.shutdown(Shutdown::Both);
        Ok(())
    }
}

/// One `Watch` stream's request and what it has already told its client.
struct WatchView {
    vis: Visibility,
    watch: Watch,
    /// Identities looked up so far; `None` for ones the caller may not see.
    known: HashMap<i64, Option<Identity>>,
    sent: HashSet<i64>,
}

impl WatchView {
    fn new(watch: &Watch, vis: Visibility) -> Self {
        WatchView {
            vis,
            watch: *watch,
            known: HashMap::new(),
            sent: HashSet::new(),
        }
    }

    /// The tick as this stream's caller may see it: scoped, grouped, ranked.
    fn chunk(&mut self, store: &Mutex<Store>, tick: &Tick) -> Result<Chunk, QueryError> {
        let mut unknown: Vec<i64> = tick
            .deltas
            .iter()
            .map(|d| d.identity_id)
            .filter(|id| !self.known.contains_key(id))
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        if !unknown.is_empty() {
            let visible = store
                .lock()
                .expect("store mutex poisoned")
                .identities_by_id(self.vis, &unknown)?;
            self.known.extend(unknown.into_iter().map(|id| (id, None)));
            self.known.extend(
                visible
                    .into_iter()
                    .map(|identity| (identity.id, Some(identity))),
            );
        }

        let by = self.watch.group_by();
        let grouped = !matches!(by, GroupBy::Identity | GroupBy::Unspecified);
        let mut sums: BTreeMap<(i64, String, i32), (u64, u64)> = BTreeMap::new();
        for delta in &tick.deltas {
            let Some(Some(identity)) = self.known.get(&delta.identity_id) else {
                continue;
            };
            if !scope_matches(self.watch.scope(), delta.scope) {
                continue;
            }
            let id = if grouped { 0 } else { identity.id };
            let sum = sums
                .entry((id, group_key(identity, by), delta.scope as i32))
                .or_default();
            sum.0 += delta.ingress_bytes;
            sum.1 += delta.egress_bytes;
        }

        let mut counters: Vec<CounterRow> = sums
            .into_iter()
            .map(
                |((identity_id, group_key, scope), (ingress_bytes, egress_bytes))| CounterRow {
                    identity_id,
                    bucket_unix_ms: tick.at_unix_ms,
                    scope,
                    ingress_bytes,
                    egress_bytes,
                    group_key,
                },
            )
            .collect();
        // Same ranking as `top`: an ordering only, the sum is not sent.
        counters.sort_by_key(|c| std::cmp::Reverse(c.ingress_bytes + c.egress_bytes));
        if self.watch.limit > 0 {
            counters.truncate(self.watch.limit as usize);
        }

        let identities = counters
            .iter()
            .filter(|c| c.identity_id != 0 && self.sent.insert(c.identity_id))
            .filter_map(|c| self.known[&c.identity_id].clone())
            .collect();
        Ok(Chunk {
            rows: Some(Rows {
                counters,
                identities,
                ..Default::default()
            }),
            interval_ms: tick.interval_ms,
        })
    }
}

fn error(id: u64, code: ErrorCode, message: String) -> Response {
    Response {
        id,
        body: Some(response::Body::Error(Error {
            code: code as i32,
            message,
        })),
    }
}

/// The connecting process's uid as the kernel recorded it at `connect()`
/// (ADR-0009). Nothing the client sends can change it.
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live, writable, and `len` is `cred`'s size.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc == 0 {
        Ok(cred.uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}
