# Identity keyed by a surrogate id over a normalized natural key

## Context

Traffic is attributed to an Identity (see CONTEXT.md), not a PID. We need a
stable, bounded way to decide when two processes — across restarts and repeated
invocations — are "the same Identity," and how that id is stored.

## Decision

- Identities live in an `identity` dimension table with a **surrogate
  autoincrement `id`**. Per-tier fact rows reference that int.
- The **natural key** (UNIQUE) is
  `(uid, unit_or_cgroup, exe, project_root, normalized_cmdline)`.
  `username` is resolved as a display-only attribute (UIDs get reused/renamed).
- `normalized_cmdline` **masks volatile tokens**: numbers → `<n>`,
  `/tmp` & `/proc` paths → `<path>`, UUIDs → `<uuid>`, post-`=` values → `<v>`.
  So `--port 3000` / `--port 3001` merge, but `npm run dev` / `npm run build`
  stay distinct.
- Raw `cmdline` and `comm` are stored as **display-only** attributes
  (last-seen-wins), never part of the key.

## Rationale

- Surrogate int over hash-as-id: smaller in the columnar store, compresses well,
  and doesn't freeze the key definition into every fact row.
- Normalizing inside the key keeps the identity table small and stable while
  preserving distinctions that matter. Raw cmdline in the key would guarantee
  cardinality creep from ports/PIDs/seeds.

## Consequences

- Normalization is **heuristic**: it may occasionally over-merge (a numeric arg
  that was semantically meaningful) or under-merge (an unanticipated volatile
  token).
- Changing normalization rules later splits/merges *future* Identities without
  rewriting history.
- Coarser views (by `exe`, `project_root`, `comm`) are produced by query-time
  rollup over the dimension columns, not by re-keying.
