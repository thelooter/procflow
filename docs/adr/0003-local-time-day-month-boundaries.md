# Day/month buckets use local-time boundaries; timestamps stored as UTC

## Context

procflow rolls traffic into minute/hour/day/month Tiers. "How much did I use
today / this month" must match what a human (and their ISP data-cap cycle)
means — which is local calendar time, not UTC.

## Decision

- Bucket start times are **stored as UTC** timestamps.
- **Minute and hour** tiers truncate UTC directly (an hour is an hour).
- **Day and month** tiers align to **local-timezone** boundaries (configurable,
  default = system timezone), vnstat-style.
- Bucketing uses **wall-clock** time, not a monotonic clock, because we need
  calendar alignment.

## Consequences

- Under DST a day bucket may legitimately span 23 or 25 hours. Correct, just
  non-uniform.
- On a timezone change (travel), historical buckets stay as-recorded; only new
  buckets use the new zone. We do **not** retroactively rebucket.
- A laptop suspend is simply a gap with no traffic; on resume the daemon keeps
  draining. No special handling.

## Considered options

- **Everything UTC, rebucket at query time.** Rejected: "today" would drift for
  anyone not on UTC, which is wrong for a data-cap tool. Reversing this decision
  later requires migrating stored day/month buckets.
