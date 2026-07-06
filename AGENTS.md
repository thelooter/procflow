# procflow

## Building & testing

Cargo workspace: `procflow-ipc` (protocol; `.proto` + prost codegen, needs
`protoc`), `procflowd` (daemon; bundled DuckDB — first build compiles the C++
engine and takes several minutes), `procflow` (CLI).

- `cargo test --workspace` — build + tests. The store tests in
  `crates/procflowd/src/store.rs` run the real migrations against in-memory
  DuckDB; schema changes must keep them green.
- Schema changes are **new** `crates/procflowd/migrations/NNNN_*.sql` files
  (applied in order past the recorded `schema_version`) — never edit an
  already-committed migration.
- The design record is binding: check `CONTEXT.md` (domain terms) and
  `docs/adr/` before changing storage keys, protocol shape, privileges, or
  metric semantics.

## Agent skills

### Issue tracker

Issues and PRDs live as GitHub issues (via the `gh` CLI); external PRs are also pulled into the triage queue. See `docs/agents/issue-tracker.md`.

### Triage labels

Default canonical label vocabulary (`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout (`CONTEXT.md` + `docs/adr/` at the repo root). See `docs/agents/domain.md`.
