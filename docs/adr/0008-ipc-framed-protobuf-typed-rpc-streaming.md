# IPC query protocol: framed protobuf, typed RPC verbs, streaming over a Unix socket

## Context

ADR-0001 makes the daemon the sole owner of DuckDB; the CLI queries over a local
Unix socket and is inert without the daemon. ADR-0001 explicitly defers "build
and version an IPC query protocol" to here. Constraints: query results are tiny
(hundreds of thousands of rows *total* at rest); a live `watch` view is in v1
scope (needs streaming); and the CLI and daemon may be upgraded independently, so
the protocol must tolerate version drift gracefully.

## Decision

- **Transport:** a `SOCK_STREAM` Unix domain socket at
  `/run/procflow/procflow.sock`, created by the systemd-managed daemon. Filesystem
  permissions are a coarse outer gate; the real visibility boundary is the peer-uid
  check in ADR-0009.
- **Framing:** length-prefixed — a `u32` little-endian byte length followed by that
  many bytes of payload. One frame carries one encoded protobuf message. (Protobuf
  is not self-delimiting on a stream, so we supply the delimiter.)
- **Encoding:** **protobuf**, compiled with [`prost`](https://docs.rs/prost) /
  `prost-build` from a `.proto` schema that lives in the shared IPC crate. Chosen
  for rigorous, field-number-based schema evolution across independently-upgraded
  CLI and daemon — deliberately trading away wire-readability (see Consequences).
- **Shape — typed RPC.** The client sends one `Request`; the daemon replies with
  one or more `Response` frames on the same connection.
  - `Request { proto: uint32, id: uint64, body: oneof }` — `proto` is the protocol
    version, `id` correlates responses, `body` is a `oneof` verb such as
    `TopIdentities{ window, tier, direction?, scope?, limit }`,
    `Series{ identity_id, tier, from, to, direction?, scope? }`,
    `ListIdentities{ filter? }`, `Resolve{ identity_id }`, `Watch{ … }`,
    `Cancel{ target_id }`, `Hello{}`.
  - `Response { id: uint64, body: oneof }` where the `oneof` is `Ok{ … }` for a
    snapshot result, `Error{ code, message }`, or — for streams — a run of
    `Chunk{ … }` terminated by `End{}`. A snapshot verb sends exactly one `Ok`
    then the daemon closes; `Watch` emits `Chunk`s until the client disconnects or
    sends `Cancel`.
- **Query surface is a fixed set of typed verbs — no raw-SQL passthrough.** The
  daemon owns all SQL and the schema; coarser-than-tier rollups (ADR-0004/0005)
  are computed server-side. A raw-SQL channel would freeze the schema into the CLI
  and be a lock/injection hazard.
- **Versioning:** protobuf **field numbers** carry additive evolution for free —
  new optional fields and new `oneof` verbs are backward/forward compatible without
  a version bump. `proto: uint32` guards *breaking* changes only: on connect the
  daemon checks it against its supported range and, on mismatch, replies
  `Error{ code: UNSUPPORTED_PROTOCOL }` naming its range, then closes. The optional
  `Hello` handshake lets the CLI report the daemon's version/build in error
  messages.
- **Streaming & connections:** one request per connection (no multiplexing); the
  CLI opens a fresh connection per invocation. `Watch` holds its connection open,
  streaming `Chunk`s (e.g. one per closed minute or per poll-interval delta).
  Cancellation is the client closing the socket (daemon sees EOF/EPIPE and drops
  the watch); an explicit `Cancel` is also honoured.

## Considered options

- **JSON (serde).** Human-readable and `jq`-pokeable with forgiving additive
  evolution — the recommended default for debuggability — but its schema-evolution
  guarantees are weaker than protobuf's field numbers. Rejected in favour of the
  stricter contract; readability is recovered via tooling (see Consequences).
- **gRPC via `tonic`.** Would give protobuf + server-streaming (a natural fit for
  `Watch`) out of the box, but drags HTTP/2 onto a single-host Unix socket — heavy
  machinery against the "simple local socket" spirit of ADR-0001. We keep `prost`
  for encoding and our own thin length-prefixed framing instead.
- **MessagePack / bincode.** Compact but not wire-readable, and bincode demands
  CLI/daemon version lockstep. No schema-evolution story worth the trade.
- **Raw-SQL passthrough.** Maximally flexible, but couples the CLI to the schema,
  invites injection, and leaks the storage model. Rejected for typed verbs.

## Consequences

- CLI and daemon share one versioned crate that generates the `Request`/`Response`
  types from the `.proto` via `prost-build` — a single source of truth — and field
  numbers keep a mismatched-but-close version interoperating additively.
- The wire is **not human-readable**: debugging needs `protoc --decode Request
  procflow.proto` (or a small `procflow debug` helper the CLI can ship), not
  `nc`/`jq`. This is the accepted cost of the protobuf choice.
- New query shapes require a new typed verb (an additive, version-compatible
  `oneof` arm), never ad-hoc SQL.
- One-request-per-connection keeps the lifecycle trivial and sidesteps
  head-of-line blocking; `watch` simply holds its own connection.
- A `.proto` schema + `prost-build` codegen step is now part of the build.
