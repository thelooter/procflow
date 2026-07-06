# License: dual MIT OR Apache-2.0, with the eBPF object under Dual MIT/GPL

## Context

procflow needs a license (the README long read "not yet chosen"). Goals: keep it
open source and maximally adoptable (packageable in distro repos, embeddable),
stay license-compatible with the whole dependency tree (aya, prost, DuckDB, clap,
tokio — all `MIT OR Apache-2.0`), and handle a procflow-specific constraint: its
eBPF programs call kernel helpers that are `gpl_only`.

## Decision

- **Userspace (daemon + CLI + shared crates): dual-licensed `MIT OR Apache-2.0`**,
  at the licensee's choice — the Rust ecosystem default. Ship `LICENSE-MIT` and
  `LICENSE-APACHE`; new source files carry the standard SPDX header
  `// SPDX-License-Identifier: MIT OR Apache-2.0`.
- **The eBPF program object declares `SEC("license") = "Dual MIT/GPL"`.** The BPF
  verifier *rejects loading* a program that uses `gpl_only` helpers
  (`bpf_probe_read_kernel`, `bpf_get_current_task`, most tracing/CO-RE helpers —
  needed by ADR-0006/0007) unless its license string is GPL-compatible.
  `"Dual MIT/GPL"` satisfies the verifier while keeping the BPF source reusable
  under MIT. This is a **per-object license string**, independent of the crate's
  `MIT OR Apache-2.0` — it does **not** make the userspace GPL.
- Copyright line: `Copyright (c) 2026 thelooter`.

## Rationale

- **Why dual, not one:** MIT alone lacks a patent grant; Apache-2.0 alone is
  incompatible with GPLv2. Offering both lets a downstream take **MIT** when it
  needs GPLv2-compatibility or brevity, or **Apache-2.0** when it wants the
  explicit patent grant + retaliation clause. Authoring once satisfies both
  audiences at zero cost, and matches every dependency.
- **Why not a use-restricting / ethical-source license** (military/government
  exclusion was considered): any such clause fails the OSI definition (#5/#6) and
  FSF freedom 0 — it would bar distro inclusion and most corporate adoption, and
  is effectively unenforceable by a solo author. An ethical stance, if desired, is
  better expressed as a **non-binding request** in the README than as an
  unenforceable license term. (Not adopted for now.)
- **Why not MPL-2.0:** its file-level copyleft is the only thing it adds over
  MIT/Apache and would only introduce OSPO-review friction with no benefit
  procflow needs; and it cannot be offered as a *third OR arm* (a licensee would
  never pick the more-restrictive option, so the arm is dead — and offering it as
  an option nullifies its copyleft).

## Consequences

- Fully OSI-open and packageable; compatible with the dependency tree and with
  downstream GPLv2 (via the MIT arm) and GPLv3/Apache projects (via either).
- Contributions are, by the conventional clause (see README), taken under the same
  `MIT OR Apache-2.0` dual terms — so every contributor's patents are granted
  through the Apache arm.
- The `Dual MIT/GPL` BPF string is a load-time requirement, not a project-wide
  copyleft; documented so a future reader doesn't "fix" it to plain `MIT` and hit
  a verifier rejection.
