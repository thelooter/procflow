# Metric is payload bytes per Identity; no remote-endpoint dimension

## Context

procflow attributes network traffic to the process / logical project that
generated it, over time. A reasonable reader would expect either wire-accurate
byte counts (to match vnstat / an ISP data cap) and/or a per-remote breakdown
(who did this process talk to). We deliberately do neither.

## Decision

- The one and only metric is **payload bytes** measured at the socket layer
  (application data, excluding TCP/IP/Ethernet headers, retransmits, ACKs),
  split by **Direction** (ingress/egress) and **Scope** (external/loopback).
- **No remote-endpoint / 5-tuple dimension** is stored at any tier. Traffic is
  aggregated purely by Identity over time.

## Rationale

- The socket layer is the *only* place an Identity (PID → project root) can be
  attributed. Lower hooks (cgroup/skb) that would yield wire bytes collapse all
  terminal-launched dev processes into one shell-session cgroup, destroying the
  per-project attribution that is procflow's entire reason to exist.
- Consequence: totals read slightly *lower* than interface counters like vnstat.
  This is expected, not a bug — procflow answers "which app/project is
  responsible," not "what is my exact wire utilization." vnstat already nails
  the latter.
- Dropping the remote dimension collapses the storage key to
  `(tier, time_bucket, identity, scope)` — Direction is the
  `ingress_bytes`/`egress_bytes` column pair on each row, stored separately and
  never summed — eliminates a cardinality bomb, and lets the eBPF collector
  accumulate simple per-Identity counters instead of tracking per-connection
  flows. procflow is not a firewall / traffic-
  analysis system.

## Deferred

A per-flow forensic tier (individual connections, optionally with endpoints)
may be added later behind a config flag; it is explicitly out of scope for v1.
