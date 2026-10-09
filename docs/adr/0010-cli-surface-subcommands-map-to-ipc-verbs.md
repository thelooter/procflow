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
  | `procflow watch`        | `Watch`          | interactive view; `--json` streams chunks |
  | `procflow status`       | `Hello`          | daemon up? versions, protocol range       |

- **Interactive view.** `procflow` with no subcommand, and `procflow watch`,
  open a full-screen view built on ratatui (amended 2026-10-09, see Considered
  options). It shows live rates from the `Watch` stream and stored totals for a
  chosen window. The user can switch window, grouping, scope and sort column,
  filter by name, and inspect one Identity. The view fetches per-Identity rows
  for both scopes and groups, scopes and sorts them itself, so those switches
  do not wait on the daemon. With `--json`, or when stdout is not a terminal,
  `watch` prints one JSON object per poll interval instead.
  Colours come from a theme: the four Catppuccin flavours, Mocha by default.
  `--theme` or `PROCFLOW_THEME` picks one, and `t` cycles through them while
  the view is open. The view paints the theme's background, so its contrast
  does not depend on the terminal's own. `--transparent`,
  `PROCFLOW_TRANSPARENT` or `b` leaves the terminal's background in place.
- **`top` flags:** `--since 24h` / `--today` / `--this-month` / `--from --to`;
  `--dir ingress|egress|both` (default both, shown as **two columns, never
  summed**); `--scope external|loopback|all` (default `external`);
  `--by identity|project|exe|user` (query-time rollup over the dimension —
  ADR-0004); `--limit`; `--tier` (else auto-picked from the window).
  `--dir` also picks the column the rows are ranked by. With `both` they are
  ranked by ingress plus egress. That sum only orders the rows and is never
  shown.
- **Output:** human table by default (bytes humanised KiB/MiB/GiB, ingress/egress
  columns). `--json` emits the decoded result as JSON — recovering at the CLI
  layer the readability the protobuf wire gives up (ADR-0008). `--bytes` for raw
  integers.
- **Window → tier auto-selection** (amended 2026-10-09). The daemon picks the
  tier when `--tier` is not given. For `top` it takes the finest tier whose
  retention (ADR-0005) still reaches the window start. A finer tier makes
  totals more exact at the window's edges and the result no longer. For
  `series` it also weighs the window's length, so the row count stays readable:
  minutes up to 3 hours, hours up to 48 hours, days up to about a quarter,
  months beyond that. Always overridable with `--tier`.
- **Addressing:** Identities are referenced by their surrogate id (ADR-0004) as
  printed by `top`/`list`; dimension filters (`--project`, `--exe`, `--user`)
  narrow queries without needing an id. They apply to `top` and `list`.
  `--project` and `--exe` match a substring, `--user` a user name or uid.
- **Daemon-down:** a missing/refused socket yields a clear "procflow daemon not
  running" error and a non-zero exit — never a silent empty result (ADR-0001).

## Considered options

- **An ad-hoc query string / SQL-ish DSL.** Rejected — the IPC is typed verbs
  (ADR-0008); mirroring them as subcommands keeps `--help` discoverable and the
  schema server-side.
- **TUI-first (ratatui) as the only UX.** Rejected. The subcommands stay, so
  procflow remains scriptable. The TUI itself ships in v1 next to them, over
  the same verbs (amended 2026-10-09; it was first deferred in favour of a
  repainting table).

## Consequences

- CLI and daemon share the generated protobuf types (ADR-0008); adding a view is
  a new verb + a new subcommand.
- `--json` keeps procflow scriptable despite the binary wire.
- Default views hide `loopback` and never sum Directions, so printed numbers match
  the domain model; a user must opt into `--scope all` / summing themselves.
