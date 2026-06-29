//! Serializable snapshot of live monitor state for the JSON / Web API.
//!
//! Builds an owned, JSON-friendly view from an [`App`] so headless consumers
//! (e.g. a web server) can serialize the same data the TUI renders without
//! depending on ratatui. The list fields stay lean; a bounded tail of the
//! richer per-session fields (token history, recent tool calls, chat tail,
//! subagents) is also included for the detail view. The unbounded file-access
//! audit and full transcripts are still omitted to keep the payload small.
//!
//! This is a pure read: [`App::to_snapshot`] never ticks or spawns anything.
//! Call it after [`App::tick_no_summaries`] (or `tick`) on a background thread.

use crate::app::App;
use crate::collector::mcp::ACTIVE_MTIME_SECS;
use crate::host_info::{AgentAggregate, HostMetrics};
use crate::model::{
    ChatRole, ChildProcess, OrphanPort, RateLimitInfo, SessionStatus, MAX_CHAT_MESSAGES,
};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

/// Top-level snapshot returned by [`App::to_snapshot`].
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Unix-epoch milliseconds when this snapshot was built.
    pub generated_at_ms: u64,
    /// Host vitals (CPU / mem / load1). `None` on unsupported platforms or
    /// before the first valid sample.
    pub host: Option<HostMetrics>,
    /// Aggregate metrics across all sessions.
    pub aggregate: AgentAggregate,
    /// Most recent per-tick token rate: the delta of *active* tokens, where
    /// active = input + output + cache_create (cache_read is excluded to avoid
    /// inflated rates). It therefore will NOT equal successive `total_tokens`
    /// diffs (which include cache_read). `0.0` on the first tick of a fresh
    /// process (no prior totals to diff against).
    pub token_rate: f64,
    /// Collector tick interval in milliseconds. Divide `token_rate` by
    /// `interval_ms / 1000` for a per-second rate.
    pub interval_ms: u64,
    /// Live agent sessions, newest first (same order as the TUI).
    pub sessions: Vec<SessionView>,
    /// Account-level rate limits (Claude, Codex, …).
    pub rate_limits: Vec<RateLimitInfo>,
    /// Ports left open by processes whose parent session has ended. Empty on a
    /// one-shot snapshot — orphan detection needs cross-tick history, so it
    /// only populates for a long-running monitor.
    pub orphan_ports: Vec<OrphanPort>,
    /// Detected MCP servers (currently `codex mcp-server`).
    pub mcp_servers: Vec<McpServerView>,
}

/// One chat line from the transcript tail (detail view only).
#[derive(Debug, Clone, Serialize)]
pub struct ChatMsgView {
    /// Speaker: the string `"user"` or `"assistant"` (stable wire values).
    pub role: &'static str,
    /// Redacted message text (tool inputs/results are excluded upstream).
    pub text: String,
}

/// One tool invocation (detail view only).
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallView {
    /// Tool name, e.g. `"Read"`, `"Edit"`, `"Bash"`, `"Grep"`, `"Agent"`.
    pub name: String,
    /// Short argument preview (file path, command prefix, or pattern).
    pub arg: String,
    /// Observed duration in milliseconds; `0` when unknown.
    pub duration_ms: u64,
}

/// One spawned subagent (detail view only).
#[derive(Debug, Clone, Serialize)]
pub struct SubAgentView {
    /// Subagent name/label.
    pub name: String,
    /// Free-text status reported for the subagent (e.g. `"working"`, `"done"`).
    pub status: String,
    /// Tokens attributed to this subagent.
    pub tokens: u64,
}

