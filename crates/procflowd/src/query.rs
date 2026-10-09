//! Read side of the store: the SQL behind the typed verbs (ADR-0008). Every
//! query takes the caller's [`Visibility`], so the uid boundary (ADR-0009)
//! is applied here and nowhere else.

use crate::rollup::table;
use crate::store::Store;
use duckdb::types::Value;
use procflow_ipc::v1::{
    CounterRow, Direction, ErrorCode, GroupBy, Identity, ListIdentities, Rows, Scope, Series, Tier,
    TimeRange, TopIdentities,
};
use std::collections::BTreeMap;

/// Which Identities a caller may see (ADR-0009).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    All,
    Uid(u32),
}

impl Visibility {
    /// root sees every Identity, anyone else their own.
    pub fn for_peer(uid: u32) -> Self {
        if uid == 0 {
            Visibility::All
        } else {
            Visibility::Uid(uid)
        }
    }
}

#[derive(Debug)]
pub enum QueryError {
    BadRequest(String),
    NotFound(String),
    Internal(anyhow::Error),
}

impl QueryError {
    pub fn code(&self) -> ErrorCode {
        match self {
            QueryError::BadRequest(_) => ErrorCode::BadRequest,
            QueryError::NotFound(_) => ErrorCode::NotFound,
            QueryError::Internal(_) => ErrorCode::Internal,
        }
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::BadRequest(m) | QueryError::NotFound(m) => f.write_str(m),
            QueryError::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for QueryError {
    fn from(e: E) -> Self {
        QueryError::Internal(e.into())
    }
}

type QueryResult<T> = Result<T, QueryError>;

/// `WHERE` fragments and their parameters, in order.
#[derive(Default)]
struct Predicates {
    clauses: Vec<String>,
    params: Vec<Value>,
}

impl Predicates {
    fn push(&mut self, clause: &str, params: impl IntoIterator<Item = Value>) {
        self.clauses.push(clause.to_string());
        self.params.extend(params);
    }

    /// The uid boundary over `identity i` (ADR-0009).
    fn visibility(&mut self, vis: Visibility) {
        if let Visibility::Uid(uid) = vis {
            self.push("i.uid = ?", [Value::UInt(uid)]);
        }
    }

    /// Dimension filters over `identity i` (ADR-0010); empty means unset.
    fn dimensions(&mut self, project: &str, exe: &str, user: &str) {
        for (column, needle) in [("i.project_root", project), ("i.exe", exe)] {
            if !needle.is_empty() {
                self.push(
                    &format!("contains(lower({column}), lower(?))"),
                    [Value::Text(needle.to_string())],
                );
            }
        }
        if !user.is_empty() {
            self.push(&format!("{USER_KEY} = ?"), [Value::Text(user.to_string())]);
        }
    }

    fn scope(&mut self, scope: Scope) {
        match scope {
            Scope::All => {}
            Scope::Loopback => self.push("t.scope = 'loopback'", []),
            Scope::External | Scope::Unspecified => self.push("t.scope = 'external'", []),
        }
    }

    fn sql(&self) -> String {
        if self.clauses.is_empty() {
            "TRUE".to_string()
        } else {
            self.clauses.join(" AND ")
        }
    }
}

/// How an Identity is labelled when grouping or filtering by user.
const USER_KEY: &str = "coalesce(i.username, CAST(i.uid AS VARCHAR))";

/// The label `identity` falls under when grouped by `by`. These are the
/// keys `top` groups on in SQL, for callers grouping live deltas.
pub fn group_key(identity: &Identity, by: GroupBy) -> String {
    match by {
        GroupBy::Project => identity.project_root.clone(),
        GroupBy::Exe => identity.exe.clone(),
        GroupBy::User if identity.username.is_empty() => identity.uid.to_string(),
        GroupBy::User => identity.username.clone(),
        GroupBy::Identity | GroupBy::Unspecified => String::new(),
    }
}

fn scope_from_sql(scope: &str) -> Scope {
    if scope == "loopback" {
        Scope::Loopback
    } else {
        Scope::External
    }
}

/// `[from, to)` in epoch seconds.
fn range(range: &Option<TimeRange>) -> QueryResult<(i64, i64)> {
    let range = range
        .as_ref()
        .ok_or_else(|| QueryError::BadRequest("a time range is required".into()))?;
    let (from, to) = (
        range.from_unix_ms.div_euclid(1000),
        range.to_unix_ms.div_euclid(1000),
    );
    if from >= to {
        return Err(QueryError::BadRequest(
            "the time range is empty: from must be before to".into(),
        ));
    }
    Ok((from, to))
}

/// The tier that keeps a series over `span_s` readable (ADR-0010): minutes
/// for a few hours, hours up to two days, days up to a quarter, then months.
fn tier_for_span(span_s: i64) -> Tier {
    const HOUR: i64 = 3600;
    match span_s {
        s if s <= 3 * HOUR => Tier::Minute,
        s if s <= 48 * HOUR => Tier::Hour,
        s if s <= 92 * 24 * HOUR => Tier::Day,
        _ => Tier::Month,
    }
}

const IDENTITY_COLUMNS: &str = "i.id, i.uid, i.unit_or_cgroup, i.exe, i.project_root,
    i.normalized_cmdline, coalesce(i.comm, ''), coalesce(i.username, ''),
    coalesce(i.raw_cmdline, ''), epoch_ms(i.first_seen), epoch_ms(i.last_seen)";

impl Store {
    /// Subquery yielding every `tier`-resolution row with a bucket start in
    /// `[from, to)`. Rollups move closed buckets only (ADR-0005), so the
    /// tier's own table stops at its watermark; the rest is still in the
    /// finer tiers and is read from there. Without this, "today" would be
    /// missing from the day tier until tomorrow.
    fn stitched(
        &self,
        tier: Tier,
        from: i64,
        to: i64,
        params: &mut Vec<Value>,
    ) -> QueryResult<String> {
        let unrolled = |t: Tier| self.watermark(t).map(|w| w.unwrap_or(0));
        let sources = match tier {
            Tier::Minute | Tier::Unspecified => vec![(Tier::Minute, 0)],
            Tier::Hour => vec![(Tier::Hour, 0), (Tier::Minute, unrolled(Tier::Hour)?)],
            Tier::Day => vec![(Tier::Day, 0), (Tier::Minute, unrolled(Tier::Day)?)],
            Tier::Month => vec![
                (Tier::Month, 0),
                (Tier::Day, unrolled(Tier::Month)?),
                (Tier::Minute, unrolled(Tier::Day)?),
            ],
        };
        let mut selects = Vec::new();
        for (source, lower) in sources {
            selects.push(format!(
                "SELECT bucket, identity_id, scope, ingress_bytes, egress_bytes FROM {}
                 WHERE bucket >= make_timestamp(?) AND bucket < make_timestamp(?)",
                table(source)
            ));
            params.push(Value::BigInt(from.max(lower) * 1_000_000));
            params.push(Value::BigInt(to * 1_000_000));
        }
        Ok(selects.join(" UNION ALL "))
    }

    fn identities(&self, predicates: &Predicates, order: &str) -> QueryResult<Vec<Identity>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {IDENTITY_COLUMNS} FROM identity i WHERE {} {order}",
            predicates.sql()
        ))?;
        let rows = stmt.query_map(duckdb::params_from_iter(predicates.params.iter()), |r| {
            Ok(Identity {
                id: r.get(0)?,
                uid: r.get(1)?,
                unit_or_cgroup: r.get(2)?,
                exe: r.get(3)?,
                project_root: r.get(4)?,
                normalized_cmdline: r.get(5)?,
                comm: r.get(6)?,
                username: r.get(7)?,
                raw_cmdline: r.get(8)?,
                first_seen_unix_ms: r.get(9)?,
                last_seen_unix_ms: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The Identities among `ids` that the caller may see.
    pub fn identities_by_id(&self, vis: Visibility, ids: &[i64]) -> QueryResult<Vec<Identity>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut predicates = Predicates::default();
        predicates.visibility(vis);
        let list = ids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        predicates.push(&format!("i.id IN ({list})"), []);
        self.identities(&predicates, "ORDER BY i.id")
    }

    /// One Identity. An Identity the caller may not see is reported exactly
    /// like one that does not exist.
    pub fn resolve(&self, vis: Visibility, identity_id: i64) -> QueryResult<Identity> {
        self.identities_by_id(vis, &[identity_id])?
            .pop()
            .ok_or_else(|| QueryError::NotFound(format!("no identity {identity_id}")))
    }

    pub fn list_identities(&self, vis: Visibility, q: &ListIdentities) -> QueryResult<Rows> {
        let mut predicates = Predicates::default();
        predicates.visibility(vis);
        predicates.dimensions(&q.project, &q.exe, &q.user);
        if !q.filter.is_empty() {
            predicates.push(
                "(contains(lower(i.exe), lower(?)) OR contains(lower(i.project_root), lower(?))
                  OR contains(lower(coalesce(i.comm, '')), lower(?)))",
                std::iter::repeat_n(Value::Text(q.filter.clone()), 3),
            );
        }
        Ok(Rows {
            identities: self.identities(&predicates, "ORDER BY i.last_seen DESC, i.id")?,
            ..Default::default()
        })
    }

    /// Totals per Identity (or per group) over a window, biggest first.
    /// One row per scope, so `Scope::All` never merges loopback into external.
    pub fn top(&self, vis: Visibility, q: &TopIdentities, now_s: i64) -> QueryResult<Rows> {
        let (from, to) = range(&q.range)?;
        // Totals don't get longer with a finer tier, only more exact at the
        // window's edges, so take the finest one that still has the data.
        let tier = match q.tier() {
            Tier::Unspecified => self.retention.finest_covering(from, now_s),
            tier => tier,
        };
        let mut params = Vec::new();
        let traffic = self.stitched(tier, from, to, &mut params)?;
        let mut predicates = Predicates {
            params,
            ..Default::default()
        };
        predicates.visibility(vis);
        predicates.dimensions(&q.project, &q.exe, &q.user);
        predicates.scope(q.scope());

        let key = match q.group_by() {
            GroupBy::Identity | GroupBy::Unspecified => "t.identity_id, ''",
            GroupBy::Project => "CAST(0 AS BIGINT), i.project_root",
            GroupBy::Exe => "CAST(0 AS BIGINT), i.exe",
            GroupBy::User => &format!("CAST(0 AS BIGINT), {USER_KEY}"),
        };
        // Ranking by the sum is an ordering only; it is never returned.
        let rank = match q.direction() {
            Direction::Ingress => "sum(t.ingress_bytes)",
            Direction::Egress => "sum(t.egress_bytes)",
            Direction::Unspecified => "sum(t.ingress_bytes) + sum(t.egress_bytes)",
        };
        let limit = if q.limit > 0 {
            format!("LIMIT {}", q.limit)
        } else {
            String::new()
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {key}, t.scope,
                    CAST(sum(t.ingress_bytes) AS UBIGINT), CAST(sum(t.egress_bytes) AS UBIGINT)
             FROM ({traffic}) t JOIN identity i ON i.id = t.identity_id
             WHERE {}
             GROUP BY 1, 2, 3
             ORDER BY {rank} DESC, 1, 2, 3
             {limit}",
            predicates.sql()
        ))?;
        let counters = stmt
            .query_map(duckdb::params_from_iter(predicates.params.iter()), |r| {
                Ok(CounterRow {
                    identity_id: r.get(0)?,
                    group_key: r.get(1)?,
                    scope: scope_from_sql(&r.get::<_, String>(2)?) as i32,
                    ingress_bytes: r.get(3)?,
                    egress_bytes: r.get(4)?,
                    bucket_unix_ms: from * 1000,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut ids: Vec<i64> = counters
            .iter()
            .map(|c| c.identity_id)
            .filter(|id| *id != 0)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        Ok(Rows {
            counters,
            identities: self.identities_by_id(vis, &ids)?,
            tier: tier as i32,
        })
    }

    /// One Identity's counters over time, one row per `tier` bucket and scope.
    pub fn series(&self, vis: Visibility, q: &Series, now_s: i64) -> QueryResult<Rows> {
        let (from, to) = range(&q.range)?;
        let identity = self.resolve(vis, q.identity_id)?;
        let tier = match q.tier() {
            Tier::Unspecified => self
                .retention
                .finest_covering(from, now_s)
                .max(tier_for_span(to - from)),
            tier => tier,
        };
        let mut params = Vec::new();
        let traffic = self.stitched(tier, from, to, &mut params)?;
        let mut predicates = Predicates {
            params,
            ..Default::default()
        };
        predicates.push("t.identity_id = ?", [Value::BigInt(identity.id)]);
        predicates.scope(q.scope());
        let mut stmt = self.conn.prepare(&format!(
            "SELECT epoch_ms(t.bucket), t.scope, t.ingress_bytes, t.egress_bytes
             FROM ({traffic}) t WHERE {}",
            predicates.sql()
        ))?;
        let rows = stmt.query_map(duckdb::params_from_iter(predicates.params.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u64>(2)?,
                r.get::<_, u64>(3)?,
            ))
        })?;

        // Rows from the finer tiers are folded into `tier` buckets here
        // rather than in SQL: day and month bounds follow the zone (ADR-0003).
        let mut buckets: BTreeMap<(i64, i32), (u64, u64)> = BTreeMap::new();
        for row in rows {
            let (bucket_ms, scope, ingress, egress) = row?;
            let start = self.zone.bounds(tier, bucket_ms / 1000).0;
            let sums = buckets
                .entry((start, scope_from_sql(&scope) as i32))
                .or_default();
            sums.0 += ingress;
            sums.1 += egress;
        }
        let counters = buckets
            .into_iter()
            .map(
                |((start, scope), (ingress_bytes, egress_bytes))| CounterRow {
                    identity_id: identity.id,
                    bucket_unix_ms: start * 1000,
                    scope,
                    ingress_bytes,
                    egress_bytes,
                    group_key: String::new(),
                },
            )
            .collect();
        Ok(Rows {
            counters,
            identities: vec![identity],
            tier: tier as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enrich::IdentityRecord;
    use crate::rollup::Zone;
    use crate::utc;

    fn identity(store: &Store, uid: u32, comm: &str, project: &str) -> i64 {
        store
            .upsert_identity(&IdentityRecord {
                uid,
                unit_or_cgroup: "/user.slice".into(),
                exe: format!("/usr/bin/{comm}"),
                project_root: project.into(),
                normalized_cmdline: comm.into(),
                comm: comm.into(),
                raw_cmdline: comm.into(),
                username: (uid == 1000).then(|| "eve".into()),
            })
            .unwrap()
    }

    /// node + curl for uid 1000 (two projects), sshd for uid 0.
    fn seeded() -> (Store, [i64; 3]) {
        let mut store = Store::open_in_memory().unwrap();
        store.zone = Zone::Fixed(chrono::FixedOffset::east_opt(0).unwrap());
        let node = identity(&store, 1000, "node", "/home/eve/web");
        let curl = identity(&store, 1000, "curl", "/home/eve/api");
        let sshd = identity(&store, 0, "sshd", "<none>");
        let minute = |at: &str, id, scope, ingress, egress| {
            store
                .record_minute(utc(at), id, scope, ingress, egress)
                .unwrap()
        };
        minute("2026-07-06 10:05", node, "external", 100, 10);
        minute("2026-07-06 10:05", node, "loopback", 900, 900);
        minute("2026-07-06 11:10", node, "external", 50, 5);
        minute("2026-07-06 11:10", curl, "external", 400, 1);
        minute("2026-07-06 11:10", sshd, "external", 7000, 7000);
        (store, [node, curl, sshd])
    }

    fn window(from: &str, to: &str) -> Option<TimeRange> {
        Some(TimeRange {
            from_unix_ms: utc(from) * 1000,
            to_unix_ms: utc(to) * 1000,
        })
    }

    fn today() -> Option<TimeRange> {
        window("2026-07-06 00:00", "2026-07-07 00:00")
    }

    /// `(identity or group, ingress, egress)` in result order.
    fn totals(rows: &Rows) -> Vec<(String, u64, u64)> {
        rows.counters
            .iter()
            .map(|c| {
                let key = if c.identity_id != 0 {
                    c.identity_id.to_string()
                } else {
                    c.group_key.clone()
                };
                (key, c.ingress_bytes, c.egress_bytes)
            })
            .collect()
    }

    #[test]
    fn top_ranks_external_traffic_and_carries_the_identities() {
        let (store, [node, curl, sshd]) = seeded();
        let now = utc("2026-07-06 11:30");
        let q = TopIdentities {
            range: today(),
            ..Default::default()
        };
        let rows = store.top(Visibility::All, &q, now).unwrap();
        assert_eq!(
            totals(&rows),
            [
                (sshd.to_string(), 7000, 7000),
                (curl.to_string(), 400, 1),
                (node.to_string(), 150, 15)
            ]
        );
        assert_eq!(rows.tier(), Tier::Minute);
        let mut names: Vec<_> = rows.identities.iter().map(|i| i.comm.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["curl", "node", "sshd"]);

        // Ranking by one direction, with a limit.
        let q = TopIdentities {
            range: today(),
            direction: Direction::Egress as i32,
            limit: 2,
            ..Default::default()
        };
        let rows = store.top(Visibility::All, &q, now).unwrap();
        assert_eq!(
            totals(&rows),
            [(sshd.to_string(), 7000, 7000), (node.to_string(), 150, 15)]
        );
    }

    #[test]
    fn a_non_root_caller_sees_only_their_own_identities() {
        let (store, [node, curl, sshd]) = seeded();
        let now = utc("2026-07-06 11:30");
        let eve = Visibility::Uid(1000);
        let q = TopIdentities {
            range: today(),
            ..Default::default()
        };
        let rows = store.top(eve, &q, now).unwrap();
        assert_eq!(
            totals(&rows),
            [(curl.to_string(), 400, 1), (node.to_string(), 150, 15)]
        );
        assert!(rows.identities.iter().all(|i| i.uid == 1000));

        let listed = store
            .list_identities(eve, &ListIdentities::default())
            .unwrap();
        assert_eq!(listed.identities.len(), 2);
        assert_eq!(
            store
                .list_identities(Visibility::All, &ListIdentities::default())
                .unwrap()
                .identities
                .len(),
            3
        );

        // Someone else's Identity looks exactly like a missing one.
        assert!(matches!(
            store.resolve(eve, sshd),
            Err(QueryError::NotFound(_))
        ));
        assert!(matches!(
            store.resolve(eve, 9999),
            Err(QueryError::NotFound(_))
        ));
        let q = Series {
            identity_id: sshd,
            range: today(),
            ..Default::default()
        };
        assert!(matches!(
            store.series(eve, &q, now),
            Err(QueryError::NotFound(_))
        ));
        assert_eq!(store.resolve(eve, node).unwrap().comm, "node");
        assert_eq!(Visibility::for_peer(0), Visibility::All);
        assert_eq!(Visibility::for_peer(1000), eve);
    }

    #[test]
    fn scope_group_and_dimension_filters() {
        let (store, [node, ..]) = seeded();
        let now = utc("2026-07-06 11:30");
        let top = |q: TopIdentities| totals(&store.top(Visibility::All, &q, now).unwrap());

        // Loopback is opt-in, and `all` keeps the two scopes as separate rows.
        let loopback = TopIdentities {
            range: today(),
            scope: Scope::Loopback as i32,
            ..Default::default()
        };
        assert_eq!(top(loopback), [(node.to_string(), 900, 900)]);
        let all = TopIdentities {
            range: today(),
            scope: Scope::All as i32,
            project: "web".into(),
            ..Default::default()
        };
        assert_eq!(
            top(all),
            [(node.to_string(), 900, 900), (node.to_string(), 150, 15)]
        );

        let by_user = TopIdentities {
            range: today(),
            group_by: GroupBy::User as i32,
            ..Default::default()
        };
        assert_eq!(
            top(by_user),
            [("0".to_string(), 7000, 7000), ("eve".to_string(), 550, 16)]
        );
        let by_project = TopIdentities {
            range: today(),
            group_by: GroupBy::Project as i32,
            user: "eve".into(),
            ..Default::default()
        };
        assert_eq!(
            top(by_project),
            [
                ("/home/eve/api".to_string(), 400, 1),
                ("/home/eve/web".to_string(), 150, 15)
            ]
        );

        let listed = |q: ListIdentities| {
            let rows = store.list_identities(Visibility::All, &q).unwrap();
            rows.identities
                .into_iter()
                .map(|i| i.comm)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            listed(ListIdentities {
                filter: "CURL".into(),
                ..Default::default()
            }),
            ["curl"]
        );
        assert_eq!(
            listed(ListIdentities {
                exe: "bin/ssh".into(),
                ..Default::default()
            }),
            ["sshd"]
        );
    }

    #[test]
    fn coarse_tiers_include_the_buckets_not_rolled_up_yet() {
        let (store, [node, ..]) = seeded();
        // 11:30: hour 10 has rolled up, hour 11 and the day are still open.
        let now = utc("2026-07-06 11:30");
        store.rollup(now).unwrap();
        for tier in [Tier::Minute, Tier::Hour, Tier::Day, Tier::Month] {
            let q = TopIdentities {
                range: today(),
                tier: tier as i32,
                user: "eve".into(),
                limit: 1,
                ..Default::default()
            };
            let rows = store.top(Visibility::All, &q, now).unwrap();
            assert_eq!(totals(&rows)[0].1, 400, "{tier:?}");
        }

        let july = window("2026-07-01 00:00", "2026-08-01 00:00");
        let series = |tier: Tier| {
            let q = Series {
                identity_id: node,
                range: july,
                tier: tier as i32,
                ..Default::default()
            };
            let rows = store.series(Visibility::All, &q, now).unwrap();
            rows.counters
                .iter()
                .map(|c| (c.bucket_unix_ms / 1000, c.ingress_bytes))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            series(Tier::Hour),
            [
                (utc("2026-07-06 10:00"), 100),
                (utc("2026-07-06 11:00"), 50)
            ]
        );
        assert_eq!(series(Tier::Day), [(utc("2026-07-06 00:00"), 150)]);
        assert_eq!(series(Tier::Month), [(utc("2026-07-01 00:00"), 150)]);

        // Nothing is counted twice once everything has rolled all the way up.
        let later = utc("2026-08-01 00:10");
        store.rollup(later).unwrap();
        assert_eq!(series(Tier::Month), [(utc("2026-07-01 00:00"), 150)]);
        assert_eq!(series(Tier::Day), [(utc("2026-07-06 00:00"), 150)]);
    }

    #[test]
    fn auto_tier_follows_the_window() {
        let (store, [node, ..]) = seeded();
        let now = utc("2026-07-06 12:00");
        let tier = |from: &str| {
            let q = Series {
                identity_id: node,
                range: window(from, "2026-07-06 12:00"),
                ..Default::default()
            };
            store.series(Visibility::All, &q, now).unwrap().tier()
        };
        assert_eq!(tier("2026-07-06 10:00"), Tier::Minute);
        assert_eq!(tier("2026-07-05 12:00"), Tier::Hour);
        assert_eq!(tier("2026-06-06 12:00"), Tier::Day);
        assert_eq!(tier("2025-07-06 12:00"), Tier::Month);
        // Totals take the finest tier whose retention reaches the window start.
        let top = |from: &str| {
            let q = TopIdentities {
                range: window(from, "2026-07-06 12:00"),
                ..Default::default()
            };
            store.top(Visibility::All, &q, now).unwrap().tier()
        };
        assert_eq!(top("2026-07-05 12:00"), Tier::Minute);
        assert_eq!(top("2026-06-06 12:00"), Tier::Hour);
        assert_eq!(top("2025-07-06 12:00"), Tier::Day);
    }

    #[test]
    fn a_missing_or_empty_range_is_a_bad_request() {
        let (store, _) = seeded();
        let q = TopIdentities::default();
        assert!(matches!(
            store.top(Visibility::All, &q, 0),
            Err(QueryError::BadRequest(_))
        ));
        let q = TopIdentities {
            range: window("2026-07-06 12:00", "2026-07-06 12:00"),
            ..Default::default()
        };
        assert!(matches!(
            store.top(Visibility::All, &q, 0),
            Err(QueryError::BadRequest(_))
        ));
    }
}
