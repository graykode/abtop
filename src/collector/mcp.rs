use super::process::{self, ProcInfo};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
use std::process::{Command, Stdio};
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
use std::time::Instant;
use std::time::{Duration, SystemTime};

/// Active-thread mtime threshold: a rollout written within the last 30 minutes
/// ACTIVE_MTIME_SECS counts as "active". File-descriptor presence alone
/// would overcount — `codex mcp-server` keeps fds open for hours after
/// a thread last wrote (so it can resume on demand), so we need a
/// freshness signal in addition to fd presence.
pub const ACTIVE_MTIME_SECS: u64 = 30 * 60;

/// One open `rollout-*.jsonl` fd held by an mcp-server process.
#[derive(Clone, Debug)]
pub struct McpRollout {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
    /// Carried for debug / future panel use; not currently rendered.
    #[allow(dead_code)]
    pub size_bytes: u64,
}

impl McpRollout {
    pub fn is_active(&self, now: SystemTime, threshold_secs: u64) -> bool {
        match self.mtime {
            Some(m) => now
                .duration_since(m)
                .map(|d| d.as_secs() < threshold_secs)
                .unwrap_or(false),
            None => false,
        }
    }
}

/// One running MCP server process. Currently only `codex mcp-server`;
/// other MCP server flavors can be added by extending the detection in
/// `is_codex_mcp_server`.
#[derive(Clone, Debug)]
pub struct McpServer {
    pub pid: u32,
    /// Parent process PID — kept for debug; not currently rendered.
    #[allow(dead_code)]
    pub ppid: u32,
    /// Resolved CLI of the parent process: "claude", "codex", or "?".
    pub parent_cli: &'static str,
    /// Full ps command — kept for debug; not currently rendered.
    #[allow(dead_code)]
    pub command: String,
    /// Value of `-c profile=<name>` if present (e.g. "qwen36-litellm").
    /// `None` for the default profile.
    pub profile: Option<String>,
    /// RSS in KB — kept for debug; not currently rendered.
    #[allow(dead_code)]
    pub mem_kb: u64,
    pub rollouts: Vec<McpRollout>,
}

impl McpServer {
    pub fn active_count(&self, now: SystemTime, threshold_secs: u64) -> usize {
        self.rollouts
            .iter()
            .filter(|r| r.is_active(now, threshold_secs))
            .count()
    }

    pub fn latest_mtime(&self) -> Option<SystemTime> {
        self.rollouts.iter().filter_map(|r| r.mtime).max()
    }
}

/// Result of one detection pass — kept as a struct so callers can mutate
/// a `SharedProcessData` with a single method.
pub struct McpDetection {
    pub servers: Vec<McpServer>,
    /// PIDs of detected mcp-server processes. CodexCollector excludes
    /// these so the same rollout isn't double-counted in the sessions
    /// panel.
    pub server_pids: HashSet<u32>,
    /// Rollout file paths currently held open by an mcp-server process.
    /// CodexCollector's "recently finished" pass skips these to avoid
    /// the PID=0 "ghost Done" rows.
    pub owned_rollouts: HashSet<PathBuf>,
}

impl McpDetection {
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
            server_pids: HashSet::new(),
            owned_rollouts: HashSet::new(),
        }
    }
}

/// Detect codex mcp-server processes from the shared `ps` snapshot,
/// then map each PID to its full set of open rollout fds.
pub fn detect(process_info: &HashMap<u32, ProcInfo>) -> McpDetection {
    let server_candidates: Vec<&ProcInfo> = process_info
        .values()
        .filter(|info| is_codex_mcp_server(&info.command))
        .collect();

    if server_candidates.is_empty() {
        return McpDetection::empty();
    }

    let pids: Vec<u32> = server_candidates.iter().map(|p| p.pid).collect();
    let pid_to_rollouts = map_pid_to_rollouts(&pids);

    let mut servers = Vec::with_capacity(server_candidates.len());
    let mut owned_rollouts: HashSet<PathBuf> = HashSet::new();
    let mut server_pids: HashSet<u32> = HashSet::new();

    for info in server_candidates {
        let parent_cli = resolve_parent_cli(info.ppid, process_info);
        let profile = parse_profile_flag(&info.command);
        let mut rollouts: Vec<McpRollout> = pid_to_rollouts
            .get(&info.pid)
            .map(|paths| paths.iter().map(rollout_for_path).collect())
            .unwrap_or_default();
        rollouts.sort_by_key(|r| std::cmp::Reverse(r.mtime));

        for r in &rollouts {
            owned_rollouts.insert(r.path.clone());
        }
        server_pids.insert(info.pid);

        servers.push(McpServer {
            pid: info.pid,
            ppid: info.ppid,
            parent_cli,
            command: info.command.clone(),
            profile,
            mem_kb: info.rss_kb,
            rollouts,
        });
    }

    servers.sort_by_key(|s| (s.parent_cli, s.pid));

    McpDetection {
        servers,
        server_pids,
        owned_rollouts,
    }
}

