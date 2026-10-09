//! Formatting shared by the plain commands and the TUI.

use chrono::{DateTime, Local};
use procflow_ipc::v1::{Identity, Scope, Tier};

/// Bytes in binary units (ADR-0010): `512 B`, `1.2 KiB`, `34.5 GiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // 1023.95 would print as "1024.0" of the smaller unit.
    while value >= 1023.95 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

/// `path` with the caller's home directory written as `~`.
pub fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_string(),
    }
}

/// Short name for an Identity: its comm, else its exe's file name.
pub fn name(identity: &Identity) -> &str {
    if identity.comm.is_empty() {
        basename(&identity.exe)
    } else {
        &identity.comm
    }
}

/// The project's directory name; `-` for an Identity outside any project.
pub fn project(identity: &Identity) -> &str {
    match identity.project_root.as_str() {
        "<none>" => "-",
        root => basename(root),
    }
}

pub fn user(identity: &Identity) -> String {
    match (identity.username.as_str(), identity.uid) {
        ("", u32::MAX) => "?".to_string(), // owner unknown (fully unresolved)
        ("", uid) => uid.to_string(),
        (name, _) => name.to_string(),
    }
}

pub fn scope(scope: Scope) -> &'static str {
    match scope {
        Scope::Loopback => "loopback",
        Scope::All => "all",
        Scope::External | Scope::Unspecified => "external",
    }
}

pub fn tier(tier: Tier) -> &'static str {
    match tier {
        Tier::Minute => "minute",
        Tier::Hour => "hour",
        Tier::Day => "day",
        Tier::Month => "month",
        Tier::Unspecified => "auto",
    }
}

pub fn local(unix_ms: i64) -> DateTime<Local> {
    DateTime::from_timestamp_millis(unix_ms)
        .unwrap_or_default()
        .with_timezone(&Local)
}

/// A bucket's start in local time, as precise as its tier.
pub fn bucket(unix_ms: i64, tier: Tier) -> String {
    let format = match tier {
        Tier::Day => "%Y-%m-%d",
        Tier::Month => "%Y-%m",
        _ => "%Y-%m-%d %H:%M",
    };
    local(unix_ms).format(format).to_string()
}

/// `text` cut to `max` characters, the last one an ellipsis if it was cut.
pub fn ellipsis(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

/// A plain-text table: columns padded to their widest cell, the columns in
/// `right` right-aligned.
pub fn table(headers: &[&str], right: &[usize], rows: &[Vec<String>]) -> String {
    let width = |text: &str| text.chars().count();
    let widths: Vec<usize> = (0..headers.len())
        .map(|col| {
            rows.iter()
                .map(|row| width(&row[col]))
                .chain([width(headers[col])])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let cells: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(col, cell)| {
                let pad = " ".repeat(widths[col] - width(cell));
                if right.contains(&col) {
                    format!("{pad}{cell}")
                } else {
                    format!("{cell}{pad}")
                }
            })
            .collect();
        cells.join("  ").trim_end().to_string()
    };
    let mut out = vec![line(headers.to_vec())];
    out.extend(
        rows.iter()
            .map(|row| line(row.iter().map(String::as_str).collect())),
    );
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_humanised_in_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024 - 1), "1.0 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(human_bytes(u64::MAX), "16384.0 PiB");
    }

    #[test]
    fn identity_labels() {
        let mut identity = Identity {
            exe: "/usr/bin/node".into(),
            project_root: "/home/eve/web".into(),
            uid: 1000,
            ..Default::default()
        };
        assert_eq!(
            (
                name(&identity),
                project(&identity),
                user(&identity).as_str()
            ),
            ("node", "web", "1000")
        );
        identity.comm = "MainThread".into();
        identity.project_root = "<none>".into();
        identity.username = "eve".into();
        assert_eq!(
            (
                name(&identity),
                project(&identity),
                user(&identity).as_str()
            ),
            ("MainThread", "-", "eve")
        );
    }

    #[test]
    fn tables_align_and_ellipsis_cuts() {
        let rows = vec![
            vec!["node".to_string(), "5 B".to_string()],
            vec!["x".to_string(), "1.5 KiB".to_string()],
        ];
        assert_eq!(
            table(&["NAME", "IN"], &[1], &rows),
            "NAME       IN\nnode      5 B\nx     1.5 KiB"
        );
        assert_eq!(ellipsis("procflow", 5), "proc…");
        assert_eq!(ellipsis("procflow", 8), "procflow");
        let home = std::env::var("HOME").unwrap();
        assert_eq!(tilde(&format!("{home}/code/web")), "~/code/web");
        assert_eq!(
            tilde(&format!("{home}-other/web")),
            format!("{home}-other/web")
        );
        assert_eq!(tilde("/usr/bin/node"), "/usr/bin/node");
    }
}
