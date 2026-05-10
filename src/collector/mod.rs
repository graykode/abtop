pub mod claude;
pub mod codex;
pub mod mcp;
pub mod opencode;
pub mod process;
pub mod rate_limit;

pub use claude::ClaudeCollector;
pub use codex::CodexCollector;
pub use mcp::McpServer;
pub use opencode::OpenCodeCollector;
pub use rate_limit::read_rate_limits;

/// Redact common secret patterns to avoid displaying credentials in the TUI.
/// Replaces the prefix and all following non-whitespace chars with [REDACTED].
/// Best-effort: covers well-known prefixed tokens, not arbitrary high-entropy strings.
pub(crate) fn redact_secrets(s: &str) -> String {
    const PATTERNS: &[&str] = &[
        // Anthropic / OpenAI / OpenRouter
        "sk-ant-",
        "sk-proj-",
        "sk-or-",
        // Stripe
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "rk_test_",
        // GitHub
        "ghp_",
        "gho_",
        "ghs_",
        "ghr_",
        "ghu_",
        "github_pat_",
        // GitLab
        "glpat-",
        // Slack
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxs-",
        // AWS access key id
        "AKIA",
        "ASIA",
        // Bearer-prefixed headers
        "Bearer ",
    ];
    let mut result = s.to_string();
    for pat in PATTERNS {
        while let Some(pos) = result.find(pat) {
            let end = result[pos..]
                .find(char::is_whitespace)
                .map(|i| pos + i)
                .unwrap_or(result.len());
            result.replace_range(pos..end, "[REDACTED]");
        }
    }
    result
}

use crate::model::{AgentSession, OrphanPort, RateLimitInfo, SessionStatus};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Trait for agent-specific session collectors.
/// Implement this to add support for a new AI coding agent.
pub trait AgentCollector {
    /// Return all live sessions for this agent type.
    fn collect(&mut self, shared: &SharedProcessData) -> Vec<AgentSession>;

    /// Return agent-specific rate limit info, if available from session data.
    fn live_rate_limit(&self) -> Option<RateLimitInfo> {
        None
    }

