use std::collections::HashMap;
use std::process::Command;

#[derive(Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub rss_kb: u64,
    pub cpu_pct: f64,
    /// Best-effort process start time derived from `ps etimes` (`now - etimes`).
    pub started_at_ms: u64,
    pub command: String,
}

pub fn get_process_info() -> HashMap<u32, ProcInfo> {
    let mut map = HashMap::new();

    // Linux supports `etimes` (elapsed seconds). macOS does not, so fall back to
    // `etime` (formatted elapsed time) and parse it ourselves.
    let output = Command::new("ps")
        .args(["-ww", "-eo", "pid,ppid,rss,%cpu,etimes,command"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .or_else(|| {
            Command::new("ps")
                .args(["-ww", "-eo", "pid,ppid,rss,%cpu,etime,command"])
                .output()
                .ok()
                .filter(|o| o.status.success())
        });

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        for line in stdout.lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 6 {
                if let (Ok(pid), Ok(ppid), Ok(rss)) = (
                    parts[0].parse::<u32>(),
                    parts[1].parse::<u32>(),
                    parts[2].parse::<u64>(),
                ) {
                    let cpu = parts[3].parse::<f64>().unwrap_or(0.0);
                    let elapsed_secs = parse_elapsed_to_secs(parts[4]);
                    let started_at_ms = now_ms.saturating_sub(elapsed_secs.saturating_mul(1000));
                    let command = parts[5..].join(" ");
                    map.insert(pid, ProcInfo {
                        pid,
                        ppid,
                        rss_kb: rss,
                        cpu_pct: cpu,
                        started_at_ms,
                        command,
                    });
                }
            }
        }
    }
    map
}

fn parse_elapsed_to_secs(s: &str) -> u64 {
    if let Ok(secs) = s.parse::<u64>() {
        return secs;
    }

    let (days, rest) = if let Some((d, rest)) = s.split_once('-') {
        (d.parse::<u64>().unwrap_or(0), rest)
    } else {
        (0, s)
    };

    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect();

    let time_secs = match parts.as_slice() {
        [h, m, s] => h.saturating_mul(3600) + m.saturating_mul(60) + s,
        [m, s] => m.saturating_mul(60) + s,
        [s] => *s,
        _ => 0,
    };

    days.saturating_mul(86_400) + time_secs
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
            let is_tcp_listen = parts.len() >= 9
                && parts[7] == "TCP"
                && line.contains("(LISTEN)");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_elapsed_to_secs() {
        assert_eq!(parse_elapsed_to_secs("42"), 42);
        assert_eq!(parse_elapsed_to_secs("01:02"), 62);
        assert_eq!(parse_elapsed_to_secs("01:02:03"), 3723);
        assert_eq!(parse_elapsed_to_secs("2-01:02:03"), 176_523);
    }
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
