//! End-to-end IPC test: real Unix socket, real frames, no privileges needed.

use procflow_ipc::v1::{request, response, ErrorCode, Hello, Request, Response, Watch};
use procflow_ipc::{read_msg, write_msg, PROTO_VERSION};
use std::os::unix::net::{UnixListener, UnixStream};

fn start_server() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "procflow-test-{}-{:?}.sock",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || procflowd::server::serve(listener));
    path
}

fn roundtrip(path: &std::path::Path, req: Request) -> Response {
    let mut stream = UnixStream::connect(path).unwrap();
    write_msg(&mut stream, &req).unwrap();
    read_msg(&mut stream).unwrap()
}

#[test]
fn hello_roundtrip_over_real_socket() {
    let path = start_server();
    let resp = roundtrip(
        &path,
        Request {
            proto: PROTO_VERSION,
            id: 7,
            body: Some(request::Body::Hello(Hello {})),
        },
    );
    assert_eq!(resp.id, 7);
    match resp.body.unwrap() {
        response::Body::HelloOk(ok) => {
            assert_eq!(ok.daemon_version, env!("CARGO_PKG_VERSION"));
            assert_eq!((ok.proto_min, ok.proto_max), (PROTO_VERSION, PROTO_VERSION));
        }
        other => panic!("expected HelloOk, got {other:?}"),
    }
}

#[test]
fn wrong_proto_version_is_rejected() {
    let path = start_server();
    let resp = roundtrip(
        &path,
        Request {
            proto: 999,
            id: 1,
            body: Some(request::Body::Hello(Hello {})),
        },
    );
    match resp.body.unwrap() {
        response::Body::Error(e) => {
            assert_eq!(e.code, ErrorCode::UnsupportedProtocol as i32);
            assert!(e.message.contains("999"));
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn unimplemented_verbs_are_reported_honestly() {
    let path = start_server();
    let resp = roundtrip(
        &path,
        Request {
            proto: PROTO_VERSION,
            id: 2,
            body: Some(request::Body::Watch(Watch::default())),
        },
    );
    match resp.body.unwrap() {
        response::Body::Error(e) => assert_eq!(e.code, ErrorCode::Unimplemented as i32),
        other => panic!("expected Error, got {other:?}"),
    }
}