/// A single session, flattened and curated for JSON consumers.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    /// Owning CLI: "claude", "codex", "opencode".
    pub agent_cli: &'static str,
    /// OS process id of the agent CLI for this session.
    pub pid: u32,
    /// Agent-assigned session identifier (stable for the life of the session).
    pub session_id: String,
    /// Project / workspace name (usually the basename of `cwd`).
    pub project_name: String,
    /// Absolute working directory of the session.
    pub cwd: String,
    /// Home-abbreviated config root (e.g. "~/.claude", "~/.codex").
    pub config_root: String,
    /// Coarse activity state; serializes as its variant name (e.g. `"Thinking"`).
    pub status: SessionStatus,
    /// Model identifier reported by the session (e.g. `"claude-opus-4-6"`).
    pub model: String,
    /// Reasoning effort (Codex only); empty when N/A.
    pub effort: String,
    /// Agent CLI version string, if known.
    pub version: String,
    /// Context-window fill, 0.0–100.0 percent.
    pub context_percent: f64,
    /// Total context-window size in tokens (e.g. 200000).
    pub context_window: u64,
    /// All token classes summed: input + output + cache read + cache write.
    pub total_tokens: u64,
    /// Cumulative input (prompt) tokens for the session.
    pub input_tokens: u64,
    /// Cumulative output (completion) tokens for the session.
    pub output_tokens: u64,
    /// Cumulative cache-read tokens (excluded from the active-token rate).
    pub cache_read_tokens: u64,
    /// Cumulative cache-write (cache-creation) tokens.
    pub cache_create_tokens: u64,
    /// Number of user/assistant turns observed.
    pub turn_count: u32,
    /// Resident memory of the session process tree, in MiB.
    pub mem_mb: u64,
    /// Current git branch of `cwd`, or empty when not a repo.
    pub git_branch: String,
    /// Files added in the working tree (git status), not session-scoped.
    pub git_added: u32,
    /// Files modified in the working tree (git status), not session-scoped.
    pub git_modified: u32,
    /// Session start, Unix-epoch milliseconds.
    pub started_at_ms: u64,
    /// Wall-clock seconds since `started_at_ms`.
    pub elapsed_secs: u64,
    /// Display summary: cached LLM title if present, else a safe raw-prompt
    /// fallback. Never triggers summary generation.
    pub summary: String,
    /// Most recent current-task line, if any.
    pub current_task: Option<String>,
    /// Child processes, each with any owned listening port.
    pub children: Vec<ChildProcess>,
    // --- richer fields for the per-session detail view ---
    /// Number of detected context-compaction events.
    pub compaction_count: u32,
    /// Per-turn token totals for a sparkline (trimmed tail). The absolute scale
    /// differs by agent (Claude counts cache tokens, Codex does not), so use it
    /// as a relative per-session trend, not for cross-session magnitude.
    pub token_history: Vec<u64>,
    /// Spawned subagents, if any.
    pub subagents: Vec<SubAgentView>,
    /// Recent tool-call timeline (trimmed tail, newest last).
    pub tool_calls: Vec<ToolCallView>,
    /// Recent chat transcript tail (user/assistant only).
    pub chat_messages: Vec<ChatMsgView>,
}

/// A detected MCP server, with the internal `SystemTime` mtime resolved to a
/// plain epoch-millis number for web clients.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerView {
    /// OS process id of the MCP server.
    pub pid: u32,
    /// Resolved parent CLI: "claude", "codex", or "?".
    pub parent_cli: &'static str,
    /// `-c profile=<name>` value, if any.
    pub profile: Option<String>,
    /// Resident memory of the MCP server process, in KiB.
    pub mem_kb: u64,
    /// Rollouts written within the active-mtime window.
    pub active_count: usize,
    /// Total open rollout fds.
    pub rollout_count: usize,
    /// Latest rollout mtime as Unix-epoch milliseconds, if known.
    pub last_activity_ms: Option<u64>,
}

/// Keep at most the last `n` items of a slice.
fn tail<T: Clone>(v: &[T], n: usize) -> Vec<T> {
    if v.len() > n {
        v[v.len() - n..].to_vec()
    } else {
        v.to_vec()
    }
}

fn epoch_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

