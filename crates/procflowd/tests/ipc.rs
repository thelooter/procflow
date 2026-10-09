//! End-to-end IPC test: real Unix socket, real frames, no privileges needed.

use procflow_ipc::v1::{
    request, response, Cancel, ErrorCode, GroupBy, Hello, Request, Resolve, Response, Scope,
    TimeRange, TopIdentities, Watch,
};
use procflow_ipc::{read_msg, write_msg, PROTO_VERSION};
use procflowd::enrich::IdentityRecord;
use procflowd::live::{Delta, Hub, Tick};
use procflowd::server::Server;
use procflowd::store::Store;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A served store holding one Identity owned by the test's own uid and one
/// owned by somebody else, each with a minute of traffic just now.
struct Fixture {
    socket: std::path::PathBuf,
    hub: Arc<Hub>,
    mine: i64,
    theirs: i64,
}

fn own_uid() -> u32 {
    std::fs::metadata("/proc/self").unwrap().uid()
}

fn start_server(collector_running: bool) -> Fixture {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let socket = std::env::temp_dir().join(format!(
        "procflow-test-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&socket);

    let store = Store::open_in_memory().unwrap();
    let identity = |uid: u32, comm: &str| {
        store
            .upsert_identity(&IdentityRecord {
                uid,
                unit_or_cgroup: "/user.slice".into(),
                exe: format!("/usr/bin/{comm}"),
                project_root: format!("/srv/{comm}"),
                normalized_cmdline: comm.into(),
                comm: comm.into(),
                raw_cmdline: comm.into(),
                username: None,
            })
            .unwrap()
    };
    let (mine, theirs) = (
        identity(own_uid(), "mine"),
        identity(own_uid() + 1, "theirs"),
    );
    let minute = procflowd::now_s() / 60 * 60;
    store
        .record_minute(minute, mine, "external", 100, 10)
        .unwrap();
    store
        .record_minute(minute, theirs, "external", 9000, 9000)
        .unwrap();

    let hub = Arc::new(Hub::default());
    let server = Arc::new(Server {
        store: Arc::new(Mutex::new(store)),
        live: collector_running.then(|| hub.clone()),
    });
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || server.serve(listener));
    Fixture {
        socket,
        hub,
        mine,
        theirs,
    }
}

fn send(socket: &std::path::Path, proto: u32, body: request::Body) -> UnixStream {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write_msg(
        &mut stream,
        &Request {
            proto,
            id: 7,
            body: Some(body),
        },
    )
    .unwrap();
    stream
}

fn roundtrip(socket: &std::path::Path, body: request::Body) -> response::Body {
    let resp: Response = read_msg(&mut send(socket, PROTO_VERSION, body)).unwrap();
    assert_eq!(resp.id, 7);
    resp.body.unwrap()
}

/// Publish the same tick over and over, since a stream only sees ticks
/// published after the server has subscribed it.
fn keep_publishing(fixture: &Fixture) {
    let hub = fixture.hub.clone();
    let delta = |identity_id, scope, bytes| Delta {
        identity_id,
        scope,
        ingress_bytes: bytes,
        egress_bytes: bytes,
    };
    let tick = Tick {
        at_unix_ms: 1_000,
        interval_ms: 1000,
        deltas: vec![
            delta(fixture.mine, Scope::External, 5),
            delta(fixture.mine, Scope::Loopback, 70),
            delta(fixture.theirs, Scope::External, 900),
        ],
    };
    std::thread::spawn(move || loop {
        hub.publish(tick.clone());
        std::thread::sleep(Duration::from_millis(5));
    });
}

fn next_chunk(stream: &mut UnixStream) -> procflow_ipc::v1::Chunk {
    match read_msg::<Response>(stream).unwrap().body.unwrap() {
        response::Body::Chunk(chunk) => chunk,
        other => panic!("expected Chunk, got {other:?}"),
    }
}

#[test]
fn hello_roundtrip_over_real_socket() {
    for collector_running in [false, true] {
        let fixture = start_server(collector_running);
        match roundtrip(&fixture.socket, request::Body::Hello(Hello {})) {
            response::Body::HelloOk(ok) => {
                assert_eq!(ok.daemon_version, env!("CARGO_PKG_VERSION"));
                assert_eq!((ok.proto_min, ok.proto_max), (PROTO_VERSION, PROTO_VERSION));
                assert_eq!(ok.collector_active, collector_running);
            }
            other => panic!("expected HelloOk, got {other:?}"),
        }
    }
}

#[test]
fn wrong_proto_version_is_rejected() {
    let fixture = start_server(true);
    for body in [
        request::Body::Hello(Hello {}),
        request::Body::Watch(Watch::default()),
    ] {
        let resp: Response = read_msg(&mut send(&fixture.socket, 999, body)).unwrap();
        match resp.body.unwrap() {
            response::Body::Error(e) => {
                assert_eq!(e.code, ErrorCode::UnsupportedProtocol as i32);
                assert!(e.message.contains("999"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

#[test]
fn queries_are_scoped_to_the_connecting_uid() {
    let fixture = start_server(false);
    let now_ms = procflowd::now_s() * 1000;
    let top = request::Body::TopIdentities(TopIdentities {
        range: Some(TimeRange {
            from_unix_ms: now_ms - 3_600_000,
            to_unix_ms: now_ms + 60_000,
        }),
        ..Default::default()
    });
    let response::Body::Ok(rows) = roundtrip(&fixture.socket, top) else {
        panic!("expected Ok")
    };
    let seen: Vec<i64> = rows.counters.iter().map(|c| c.identity_id).collect();
    let resolve_theirs = roundtrip(
        &fixture.socket,
        request::Body::Resolve(Resolve {
            identity_id: fixture.theirs,
        }),
    );
    if own_uid() == 0 {
        // root sees the whole machine (ADR-0009).
        assert_eq!(seen, [fixture.theirs, fixture.mine]);
        assert!(matches!(resolve_theirs, response::Body::Ok(_)));
    } else {
        assert_eq!(seen, [fixture.mine]);
        assert_eq!(rows.counters[0].ingress_bytes, 100);
        assert_eq!(rows.identities.len(), 1);
        match resolve_theirs {
            response::Body::Error(e) => assert_eq!(e.code, ErrorCode::NotFound as i32),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}

#[test]
fn bad_requests_are_reported() {
    let fixture = start_server(false);
    let no_range = request::Body::TopIdentities(TopIdentities::default());
    let cancel = request::Body::Cancel(Cancel { target_id: 1 });
    for body in [no_range, cancel] {
        match roundtrip(&fixture.socket, body) {
            response::Body::Error(e) => assert_eq!(e.code, ErrorCode::BadRequest as i32),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}

#[test]
fn watch_streams_chunks_until_cancelled() {
    let fixture = start_server(true);
    keep_publishing(&fixture);
    let watch = Watch {
        scope: Scope::All as i32,
        ..Default::default()
    };
    let mut stream = send(&fixture.socket, PROTO_VERSION, request::Body::Watch(watch));

    let first = next_chunk(&mut stream);
    assert_eq!(first.interval_ms, 1000);
    let rows = first.rows.unwrap();
    if own_uid() != 0 {
        // Biggest first, one row per scope, nothing of anyone else's.
        let seen: Vec<_> = rows
            .counters
            .iter()
            .map(|c| (c.identity_id, c.scope(), c.ingress_bytes))
            .collect();
        assert_eq!(
            seen,
            [
                (fixture.mine, Scope::Loopback, 70),
                (fixture.mine, Scope::External, 5)
            ]
        );
        assert_eq!(rows.identities.len(), 1);
    }
    assert!(rows
        .identities
        .iter()
        .any(|i| i.id == fixture.mine && i.comm == "mine"));

    // An Identity is described once per stream.
    let second = next_chunk(&mut stream).rows.unwrap();
    assert!(!second.counters.is_empty());
    assert!(second.identities.is_empty());

    // Cancel on the same connection ends the stream with End.
    let cancel = Request {
        proto: PROTO_VERSION,
        id: 8,
        body: Some(request::Body::Cancel(Cancel { target_id: 7 })),
    };
    write_msg(&mut stream, &cancel).unwrap();
    loop {
        match read_msg::<Response>(&mut stream).unwrap().body.unwrap() {
            response::Body::Chunk(_) => continue,
            response::Body::End(_) => break,
            other => panic!("expected End, got {other:?}"),
        }
    }
    assert!(
        read_msg::<Response>(&mut stream).is_err(),
        "the daemon closes after End"
    );
}

#[test]
fn watch_honours_scope_group_and_limit() {
    let fixture = start_server(true);
    keep_publishing(&fixture);
    // Default scope is external only.
    let mut stream = send(
        &fixture.socket,
        PROTO_VERSION,
        request::Body::Watch(Watch::default()),
    );
    let rows = next_chunk(&mut stream).rows.unwrap();
    assert!(rows.counters.iter().all(|c| c.scope() == Scope::External));

    let watch = Watch {
        scope: Scope::All as i32,
        group_by: GroupBy::Project as i32,
        limit: 1,
    };
    let mut stream = send(&fixture.socket, PROTO_VERSION, request::Body::Watch(watch));
    let rows = next_chunk(&mut stream).rows.unwrap();
    assert_eq!(rows.counters.len(), 1);
    assert_eq!(rows.counters[0].identity_id, 0);
    assert!(rows.counters[0].group_key.starts_with("/srv/"));
    assert!(rows.identities.is_empty());
}

#[test]
fn watch_without_a_collector_is_unavailable() {
    let fixture = start_server(false);
    match roundtrip(&fixture.socket, request::Body::Watch(Watch::default())) {
        response::Body::Error(e) => assert_eq!(e.code, ErrorCode::Unavailable as i32),
        other => panic!("expected Error, got {other:?}"),
    }
}
