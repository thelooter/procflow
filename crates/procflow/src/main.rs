mod client;
mod fmt;
mod theme;
mod tui;
mod ui;

use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime};
use clap::{Args, Parser, Subcommand};
use fmt::human_bytes;
use procflow_ipc::v1::{
    request, CounterRow, Direction, GroupBy, Identity, ListIdentities, Resolve, Scope, Series,
    Tier, TimeRange, TopIdentities, Watch,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{ErrorKind, IsTerminal, Write};

/// Per-process network traffic, tracked over time.
///
/// Run without a subcommand for the interactive view.
#[derive(Parser)]
#[command(name = "procflow", version)]
struct Cli {
    /// Colour theme of the interactive view
    #[arg(
        long,
        global = true,
        env = "PROCFLOW_THEME",
        default_value = "mocha",
        value_parser = theme_arg,
        value_name = theme::NAMES
    )]
    theme: usize,
    /// Keep the terminal's own background in the interactive view
    #[arg(long, global = true, env = "PROCFLOW_TRANSPARENT")]
    transparent: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

// Subcommands map ~1:1 to IPC verbs (ADR-0010).
#[derive(Subcommand)]
enum Command {
    /// Biggest talkers in a time window
    Top {
        #[command(flatten)]
        window: Window,
        /// Direction to rank by and show
        #[arg(long, value_parser = direction_arg, default_value = "both", value_name = "ingress|egress|both")]
        dir: Direction,
        #[arg(long, value_parser = scope_arg, default_value = "external", value_name = SCOPES)]
        scope: Scope,
        /// Roll identities up by one of their dimensions
        #[arg(long, value_parser = group_arg, default_value = "identity", value_name = GROUPS)]
        by: GroupBy,
        /// Rows to show; 0 for all
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, value_parser = tier_arg, value_name = TIERS)]
        tier: Option<Tier>,
        #[command(flatten)]
        dimensions: Dimensions,
        #[command(flatten)]
        output: Output,
    },
    /// One identity's history over time
    Series {
        identity_id: i64,
        #[command(flatten)]
        window: Window,
        #[arg(long, value_parser = scope_arg, default_value = "external", value_name = SCOPES)]
        scope: Scope,
        #[arg(long, value_parser = tier_arg, value_name = TIERS)]
        tier: Option<Tier>,
        #[command(flatten)]
        output: Output,
    },
    /// Browse/search identities
    List {
        /// Substring of the exe, project root or command name
        filter: Option<String>,
        #[command(flatten)]
        dimensions: Dimensions,
        /// Print JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Full identity detail
    Show {
        identity_id: i64,
        /// Print JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Live view; the interactive one unless --json is given or stdout is piped
    Watch {
        #[arg(long, value_parser = scope_arg, default_value = "external", value_name = SCOPES)]
        scope: Scope,
        #[arg(long, value_parser = group_arg, default_value = "identity", value_name = GROUPS)]
        by: GroupBy,
        /// With --json: rows per interval; 0 for all
        #[arg(long, default_value_t = 0)]
        limit: u32,
        /// Stream one JSON object per poll interval
        #[arg(long)]
        json: bool,
    },
    /// Daemon status and versions
    Status,
}

const SCOPES: &str = "external|loopback|all";
const GROUPS: &str = "identity|project|exe|user";
const TIERS: &str = "minute|hour|day|month";

/// The time window a query covers. Defaults to the last 24 hours.
#[derive(Args)]
struct Window {
    /// Window ending now: 90m, 24h, 7d, 4w
    #[arg(long, group = "start", value_parser = duration_arg, value_name = "DURATION")]
    since: Option<i64>,
    /// Since local midnight
    #[arg(long, group = "start")]
    today: bool,
    /// Since the first of this month, local time
    #[arg(long, group = "start")]
    this_month: bool,
    /// Window start: YYYY-MM-DD or "YYYY-MM-DD HH:MM", local time
    #[arg(long, group = "start", value_parser = instant_arg, value_name = "TIME")]
    from: Option<i64>,
    /// Window end, same format; defaults to now
    #[arg(long, requires = "from", value_parser = instant_arg, value_name = "TIME")]
    to: Option<i64>,
}

impl Window {
    fn range(&self, now: DateTime<Local>) -> TimeRange {
        let today = now.date_naive();
        let from_unix_ms = if let Some(from) = self.from {
            from
        } else if self.today {
            local_midnight_ms(today)
        } else if self.this_month {
            local_midnight_ms(today.with_day(1).expect("every month has a day 1"))
        } else {
            now.timestamp_millis() - self.since.unwrap_or(24 * 3600) * 1000
        };
        TimeRange {
            from_unix_ms,
            to_unix_ms: self.to.unwrap_or(now.timestamp_millis()),
        }
    }
}

/// Epoch milliseconds of a local wall time. A time skipped by a DST jump
/// resolves to the instant an hour later, the first that exists.
fn local_ms(time: NaiveDateTime) -> i64 {
    time.and_local_timezone(Local)
        .earliest()
        .or_else(|| {
            (time + chrono::Duration::hours(1))
                .and_local_timezone(Local)
                .earliest()
        })
        .map_or(time.and_utc().timestamp_millis(), |t| t.timestamp_millis())
}

pub(crate) fn local_midnight_ms(date: NaiveDate) -> i64 {
    local_ms(date.and_hms_opt(0, 0, 0).expect("midnight is a valid time"))
}

/// Narrow a query by Identity dimension (ADR-0010).
#[derive(Args)]
struct Dimensions {
    /// Only identities whose project root contains this
    #[arg(long, default_value = "", hide_default_value = true)]
    project: String,
    /// Only identities whose exe path contains this
    #[arg(long, default_value = "", hide_default_value = true)]
    exe: String,
    /// Only identities of this user (name or uid)
    #[arg(long, default_value = "", hide_default_value = true)]
    user: String,
}

#[derive(Args)]
struct Output {
    /// Print JSON instead of a table
    #[arg(long)]
    json: bool,
    /// Print byte counts as plain integers
    #[arg(long)]
    bytes: bool,
}

impl Output {
    fn bytes(&self, n: u64) -> String {
        if self.bytes {
            n.to_string()
        } else {
            human_bytes(n)
        }
    }
}

/// Seconds in `90m`, `24h`, `7d`, `4w`.
fn duration_arg(value: &str) -> Result<i64, String> {
    let split = value.len().saturating_sub(1);
    let unit = match value.get(split..) {
        Some("m") => 60,
        Some("h") => 3600,
        Some("d") => 86_400,
        Some("w") => 7 * 86_400,
        _ => return Err("expected a number and a unit: 90m, 24h, 7d, 4w".into()),
    };
    match value[..split].parse::<i64>() {
        Ok(n) if n > 0 => Ok(n * unit),
        _ => Err("expected a positive number before the unit".into()),
    }
}

/// Epoch milliseconds of a local date or date-time (or any RFC 3339 instant).
fn instant_arg(value: &str) -> Result<i64, String> {
    if let Ok(instant) = DateTime::parse_from_rfc3339(value) {
        return Ok(instant.timestamp_millis());
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(local_midnight_ms(date));
    }
    ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
        .map(local_ms)
        .ok_or_else(|| "expected YYYY-MM-DD or \"YYYY-MM-DD HH:MM\"".to_string())
}

/// Look a flag value up among a protobuf enum's names (`SCOPE_EXTERNAL`…).
fn enum_arg<E>(
    value: &str,
    prefix: &str,
    lookup: fn(&str) -> Option<E>,
    choices: &str,
) -> Result<E, String> {
    lookup(&format!("{prefix}_{}", value.to_uppercase()))
        .ok_or_else(|| format!("expected {choices}"))
}

fn theme_arg(value: &str) -> Result<usize, String> {
    theme::index(value).ok_or_else(|| format!("expected {}", theme::NAMES))
}

fn scope_arg(value: &str) -> Result<Scope, String> {
    enum_arg(value, "SCOPE", Scope::from_str_name, SCOPES)
}

fn group_arg(value: &str) -> Result<GroupBy, String> {
    enum_arg(value, "GROUP_BY", GroupBy::from_str_name, GROUPS)
}

fn tier_arg(value: &str) -> Result<Tier, String> {
    enum_arg(value, "TIER", Tier::from_str_name, TIERS)
}

fn direction_arg(value: &str) -> Result<Direction, String> {
    if value == "both" {
        return Ok(Direction::Unspecified);
    }
    enum_arg(
        value,
        "DIRECTION",
        Direction::from_str_name,
        "ingress|egress|both",
    )
}

fn main() -> Result<()> {
    let Cli {
        theme,
        transparent,
        command,
    } = Cli::parse();
    match command {
        None => tui::run(Scope::External, GroupBy::Identity, theme, transparent),
        Some(Command::Watch {
            scope,
            by,
            limit,
            json,
        }) => {
            if json || !std::io::stdout().is_terminal() {
                watch_json(Watch {
                    scope: scope as i32,
                    group_by: by as i32,
                    limit,
                })
            } else {
                tui::run(scope, by, theme, transparent)
            }
        }
        Some(Command::Top {
            window,
            dir,
            scope,
            by,
            limit,
            tier,
            dimensions,
            output,
        }) => {
            let query = TopIdentities {
                range: Some(window.range(Local::now())),
                tier: tier.unwrap_or_default() as i32,
                scope: scope as i32,
                group_by: by as i32,
                limit,
                direction: dir as i32,
                project: dimensions.project,
                exe: dimensions.exe,
                user: dimensions.user,
            };
            top(query, &output)
        }
        Some(Command::Series {
            identity_id,
            window,
            scope,
            tier,
            output,
        }) => {
            let query = Series {
                identity_id,
                tier: tier.unwrap_or_default() as i32,
                range: Some(window.range(Local::now())),
                scope: scope as i32,
            };
            series(query, &output)
        }
        Some(Command::List {
            filter,
            dimensions,
            json,
        }) => list(
            ListIdentities {
                filter: filter.unwrap_or_default(),
                project: dimensions.project,
                exe: dimensions.exe,
                user: dimensions.user,
            },
            json,
        ),
        Some(Command::Show { identity_id, json }) => show(identity_id, json),
        Some(Command::Status) => status(),
    }
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn rfc3339(unix_ms: i64) -> String {
    fmt::local(unix_ms).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn identity_json(identity: &Identity) -> Value {
    json!({
        "id": identity.id,
        "name": fmt::name(identity),
        "uid": identity.uid,
        "user": fmt::user(identity),
        "exe": identity.exe,
        "project_root": identity.project_root,
        "unit_or_cgroup": identity.unit_or_cgroup,
        "normalized_cmdline": identity.normalized_cmdline,
        "raw_cmdline": identity.raw_cmdline,
        "first_seen": rfc3339(identity.first_seen_unix_ms),
        "last_seen": rfc3339(identity.last_seen_unix_ms),
    })
}

/// A counter row with its Identity (or group) spelled out.
fn counter_json(counter: &CounterRow, identities: &HashMap<i64, Identity>) -> Value {
    let mut row = json!({
        "scope": fmt::scope(counter.scope()),
        "ingress_bytes": counter.ingress_bytes,
        "egress_bytes": counter.egress_bytes,
    });
    match identities.get(&counter.identity_id) {
        Some(identity) => row["identity"] = identity_json(identity),
        None => row["group"] = json!(counter.group_key),
    }
    row
}

fn by_id(identities: Vec<Identity>) -> HashMap<i64, Identity> {
    identities
        .into_iter()
        .map(|identity| (identity.id, identity))
        .collect()
}

fn top(query: TopIdentities, output: &Output) -> Result<()> {
    let range = query.range.expect("a window always has a range");
    let (scope, direction) = (query.scope(), query.direction());
    let group = match query.group_by() {
        GroupBy::Project => Some("PROJECT"),
        GroupBy::Exe => Some("EXE"),
        GroupBy::User => Some("USER"),
        GroupBy::Identity | GroupBy::Unspecified => None,
    };
    let rows = client::rows(request::Body::TopIdentities(query))?;
    let tier = rows.tier();
    let identities = by_id(rows.identities);
    if output.json {
        return print_json(&json!({
            "from": rfc3339(range.from_unix_ms),
            "to": rfc3339(range.to_unix_ms),
            "tier": fmt::tier(tier),
            "rows": rows.counters.iter().map(|c| counter_json(c, &identities)).collect::<Vec<_>>(),
        }));
    }
    if rows.counters.is_empty() {
        println!("no traffic recorded in this window");
        return Ok(());
    }

    // Direction is two columns, never one sum (CONTEXT.md).
    let mut headers = match group {
        Some(group) => vec![group],
        None => vec!["ID", "NAME", "PROJECT", "USER", "COMMAND"],
    };
    let first_number = headers.len();
    if scope == Scope::All {
        headers.push("SCOPE");
    }
    let (ingress, egress) = (
        direction != Direction::Egress,
        direction != Direction::Ingress,
    );
    headers.extend(ingress.then_some("INGRESS"));
    headers.extend(egress.then_some("EGRESS"));

    let table: Vec<Vec<String>> = rows
        .counters
        .iter()
        .map(|counter| {
            let mut cells = match identities.get(&counter.identity_id) {
                Some(identity) => vec![
                    identity.id.to_string(),
                    fmt::name(identity).to_string(),
                    fmt::project(identity).to_string(),
                    fmt::user(identity),
                    fmt::ellipsis(&identity.normalized_cmdline, 48),
                ],
                None => vec![counter.group_key.clone()],
            };
            if scope == Scope::All {
                cells.push(fmt::scope(counter.scope()).to_string());
            }
            cells.extend(ingress.then(|| output.bytes(counter.ingress_bytes)));
            cells.extend(egress.then(|| output.bytes(counter.egress_bytes)));
            cells
        })
        .collect();
    // Byte columns are right-aligned, and so is the id.
    let mut numbers: Vec<usize> =
        (first_number + usize::from(scope == Scope::All)..headers.len()).collect();
    numbers.extend(group.is_none().then_some(0));
    println!(
        "{} → {} · scope {} · tier {}",
        fmt::bucket(range.from_unix_ms, Tier::Minute),
        fmt::bucket(range.to_unix_ms, Tier::Minute),
        fmt::scope(scope),
        fmt::tier(tier)
    );
    println!("{}", fmt::table(&headers, &numbers, &table));
    Ok(())
}

fn series(query: Series, output: &Output) -> Result<()> {
    let scope = query.scope();
    let rows = client::rows(request::Body::Series(query))?;
    let tier = rows.tier();
    let identity = rows.identities.first().cloned().unwrap_or_default();
    if output.json {
        let counters: Vec<Value> = rows
            .counters
            .iter()
            .map(|c| {
                json!({
                    "bucket": rfc3339(c.bucket_unix_ms),
                    "scope": fmt::scope(c.scope()),
                    "ingress_bytes": c.ingress_bytes,
                    "egress_bytes": c.egress_bytes,
                })
            })
            .collect();
        return print_json(&json!({
            "identity": identity_json(&identity),
            "tier": fmt::tier(tier),
            "rows": counters,
        }));
    }
    println!(
        "{} · {} · {} · scope {} · tier {}",
        fmt::name(&identity),
        fmt::project(&identity),
        fmt::user(&identity),
        fmt::scope(scope),
        fmt::tier(tier)
    );
    if rows.counters.is_empty() {
        println!("no traffic recorded in this window");
        return Ok(());
    }
    let mut headers = vec!["BUCKET"];
    headers.extend((scope == Scope::All).then_some("SCOPE"));
    headers.extend(["INGRESS", "EGRESS"]);
    let table: Vec<Vec<String>> = rows
        .counters
        .iter()
        .map(|c| {
            let mut cells = vec![fmt::bucket(c.bucket_unix_ms, tier)];
            cells.extend((scope == Scope::All).then(|| fmt::scope(c.scope()).to_string()));
            cells.extend([output.bytes(c.ingress_bytes), output.bytes(c.egress_bytes)]);
            cells
        })
        .collect();
    println!(
        "{}",
        fmt::table(&headers, &[headers.len() - 2, headers.len() - 1], &table)
    );
    Ok(())
}

fn list(query: ListIdentities, json: bool) -> Result<()> {
    let identities = client::rows(request::Body::ListIdentities(query))?.identities;
    if json {
        return print_json(&identities.iter().map(identity_json).collect());
    }
    if identities.is_empty() {
        println!("no identities match");
        return Ok(());
    }
    let table: Vec<Vec<String>> = identities
        .iter()
        .map(|identity| {
            vec![
                identity.id.to_string(),
                fmt::name(identity).to_string(),
                fmt::project(identity).to_string(),
                fmt::user(identity),
                fmt::ellipsis(&identity.normalized_cmdline, 48),
                fmt::bucket(identity.last_seen_unix_ms, Tier::Minute),
            ]
        })
        .collect();
    println!(
        "{}",
        fmt::table(
            &["ID", "NAME", "PROJECT", "USER", "COMMAND", "LAST SEEN"],
            &[0],
            &table
        )
    );
    Ok(())
}

fn show(identity_id: i64, json: bool) -> Result<()> {
    let rows = client::rows(request::Body::Resolve(Resolve { identity_id }))?;
    let identity = rows.identities.first().cloned().unwrap_or_default();
    if json {
        return print_json(&identity_json(&identity));
    }
    let seen = |unix_ms| fmt::bucket(unix_ms, Tier::Minute);
    for (label, value) in [
        ("identity", identity.id.to_string()),
        ("name", fmt::name(&identity).to_string()),
        (
            "user",
            format!("{} (uid {})", fmt::user(&identity), identity.uid),
        ),
        ("exe", identity.exe.clone()),
        ("project", identity.project_root.clone()),
        ("unit", identity.unit_or_cgroup.clone()),
        ("command", identity.normalized_cmdline.clone()),
        ("last run as", identity.raw_cmdline.clone()),
        ("first seen", seen(identity.first_seen_unix_ms)),
        ("last seen", seen(identity.last_seen_unix_ms)),
    ] {
        println!("{label:<12}{value}");
    }
    Ok(())
}

/// One JSON object per poll interval, for scripts.
fn watch_json(watch: Watch) -> Result<()> {
    let mut identities = HashMap::new();
    let mut stdout = std::io::stdout().lock();
    for chunk in client::watch(watch)? {
        let chunk = chunk?;
        let rows = chunk.rows.unwrap_or_default();
        // The daemon describes each Identity once per stream.
        identities.extend(by_id(rows.identities));
        let at = rows
            .counters
            .first()
            .map_or(Local::now().timestamp_millis(), |c| c.bucket_unix_ms);
        let line = json!({
            "at": rfc3339(at),
            "interval_ms": chunk.interval_ms,
            "rows": rows.counters.iter().map(|c| counter_json(c, &identities)).collect::<Vec<_>>(),
        });
        // A closed pipe, as in `procflow watch --json | head`, is the reader
        // saying it has enough.
        match writeln!(stdout, "{line}") {
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(()),
            result => result?,
        }
    }
    Ok(())
}

fn status() -> Result<()> {
    let hello = client::hello()?;
    let collector = if hello.collector_active {
        "running"
    } else {
        "not running (stored history only)"
    };
    println!("daemon:    procflowd {}", hello.daemon_version);
    println!("collector: {collector}");
    println!(
        "protocol:  v{}..=v{} (client v{})",
        hello.proto_min,
        hello.proto_max,
        procflow_ipc::PROTO_VERSION
    );
    println!("socket:    {}", procflow_ipc::socket_path().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn durations_and_enums_parse() {
        assert_eq!(duration_arg("90m"), Ok(5400));
        assert_eq!(duration_arg("24h"), Ok(86_400));
        assert_eq!(duration_arg("2w"), Ok(14 * 86_400));
        for bad in ["", "h", "0h", "-3h", "12", "1y", "1.5h"] {
            assert!(duration_arg(bad).is_err(), "{bad:?}");
        }
        assert_eq!(scope_arg("loopback"), Ok(Scope::Loopback));
        assert_eq!(group_arg("project"), Ok(GroupBy::Project));
        assert_eq!(tier_arg("Day"), Ok(Tier::Day));
        assert_eq!(direction_arg("both"), Ok(Direction::Unspecified));
        assert_eq!(direction_arg("egress"), Ok(Direction::Egress));
        assert!(scope_arg("lan").is_err());
    }

    #[test]
    fn the_theme_flags_work_before_and_after_the_subcommand() {
        let parse =
            |args: &[&str]| Cli::try_parse_from(args).map(|cli| (cli.theme, cli.transparent));
        assert_eq!(
            parse(&["procflow", "--theme", "latte"]).unwrap(),
            (3, false)
        );
        assert_eq!(
            parse(&["procflow", "watch", "--theme", "frappe", "--transparent"]).unwrap(),
            (2, true)
        );
        assert!(parse(&["procflow", "--theme", "solarized"]).is_err());
    }

    #[test]
    fn windows_resolve_against_local_time() {
        let parse = |args: &[&str]| match Cli::try_parse_from([&["procflow", "top"], args].concat())
        {
            Ok(Cli {
                command: Some(Command::Top { window, .. }),
                ..
            }) => window,
            Ok(_) => unreachable!(),
            Err(e) => panic!("{e}"),
        };
        let now = Local::now();
        let now_ms = now.timestamp_millis();
        let range = parse(&[]).range(now);
        assert_eq!(
            (range.from_unix_ms, range.to_unix_ms),
            (now_ms - 86_400_000, now_ms)
        );
        assert_eq!(
            parse(&["--since", "90m"]).range(now).from_unix_ms,
            now_ms - 5_400_000
        );

        let midnight = parse(&["--today"]).range(now).from_unix_ms;
        assert_eq!(
            fmt::local(midnight).format("%H:%M:%S").to_string(),
            "00:00:00"
        );
        assert_eq!(fmt::local(midnight).date_naive(), now.date_naive());
        let month = parse(&["--this-month"]).range(now).from_unix_ms;
        assert_eq!(fmt::local(month).format("%d %H:%M").to_string(), "01 00:00");

        let explicit = parse(&["--from", "2026-07-06", "--to", "2026-07-06 12:30"]).range(now);
        assert_eq!(
            fmt::bucket(explicit.from_unix_ms, Tier::Minute),
            "2026-07-06 00:00"
        );
        assert_eq!(
            fmt::bucket(explicit.to_unix_ms, Tier::Minute),
            "2026-07-06 12:30"
        );

        // One way of naming the window at a time, and --to needs --from.
        assert!(Cli::try_parse_from(["procflow", "top", "--today", "--since", "1h"]).is_err());
        assert!(Cli::try_parse_from(["procflow", "top", "--to", "2026-07-06"]).is_err());
    }
}