impl App {
    /// Build an owned, JSON-serializable snapshot of the current monitor state.
    ///
    /// Pure read — does not tick or spawn anything. Intended flow for a web
    /// server: lock the `App`, `tick_no_summaries()`, `to_snapshot()`, release.
    pub fn to_snapshot(&self, interval_ms: u64) -> Snapshot {
        let now = SystemTime::now();

        let sessions = self
            .sessions
            .iter()
            .map(|s| SessionView {
                agent_cli: s.agent_cli,
                pid: s.pid,
                session_id: s.session_id.clone(),
                project_name: s.project_name.clone(),
                cwd: s.cwd.clone(),
                config_root: s.config_root.clone(),
                status: s.status.clone(),
                model: s.model.clone(),
                effort: s.effort.clone(),
                version: s.version.clone(),
                context_percent: s.context_percent,
                context_window: s.context_window,
                total_tokens: s.total_tokens(),
                input_tokens: s.total_input_tokens,
                output_tokens: s.total_output_tokens,
                cache_read_tokens: s.total_cache_read,
                cache_create_tokens: s.total_cache_create,
                turn_count: s.turn_count,
                mem_mb: s.mem_mb,
                git_branch: s.git_branch.clone(),
                git_added: s.git_added,
                git_modified: s.git_modified,
                started_at_ms: s.started_at,
                elapsed_secs: s.elapsed().as_secs(),
                summary: self.session_summary(s),
                current_task: s.current_tasks.last().cloned(),
                children: s.children.clone(),
                compaction_count: s.compaction_count,
                token_history: tail(&s.token_history, 64),
                subagents: tail(&s.subagents, 16)
                    .iter()
                    .map(|a| SubAgentView {
                        name: a.name.clone(),
                        status: a.status.clone(),
                        tokens: a.tokens,
                    })
                    .collect(),
                tool_calls: tail(&s.tool_calls, 24)
                    .iter()
                    .map(|t| ToolCallView {
                        name: t.name.clone(),
                        arg: t.arg.clone(),
                        duration_ms: t.duration_ms,
                    })
                    .collect(),
                chat_messages: tail(&s.chat_messages, MAX_CHAT_MESSAGES)
                    .iter()
                    .map(|m| ChatMsgView {
                        role: match &m.role {
                            ChatRole::User => "user",
                            ChatRole::Assistant => "assistant",
                        },
                        text: m.text.clone(),
                    })
                    .collect(),
            })
            .collect();

        let mcp_servers = self
            .mcp_servers
            .iter()
            .map(|m| McpServerView {
                pid: m.pid,
                parent_cli: m.parent_cli,
                profile: m.profile.clone(),
                mem_kb: m.mem_kb,
                active_count: m.active_count(now, ACTIVE_MTIME_SECS),
                rollout_count: m.rollouts.len(),
                last_activity_ms: m.latest_mtime().and_then(epoch_ms),
            })
            .collect();

        Snapshot {
            generated_at_ms: epoch_ms(now).unwrap_or(0),
            host: self.host_metrics,
            aggregate: self.agent_aggregate,
            token_rate: self.token_rates.back().copied().unwrap_or(0.0),
            interval_ms,
            sessions,
            rate_limits: self.rate_limits.clone(),
            orphan_ports: self.orphan_ports.clone(),
            mcp_servers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::PanelVisibility;
    use crate::demo::populate_demo;
    use crate::model::SessionStatus;
    use crate::theme::Theme;
    use std::time::{Duration, UNIX_EPOCH};

    fn demo_app() -> App {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        populate_demo(&mut app);
        app
    }

    #[test]
    fn tail_keeps_last_n_and_handles_short_inputs() {
        let v = vec![1, 2, 3, 4, 5];
        assert_eq!(tail(&v, 2), vec![4, 5]); // last n
        assert_eq!(tail(&v, 5), vec![1, 2, 3, 4, 5]); // exact fit
        assert_eq!(tail(&v, 9), vec![1, 2, 3, 4, 5]); // n > len → full clone
        assert_eq!(tail(&v, 0), Vec::<i32>::new()); // n = 0 → empty
        assert_eq!(tail(&Vec::<i32>::new(), 3), Vec::<i32>::new()); // empty input
    }

    #[test]
    fn epoch_ms_is_monotonic_and_zero_at_unix_epoch() {
        assert_eq!(epoch_ms(UNIX_EPOCH), Some(0));
        let later = UNIX_EPOCH + Duration::from_millis(1_500);
        assert_eq!(epoch_ms(later), Some(1_500));
    }

    #[test]
    fn session_status_serializes_as_variant_name() {
        // The web UI matches on these exact strings — they are part of the
        // stable JSON contract and must not be renamed without a major bump.
        for (status, wire) in [
            (SessionStatus::Thinking, "\"Thinking\""),
            (SessionStatus::Executing, "\"Executing\""),
            (SessionStatus::Waiting, "\"Waiting\""),
            (SessionStatus::Unknown, "\"Unknown\""),
            (SessionStatus::RateLimited, "\"RateLimited\""),
            (SessionStatus::Done, "\"Done\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), wire);
        }
    }

    #[test]
    fn to_snapshot_is_a_pure_read() {
        let app = demo_app();
        let before = app.sessions.len();
        let a = app.to_snapshot(2_000);
        let b = app.to_snapshot(2_000);
        // No mutation of the App, and repeated calls agree on shape.
        assert_eq!(app.sessions.len(), before);
        assert_eq!(a.sessions.len(), b.sessions.len());
        assert_eq!(a.sessions.len(), before);
    }

    #[test]
    fn to_snapshot_maps_fields_and_passes_interval_through() {
        let app = demo_app();
        let snap = app.to_snapshot(1_234);

        assert_eq!(snap.interval_ms, 1_234);
        assert!(snap.generated_at_ms > 0);
        assert!(!snap.sessions.is_empty());
        assert!(snap.host.is_some(), "demo populates host metrics");
        assert!(!snap.rate_limits.is_empty(), "demo populates rate limits");

        for s in &snap.sessions {
            // Bounded tails.
            assert!(s.token_history.len() <= 64);
            assert!(s.tool_calls.len() <= 24);
            // Chat roles map to the stable wire strings only.
            for m in &s.chat_messages {
                assert!(m.role == "user" || m.role == "assistant");
            }
        }
    }

    #[test]
    fn snapshot_round_trips_through_serde_json() {
        let snap = demo_app().to_snapshot(2_000);
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        assert!(json.contains("\"sessions\""));
        assert!(json.contains("\"interval_ms\":2000"));
        // Re-parse as generic JSON to confirm it is well-formed.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed["sessions"].is_array());
    }

    #[test]
    fn readme_documents_json_snapshot_privacy_surface() {
        let readme = include_str!("../README.md");
        assert!(readme.contains("--json"));
        assert!(readme.contains("JSON snapshot includes"));
        assert!(readme.contains("chat_messages"));
        assert!(readme.contains("summary"));
    }
}
