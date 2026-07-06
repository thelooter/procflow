//! tgid → Identity enrichment (ADR-0004, ADR-0007).
//!
//! Reads `/proc/<tgid>` while the process is (hopefully still) alive and
//! derives the Identity natural key: `(uid, unit_or_cgroup, exe,
//! project_root, normalized_cmdline)`. When the process is already gone the
//! kernel-captured [`PidMeta`] yields a degraded **Unresolved Identity**
//! instead — race-lost bytes are never dropped.

use anyhow::{Context, Result};
use procflow_common::PidMeta;
use std::path::{Path, PathBuf};

/// Sentinel for fields a lost dead-PID race made unknowable (CONTEXT.md).
pub const UNRESOLVED: &str = "<unresolved>";
/// Sentinel project root for processes with no marker above their cwd.
pub const NO_PROJECT: &str = "<none>";

/// Everything the store needs to upsert an Identity (ADR-0004): the natural
/// key plus display-only attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRecord {
    pub uid: u32,
    pub unit_or_cgroup: String,
    pub exe: String,
    pub project_root: String,
    pub normalized_cmdline: String,
    pub comm: String,
    pub raw_cmdline: String,
    pub username: Option<String>,
}

/// Enrich from a live `/proc/<tgid>`. Fails if the process is gone or
/// essential files are unreadable — callers fall back to [`from_pid_meta`].
pub fn from_proc(tgid: u32) -> Result<IdentityRecord> {
    let proc_dir = PathBuf::from(format!("/proc/{tgid}"));

    let exe = std::fs::read_link(proc_dir.join("exe"))
        .context("readlink exe")?
        .to_string_lossy()
        .trim_end_matches(" (deleted)")
        .to_string();
    let cwd = std::fs::read_link(proc_dir.join("cwd")).context("readlink cwd")?;
    let comm = std::fs::read_to_string(proc_dir.join("comm"))
        .context("read comm")?
        .trim()
        .to_string();

    let raw_cmdline = std::fs::read(proc_dir.join("cmdline"))
        .context("read cmdline")?
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>()
        .join(" ");

    let status = std::fs::read_to_string(proc_dir.join("status")).context("read status")?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|real| real.parse::<u32>().ok())
        .context("parse Uid from status")?;

    // cgroup v2: a single "0::<path>" line. The path names the systemd
    // unit/scope hierarchy — stored whole (ADR-0004 "unit_or_cgroup").
    let unit_or_cgroup = std::fs::read_to_string(proc_dir.join("cgroup"))
        .context("read cgroup")?
        .lines()
        .find_map(|line| line.splitn(3, ':').nth(2).map(str::to_string))
        .unwrap_or_default();

    let project_root = find_project_root(&cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| NO_PROJECT.to_string());

    Ok(IdentityRecord {
        uid,
        unit_or_cgroup,
        exe,
        project_root,
        normalized_cmdline: normalize_cmdline(&raw_cmdline),
        comm,
        raw_cmdline,
        username: username_for(uid),
    })
}

/// Degraded Unresolved Identity from the kernel-captured first-sight
/// metadata (ADR-0007). Still keyed by uid + cgroup + comm — comm rides in
/// the normalized_cmdline sentinel because the ADR-0004 natural key treats
/// the comm *column* as display-only.
pub fn from_pid_meta(meta: &PidMeta) -> IdentityRecord {
    let comm_len = meta.comm.iter().position(|b| *b == 0).unwrap_or(meta.comm.len());
    let comm = String::from_utf8_lossy(&meta.comm[..comm_len]).into_owned();
    IdentityRecord {
        uid: meta.uid,
        unit_or_cgroup: format!("cgroup-id:{}", meta.cgroup_id),
        exe: UNRESOLVED.to_string(),
        project_root: UNRESOLVED.to_string(),
        normalized_cmdline: format!("<unresolved:{comm}>"),
        comm,
        raw_cmdline: String::new(),
        username: username_for(meta.uid),
    }
}

/// Last-resort record when even PID_META is gone (e.g. ring-buffer loss plus
/// map eviction). uid u32::MAX marks "owner unknown" without misattributing
/// to root.
pub fn fully_unresolved() -> IdentityRecord {
    IdentityRecord {
        uid: u32::MAX,
        unit_or_cgroup: UNRESOLVED.to_string(),
        exe: UNRESOLVED.to_string(),
        project_root: UNRESOLVED.to_string(),
        normalized_cmdline: UNRESOLVED.to_string(),
        comm: String::new(),
        raw_cmdline: String::new(),
        username: None,
    }
}

