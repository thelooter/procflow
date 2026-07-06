# PID→Identity enrichment: eager first-seen capture, in-kernel fallback metadata, Unresolved Identity

## Context

The collector (ADR-0006) emits `tgid`-keyed counters; the daemon must resolve
each `tgid` to an Identity (ADR-0004: `uid`, `unit_or_cgroup`, `exe`,
`project_root`, `normalized_cmdline`). The last three come from
`/proc/<tgid>/{exe,cwd,cmdline}` plus the cgroup. But a `tgid` can **exit before
the daemon reads `/proc`** — the *dead-PID race* — which would lose enrichment
for traffic that provably happened. Short-lived net clients (`curl`, `git fetch`,
a one-shot `npm` script) are exactly the common case and exactly the hardest.

## Decision

Front-load enrichment to *activity time*, and keep an in-kernel fallback so a
lost race still yields a correctly-attributed, if coarse, Identity.

- **In-kernel, on first sight of a `tgid`**, capture the cheap fields that vanish
  with the process into a `pid_meta` map (`HASH`, key `u32 tgid`, value
  `{ u32 uid, u64 cgroup_id, u8 comm[16] }`) — from `bpf_get_current_uid_gid`,
  `bpf_get_current_cgroup_id`, `bpf_get_current_comm`. Written once
  (`BPF_NOEXIST`).
- **Also push the `tgid` to a `new_pid` `RingBuf`** on first sight. The daemon
  drains it continuously and enriches **eagerly** — reading `/proc` while the
  process is (almost always still) alive — caching `tgid` → `identity_id`. This
  moves enrichment off the poll path, shrinking the race to "a process that
  transmitted, then died before the ring-buffer drain."
- **On the poll path**, for each `tgid` counter: look up the `tgid` →
  `identity_id` cache. On a **miss** (process already gone, or `/proc` read
  failed), fall back to `pid_meta`'s kernel-captured `(uid, cgroup_id, comm)` to
  construct a **degraded / Unresolved Identity** — `exe`, `project_root`, and
  `normalized_cmdline` set to a sentinel `<unresolved>`, still correctly keyed by
  `uid` + cgroup + `comm`. Race-lost bytes are **never silently dropped**.
- Identity dedup/persist follows ADR-0004 (surrogate id over the natural key). An
  Unresolved Identity is a *distinct* natural key (the sentinel is part of it);
  we do **not** retroactively re-merge it once the same program is seen resolved
  later — a subsequent run enriches normally into its own Identity.
- **Map GC:** when a `tgid` is absent from `/proc` and its counters have been
  drained, evict it from `pid_meta`. This bounds map size to live `tgid`s.

## Considered options

- **Poll-time-only enrichment (no ring buffer).** Simplest, but every process
  that sends then exits within one poll interval is unattributable. Rejected —
  that population is large and is precisely what procflow exists to attribute.
- **Compute `exe` in-kernel via `bpf_d_path` on `task->mm->exe_file`.** Gets
  `exe` but not the `cwd` → `project_root` filesystem walk (the *distinguishing*
  field), and `bpf_d_path` is restricted to a subset of hooks. Doesn't remove the
  userspace `/proc` step, so it doesn't solve the race.
- **Attribute all race-lost bytes to a single global `unknown`.** Throws away the
  `uid`/`cgroup`/`comm` we *do* have from the kernel. A degraded-but-partial
  Identity is strictly more useful and still honest.

## Consequences

- A small, bounded fraction of bytes may land under `<unresolved>` Identities
  (very short-lived senders). These are **visible** in views, not hidden — the
  honest signal that some attribution was coarse.
- `pid_meta`'s first-write is always correct for the `tgid` (captured in-context);
  the only thing a lost race costs is the three userspace-derived fields.
- Adds a ring-buffer consumer, a `/proc` reader, and a `tgid` → `identity_id`
  cache to the daemon. Enrichment is **once per `tgid`**, not per poll.
