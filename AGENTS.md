# procflow

## Building & testing

Cargo workspace: `procflow-ipc` (protocol; `.proto` + prost codegen, needs
`protoc`), `procflowd` (daemon; bundled DuckDB — first build compiles the C++
engine and takes several minutes), `procflow` (CLI).

- `cargo test --workspace` — build + tests (stable toolchain only; the eBPF
  crate is excluded). The store tests in `crates/procflowd/src/store.rs` run
  the real migrations against in-memory DuckDB; schema changes must keep them
  green.
- `scripts/build-ebpf.sh` — builds `crates/procflow-ebpf` to BPF bytecode.
  Needs `rustup toolchain install nightly --component rust-src`,
  `cargo install bpf-linker`, and `protoc`. `src/bindings.rs` is generated
  from the build machine's kernel BTF via `aya-tool generate sock` (needs
  `cargo install bindgen-cli`); regenerate rather than hand-edit. The object's
  `license` section must stay `Dual MIT/GPL` (ADR-0012).
- Dev runs use env overrides: `PROCFLOW_SOCKET` (IPC socket path) and
  `PROCFLOW_BPF_OBJECT` (BPF object path). Without CAP_BPF+CAP_PERFMON the
  daemon logs "collector disabled" and still serves IPC — expected.
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