/// Walk up from `cwd` for a project marker (CONTEXT.md "Project root").
/// Deepest match wins; `None` if nothing up to the filesystem root.
pub fn find_project_root(cwd: &Path) -> Option<PathBuf> {
    const MARKERS: [&str; 4] = [".git", "package.json", "pyproject.toml", "Cargo.toml"];
    cwd.ancestors()
        .find(|dir| MARKERS.iter().any(|m| dir.join(m).exists()))
        .map(Path::to_path_buf)
}

/// Mask volatile cmdline tokens (ADR-0004): post-`=` values → `<v>`,
/// UUIDs → `<uuid>`, `/tmp` & `/proc` paths → `<path>`, numbers → `<n>`.
/// `--port 3000` and `--port 3001` merge; `npm run dev` and `npm run build`
/// stay distinct.
pub fn normalize_cmdline(raw: &str) -> String {
    raw.split_whitespace()
        .map(|token| {
            if let Some(eq) = token.find('=') {
                format!("{}=<v>", &token[..eq])
            } else if is_uuid(token) {
                "<uuid>".to_string()
            } else if token.starts_with("/tmp/") || token.starts_with("/proc/") {
                "<path>".to_string()
            } else if token.parse::<f64>().is_ok() {
                "<n>".to_string()
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_uuid(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Display-only username lookup (ADR-0004: UIDs get reused/renamed, so this
/// is resolved at enrichment time, not stored as truth).
fn username_for(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let entry_uid: u32 = fields.next()?.parse().ok()?;
        (entry_uid == uid).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_masks_volatile_tokens() {
        // Ports merge (ADR-0004's canonical example).
        assert_eq!(
            normalize_cmdline("npm run dev --port 3000"),
            normalize_cmdline("npm run dev --port 3001"),
        );
        // Distinct scripts stay distinct.
        assert_ne!(normalize_cmdline("npm run dev"), normalize_cmdline("npm run build"));
        assert_eq!(normalize_cmdline("--config=/etc/app.conf"), "--config=<v>");
        assert_eq!(
            normalize_cmdline("worker 550e8400-e29b-41d4-a716-446655440000"),
            "worker <uuid>"
        );
        assert_eq!(normalize_cmdline("cat /tmp/build-123/log"), "cat <path>");
        assert_eq!(normalize_cmdline("sleep 3.5"), "sleep <n>");
        // Digit-containing words are not numbers.
        assert_eq!(normalize_cmdline("python3 serve.py"), "python3 serve.py");
    }

    #[test]
    fn project_root_walk_finds_deepest_marker() {
        let base = std::env::temp_dir().join(format!("procflow-enrich-test-{}", std::process::id()));
        let nested = base.join("repo/sub/dir");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(base.join("repo/Cargo.toml"), "").unwrap();

        assert_eq!(find_project_root(&nested), Some(base.join("repo")));
        // With the marker gone, nothing under `base` matches any more (a
        // marker above the temp dir would be outside `base` entirely).
        std::fs::remove_file(base.join("repo/Cargo.toml")).unwrap();
        assert!(find_project_root(&nested).is_none_or(|p| !p.starts_with(&base)));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn enriches_own_process_from_proc() {
        let rec = from_proc(std::process::id()).unwrap();
        // The test binary is this crate's unit-test executable.
        assert!(rec.exe.contains("procflowd"), "exe = {}", rec.exe);
        assert_eq!(rec.uid, unsafe { libc_geteuid() });
        // cargo test runs with cwd = crate dir, which has a Cargo.toml.
        assert!(rec.project_root.ends_with("procflowd") || rec.project_root.ends_with("procflow"),
            "project_root = {}", rec.project_root);
        assert!(!rec.normalized_cmdline.is_empty());
        assert!(rec.username.is_some());
    }

    // Avoid a libc dep for one call: geteuid via /proc/self/status parse
    // would be circular; use the syscall through std's UID on the fs.
    unsafe fn libc_geteuid() -> u32 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(u32::MAX)
    }

    #[test]
    fn unresolved_identity_keys_by_uid_cgroup_comm() {
        let meta = PidMeta {
            cgroup_id: 42,
            uid: 1000,
            comm: *b"curl\0\0\0\0\0\0\0\0\0\0\0\0",
            _pad: [0; 4],
        };
        let rec = from_pid_meta(&meta);
        assert_eq!(rec.exe, UNRESOLVED);
        assert_eq!(rec.unit_or_cgroup, "cgroup-id:42");
        assert_eq!(rec.normalized_cmdline, "<unresolved:curl>");
        assert_eq!(rec.comm, "curl");
    }
}
