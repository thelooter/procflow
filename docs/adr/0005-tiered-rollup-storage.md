# Tiered rollup storage with daemon-driven downsampling

## Context

procflow keeps per-Identity traffic history for years on a personal machine.
Storing fine-grained data forever is wasteful; storing only coarse data loses
recent detail. We need bounded growth with useful recent resolution.

## Decision

Store aggregated counters in four **Tiers**, each a table keyed by
`(time_bucket, identity_id, scope)` with an `ingress_bytes` / `egress_bytes`
column pair (Direction is the column split — stored separately, never summed;
writing the real DDL showed direction-in-key and two byte columns are mutually
exclusive, and the column pair halves row count):

| Tier    | Resolution | Default retention |
|---------|-----------|-------------------|
| minute  | 1 min     | 48h               |
| hour    | 1 hour    | 90d               |
| day     | 1 day     | 2y                |
| month   | 1 month   | forever           |

All retentions are config-driven. There is **no per-flow / per-event tier** in
v1 (that, and per-remote-endpoint detail, are deferred behind a future config
flag — see ADR-0002).

Data moves up the tiers via a **rollup+prune job that runs inside the daemon**
(the daemon holds the exclusive DuckDB connection — ADR-0001 — so rollup cannot
live in an external process):

- Each run aggregates only **fully-closed** finer buckets into the coarser tier
  (`INSERT … SELECT date_trunc(…) … GROUP BY …`), never the in-progress bucket.
- A **per-tier watermark** (`rolled_up_through`) makes the job **idempotent and
  self-healing**: after downtime it processes whatever closed buckets are past
  the watermark, with no double-counting.
- The same job prunes finer-tier rows past their retention window.
- Hour and day buckets are both built from minute rows, month buckets from day
  rows. Day reads minutes rather than hours because in a zone with a sub-hour
  UTC offset, local midnight falls inside a UTC hour bucket (ADR-0003).
- A bucket rolls up once it has been closed for two minutes. The collector
  writes a closed minute on its next poll, and a minute written after its hour
  had rolled up would be missing from every coarser tier.
- Pruning never deletes a row that a coarser tier has yet to be built from.
  Minute rows stay until both the hour and the day watermark have passed them,
  day rows until the month watermark has.

## Consequences

- Bounded, predictable storage (low hundreds of thousands of rows total).
- Recent data is minute-resolution; long-term history is coarse but complete.
- Day/month bucket boundaries follow local time (ADR-0003).
- Coarser groupings than a tier's grain are computed at query time over the
  Identity dimensions, not materialized separately.
- A coarse tier holds closed buckets only, so its own table has no row for
  today or for this month. Queries read a tier's table up to its watermark and
  take the rest from the finer tiers, so every window includes the traffic so
  far. The query layer does this for every verb.
