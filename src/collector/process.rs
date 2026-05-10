use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::process::Command;

#[derive(Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub rss_kb: u64,
    pub cpu_pct: f64,
    pub command: String,
}

/// Resolve all symlinks in /proc/{pid}/fd, returning their targets.
/// Used by both port discovery (socket inodes) and Codex JSONL discovery.
#[cfg(target_os = "linux")]
pub fn scan_proc_fds(pid: u32) -> Vec<std::path::PathBuf> {
    let fd_dir = format!("/proc/{}/fd", pid);
    let entries = match fs::read_dir(&fd_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    entries
        .flatten()
        .filter_map(|e| fs::read_link(e.path()).ok())
        .collect()
}

#[cfg(target_os = "linux")]
pub fn get_process_info() -> HashMap<u32, ProcInfo> {
    let mut map = HashMap::new();

    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;

    let uptime_secs: f64 = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);

    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid: u32 = match name.to_str().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };

        // /proc/{pid}/stat - parse fields after (comm)
        let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // comm can contain spaces/parens, so find last ')'
        let after_comm = match stat.rfind(')') {
            Some(pos) if pos + 2 < stat.len() => &stat[pos + 2..],
            _ => continue,
        };
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // fields[0]=state, [1]=ppid, [11]=utime, [12]=stime, [19]=starttime, [21]=rss
        if fields.len() < 22 {
            continue;
        }
        let ppid: u32 = fields[1].parse().unwrap_or(0);
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        let starttime: u64 = fields[19].parse().unwrap_or(0);
        let rss_pages: u64 = fields[21].parse().unwrap_or(0);

        let rss_kb = rss_pages * page_size / 1024;

        // CPU%: lifetime average (total CPU time / wall time).
        // This differs from ps's instantaneous %CPU but is sufficient for
        // abtop's Working/Waiting threshold (cpu_pct > 1.0). A long-idle
        // process that was busy at startup will show a declining average,
        // eventually dropping below 1.0 as elapsed time grows.
        let uptime_ticks = (uptime_secs * clk_tck) as u64;
        let elapsed_ticks = uptime_ticks.saturating_sub(starttime);
        let cpu_pct = if elapsed_ticks > 0 {
            ((utime + stime) as f64 / elapsed_ticks as f64) * 100.0
        } else {
            0.0
        };

        // /proc/{pid}/cmdline: NUL-separated
        let command = fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ")
            .trim()
            .to_string();
        if command.is_empty() {
            continue; // kernel thread, skip
        }

        map.insert(
            pid,
            ProcInfo {
                pid,
                ppid,
                rss_kb,
                cpu_pct,
                command,
            },
        );
    }
    map
}

#[cfg(target_os = "windows")]
pub fn get_process_info() -> HashMap<u32, ProcInfo> {
    use std::sync::{Mutex, OnceLock};

    // sysinfo's `cpu_usage()` is a delta between two refreshes — a freshly
    // constructed `System` always reports 0. Hold one across calls so the
    // second tick onward returns real CPU%, instead of every Windows process
    // looking idle (which would break `has_active_descendant` and the
    // Working/Waiting threshold downstream).
    static SYS: OnceLock<Mutex<sysinfo::System>> = OnceLock::new();
    let sys_mutex = SYS.get_or_init(|| Mutex::new(sysinfo::System::new()));
    let mut sys = sys_mutex
        .lock()
        .expect("process-info system mutex poisoned");

    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::All,
        true,
        sysinfo::ProcessRefreshKind::new()
            .with_cpu()
            .with_memory()
            .with_cmd(sysinfo::UpdateKind::Always),
    );

    let mut map = HashMap::new();
    for (pid, proc_) in sys.processes() {
        let pid_u32 = pid.as_u32();
        // cmd() can be empty on Windows (cmdline retrieval failed for this
        // process); fall back to the executable name so cmd_has_binary still
        // matches `claude` / `codex` for those processes.
        let command = if proc_.cmd().is_empty() {
            proc_.name().to_string_lossy().into_owned()
        } else {
            proc_
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        };
        if command.is_empty() {
            continue;
        }
        map.insert(
            pid_u32,
            ProcInfo {
                pid: pid_u32,
                ppid: proc_.parent().map(|p| p.as_u32()).unwrap_or(0),
                rss_kb: proc_.memory() / 1024,
                cpu_pct: proc_.cpu_usage() as f64,
                command,
            },
        );
    }
    map
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
pub fn get_process_info() -> HashMap<u32, ProcInfo> {
    let mut map = HashMap::new();
    let output = Command::new("ps")
        .args(["-ww", "-eo", "pid,ppid,rss,%cpu,command"])
        .output()
        .ok();

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                if let (Ok(pid), Ok(ppid), Ok(rss)) = (
                    parts[0].parse::<u32>(),
                    parts[1].parse::<u32>(),
                    parts[2].parse::<u64>(),
                ) {
                    let cpu = parts[3].parse::<f64>().unwrap_or(0.0);
                    let command = parts[4..].join(" ");
                    map.insert(
                        pid,
                        ProcInfo {
                            pid,
                            ppid,
                            rss_kb: rss,
                            cpu_pct: cpu,
                            command,
                        },
                    );
                }
            }
        }
    }
    map
}

