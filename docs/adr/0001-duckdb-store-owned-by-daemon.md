# Storage engine: embedded DuckDB, owned exclusively by the daemon

## Context

procflow has a long-running collector **daemon** that continuously writes
aggregated traffic counters, and a **CLI** that queries history on demand. The
store must survive years of retention and procflow upgrades. Data volume is
small (no remote/endpoint dimension — see ADR-0002), low hundreds of thousands
of rows across all tiers.

## Decision

Use **embedded DuckDB**, bundled and pinned (≥1.0), as the system of record.
The **daemon is the sole process that opens the database**; the CLI never
touches the file directly — it sends queries to the daemon over a local Unix
socket (Topology B) and receives results back.

## Considered options

- **SQLite (WAL), CLI reads the file directly (Topology A).** Rejected. It is
  the simpler topology and SQLite's WAL mode is purpose-built for one writer +
  N cross-process readers — but it forgoes DuckDB's vectorized analytical
  engine, cleaner time-bucketing/window SQL for downsampling, native Parquet
  export, and headroom for a future per-flow forensic tier.
- **DuckDB with the CLI opening the file directly.** Impossible: DuckDB takes an
  exclusive cross-process file lock in read-write mode ("Database is already
  opened by another process"). A continuously-writing daemon would lock out a
  concurrent CLI reader. This is *why* DuckDB forces Topology B.

## Consequences

- We must build and version an **IPC query protocol** over a Unix socket.
- The **CLI is inert when the daemon is not running** — there is no
  direct-file-read fallback.
- The daemon and CLI **bundle the same pinned DuckDB version**, so on-disk
  format never mismatches. Reading aged data relies on DuckDB's ≥1.0 backward-
  compatibility guarantee (forward compat is best-effort but irrelevant here).
- Daemon binary is larger (~tens of MB of bundled C++); acceptable for a daemon.
