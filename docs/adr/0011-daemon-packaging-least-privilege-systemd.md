# Daemon packaging: dedicated system user, least-privilege capabilities, systemd-managed FHS layout

## Context

The collector daemon must load eBPF (ADR-0006/0007), read every process's `/proc`
for enrichment, own the DuckDB store (ADR-0001), and serve the query socket
(ADR-0008/0009). It runs continuously and starts at boot. We want **least
privilege**, not blanket root.

## Decision

- Ship **two binaries**: `procflowd` (daemon) and `procflow` (CLI, ADR-0010).
- Run `procflowd` as a **dedicated system user `procflow` under systemd — not
  root**. Capabilities (kernel ≥5.8, ADR-0006):
  - **`CAP_BPF`** — load programs and maps.
  - **`CAP_PERFMON`** — load fentry/fexit *tracing* programs (this is what replaces
    the old blanket `CAP_SYS_ADMIN` since 5.8).
  - **`CAP_SYS_PTRACE`** — `readlink` `/proc/<pid>/{exe,cwd}` for processes owned by
    *other* users (governed by `PTRACE_MODE_READ_FSCREDS`). **Verify at
    implementation time:** `CAP_PERFMON` — already held — may itself satisfy this
    `/proc` read check on recent kernels; if so, **drop `CAP_SYS_PTRACE`** (it is
    the widest grant here). Do not assume; test on the target kernel.
  - Granted via systemd `AmbientCapabilities=` with a matching
    `CapabilityBoundingSet=` and `NoNewPrivileges=yes`.
- **systemd unit:** `Type=notify` (sd_notify `READY=1` only after BPF attach + DB
  open + socket bind); `User=procflow`; `RuntimeDirectory=procflow` →
  `/run/procflow` (owns the socket, ADR-0008); `StateDirectory=procflow` →
  `/var/lib/procflow` (the DuckDB file); hardening
  (`ProtectSystem=strict`, `ProtectHome=yes`, `RestrictAddressFamilies=AF_UNIX`,
  and `MemoryDenyWriteExecute` where it proves compatible). `/proc` stays readable
  — enrichment needs it.
- **FHS layout:** DB `/var/lib/procflow/procflow.duckdb`; socket
  `/run/procflow/procflow.sock`; config `/etc/procflow/config.toml`.
- **Config = TOML** at `/etc/procflow/config.toml`: per-tier retention (ADR-0005),
  timezone override (ADR-0003), default scope + loopback inclusion, poll interval,
  BPF map sizing, and the admin group for whole-machine viewing (ADR-0009).
- **Lifecycle:** on start — open/migrate DuckDB, load+attach BPF, bind socket,
  sd_notify `READY`. On stop — detach BPF, **flush the in-flight minute
  accumulator**, close the DB cleanly.

## Considered options

- **Run as root.** Simplest, rejected — a continuously-running, network-adjacent,
  privileged daemon is exactly the case least-privilege capabilities exist for.
- **`setcap` on the binary instead of systemd ambient caps.** Brittle next to a
  managed unit; a dedicated user + `AmbientCapabilities=` is the modern approach
  and keeps the grant in one auditable place.
- **XDG/per-user paths.** Rejected — this is a system daemon; FHS system paths +
  a system user fit, and the CLI is the per-user unprivileged half.

## Consequences

- Requires a systemd host with a BTF-enabled ≥5.8 kernel (already implied by
  ADR-0006). Non-systemd init must replicate the caps + directories manually —
  documented, not a v1 target.
- A dedicated user with a capped set bounds blast radius: the daemon can load BPF
  and read `/proc`, but is not full root.
- `CAP_SYS_PTRACE` (if retained) lets the daemon read every process's `exe`/`cwd`
  — necessary for cross-user attribution and flagged as the widest privilege,
  potentially reducible to `CAP_PERFMON`-only (see Decision).