pub fn get_children_map(procs: &HashMap<u32, ProcInfo>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in procs.values() {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }
    children
}

/// Walk the ppid chain from `pid` and return true if `ancestor` is reached.
/// Used to identify processes spawned by abtop itself (e.g. `claude --print`
/// summary children) so they can be filtered without dropping unrelated
/// non-interactive sessions started by the user.
pub fn is_descendant_of(pid: u32, ancestor: u32, process_info: &HashMap<u32, ProcInfo>) -> bool {
    if pid == 0 || ancestor == 0 || pid == ancestor {
        return false;
    }
    let mut current = pid;
    let mut visited = std::collections::HashSet::new();
    while visited.insert(current) {
        let Some(info) = process_info.get(&current) else {
            return false;
        };
        if info.ppid == ancestor {
            return true;
        }
        if info.ppid == 0 || info.ppid == 1 {
            return false;
        }
        current = info.ppid;
    }
    false
}

pub fn has_active_descendant(
    pid: u32,
    children_map: &HashMap<u32, Vec<u32>>,
    process_info: &HashMap<u32, ProcInfo>,
    cpu_threshold: f64,
) -> bool {
    let mut stack = vec![pid];
    let mut visited = std::collections::HashSet::new();
    while let Some(p) = stack.pop() {
        if !visited.insert(p) {
            continue;
        }
        if let Some(kids) = children_map.get(&p) {
            for &kid in kids {
                if process_info
                    .get(&kid)
                    .is_some_and(|p| p.cpu_pct > cpu_threshold)
                {
                    return true;
                }
                stack.push(kid);
            }
        }
    }
    false
}

/// On Linux, parse /proc/net/tcp[6] for LISTEN sockets, then match inodes
/// via scan_proc_fds. Only scans FDs for PIDs in `known_pids` (from
/// get_process_info) to avoid scanning all 500+ /proc entries.
#[cfg(target_os = "linux")]
pub fn get_listening_ports() -> HashMap<u32, Vec<u16>> {
    // Step 1: Parse /proc/net/tcp + tcp6 for LISTEN sockets -> inode -> port
    let mut inode_to_port: HashMap<u64, u16> = HashMap::new();
    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() < 10 || fields[3] != "0A" {
                    continue;
                }
                if let Some(port_hex) = fields[1].rsplit(':').next() {
                    if let Ok(port) = u16::from_str_radix(port_hex, 16) {
                        if let Ok(inode) = fields[9].parse::<u64>() {
                            inode_to_port.insert(inode, port);
                        }
                    }
                }
            }
        }
    }

    if inode_to_port.is_empty() {
        return HashMap::new();
    }

    // Step 2: Scan FDs of all PIDs for matching socket inodes.
    // We scan all /proc PIDs rather than just known agent PIDs because
    // child processes (servers, databases) that own ports may not be in
    // the agent PID set but are still relevant for orphan port detection.
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    let proc_entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in proc_entries.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        for target in scan_proc_fds(pid) {
            let target_str = target.to_string_lossy();
            if let Some(inode_str) = target_str
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
            {
                if let Ok(inode) = inode_str.parse::<u64>() {
                    if let Some(&port) = inode_to_port.get(&inode) {
                        map.entry(pid).or_default().push(port);
                    }
                }
            }
        }
    }
    map
}

