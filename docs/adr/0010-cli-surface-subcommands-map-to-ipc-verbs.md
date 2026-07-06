# CLI surface: a single `procflow` binary whose subcommands map to IPC verbs

## Context

ADR-0008 defines typed IPC verbs; the user-facing half is a CLI over that
socket. It should feel like `vnstat`/`nethogs`, honour the domain rules
(Direction is never summed — ADR-0002 / CONTEXT; `external` Scope is the default —
CONTEXT; Tiers — ADR-0005), be inert-but-clear when the daemon is down (ADR-0001),
and be scriptable.

## Decision

- A single **unprivileged `procflow` binary** (clap derive). Subcommands map ~1:1
  to the IPC verbs (ADR-0008):

  | Subcommand              | Verb             | Purpose                                   |
  |-------------------------|------------------|-------------------------------------------|
  | `procflow top`          | `TopIdentities`  | marquee view — biggest talkers in a window |
  | `procflow series <id>`  | `Series`         | one Identity's history over time          |
  | `procflow list`         | `ListIdentities` | browse/search the Identity dimension      |
  | `procflow show <id>`    | `Resolve`        | full Identity detail                      |
  | `procflow watch`        | `Watch`          | live-updating table (streamed)            |
  | `procflow status`       | `Hello`          | daemon up? versions, protocol range       |

- **`top` flags:** `--since 24h` / `--today` / `--this-month` / `--from --to`;
  `--dir ingress|egress|both` (default both, shown as **two columns, never
  summed**); `--scope external|loopback|all` (default `external`);
  `--by identity|project|exe|user` (query-time rollup over the dimension —
  ADR-0004); `--limit`; `--tier` (else auto-picked from the window).
- **Output:** human table by default (bytes humanised KiB/MiB/GiB, ingress/egress
  columns). `--json` emits the decoded result as JSON — recovering at the CLI
  layer the readability the protobuf wire gives up (ADR-0008). `--bytes` for raw
  integers.
- **Window → tier auto-selection:** short windows (≤48h) resolve to minute/hour,
  multi-day to day, `--this-month`/longer to month — matching each tier's
  retention (ADR-0005). Always overridable with `--tier`.
- **Addressing:** Identities are referenced by their surrogate id (ADR-0004) as
  printed by `top`/`list`; dimension filters (`--project`, `--exe`, `--user`)
  narrow queries without needing an id.
- **Daemon-down:** a missing/refused socket yields a clear "procflow daemon not
  running" error and a non-zero exit — never a silent empty result (ADR-0001).

## Considered options

- **An ad-hoc query string / SQL-ish DSL.** Rejected — the IPC is typed verbs
  (ADR-0008); mirroring them as subcommands keeps `--help` discoverable and the
  schema server-side.
- **TUI-first (ratatui) as the primary UX.** Deferred. `watch` is a simple
  repainting table for v1; a richer TUI can come later over the same `Watch`
  stream.

## Consequences

- CLI and daemon share the generated protobuf types (ADR-0008); adding a view is
  a new verb + a new subcommand.
- `--json` keeps procflow scriptable despite the binary wire.
- Default views hide `loopback` and never sum Directions, so printed numbers match
  the domain model; a user must opt into `--scope all` / summing themselves.
