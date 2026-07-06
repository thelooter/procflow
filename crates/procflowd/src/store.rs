//! The DuckDB store, owned exclusively by the daemon (ADR-0001).

use anyhow::{Context, Result};
use duckdb::Connection;
use std::path::Path;

/// Ordered, embedded migrations. Applied transactionally past the recorded
/// version; the daemon migrates on startup (ADR-0011 lifecycle).
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("../migrations/0001_init.sql"))];

pub struct Store {
    conn: Connection,
}

impl Store {
    #[allow(dead_code)] // the daemon opens its real store file once wired up (ADR-0011)
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path).context("opening DuckDB store")?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                 version    BIGINT PRIMARY KEY,
                 applied_at TIMESTAMP NOT NULL DEFAULT now()
             );",
        )?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let current = self.schema_version()?;
        for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
            self.conn
                .execute_batch(&format!(
                    "BEGIN;\n{sql}\nINSERT INTO schema_version (version) VALUES ({version});\nCOMMIT;"
                ))
                .with_context(|| format!("applying migration {version}"))?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT coalesce(max(version), 0) FROM schema_version", [], |r| {
                r.get(0)
            })?)
    }

    /// Upsert an Identity by its natural key (ADR-0004) and return the
    /// surrogate id. Display-only attributes are last-seen-wins.
    pub fn upsert_identity(&self, rec: &crate::enrich::IdentityRecord) -> Result<i64> {
        Ok(self.conn.query_row(
            "INSERT INTO identity
                 (uid, unit_or_cgroup, exe, project_root, normalized_cmdline,
                  comm, raw_cmdline, username, first_seen, last_seen)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, now(), now())
             ON CONFLICT (uid, unit_or_cgroup, exe, project_root, normalized_cmdline)
             DO UPDATE SET
                 last_seen   = now(),
                 comm        = excluded.comm,
                 raw_cmdline = excluded.raw_cmdline,
                 username    = excluded.username
             RETURNING id",
            duckdb::params![
                rec.uid,
                rec.unit_or_cgroup,
                rec.exe,
                rec.project_root,
                rec.normalized_cmdline,
                rec.comm,
                rec.raw_cmdline,
                rec.username,
            ],
            |r| r.get(0),
        )?)
    }

    /// Fold one closed minute-bucket delta into the minute tier (ADR-0005).
    /// `bucket_epoch_s` is the UTC minute start; upsert accumulates so a
    /// re-flush after partial failure cannot lose data, only re-add it —
    /// callers must only flush each accumulator entry once.
    pub fn record_minute(
        &self,
        bucket_epoch_s: i64,
        identity_id: i64,
        scope: &str,
        ingress_bytes: u64,
        egress_bytes: u64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO traffic_minute (bucket, identity_id, scope, ingress_bytes, egress_bytes)
             VALUES (make_timestamp(?), ?, ?, ?, ?)
             ON CONFLICT (bucket, identity_id, scope) DO UPDATE SET
                 ingress_bytes = traffic_minute.ingress_bytes + excluded.ingress_bytes,
                 egress_bytes  = traffic_minute.egress_bytes + excluded.egress_bytes",
            duckdb::params![
                bucket_epoch_s * 1_000_000, // make_timestamp takes epoch micros
                identity_id,
                scope,
                ingress_bytes,
                egress_bytes,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_applies_cleanly() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
        // Re-running migrate is a no-op (idempotent).
        store.migrate().unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
    }

    #[test]
    fn identity_natural_key_is_enforced_and_surrogate_autoincrements() {
        let store = Store::open_in_memory().unwrap();
        let insert = "INSERT INTO identity
             (uid, unit_or_cgroup, exe, project_root, normalized_cmdline, first_seen, last_seen)
             VALUES (1000, 'user.slice', '/usr/bin/node', '/home/x/proj', 'npm run dev', now(), now())
             RETURNING id";
        let id1: i64 = store.conn.query_row(insert, [], |r| r.get(0)).unwrap();
        // Same natural key again must violate the UNIQUE constraint (ADR-0004).
        assert!(store.conn.query_row(insert, [], |r| r.get(0)).map(|_: i64| ()).is_err());
        // A different normalized_cmdline is a distinct Identity with a fresh id.
        let id2: i64 = store
            .conn
            .query_row(
                "INSERT INTO identity
                 (uid, unit_or_cgroup, exe, project_root, normalized_cmdline, first_seen, last_seen)
                 VALUES (1000, 'user.slice', '/usr/bin/node', '/home/x/proj', 'npm run build', now(), now())
                 RETURNING id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(id2 > id1);
    }

    #[test]
    fn counter_upsert_accumulates() {
        // The flush path folds the in-memory accumulator into the minute tier
        // via upsert (ADR-0006); prove DuckDB's ON CONFLICT arithmetic works.
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO identity
                 (id, uid, unit_or_cgroup, exe, project_root, normalized_cmdline, first_seen, last_seen)
                 VALUES (1, 1000, 'u', '/bin/x', '<none>', 'x', now(), now());",
            )
            .unwrap();
        let upsert = "INSERT INTO traffic_minute (bucket, identity_id, scope, ingress_bytes, egress_bytes)
             VALUES ('2026-07-06 12:00:00', 1, 'external', 100, 50)
             ON CONFLICT (bucket, identity_id, scope) DO UPDATE SET
                 ingress_bytes = traffic_minute.ingress_bytes + excluded.ingress_bytes,
                 egress_bytes  = traffic_minute.egress_bytes + excluded.egress_bytes";
        store.conn.execute_batch(upsert).unwrap();
        store.conn.execute_batch(upsert).unwrap();
        let (ingress, egress): (u64, u64) = store
            .conn
            .query_row(
                "SELECT ingress_bytes, egress_bytes FROM traffic_minute
                 WHERE identity_id = 1 AND scope = 'external'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((ingress, egress), (200, 100));
        // Scope CHECK constraint rejects anything but external/loopback.
        assert!(store
            .conn
            .execute_batch(
                "INSERT INTO traffic_minute (bucket, identity_id, scope)
                 VALUES ('2026-07-06 12:00:00', 1, 'lan')"
            )
            .is_err());
    }

    #[test]
    fn identity_upsert_returns_stable_id_and_updates_display_fields() {
        let store = Store::open_in_memory().unwrap();
        let mut rec = crate::enrich::IdentityRecord {
            uid: 1000,
            unit_or_cgroup: "/user.slice/app.slice/dev.scope".into(),
            exe: "/usr/bin/node".into(),
            project_root: "/home/x/proj".into(),
            normalized_cmdline: "npm run dev --port <n>".into(),
            comm: "node".into(),
            raw_cmdline: "npm run dev --port 3000".into(),
            username: Some("x".into()),
        };
        let id1 = store.upsert_identity(&rec).unwrap();
        // Same natural key, different display attrs → same id, attrs updated.
        rec.raw_cmdline = "npm run dev --port 3001".into();
        let id2 = store.upsert_identity(&rec).unwrap();
        assert_eq!(id1, id2);
        let raw: String = store
            .conn
            .query_row("SELECT raw_cmdline FROM identity WHERE id = ?", [id1], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, "npm run dev --port 3001");
        // Different key member → new Identity.
        rec.normalized_cmdline = "npm run build".into();
        assert_ne!(store.upsert_identity(&rec).unwrap(), id1);
    }

    #[test]
    fn record_minute_accumulates_via_api() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .upsert_identity(&crate::enrich::fully_unresolved())
            .unwrap();
        let bucket = 1_782_000_000 / 60 * 60; // any minute-aligned epoch
        store.record_minute(bucket, id, "external", 100, 50).unwrap();
        store.record_minute(bucket, id, "external", 20, 5).unwrap();
        let (ingress, egress): (u64, u64) = store
            .conn
            .query_row(
                "SELECT ingress_bytes, egress_bytes FROM traffic_minute
                 WHERE identity_id = ? AND scope = 'external'
                   AND bucket = make_timestamp(?)",
                duckdb::params![id, bucket * 1_000_000],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((ingress, egress), (120, 55));
    }

    #[test]
    fn rollup_watermark_roundtrips() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO rollup_watermark VALUES ('hour', '2026-07-06 11:00:00');
                 UPDATE rollup_watermark SET rolled_up_through = '2026-07-06 12:00:00'
                 WHERE tier = 'hour';",
            )
            .unwrap();
        let ts: String = store
            .conn
            .query_row(
                "SELECT strftime(rolled_up_through, '%Y-%m-%d %H:%M:%S')
                 FROM rollup_watermark WHERE tier = 'hour'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ts, "2026-07-06 12:00:00");
    }
}