#[cfg(target_os = "windows")]
pub fn get_listening_ports() -> HashMap<u32, Vec<u16>> {
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    let output = Command::new("netstat")
        .args(["-ano", "-p", "TCP"])
        .output()
        .ok();

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if !line.contains("LISTENING") {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            let local_addr = parts.first();
            let pid_str = parts.last();
            if let (Some(addr), Some(pid_s)) = (local_addr, pid_str) {
                if let (Some(port_str), Ok(pid)) = (addr.rsplit(':').next(), pid_s.parse::<u32>()) {
                    if let Ok(port) = port_str.parse::<u16>() {
                        map.entry(pid).or_default().push(port);
                    }
                }
            }
        }
    }
    map
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
pub fn get_listening_ports() -> HashMap<u32, Vec<u16>> {
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    let output = Command::new("lsof")
        .args(["-i", "-P", "-n", "-sTCP:LISTEN"])
        .output()
        .ok();

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let is_tcp_listen = parts.len() >= 9 && parts[7] == "TCP" && line.contains("(LISTEN)");
            if is_tcp_listen {
                if let Ok(pid) = parts[1].parse::<u32>() {
                    if let Some(addr) = parts.get(8) {
                        if let Some(port_str) = addr.rsplit(':').next() {
                            if let Ok(port) = port_str.parse::<u16>() {
                                map.entry(pid).or_default().push(port);
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

/// Return the last segment of a path-like string. Splits on `/` everywhere
/// plus `\` on Windows, so non-Windows callers don't accidentally treat
/// backslash as a separator (it's a legal filename character on unix).
pub fn last_path_segment(s: &str) -> Option<&str> {
    #[cfg(windows)]
    let segment = s.rsplit(['/', '\\']).next();
    #[cfg(not(windows))]
    let segment = s.rsplit('/').next();
    segment
}

/// Check if a command string has a given binary name in executable position.
/// Checks the first two argv tokens only (covers direct invocation and
/// interpreter-wrapped scripts like `node /path/to/codex ...`).
///
/// Also matches the autoupdater layout used by Claude Code 2.x where the
/// running binary is named after its version (e.g.
/// `~/.local/share/claude/versions/2.1.121`) — basename equality alone would
/// miss this, so we also accept any path of the form `<...>/<name>/versions/<filename>`.
#[cfg(not(windows))]
pub fn cmd_has_binary(cmd: &str, name: &str) -> bool {
    let mut tokens = cmd.split_whitespace().take(2);
    tokens.any(|tok| unix_token_has_binary(tok, name))
}

#[cfg(not(windows))]
pub fn cmd_first_token_has_binary(cmd: &str, name: &str) -> bool {
    cmd.split_whitespace()
        .next()
        .is_some_and(|tok| unix_token_has_binary(tok, name))
}

#[cfg(not(windows))]
fn unix_token_has_binary(tok: &str, name: &str) -> bool {
    let mut iter = tok.rsplit('/');
    let base = iter.next().unwrap_or(tok);
    if base == name {
        return true;
    }
    // Strip .exe suffix for compatibility with claude.exe on non-Windows (e.g. @anthropic-ai/claude-code npm package)
    if let Some(stripped) = base.strip_suffix(".exe") {
        if stripped == name {
            return true;
        }
    }
    matches!((iter.next(), iter.next()), (Some("versions"), Some(parent)) if parent == name)
}

/// Windows variant: checks executable-position tokens, splits on `\`, strips a
/// trailing `.exe` and common script extensions (`.js`, `.sh`, `.py`), and
/// matches case-insensitively.
/// Kept separate from the unix impl so non-Windows matching stays exact
/// (`Claude` must not match `claude` on linux/macOS).
#[cfg(windows)]
pub fn cmd_has_binary(cmd: &str, name: &str) -> bool {
    windows_command_tokens(cmd)
        .into_iter()
        .take(2)
        .any(|tok| windows_token_has_binary(&tok, name))
}

#[cfg(windows)]
pub fn cmd_first_token_has_binary(cmd: &str, name: &str) -> bool {
    windows_command_tokens(cmd)
        .first()
        .is_some_and(|tok| windows_token_has_binary(tok, name))
}

#[cfg(windows)]
fn windows_token_has_binary(tok: &str, name: &str) -> bool {
    let mut iter = tok.rsplit(['/', '\\']);
    let base = iter.next().unwrap_or(tok);
    let base = base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".js"))
        .or_else(|| base.strip_suffix(".sh"))
        .or_else(|| base.strip_suffix(".py"))
        .unwrap_or(base);
    if base.eq_ignore_ascii_case(name) {
        return true;
    }
    matches!(
        (iter.next(), iter.next()),
        (Some(versions), Some(parent))
            if versions.eq_ignore_ascii_case("versions") && parent.eq_ignore_ascii_case(name)
    )
}

#[cfg(windows)]
fn windows_command_tokens(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in cmd.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

pub fn collect_git_stats(cwd: &str) -> (u32, u32) {
    // Validate cwd is an existing directory before running git
    if !std::path::Path::new(cwd).is_dir() {
        return (0, 0);
    }
    let output = Command::new("git")
        .args(["-C", cwd, "status", "--porcelain"])
        .output()
        .ok();

    let mut added = 0u32;
    let mut modified = 0u32;

    if let Some(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.len() < 2 {
                    continue;
                }
                let status_code = &line[..2];
                if status_code.contains('?') || status_code.contains('A') {
                    added += 1;
                } else if status_code.contains('M') {
                    modified += 1;
                }
            }
        }
    }

    (added, modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_has_binary_basename_match() {
        assert!(cmd_has_binary("/usr/local/bin/claude --foo", "claude"));
        assert!(cmd_has_binary("claude", "claude"));
        assert!(!cmd_has_binary("/usr/local/bin/claude-launch", "claude"));
    }

    #[cfg(not(windows))]
    #[test]
    fn cmd_has_binary_exe_suffix_on_unix() {
        // The @anthropic-ai/claude-code npm package ships a binary named
        // `claude.exe` even on macOS/Linux. Ensure we still detect it.
        assert!(cmd_has_binary(
            "/usr/local/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe --session-id abc",
            "claude",
        ));
        assert!(cmd_has_binary("claude.exe", "claude"));
        // Must not match unrelated .exe binaries
        assert!(!cmd_has_binary("/usr/bin/notclaude.exe", "claude"));
    }

    #[test]
    fn cmd_has_binary_autoupdater_layout() {
        // Claude Code 2.x: actual binary is named after its version, but the
        // path has `<name>/versions/<file>` structure we can match on.
        assert!(cmd_has_binary(
            "/Users/a/.local/share/claude/versions/2.1.121 --allow-dangerously-skip-permissions",
            "claude",
        ));
        assert!(cmd_has_binary("/opt/codex/versions/0.42.0 --foo", "codex",));
    }

    #[test]
    fn cmd_has_binary_does_not_overmatch() {
        // A sibling dir under `claude/` but not under `versions/` shouldn't match.
        assert!(!cmd_has_binary(
            "/Users/a/.local/share/claude/foo",
            "claude"
        ));
        // A `versions/` dir not under `<name>/` shouldn't match either.
        assert!(!cmd_has_binary("/some/versions/2.1.121", "claude"));
    }

    #[cfg(windows)]
    #[test]
    fn cmd_has_binary_windows_detects_node_wrapped_codex() {
        assert!(cmd_has_binary(
            r#""C:\Program Files\nodejs\node.exe" C:\Users\GK\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js -m gpt-5.5"#,
            "codex",
        ));
    }

    #[cfg(windows)]
    #[test]
    fn cmd_has_binary_windows_ignores_codex_in_later_args() {
        assert!(!cmd_has_binary(
            r#""C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile "C:\Users\GK\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js""#,
            "codex",
        ));
    }

    fn proc(pid: u32, ppid: u32) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            rss_kb: 0,
            cpu_pct: 0.0,
            command: "x".to_string(),
        }
    }

    #[test]
    fn is_descendant_of_direct_child() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        m.insert(20, proc(20, 10));
        assert!(is_descendant_of(20, 10, &m));
    }

    #[test]
    fn is_descendant_of_walks_chain() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        m.insert(20, proc(20, 10));
        m.insert(30, proc(30, 20));
        assert!(is_descendant_of(30, 10, &m));
    }

    #[test]
    fn is_descendant_of_unrelated_returns_false() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        m.insert(20, proc(20, 1));
        assert!(!is_descendant_of(20, 10, &m));
    }

    #[test]
    fn is_descendant_of_self_returns_false() {
        let mut m = HashMap::new();
        m.insert(10, proc(10, 1));
        assert!(!is_descendant_of(10, 10, &m));
    }

    #[test]
    fn is_descendant_of_zero_ancestor_or_pid_returns_false() {
        let m: HashMap<u32, ProcInfo> = HashMap::new();
        assert!(!is_descendant_of(0, 10, &m));
        assert!(!is_descendant_of(10, 0, &m));
    }

    #[test]
    fn is_descendant_of_handles_cycle() {
        // Synthetic cycle (real /proc shouldn't produce one, but be safe).
        let mut m = HashMap::new();
        m.insert(10, proc(10, 20));
        m.insert(20, proc(20, 10));
        assert!(!is_descendant_of(10, 99, &m));
    }

    #[test]
    fn is_descendant_of_missing_ancestor_in_chain() {
        // ppid points at a PID that no longer exists (parent already exited).
        let mut m = HashMap::new();
        m.insert(20, proc(20, 99));
        assert!(!is_descendant_of(20, 7, &m));
    }
}
