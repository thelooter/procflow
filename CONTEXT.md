# procflow

A tool that tracks network traffic over time, attributed to the process (or
logical project) that generated it — conceptually nethogs (per-process) crossed
with vnstat (historical rollups).

## Language

**Identity**:
The stable, deduplicated record of *who* generated traffic — the unit traffic is
attributed to and aggregated by. Derived from process dimensions (owning user,
cgroup/unit, exe, project root, normalized cmdline, comm), never from PID.
Repeated runs of the same thing collapse to one Identity.
_Avoid_: process, PID, task (a PID is ephemeral; an Identity is durable)

**Project root**:
The directory identifying the *logical project* a process belongs to, found by
walking up from the process's working directory for a marker (`.git`,
`package.json`, `pyproject.toml`, `Cargo.toml`). Distinguishes two `pnpm run
dev` invocations from different repos that share the same `comm` and `exe`.
_Avoid_: cwd, working directory (those are the raw input; project root is derived)

**Tier**:
A fixed time-resolution band of stored traffic counters (minute, hour, day,
month). Coarser tiers are downsampled from finer ones as data ages; each tier
has its own retention window.
_Avoid_: bucket, rollup level

**Direction**:
Whether bytes were received (ingress) or sent (egress). Stored separately, never
summed into a single "total" at rest.
_Avoid_: rx/tx (use ingress/egress in the domain), up/down

**Payload bytes**:
procflow's one and only traffic metric: application-level data bytes seen at the
socket layer, excluding TCP/IP/Ethernet headers, retransmits, and ACKs. Chosen
because the socket layer is the only place an Identity (PID → project root) can
be attributed; consequently totals read slightly *lower* than interface counters
like vnstat, and that is expected, not a bug.
_Avoid_: wire bytes, on-wire bytes (a different, unsupported metric)

**Unresolved Identity**:
A degraded Identity used when a process exited before the daemon could read
`/proc` to derive its `exe`, `project_root`, and `normalized_cmdline`. Still
correctly keyed by the kernel-captured `uid`, cgroup, and `comm`, with those
three fields set to a `<unresolved>` sentinel. Surfaced in views, never silently
dropped — the honest marker that some short-lived traffic was only coarsely
attributed.
_Avoid_: unknown, missing (attribution is partial, not absent)

**Scope**:
Whether traffic actually left the machine (`external`) or stayed on loopback
(`loopback`) — local services, dev-server-to-local-DB, IPC over TCP. A stored
dimension on every counter. Default views show `external` only; `loopback` is
opt-in, never silently discarded.
_Avoid_: local/remote, internal/external (use loopback/external)
