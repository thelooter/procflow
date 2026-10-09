//! Tier rollup + prune job (ADR-0005) and the calendar it buckets by
//! (ADR-0003).

use crate::store::Store;
use anyhow::{Context, Result};
use chrono::{Datelike, FixedOffset, Local, LocalResult, Months, NaiveDate, TimeZone};
use procflow_ipc::v1::Tier;

/// The zone day and month buckets align to (ADR-0003). `Fixed` keeps tests
/// independent of the machine's zone.
#[derive(Clone, Copy, Debug)]
pub enum Zone {
    Local,
    Fixed(FixedOffset),
}

impl Zone {
    /// `[start, end)` of the `tier` bucket containing `epoch_s`, as epoch
    /// seconds. Minute and hour truncate UTC; day and month follow the zone.
    pub fn bounds(&self, tier: Tier, epoch_s: i64) -> (i64, i64) {
        let fixed = |len: i64| {
            let start = epoch_s - epoch_s.rem_euclid(len);
            (start, start + len)
        };
        match (tier, self) {
            (Tier::Minute | Tier::Unspecified, _) => fixed(60),
            (Tier::Hour, _) => fixed(3600),
            (_, Zone::Local) => calendar_bounds(&Local, tier, epoch_s),
            (_, Zone::Fixed(tz)) => calendar_bounds(tz, tier, epoch_s),
        }
    }
}

fn calendar_bounds<Tz: TimeZone>(tz: &Tz, tier: Tier, epoch_s: i64) -> (i64, i64) {
    let date = tz
        .timestamp_opt(epoch_s, 0)
        .single()
        .expect("epoch seconds map to exactly one instant")
        .date_naive();
    let (first, next) = if tier == Tier::Month {
        let first = date.with_day(1).expect("every month has a day 1");
        (first, first + Months::new(1))
    } else {
        (date, date.succ_opt().expect("date within chrono's range"))
    };
    (local_midnight(tz, first), local_midnight(tz, next))
}

/// The instant `date` begins in `tz`. A DST jump can skip midnight; the day
/// then begins at the first instant that exists.
fn local_midnight<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> i64 {
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight is a valid time");
    match tz.from_local_datetime(&midnight) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t.timestamp(),
        LocalResult::None => tz
            .from_local_datetime(&(midnight + chrono::Duration::hours(1)))
            .earliest()
            .map_or(midnight.and_utc().timestamp(), |t| t.timestamp()),
    }
}

/// How long each tier keeps its rows (ADR-0005 defaults); the month tier is
/// kept forever. Becomes config-driven with `/etc/procflow/config.toml`
/// (ADR-0011).
#[derive(Clone, Copy, Debug)]
pub struct Retention {
    pub minute_s: i64,
    pub hour_s: i64,
    pub day_s: i64,
}

impl Default for Retention {
    fn default() -> Self {
        const DAY: i64 = 86_400;
        Retention {
            minute_s: 2 * DAY,
            hour_s: 90 * DAY,
            day_s: 730 * DAY,
        }
    }
}

impl Retention {
    /// The finest tier that still holds rows as old as `from_s`.
    pub fn finest_covering(&self, from_s: i64, now_s: i64) -> Tier {
        let age = now_s - from_s;
        if age <= self.minute_s {
            Tier::Minute
        } else if age <= self.hour_s {
            Tier::Hour
        } else if age <= self.day_s {
            Tier::Day
        } else {
            Tier::Month
        }
    }
}

/// How long a bucket must have been closed before it rolls up. The collector
/// writes a closed minute on its next poll, and a minute that arrived after
/// its hour had rolled up would never be counted in the coarser tiers.
const GRACE_S: i64 = 120;

/// `(tier, the tier it is rolled up from)`. Day reads minutes rather than
/// hours: in a zone with a sub-hour UTC offset, local midnight falls inside
/// a UTC hour bucket.
const ROLLUPS: [(Tier, Tier); 3] = [
    (Tier::Hour, Tier::Minute),
    (Tier::Day, Tier::Minute),
    (Tier::Month, Tier::Day),
];

pub(crate) fn table(tier: Tier) -> &'static str {
    match tier {
        Tier::Minute | Tier::Unspecified => "traffic_minute",
        Tier::Hour => "traffic_hour",
        Tier::Day => "traffic_day",
        Tier::Month => "traffic_month",
    }
}

fn watermark_name(tier: Tier) -> &'static str {
    &table(tier)["traffic_".len()..]
}

impl Store {
    /// Roll every closed bucket past its watermark into the coarser tiers,
    /// then prune rows past retention (ADR-0005). A bucket and its watermark
    /// move in one transaction, so a rerun never double-counts and a run
    /// after downtime picks up wherever the last one stopped.
    pub fn rollup(&self, now_s: i64) -> Result<()> {
        for (tier, from) in ROLLUPS {
            while self.rollup_next(tier, from, now_s)? {}
        }
        self.prune(now_s)
    }

