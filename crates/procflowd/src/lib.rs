//! procflowd internals, exposed as a library so integration tests can drive
//! the server without a privileged install.

pub mod collector;
pub mod enrich;
pub mod rollup;
pub mod server;
pub mod store;

/// Wall-clock seconds since the epoch (ADR-0003: buckets follow wall time).
pub fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}

/// Epoch seconds of a UTC wall time such as `2026-07-06 10:05`.
#[cfg(test)]
pub(crate) fn utc(s: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc().timestamp()
}
