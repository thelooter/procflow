# procflow

## Building & testing

Cargo workspace: `procflow-ipc` (protocol; `.proto` + prost codegen, needs
`protoc`), `procflowd` (daemon; links the prebuilt libduckdb release, which the
first build downloads from GitHub into `target/duckdb-download/`), `procflow`
(CLI).

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
- Dev runs use env overrides: `PROCFLOW_SOCKET` (IPC socket path),
  `PROCFLOW_DB` (store file, or `:memory:`) and `PROCFLOW_BPF_OBJECT` (BPF
  object path). Without CAP_BPF+CAP_PERFMON the daemon logs "collector
  disabled" and still serves IPC — expected.
- `cargo run -p procflowd --example demo` serves invented traffic through the
  real IPC server on `/tmp/procflow-demo.sock` (or `$PROCFLOW_SOCKET`), with no
  privileges. Use it to run the CLI and the TUI by hand. The TUI's own tests
  render to ratatui's `TestBackend` in `crates/procflow/src/ui.rs`.
- DuckDB is not compiled here. `.cargo/config.toml` sets
  `DUCKDB_DOWNLOAD_LIB=1`, so libduckdb-sys downloads the release matching its
  own version and links `libduckdb.so`. `crates/procflowd/build.rs` adds the
  rpath that lets the binaries run outside `cargo run`. If a link fails with
  `unable to find library -lduckdb`, the download under `target/` was removed
  while the build script still counts as done: run
  `cargo clean -p libduckdb-sys`.
- Do not turn on `duckdb/bundled` for everyday builds. It compiles DuckDB's
  C++ with one job per core, each peaking at 1 GiB or more, and again for
  every tool with its own flags (an IDE's `cargo check`, clippy, `-p` builds).
  That hard-locked a 62 GiB machine on 2026-10-09. If a release build needs
  it, pass `-j 8`.
- CI (`.github/workflows/ci.yml`) runs `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets --locked -- --deny warnings` and
  `cargo test --workspace --locked`. A fourth job builds the eBPF object,
  checks it with `scripts/check-ebpf.sh`, and runs `scripts/smoke-test.sh`,
  which starts the daemon under sudo against the runner's kernel. Run the
  first three before pushing. The Rust version, the nightly and bpf-linker
  are pinned at the top of the workflow.
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
