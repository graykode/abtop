pub mod claude;
pub mod codex;
pub mod process;
pub mod rate_limit;

pub use claude::ClaudeCollector;
pub use codex::CodexCollector;
pub use rate_limit::read_rate_limits;

use crate::model::{AgentSession, OrphanPort, SessionStatus};
use std::collections::HashMap;

/// Process data fetched once per tick and shared across all collectors.
/// Avoids duplicate ps/lsof calls.
pub struct SharedProcessData {
    pub process_info: HashMap<u32, process::ProcInfo>,
    pub children_map: HashMap<u32, Vec<u32>>,
    pub ports: HashMap<u32, Vec<u16>>,
}

impl SharedProcessData {
    /// Fetch process info every tick, but reuse cached ports when `cached_ports` is provided.
    pub fn fetch(cached_ports: Option<&HashMap<u32, Vec<u16>>>) -> Self {
        let process_info = process::get_process_info();
        let children_map = process::get_children_map(&process_info);
        let ports = match cached_ports {
            Some(p) => p.clone(),
            None => process::get_listening_ports(),
        };
        Self { process_info, children_map, ports }
    }
}

/// Info about a child process that owns an open port, tracked for orphan detection.
#[derive(Clone)]
struct TrackedPortChild {
    port: u16,
    command: String,
    project_name: String,
}

/// Aggregates sessions from multiple collectors (Claude, Codex, etc.)
pub struct MultiCollector {
    claude: ClaudeCollector,
    codex: CodexCollector,
    tick_count: u32,
    cached_ports: HashMap<u32, Vec<u16>>,
    /// PID set snapshot from last port scan — invalidate cache when PIDs change.
    cached_port_pids: Vec<u32>,
    cached_git: HashMap<String, (u32, u32)>,
    /// Port-owning children from previous ticks, keyed by child PID.
    /// Used to detect orphans when a session dies.
    tracked_port_children: HashMap<u32, TrackedPortChild>,
    /// Detected orphan ports (updated each tick).
    pub orphan_ports: Vec<OrphanPort>,
}

/// How often to refresh expensive I/O (in ticks). 5 ticks × 2s = 10s.
const SLOW_POLL_INTERVAL: u32 = 5;

impl MultiCollector {
    pub fn new() -> Self {
        Self {
            claude: ClaudeCollector::new(),
            codex: CodexCollector::new(),
            tick_count: SLOW_POLL_INTERVAL, // trigger on first tick
            cached_ports: HashMap::new(),
            cached_port_pids: Vec::new(),
            cached_git: HashMap::new(),
            tracked_port_children: HashMap::new(),
            orphan_ports: Vec::new(),
        }
    }

    /// Get the latest Codex rate limit info (parsed from JSONL token_count events).
    pub fn codex_rate_limit(&self) -> Option<&crate::model::RateLimitInfo> {
        self.codex.last_rate_limit.as_ref()
    }

    pub fn collect(&mut self) -> Vec<AgentSession> {
        let slow_tick = self.tick_count >= SLOW_POLL_INTERVAL;
        if slow_tick {
            self.tick_count = 0;
        }
        self.tick_count += 1;

        // Ports: refresh on slow tick or when the PID set changes (PID reuse safety)
        let fresh_process = SharedProcessData::fetch(Some(&self.cached_ports));
        let mut current_pids: Vec<u32> = fresh_process.process_info.keys().copied().collect();
        current_pids.sort_unstable();
        let pids_changed = current_pids != self.cached_port_pids;

        let shared = if slow_tick || pids_changed {
            let s = SharedProcessData::fetch(None);
            self.cached_ports = s.ports.clone();
            self.cached_port_pids = current_pids;
            s
        } else {
            fresh_process
        };

        let mut all = Vec::new();
        all.extend(self.claude.collect(&shared));
        all.extend(self.codex.collect(&shared));

        // Git stats: refresh only on slow tick
        if slow_tick {
            self.cached_git.clear();
            for s in &mut all {
                let stats = process::collect_git_stats(&s.cwd);
                self.cached_git.insert(s.cwd.clone(), stats);
                s.git_added = stats.0;
                s.git_modified = stats.1;
            }
        } else {
            for s in &mut all {
                if let Some(&(added, modified)) = self.cached_git.get(&s.cwd) {
                    s.git_added = added;
                    s.git_modified = modified;
                } else {
                    // New cwd not yet in cache — compute on demand to avoid false clean
                    let stats = process::collect_git_stats(&s.cwd);
                    self.cached_git.insert(s.cwd.clone(), stats);
                    s.git_added = stats.0;
                    s.git_modified = stats.1;
                }
            }
        }

        // Hide dead Codex sessions (pid==0 is the Codex sentinel for exited process).
        // Claude sessions keep their original PID and are removed when session file disappears.
        all.retain(|s| !(matches!(s.status, SessionStatus::Done) && s.pid == 0));
        all.sort_by_key(|s| std::cmp::Reverse(s.started_at));

        // --- Orphan port detection ---
        // 1. Update tracked port children from live sessions
        let mut live_child_pids = std::collections::HashSet::new();
        for s in &all {
            if !matches!(s.status, SessionStatus::Done) {
                for child in &s.children {
                    live_child_pids.insert(child.pid);
                    if let Some(port) = child.port {
                        self.tracked_port_children.insert(child.pid, TrackedPortChild {
                            port,
                            command: child.command.clone(),
                            project_name: s.project_name.clone(),
                        });
                    }
                }
            }
        }

        // 2. Detect orphans: tracked PIDs that are no longer children of any live session
        //    but are still alive and have an open port
        self.orphan_ports.clear();
        let mut stale_pids = Vec::new();
        for (pid, tracked) in &self.tracked_port_children {
            if live_child_pids.contains(pid) {
                continue; // still owned by a live session
            }
            // Check if process is still alive and still has the port open
            let still_listening = shared.ports.get(pid)
                .is_some_and(|ports| ports.contains(&tracked.port));
            let still_alive = shared.process_info.contains_key(pid);
            if still_alive && still_listening {
                self.orphan_ports.push(OrphanPort {
                    port: tracked.port,
                    pid: *pid,
                    command: tracked.command.clone(),
                    project_name: tracked.project_name.clone(),
                });
            } else {
                stale_pids.push(*pid);
            }
        }
        // Clean up dead tracked entries
        for pid in stale_pids {
            self.tracked_port_children.remove(&pid);
        }
        self.orphan_ports.sort_by_key(|o| o.port);

        all
    }
}