    /// Return config directories discovered from running agent processes.
    /// Used to feed rate limit lookups across all active config dirs.
    fn discovered_config_dirs(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

/// Process data fetched once per tick and shared across all collectors.
/// Avoids duplicate ps/lsof calls.
pub struct SharedProcessData {
    pub process_info: HashMap<u32, process::ProcInfo>,
    pub children_map: HashMap<u32, Vec<u32>>,
    pub ports: HashMap<u32, Vec<u16>>,
    /// True on slow poll ticks (every 5 ticks ≈ 10s). Collectors should
    /// defer expensive discovery (e.g. /proc reads) to slow ticks.
    pub slow_tick: bool,
    /// PIDs of detected codex mcp-server processes. Populated by
    /// `MultiCollector` after McpDetection runs; CodexCollector
    /// excludes these so a single mcp-server PID isn't double-counted
    /// in the sessions panel.
    pub mcp_server_pids: HashSet<u32>,
    /// Rollout file paths held open by an mcp-server process. The
    /// CodexCollector "recently finished" pass skips these to avoid
    /// PID=0 ghost rows for threads that the mcp-server is still
    /// holding fds for.
    pub mcp_owned_rollouts: HashSet<PathBuf>,
    /// When false, the suppression sets above are empty so the
    /// sessions panel restores upstream behavior. Driven by the user
    /// toggle (Shift+M).
    pub mcp_suppress: bool,
}

impl SharedProcessData {
    /// Fetch process info every tick, but reuse cached ports when `cached_ports` is provided.
    pub fn fetch(cached_ports: Option<&HashMap<u32, Vec<u16>>>, slow_tick: bool) -> Self {
        let process_info = process::get_process_info();
        let children_map = process::get_children_map(&process_info);
        let ports = match cached_ports {
            Some(p) => p.clone(),
            None => process::get_listening_ports(),
        };
        Self {
            process_info,
            children_map,
            ports,
            slow_tick,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
        }
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
    collectors: Vec<Box<dyn AgentCollector>>,
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
    /// MCP servers (codex mcp-server) detected on the most recent tick.
    pub mcp_servers: Vec<McpServer>,
    /// Whether to hide mcp-server-owned rollouts from the sessions
    /// panel. When `false`, sessions panel reverts to upstream
    /// behavior (mcp-server PIDs and their rollouts appear there too,
    /// with the existing 1-of-N HashMap-overwrite caveat).
    pub mcp_suppress: bool,
}

/// How often to refresh expensive I/O (in ticks). 5 ticks × 2s = 10s.
const SLOW_POLL_INTERVAL: u32 = 5;

impl MultiCollector {
    /// Build a collector, skipping agents whose identifier is in `hidden`.
    /// Identifiers are matched case-insensitively against each collector's
    /// `agent_cli` name (e.g. `"claude"`, `"codex"`).
    pub fn with_hidden(hidden: &[String]) -> Self {
        let is_hidden = |name: &str| hidden.iter().any(|h| h.eq_ignore_ascii_case(name));
        let mut collectors: Vec<Box<dyn AgentCollector>> = Vec::new();
        if !is_hidden("claude") {
            collectors.push(Box::new(ClaudeCollector::new()));
        }
        if !is_hidden("codex") {
            collectors.push(Box::new(CodexCollector::new()));
        }
        if !is_hidden("opencode") {
            collectors.push(Box::new(OpenCodeCollector::new()));
        }
        Self {
            collectors,
            tick_count: SLOW_POLL_INTERVAL, // trigger on first tick
            cached_ports: HashMap::new(),
            cached_port_pids: Vec::new(),
            cached_git: HashMap::new(),
            tracked_port_children: HashMap::new(),
            orphan_ports: Vec::new(),
            mcp_servers: Vec::new(),
            mcp_suppress: true,
        }
    }

    pub fn set_mcp_suppress(&mut self, on: bool) {
        self.mcp_suppress = on;
    }

    /// Collect rate limit info from all registered collectors.
    pub fn agent_rate_limits(&self) -> Vec<RateLimitInfo> {
        self.collectors
            .iter()
            .filter_map(|c| c.live_rate_limit())
            .collect()
    }

    /// Return all config directories discovered across all collectors.
    pub fn all_config_dirs(&self) -> Vec<std::path::PathBuf> {
        self.collectors
            .iter()
            .flat_map(|c| c.discovered_config_dirs())
            .collect()
    }

    pub fn collect(&mut self) -> Vec<AgentSession> {
        let slow_tick = self.tick_count >= SLOW_POLL_INTERVAL;
        if slow_tick {
            self.tick_count = 0;
        }
        self.tick_count += 1;

        // Ports: refresh on slow tick or when the PID set changes (PID reuse safety)
        let fresh_process = SharedProcessData::fetch(Some(&self.cached_ports), slow_tick);
        let mut current_pids: Vec<u32> = fresh_process.process_info.keys().copied().collect();
        current_pids.sort_unstable();
        let pids_changed = current_pids != self.cached_port_pids;

        let mut shared = if slow_tick || pids_changed {
            let s = SharedProcessData::fetch(None, slow_tick);
            self.cached_ports = s.ports.clone();
            self.cached_port_pids = current_pids;
            s
        } else {
            fresh_process
        };

        // Detect MCP servers and stash the suppression sets in `shared`
        // so CodexCollector can avoid double-counting their rollouts.
        let detection = mcp::detect(&shared.process_info);
        self.mcp_servers = detection.servers;
        shared.mcp_suppress = self.mcp_suppress;
        if self.mcp_suppress {
            shared.mcp_server_pids = detection.server_pids;
            shared.mcp_owned_rollouts = detection.owned_rollouts;
        }

        let mut all = Vec::new();
        for collector in &mut self.collectors {
            all.extend(collector.collect(&shared));
        }

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

        // Hide dead sessions: Codex uses pid==0 sentinel, Claude is filtered in collect().
        all.retain(|s| !matches!(s.status, SessionStatus::Done));
        all.sort_by_key(|s| std::cmp::Reverse(s.started_at));

        // --- Orphan port detection ---
        // 1. Update tracked port children from live sessions
        let mut live_child_pids = std::collections::HashSet::new();
        for s in &all {
            if !matches!(s.status, SessionStatus::Done) {
                for child in &s.children {
                    live_child_pids.insert(child.pid);
                    if let Some(port) = child.port {
                        self.tracked_port_children.insert(
                            child.pid,
                            TrackedPortChild {
                                port,
                                command: child.command.clone(),
                                project_name: s.project_name.clone(),
                            },
                        );
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
            let still_listening = shared
                .ports
                .get(pid)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_hidden_empty_keeps_all_collectors() {
        let mc = MultiCollector::with_hidden(&[]);
        assert_eq!(mc.collectors.len(), 3);
    }

    #[test]
    fn with_hidden_codex_drops_codex_only() {
        let mc = MultiCollector::with_hidden(&["codex".to_string()]);
        assert_eq!(mc.collectors.len(), 2);
    }

    #[test]
    fn with_hidden_is_case_insensitive() {
        let mc = MultiCollector::with_hidden(&["CODEX".to_string()]);
        assert_eq!(mc.collectors.len(), 2);
        let mc = MultiCollector::with_hidden(&["Claude".to_string()]);
        assert_eq!(mc.collectors.len(), 2);
    }

    #[test]
    fn with_hidden_unknown_names_are_ignored() {
        let mc = MultiCollector::with_hidden(&["kiro".to_string(), "gemini".to_string()]);
        assert_eq!(mc.collectors.len(), 3);
    }

    #[test]
    fn with_hidden_all_agents_yields_empty() {
        let mc = MultiCollector::with_hidden(&[
            "claude".to_string(),
            "codex".to_string(),
            "opencode".to_string(),
        ]);
        assert!(mc.collectors.is_empty());
    }
}
