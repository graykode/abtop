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
    entries.flatten()
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

        map.insert(pid, ProcInfo { pid, ppid, rss_kb, cpu_pct, command });
    }
    map
}

#[cfg(not(target_os = "linux"))]
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
                    map.insert(pid, ProcInfo {
                        pid,
                        ppid,
                        rss_kb: rss,
                        cpu_pct: cpu,
                        command,
                    });
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
                if process_info.get(&kid).is_some_and(|p| p.cpu_pct > cpu_threshold) {
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

#[cfg(not(target_os = "linux"))]
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
            let is_tcp_listen =
                parts.len() >= 9 && parts[7] == "TCP" && line.contains("(LISTEN)");
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

/// Check if a command string has a given binary name in executable position.
/// Checks the first two argv tokens only (covers direct invocation and
/// interpreter-wrapped scripts like `node /path/to/codex ...`).
pub fn cmd_has_binary(cmd: &str, name: &str) -> bool {
    let mut tokens = cmd.split_whitespace().take(2);
    tokens.any(|tok| {
        let base = tok.rsplit('/').next().unwrap_or(tok);
        base == name
    })
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