    /// Roll up the oldest `tier` bucket that has unrolled source rows.
    /// Returns false once that bucket is still open (or nothing is left).
    fn rollup_next(&self, tier: Tier, from: Tier, now_s: i64) -> Result<bool> {
        let watermark = self.watermark(tier)?;
        let oldest_ms: Option<i64> = self.conn.query_row(
            &format!(
                "SELECT epoch_ms(min(bucket)) FROM {} WHERE bucket >= make_timestamp(?)",
                table(from)
            ),
            [watermark.unwrap_or(0) * 1_000_000],
            |r| r.get(0),
        )?;
        let Some(oldest_ms) = oldest_ms else {
            return Ok(false);
        };
        let (start, end) = self.zone.bounds(tier, oldest_ms / 1000);
        if end + GRACE_S > now_s {
            return Ok(false);
        }
        // After a zone change the bucket can begin before the watermark;
        // rows below it are already counted elsewhere (ADR-0003).
        let lower = watermark.map_or(start, |w| w.max(start));
        let (dst, src, name) = (table(tier), table(from), watermark_name(tier));
        let [start_us, lower_us, end_us] = [start, lower, end].map(|s| s * 1_000_000);
        let batch = format!(
            "BEGIN;
             INSERT INTO {dst} (bucket, identity_id, scope, ingress_bytes, egress_bytes)
             SELECT make_timestamp({start_us}), identity_id, scope,
                    CAST(sum(ingress_bytes) AS UBIGINT), CAST(sum(egress_bytes) AS UBIGINT)
             FROM {src}
             WHERE bucket >= make_timestamp({lower_us})
               AND bucket < make_timestamp({end_us})
             GROUP BY identity_id, scope
             ON CONFLICT (bucket, identity_id, scope) DO UPDATE SET
                 ingress_bytes = {dst}.ingress_bytes + excluded.ingress_bytes,
                 egress_bytes  = {dst}.egress_bytes + excluded.egress_bytes;
             INSERT INTO rollup_watermark VALUES ('{name}', make_timestamp({end_us}))
             ON CONFLICT (tier) DO UPDATE SET rolled_up_through = excluded.rolled_up_through;
             COMMIT;"
        );
        if let Err(e) = self.conn.execute_batch(&batch) {
            let _ = self.conn.execute_batch("ROLLBACK");
            return Err(e).with_context(|| format!("rolling {src} up into {dst} bucket {start}"));
        }
        Ok(true)
    }

    /// Exclusive upper bound, epoch seconds, of the source rows already
    /// rolled into `tier`; `None` before its first rollup.
    pub(crate) fn watermark(&self, tier: Tier) -> Result<Option<i64>> {
        let ms: Option<i64> = self.conn.query_row(
            "SELECT epoch_ms(max(rolled_up_through)) FROM rollup_watermark WHERE tier = ?",
            [watermark_name(tier)],
            |r| r.get(0),
        )?;
        Ok(ms.map(|ms| ms / 1000))
    }

