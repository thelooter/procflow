# Collection is global; viewing is scoped to the caller's uid via SO_PEERCRED

## Context

The daemon collects all traffic on the host regardless of user — it runs
privileged and attributes by `tgid` → Identity (ADR-0006/0007), and every
Identity carries a `uid` dimension (ADR-0004). On a shared machine, one user
seeing another user's per-project traffic is a mild privacy leak. We want full
collection but per-user viewing.

## Decision

- **Collection is unconditional and global.** The store holds every Identity's
  counters, keyed with `uid` as an Identity dimension. Scoping is a *query-layer*
  concern only, never a collection-layer one — there is exactly one complete store.
- **The daemon authenticates each IPC connection via `SO_PEERCRED`** (a
  `getsockopt` on the accepted socket), reading the caller's uid — kernel-provided
  and unspoofable for local sockets, unlike anything sent in-band.
  - A **non-root** caller sees only Identities whose `uid` equals the caller's;
    every typed verb (ADR-0008) gains an implicit server-side `uid` predicate.
    Unresolved Identities (ADR-0007) carry a kernel-captured `uid`, so they scope
    correctly too.
  - **root** (uid 0), and optionally members of a configured `procflow` admin
    group, see all Identities. An admin caller may pass an explicit `uid` filter
    (or "all") to widen or narrow the view.
- Socket filesystem permissions gate *reachability* coarsely (e.g. the `procflow`
  group, or `0666` with the daemon doing all gating); the `SO_PEERCRED` uid check
  is the actual visibility boundary.

## Rationale

- Enforcing at read time rather than collection time keeps a single complete
  store and lets an admin get a whole-machine view without re-collecting — the
  privacy boundary sits exactly where it belongs: on reads.
- The visibility rule lives in **one place** (the query layer), applied uniformly
  to every verb, instead of being re-implemented per query shape.

## Consequences

- The CLI cannot see other users' traffic unless run as root or in the admin
  group — expected and intended.
- A future web/remote UI would need its own authentication and must **reuse this
  same scoping rule**, not bypass it — the daemon, not the transport, owns the
  boundary.
- `SO_PEERCRED` is Linux-specific; this ties the IPC boundary to Linux, which is
  already a hard requirement (eBPF, ADR-0006).
