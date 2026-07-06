-- 0001_init.sql — initial procflow schema.
-- Decisions: ADR-0001 (DuckDB, daemon-owned), ADR-0002 (payload bytes, no
-- endpoint dimension), ADR-0003 (buckets stored as UTC), ADR-0004 (identity
-- surrogate id over normalized natural key), ADR-0005 (tiered rollups with
-- watermarks). All timestamps are naive-UTC.

-- --------------------------------------------------------------------------
-- Identity dimension (ADR-0004)
-- --------------------------------------------------------------------------

CREATE SEQUENCE identity_id_seq START 1;

CREATE TABLE identity (
    id                 BIGINT PRIMARY KEY DEFAULT nextval('identity_id_seq'),
    -- natural key (UNIQUE below)
    uid                UINTEGER NOT NULL,
    unit_or_cgroup     TEXT NOT NULL,
    exe                TEXT NOT NULL,
    project_root       TEXT NOT NULL,
    normalized_cmdline TEXT NOT NULL,
    -- display-only attributes, last-seen-wins, never part of the key
    comm               TEXT,
    raw_cmdline        TEXT,
    username           TEXT,
    first_seen         TIMESTAMP NOT NULL,
    last_seen          TIMESTAMP NOT NULL,
    UNIQUE (uid, unit_or_cgroup, exe, project_root, normalized_cmdline)
);

-- --------------------------------------------------------------------------
-- Tier fact tables (ADR-0005). One row per (bucket, identity, scope);
-- Direction is the ingress/egress column split — stored separately, never
-- summed at rest (CONTEXT.md).
-- --------------------------------------------------------------------------

CREATE TABLE traffic_minute (
    bucket        TIMESTAMP NOT NULL, -- UTC minute start
    identity_id   BIGINT NOT NULL REFERENCES identity (id),
    scope         TEXT NOT NULL CHECK (scope IN ('external', 'loopback')),
    ingress_bytes UBIGINT NOT NULL DEFAULT 0,
    egress_bytes  UBIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, identity_id, scope)
);

CREATE TABLE traffic_hour (
    bucket        TIMESTAMP NOT NULL, -- UTC hour start
    identity_id   BIGINT NOT NULL REFERENCES identity (id),
    scope         TEXT NOT NULL CHECK (scope IN ('external', 'loopback')),
    ingress_bytes UBIGINT NOT NULL DEFAULT 0,
    egress_bytes  UBIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, identity_id, scope)
);

CREATE TABLE traffic_day (
    bucket        TIMESTAMP NOT NULL, -- UTC instant of the LOCAL day start (ADR-0003)
    identity_id   BIGINT NOT NULL REFERENCES identity (id),
    scope         TEXT NOT NULL CHECK (scope IN ('external', 'loopback')),
    ingress_bytes UBIGINT NOT NULL DEFAULT 0,
    egress_bytes  UBIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, identity_id, scope)
);

CREATE TABLE traffic_month (
    bucket        TIMESTAMP NOT NULL, -- UTC instant of the LOCAL month start (ADR-0003)
    identity_id   BIGINT NOT NULL REFERENCES identity (id),
    scope         TEXT NOT NULL CHECK (scope IN ('external', 'loopback')),
    ingress_bytes UBIGINT NOT NULL DEFAULT 0,
    egress_bytes  UBIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (bucket, identity_id, scope)
);

-- --------------------------------------------------------------------------
-- Rollup watermarks (ADR-0005): each coarser tier records the exclusive
-- upper bound of finer-tier buckets already rolled into it, making the
-- rollup+prune job idempotent and self-healing.
-- --------------------------------------------------------------------------

CREATE TABLE rollup_watermark (
    tier              TEXT PRIMARY KEY CHECK (tier IN ('hour', 'day', 'month')),
    rolled_up_through TIMESTAMP NOT NULL
);