    /// Delete rows past retention, but never a row a coarser tier has yet
    /// to be built from.
    fn prune(&self, now_s: i64) -> Result<()> {
        let delete = |tier: Tier, before_s: i64| {
            self.conn.execute(
                &format!(
                    "DELETE FROM {} WHERE bucket < make_timestamp(?)",
                    table(tier)
                ),
                [before_s * 1_000_000],
            )
        };
        if let (Some(hour), Some(day)) = (self.watermark(Tier::Hour)?, self.watermark(Tier::Day)?) {
            delete(
                Tier::Minute,
                (now_s - self.retention.minute_s).min(hour).min(day),
            )?;
        }
        delete(Tier::Hour, now_s - self.retention.hour_s)?;
        if let Some(month) = self.watermark(Tier::Month)? {
            delete(Tier::Day, (now_s - self.retention.day_s).min(month))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utc;

    fn store_in(offset_minutes: i32) -> (Store, i64) {
        let mut store = Store::open_in_memory().unwrap();
        store.zone = Zone::Fixed(FixedOffset::east_opt(offset_minutes * 60).unwrap());
        let id = store
            .upsert_identity(&crate::enrich::fully_unresolved())
            .unwrap();
        (store, id)
    }

    /// `(bucket epoch s, ingress, egress)` of every row in a tier.
    fn rows(store: &Store, tier: Tier) -> Vec<(i64, u64, u64)> {
        let mut stmt = store
            .conn
            .prepare(&format!(
                "SELECT epoch_ms(bucket), ingress_bytes, egress_bytes FROM {} ORDER BY bucket",
                table(tier)
            ))
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)? / 1000, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[test]
    fn calendar_bounds_follow_the_zone() {
        let india = Zone::Fixed(FixedOffset::east_opt(330 * 60).unwrap());
        // 2026-07-06 20:00 UTC is 01:30 on the 7th in +05:30.
        let t = utc("2026-07-06 20:00");
        assert_eq!(
            india.bounds(Tier::Day, t),
            (utc("2026-07-06 18:30"), utc("2026-07-07 18:30"))
        );
        assert_eq!(
            india.bounds(Tier::Month, t),
            (utc("2026-06-30 18:30"), utc("2026-07-31 18:30"))
        );
        // Minute and hour ignore the zone (ADR-0003).
        assert_eq!(india.bounds(Tier::Hour, t + 61), (t, t + 3600));
        assert_eq!(india.bounds(Tier::Minute, t + 61), (t + 60, t + 120));
    }

    #[test]
    fn rollup_moves_only_closed_buckets_and_is_idempotent() {
        let (store, id) = store_in(0);
        store
            .record_minute(utc("2026-07-06 10:05"), id, "external", 100, 10)
            .unwrap();
        store
            .record_minute(utc("2026-07-06 10:55"), id, "external", 50, 5)
            .unwrap();
        store
            .record_minute(utc("2026-07-06 11:10"), id, "external", 7, 1)
            .unwrap();

        // 11:30: hour 10 is closed, hour 11 and the day are still open.
        let now = utc("2026-07-06 11:30");
        store.rollup(now).unwrap();
        store.rollup(now).unwrap(); // a rerun must not double-count
        assert_eq!(
            rows(&store, Tier::Hour),
            [(utc("2026-07-06 10:00"), 150, 15)]
        );
        assert_eq!(
            store.watermark(Tier::Hour).unwrap(),
            Some(utc("2026-07-06 11:00"))
        );
        assert!(rows(&store, Tier::Day).is_empty());

        // Two days later everything has closed; nothing is a month old yet.
        store.rollup(utc("2026-07-08 00:10")).unwrap();
        assert_eq!(
            rows(&store, Tier::Hour),
            [
                (utc("2026-07-06 10:00"), 150, 15),
                (utc("2026-07-06 11:00"), 7, 1)
            ]
        );
        assert_eq!(
            rows(&store, Tier::Day),
            [(utc("2026-07-06 00:00"), 157, 16)]
        );
        assert!(rows(&store, Tier::Month).is_empty());

        store.rollup(utc("2026-08-01 00:10")).unwrap();
        assert_eq!(
            rows(&store, Tier::Month),
            [(utc("2026-07-01 00:00"), 157, 16)]
        );
    }

    #[test]
    fn a_bucket_inside_the_grace_period_waits() {
        let (store, id) = store_in(0);
        store
            .record_minute(utc("2026-07-06 10:59"), id, "external", 1, 1)
            .unwrap();
        store.rollup(utc("2026-07-06 11:01")).unwrap();
        assert!(rows(&store, Tier::Hour).is_empty());
        store.rollup(utc("2026-07-06 11:02")).unwrap();
        assert_eq!(rows(&store, Tier::Hour).len(), 1);
    }

    #[test]
    fn day_buckets_split_at_local_midnight_even_inside_a_utc_hour() {
        // +05:30: local midnight is 18:30 UTC, inside the 18:00 hour bucket.
        let (store, id) = store_in(330);
        store
            .record_minute(utc("2026-07-06 18:29"), id, "external", 1, 0)
            .unwrap();
        store
            .record_minute(utc("2026-07-06 18:30"), id, "external", 2, 0)
            .unwrap();
        store.rollup(utc("2026-07-08 00:00")).unwrap();
        assert_eq!(
            rows(&store, Tier::Day),
            [
                (utc("2026-07-05 18:30"), 1, 0),
                (utc("2026-07-06 18:30"), 2, 0)
            ]
        );
    }

    #[test]
    fn prune_respects_retention_and_watermarks() {
        let (mut store, id) = store_in(0);
        store.retention = Retention {
            minute_s: 3600,
            hour_s: 7200,
            day_s: 86_400,
        };
        store
            .record_minute(utc("2026-07-06 10:05"), id, "external", 100, 10)
            .unwrap();
        store
            .record_minute(utc("2026-07-06 11:10"), id, "external", 7, 1)
            .unwrap();

        // Both minutes are past retention, but the day tier has not been
        // built from them yet, so they stay.
        store.rollup(utc("2026-07-06 13:00")).unwrap();
        assert_eq!(rows(&store, Tier::Minute).len(), 2);

        // Once the day has rolled up, minutes and hours past retention go.
        store.rollup(utc("2026-07-07 00:10")).unwrap();
        assert!(rows(&store, Tier::Minute).is_empty());
        assert!(rows(&store, Tier::Hour).is_empty());
        assert_eq!(
            rows(&store, Tier::Day),
            [(utc("2026-07-06 00:00"), 107, 11)]
        );
    }
}