/// True when `cmd` is a `codex mcp-server [...]` invocation.
fn is_codex_mcp_server(cmd: &str) -> bool {
    process::cmd_has_binary(cmd, "codex")
        && cmd.contains("mcp-server")
        && !cmd.contains("grep")
        && !cmd.contains("app-server")
}

/// Pick the parent CLI name from the parent's command line, returning
/// the static label used by the sessions panel.
fn resolve_parent_cli(ppid: u32, process_info: &HashMap<u32, ProcInfo>) -> &'static str {
    let Some(parent) = process_info.get(&ppid) else {
        return "?";
    };
    let cmd = &parent.command;
    if process::cmd_has_binary(cmd, "claude") {
        "claude"
    } else if process::cmd_has_binary(cmd, "codex") {
        "codex"
    } else {
        "?"
    }
}

/// Extract `-c profile=<name>` if present. Codex accepts either
/// `-c profile=NAME` (one arg) or `-c` `profile=NAME` (two args). The
/// substring match handles both since both produce contiguous bytes
/// in `ps`-style output.
fn parse_profile_flag(cmd: &str) -> Option<String> {
    let needle = "profile=";
    let pos = cmd.find(needle)?;
    let tail = &cmd[pos + needle.len()..];
    let end = tail.find(|c: char| c.is_whitespace()).unwrap_or(tail.len());
    let value = tail[..end].trim_matches(|c: char| c == '"' || c == '\'');
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn rollout_for_path(path: &PathBuf) -> McpRollout {
    let (mtime, size_bytes) = match std::fs::metadata(path) {
        Ok(meta) => (meta.modified().ok(), meta.len()),
        Err(_) => (None, 0),
    };
    McpRollout {
        path: path.clone(),
        mtime,
        size_bytes,
    }
}

/// Map mcp-server PIDs to all their open `rollout-*.jsonl` paths.
/// Returns one Vec per PID — preserves the multi-rollout fact rather
/// than the single-PathBuf overwrite the existing CodexCollector path
/// uses (that is intentional in CodexCollector — see issue notes —
/// since fixing it without this MCP panel would flood the sessions
/// panel with phantom rows).
pub(crate) fn map_pid_to_rollouts(pids: &[u32]) -> HashMap<u32, Vec<PathBuf>> {
    let mut map: HashMap<u32, Vec<PathBuf>> = HashMap::new();
    if pids.is_empty() {
        return map;
    }

    #[cfg(target_os = "linux")]
    {
        for &pid in pids {
            for target in process::scan_proc_fds(pid) {
                if is_rollout_path(&target) {
                    map.entry(pid).or_default().push(target);
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        let mut sys = sysinfo::System::new();
        let pids_sys: Vec<sysinfo::Pid> = pids
            .iter()
            .copied()
            .map(|p| sysinfo::Pid::from(p as usize))
            .collect();
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&pids_sys),
            true,
            sysinfo::ProcessRefreshKind::new().with_memory(),
        );
        for &pid_u32 in pids {
            let pid = sysinfo::Pid::from(pid_u32 as usize);
            if let Some(proc_) = sys.process(pid) {
                if let Some(cwd) = proc_.cwd() {
                    if let Ok(entries) = std::fs::read_dir(cwd) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if is_rollout_path(&p) {
                                map.entry(pid_u32).or_default().push(p);
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    {
        let pid_args: Vec<String> = pids.iter().map(|p| format!("-p{}", p)).collect();
        let mut args = vec!["-F", "pn"];
        for pa in &pid_args {
            args.push(pa);
        }
        let output = Command::new("lsof").args(&args).output().ok();
        if let Some(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            map = parse_lsof_rollout_output(&stdout);
        }
    }

    map
}

pub(crate) fn map_pid_to_rollouts_with_timeout_and_pid_slot(
    pids: &[u32],
    timeout: Duration,
    child_pid_slot: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
) -> Option<HashMap<u32, Vec<PathBuf>>> {
    if pids.is_empty() {
        return Some(HashMap::new());
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        let _ = (timeout, child_pid_slot);
        Some(map_pid_to_rollouts(pids))
    }

    #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
    {
        let pid_args: Vec<String> = pids.iter().map(|p| format!("-p{}", p)).collect();
        let mut args = vec!["-F", "pn"];
        for pa in &pid_args {
            args.push(pa);
        }

        let output_file = tempfile::NamedTempFile::new().ok()?;
        let output_for_child = output_file.reopen().ok()?;
        let mut child = match Command::new("lsof")
            .args(&args)
            .stdout(Stdio::from(output_for_child))
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return None,
        };
        let child_pid = child.id();
        if let Some(slot) = &child_pid_slot {
            slot.store(child_pid, std::sync::atomic::Ordering::SeqCst);
        }

        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    if let Some(slot) = &child_pid_slot {
                        slot.store(0, std::sync::atomic::Ordering::SeqCst);
                    }
                    let stdout = std::fs::read_to_string(output_file.path()).ok();
                    return stdout.map(|s| parse_lsof_rollout_output(&s));
                }
                Ok(None) if started.elapsed() >= timeout => {
                    if let Some(slot) = &child_pid_slot {
                        slot.store(0, std::sync::atomic::Ordering::SeqCst);
                    }
                    let _ = Command::new("kill")
                        .args(["-9", &child_pid.to_string()])
                        .status();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => {
                    if let Some(slot) = &child_pid_slot {
                        slot.store(0, std::sync::atomic::Ordering::SeqCst);
                    }
                    return None;
                }
            }
        }
    }
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
fn kill_pid(pid: u32) {
    if pid != 0 {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn kill_pid(_pid: u32) {}

pub(crate) fn kill_rollout_scan_child(pid: u32) {
    kill_pid(pid);
}

#[cfg(any(test, all(not(target_os = "linux"), not(target_os = "windows"))))]
pub(crate) fn parse_lsof_rollout_output(stdout: &str) -> HashMap<u32, Vec<PathBuf>> {
    let mut map: HashMap<u32, Vec<PathBuf>> = HashMap::new();
    let mut current_pid: Option<u32> = None;
    for line in stdout.lines() {
        if let Some(pid_str) = line.strip_prefix('p') {
            current_pid = pid_str.parse::<u32>().ok();
        } else if let Some(name) = line.strip_prefix('n') {
            if let Some(pid) = current_pid {
                let path = PathBuf::from(name);
                if is_rollout_path(&path) {
                    map.entry(pid).or_default().push(path);
                }
            }
        }
    }
    map
}

fn is_rollout_path(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, command: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            rss_kb: 0,
            cpu_pct: 0.0,
            command: command.to_string(),
        }
    }

    #[test]
    fn detects_codex_mcp_server_default_profile() {
        let mut info = HashMap::new();
        info.insert(100, proc(100, 50, "codex mcp-server"));
        info.insert(50, proc(50, 1, "/usr/local/bin/claude --foo"));
        let det = detect(&info);
        assert_eq!(det.servers.len(), 1);
        assert_eq!(det.servers[0].pid, 100);
        assert_eq!(det.servers[0].parent_cli, "claude");
        assert!(det.servers[0].profile.is_none());
    }

    #[test]
    fn parses_profile_flag() {
        let mut info = HashMap::new();
        info.insert(
            101,
            proc(101, 50, "codex mcp-server -c profile=qwen36-litellm"),
        );
        info.insert(50, proc(50, 1, "claude"));
        let det = detect(&info);
        assert_eq!(det.servers.len(), 1);
        assert_eq!(det.servers[0].profile.as_deref(), Some("qwen36-litellm"));
    }

    #[test]
    fn parent_cli_unknown_when_ppid_missing() {
        let mut info = HashMap::new();
        info.insert(102, proc(102, 999, "codex mcp-server"));
        let det = detect(&info);
        assert_eq!(det.servers[0].parent_cli, "?");
    }

    #[test]
    fn ignores_non_mcp_codex_processes() {
        let mut info = HashMap::new();
        info.insert(103, proc(103, 1, "codex"));
        info.insert(104, proc(104, 1, "codex exec something"));
        info.insert(105, proc(105, 1, "/path/to/codex --resume xyz"));
        let det = detect(&info);
        assert!(det.servers.is_empty());
    }

    #[test]
    fn ignores_non_codex_mcp_servers() {
        let mut info = HashMap::new();
        // claude has its own `mcp serve` that we don't want to pick up here.
        info.insert(106, proc(106, 1, "/path/to/claude mcp serve"));
        let det = detect(&info);
        assert!(det.servers.is_empty());
    }

    #[test]
    fn rollout_active_threshold_excludes_old_mtime() {
        let now = SystemTime::now();
        let stale = McpRollout {
            path: PathBuf::from("/x"),
            mtime: Some(now - std::time::Duration::from_secs(31 * 60)),
            size_bytes: 0,
        };
        let fresh = McpRollout {
            path: PathBuf::from("/y"),
            mtime: Some(now - std::time::Duration::from_secs(5)),
            size_bytes: 0,
        };
        assert!(!stale.is_active(now, ACTIVE_MTIME_SECS));
        assert!(fresh.is_active(now, ACTIVE_MTIME_SECS));
    }

    #[test]
    fn parses_lsof_rollout_output_for_multiple_pids_and_paths() {
        let stdout = "\
p100
n/Users/me/.codex/sessions/2026/05/29/rollout-a.jsonl
n/Users/me/.codex/sessions/2026/05/29/not-a-rollout.txt
p200
n/Users/me/.codex/sessions/2026/05/29/rollout-b.jsonl
n/Users/me/.codex/sessions/2026/05/29/rollout-c.jsonl
";

        let parsed = parse_lsof_rollout_output(stdout);

        assert_eq!(
            parsed.get(&100),
            Some(&vec![PathBuf::from(
                "/Users/me/.codex/sessions/2026/05/29/rollout-a.jsonl"
            )])
        );
        assert_eq!(
            parsed.get(&200),
            Some(&vec![
                PathBuf::from("/Users/me/.codex/sessions/2026/05/29/rollout-b.jsonl"),
                PathBuf::from("/Users/me/.codex/sessions/2026/05/29/rollout-c.jsonl"),
            ])
        );
    }
}
