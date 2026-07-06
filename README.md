# procflow

**Per-process network traffic, tracked over time.**

procflow is conceptually [nethogs](https://github.com/raboof/nethogs) crossed
with [vnstat](https://github.com/vergoh/vnstat): nethogs shows you *which
process* is using the network but keeps no history; vnstat keeps beautiful
historical rollups but only *per interface*. procflow sits in the gap —
attributing traffic to the process (or logical project) that generated it, and
persisting it so you can answer "what's eaten my bandwidth this week, by app?"

> [!IMPORTANT]
> **This project is heavily AI-written.** The design and documentation — and
> the code to come — are produced largely by an AI agent (Claude) working
> interactively with the author. Treat it accordingly: review before relying on
> it, and don't assume a human hand-verified every line. Contributions and a
> healthy dose of skepticism are welcome.

## Status

🚧 **Design phase — no code yet.** What exists today is the design record:
a domain glossary and a set of Architecture Decision Records. The implementation
hasn't started.

## How it's meant to work

- A privileged **daemon** uses **eBPF** (via [`aya`](https://aya-rs.dev/),
  pure-Rust) to attribute network payload bytes to the originating process,
  enriched with `/proc` metadata (executable, working directory, cgroup/unit).
- Traffic is aggregated by a stable **Identity** — not a PID — so repeated runs
  of the same thing collapse together, and terminal-launched dev processes are
  distinguished by their **project root** (the repo they ran from).
- Counters roll up into time **Tiers** (minute → hour → day → month) stored in
  an embedded **DuckDB** database owned by the daemon.
- A **CLI** queries history over a local Unix socket.

## Design docs

The reasoning lives in the repo, not in anyone's head:

- [`CONTEXT.md`](./CONTEXT.md) — the domain glossary (Identity, Project root,
  Tier, Direction, Payload bytes, Scope).
- [`docs/adr/`](./docs/adr/) — Architecture Decision Records:
  - [0001](./docs/adr/0001-duckdb-store-owned-by-daemon.md) — DuckDB store owned by the daemon; CLI over a socket
  - [0002](./docs/adr/0002-payload-bytes-no-endpoint-dimension.md) — payload bytes per Identity; no per-endpoint tracking
  - [0003](./docs/adr/0003-local-time-day-month-boundaries.md) — local-time day/month boundaries
  - [0004](./docs/adr/0004-identity-natural-key-normalized-cmdline.md) — Identity keying
  - [0005](./docs/adr/0005-tiered-rollup-storage.md) — tiered rollup storage
  - [0006](./docs/adr/0006-ebpf-socket-hooks-kernel-counts-pid.md) — eBPF socket-layer hooks; kernel counts per-PID, userspace resolves Identity
  - [0007](./docs/adr/0007-pid-identity-enrichment-dead-pid-race.md) — PID→Identity enrichment and the dead-PID race

## Scope, deliberately

procflow answers *which app/project is responsible for my traffic* — not *what
exactly went over the wire* (vnstat already nails that) and not *who did it talk
to* (it's not a firewall or traffic-analysis system). It measures application
**payload bytes**, so totals read slightly lower than interface counters. That's
expected, not a bug.

## License

Not yet chosen.
