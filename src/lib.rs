//! Core logic for ramtop: collecting processes, figuring out which
//! service owns them, filtering, and killing. Kept free of any UI code
//! so it can be tested directly.

pub mod gui;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    /// Resident memory in bytes.
    pub mem: u64,
    /// systemd unit (service/scope) the process belongs to, or "-".
    pub unit: String,
    pub cmd: String,
    pub category: Category,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Launched from the desktop/terminal (lives in an `app-*` unit).
    App,
    /// Daemons and services running in the background.
    Background,
    /// Kernel threads (no command line, no user memory).
    Kernel,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::App => "App",
            Category::Background => "Background",
            Category::Kernel => "Kernel",
        }
    }
}

pub fn categorize(unit: &str, cmd: &str, mem: u64) -> Category {
    if cmd.is_empty() && mem == 0 {
        Category::Kernel
    } else if unit.starts_with("app-") {
        Category::App
    } else {
        Category::Background
    }
}

/// Total RAM per service unit, biggest first: (unit, bytes, process count).
pub fn group_by_unit<'a>(
    procs: impl IntoIterator<Item = &'a ProcInfo>,
) -> Vec<(String, u64, usize)> {
    let mut map: std::collections::HashMap<&str, (u64, usize)> = Default::default();
    for p in procs {
        let e = map.entry(p.unit.as_str()).or_default();
        e.0 += p.mem;
        e.1 += 1;
    }
    let mut out: Vec<_> = map
        .into_iter()
        .map(|(u, (m, n))| (u.to_string(), m, n))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Refresh `sys` and return all processes (threads excluded), biggest RAM first.
pub fn snapshot(sys: &mut System) -> Vec<ProcInfo> {
    sys.refresh_memory();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );
    let mut procs: Vec<ProcInfo> = sys
        .processes()
        .values()
        .filter(|p| p.thread_kind().is_none())
        .map(|p| {
            let pid = p.pid().as_u32();
            let cmd = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            let unit = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                .map(|s| unit_from_cgroup(&s))
                .unwrap_or_else(|_| "-".into());
            ProcInfo {
                pid,
                name: p.name().to_string_lossy().into_owned(),
                mem: p.memory(),
                category: categorize(&unit, &cmd, p.memory()),
                unit,
                cmd,
            }
        })
        .collect();
    procs.sort_by(|a, b| b.mem.cmp(&a.mem).then(a.pid.cmp(&b.pid)));
    procs
}

/// Extract the innermost systemd unit (e.g. `docker.service`,
/// `app-firefox-1234.scope`) from the contents of `/proc/<pid>/cgroup`.
pub fn unit_from_cgroup(contents: &str) -> String {
    // cgroup v2 line looks like `0::/user.slice/user-1000.slice/user@1000.service/app.slice/foo.service`
    let path = contents
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .or_else(|| {
            contents
                .lines()
                .next()
                .and_then(|l| l.splitn(3, ':').nth(2))
        })
        .unwrap_or("");
    path.rsplit('/')
        .find(|seg| seg.ends_with(".service") || seg.ends_with(".scope"))
        .unwrap_or("-")
        .to_string()
}

/// Case-insensitive match on name, service unit, command line or PID.
pub fn filter<'a>(procs: &'a [ProcInfo], query: &str) -> Vec<&'a ProcInfo> {
    let q = query.to_lowercase();
    procs
        .iter()
        .filter(|p| {
            q.is_empty()
                || p.name.to_lowercase().contains(&q)
                || p.unit.to_lowercase().contains(&q)
                || p.cmd.to_lowercase().contains(&q)
                || p.pid.to_string() == q
        })
        .collect()
}

/// Send SIGTERM (or SIGKILL when `force`) to `pid`.
pub fn kill(pid: u32, force: bool) -> Result<(), String> {
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: kill(2) has no memory-safety preconditions.
    if unsafe { libc::kill(pid as libc::pid_t, sig) } == 0 {
        return Ok(());
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EPERM) => Err(format!("permission denied for PID {pid} (run with sudo)")),
        Some(libc::ESRCH) => Err(format!("PID {pid} no longer exists")),
        _ => Err(std::io::Error::last_os_error().to_string()),
    }
}

pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_service() {
        assert_eq!(
            unit_from_cgroup("0::/system.slice/docker.service\n"),
            "docker.service"
        );
    }

    #[test]
    fn parses_innermost_user_unit() {
        let s =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox@abc.service\n";
        assert_eq!(unit_from_cgroup(s), "app-firefox@abc.service");
    }

    #[test]
    fn parses_scope_and_missing() {
        assert_eq!(
            unit_from_cgroup("0::/user.slice/user-1000.slice/session-2.scope"),
            "session-2.scope"
        );
        assert_eq!(unit_from_cgroup("0::/"), "-");
        assert_eq!(unit_from_cgroup(""), "-");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn filters_by_name_unit_and_pid() {
        let p = |pid, name: &str, unit: &str| ProcInfo {
            pid,
            name: name.into(),
            mem: 0,
            unit: unit.into(),
            cmd: String::new(),
            category: Category::Background,
        };
        let procs = vec![
            p(1, "systemd", "-"),
            p(42, "dockerd", "docker.service"),
            p(7, "firefox", "app.scope"),
        ];
        assert_eq!(filter(&procs, "").len(), 3);
        assert_eq!(filter(&procs, "DOCKER")[0].pid, 42);
        assert_eq!(filter(&procs, "app.scope")[0].pid, 7);
        assert_eq!(filter(&procs, "42")[0].pid, 42);
    }

    #[test]
    fn categorizes() {
        assert_eq!(categorize("-", "", 0), Category::Kernel);
        assert_eq!(
            categorize("app-firefox@x.service", "/usr/lib/firefox", 10),
            Category::App
        );
        assert_eq!(
            categorize("docker.service", "dockerd", 10),
            Category::Background
        );
        assert_eq!(categorize("-", "/sbin/init", 10), Category::Background);
    }

    #[test]
    fn groups_by_unit() {
        let p = |unit: &str, mem| ProcInfo {
            pid: 0,
            name: String::new(),
            mem,
            unit: unit.into(),
            cmd: String::new(),
            category: Category::App,
        };
        let procs = [p("a.service", 10), p("b.service", 50), p("a.service", 45)];
        assert_eq!(
            group_by_unit(&procs),
            vec![("a.service".into(), 55, 2), ("b.service".into(), 50, 1)]
        );
    }
}
