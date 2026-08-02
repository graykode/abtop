use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Type of file operation performed by the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum FileOp {
    Read,
    Write,
    Edit,
}

impl fmt::Display for FileOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileOp::Read => write!(f, "R"),
            FileOp::Write => write!(f, "W"),
            FileOp::Edit => write!(f, "E"),
        }
    }
}

/// A single file access event recorded from agent tool usage.
#[derive(Debug, Clone)]
pub struct FileAccess {
    pub path: String,
    pub operation: FileOp,
    #[allow(dead_code)]
    pub turn_index: u32,
}

/// Maximum file access entries kept per session to bound memory.
pub const MAX_FILE_ACCESSES: usize = 1000;

/// Account-level rate limit info (shared across all sessions).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RateLimitInfo {
    /// "claude" or "codex"
    pub source: String,
    /// 5-hour window usage percentage (0-100)
    pub five_hour_pct: Option<f64>,
    /// 5-hour window reset timestamp (epoch seconds)
    pub five_hour_resets_at: Option<u64>,
    /// 5-hour slot duration in minutes, when reported by the source.
    pub five_hour_window_minutes: Option<u64>,
    /// 7-day window usage percentage (0-100)
    ///
    /// Historical field name kept for compatibility; Codex may use this slot
    /// for a longer account-level window such as 30 days.
    pub seven_day_pct: Option<f64>,
    /// 7-day window reset timestamp (epoch seconds)
    pub seven_day_resets_at: Option<u64>,
    /// Long-window slot duration in minutes, when reported by the source.
    pub seven_day_window_minutes: Option<u64>,
    /// When this data was last updated
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SessionStatus {
    /// Model is generating a response and no tool is currently running
    Thinking,
    /// A tool, background terminal, or active child is doing work
    Executing,
    /// An exact provider signal says the session needs user input or approval
    Waiting,
    /// Process is alive, but no model turn, tool, or user interaction is active
    Idle,
    /// No sufficiently fresh, complete, and trustworthy lifecycle proof exists
    #[default]
    Unknown,
    /// Waiting due to rate limit
    RateLimited,
    /// Provider reported a live session or turn failure
    Error,
    /// Session finished
    Done,
}

impl SessionStatus {
    /// Returns true for states where the agent is actively doing work.
    pub fn is_active(&self) -> bool {
        matches!(self, SessionStatus::Thinking | SessionStatus::Executing)
    }
}

/// Maximum status samples retained on a live session.
pub const MAX_STATUS_OBSERVATIONS: usize = 128;

/// Maximum status samples included in a JSON snapshot and rendered in detail.
pub const MAX_VISIBLE_STATUS_OBSERVATIONS: usize = 5;

/// How directly abtop can substantiate the displayed lifecycle status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StatusAuthority {
    /// Exact provider-emitted lifecycle data supplied the status.
    Provider,
    /// abtop derived the status from local files or process metadata.
    Heuristic,
    /// No sufficiently reliable status source is currently available.
    #[default]
    Unavailable,
}

impl StatusAuthority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Heuristic => "heuristic",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Machine-readable explanation for a status observation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StatusReason {
    ProviderIdle,
    ProviderThinking,
    ProviderExecuting,
    ProviderWaitingApproval,
    ProviderWaitingUserInput,
    ProviderRateLimit,
    ProviderError,
    ProcessExited,
    ProtocolUnknown,
    ProtocolMalformed,
    Disconnected,
    Stale,
    Bootstrap,
    BackgroundProbePending,
    BackgroundProbeFailed,
    BackgroundTerminalActive,
    OwnershipUnconfirmed,
    CollectorInference,
    /// The installed Codex hook integration cannot be verified exactly.
    HookIntegrationUnverified,
    /// The installed Codex hook declaration/helper identity changed.
    HookConfigChanged,
    /// Required Codex hook lifecycle events are missing or out of order.
    HookEventGap,
    /// The private Codex hook state failed schema or filesystem validation.
    HookStateMalformed,
    /// Codex emitted an interaction request without an exact resolution event.
    HookInteractionResolutionUnavailable,
    /// A covered Codex tool call is open in both hook and rollout evidence.
    HookToolOpen,
    /// A complete Codex hook subagent set matches active direct-child model work.
    HookSubagentActive,
    /// A Codex turn is open in both hook and rollout evidence.
    HookTurnOpen,
    /// A Codex stop hook and rollout terminal event agree that the turn ended.
    HookTurnComplete,
    #[default]
    Unavailable,
}

impl StatusReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderIdle => "provider idle",
            Self::ProviderThinking => "provider thinking",
            Self::ProviderExecuting => "provider executing",
            Self::ProviderWaitingApproval => "waiting for approval",
            Self::ProviderWaitingUserInput => "waiting for user input",
            Self::ProviderRateLimit => "provider rate limit",
            Self::ProviderError => "provider error",
            Self::ProcessExited => "process exited",
            Self::ProtocolUnknown => "unknown protocol state",
            Self::ProtocolMalformed => "malformed protocol state",
            Self::Disconnected => "disconnected",
            Self::Stale => "stale observation",
            Self::Bootstrap => "initializing",
            Self::BackgroundProbePending => "checking background work",
            Self::BackgroundProbeFailed => "background check failed",
            Self::BackgroundTerminalActive => "background terminal active",
            Self::OwnershipUnconfirmed => "ownership unconfirmed",
            Self::CollectorInference => "collector inference",
            Self::HookIntegrationUnverified => "Codex hook integration unverified",
            Self::HookConfigChanged => "Codex hook configuration changed",
            Self::HookEventGap => "Codex hook event gap",
            Self::HookStateMalformed => "malformed Codex hook state",
            Self::HookInteractionResolutionUnavailable => {
                "Codex interaction resolution unavailable"
            }
            Self::HookToolOpen => "Codex hook tool open",
            Self::HookSubagentActive => "Codex hook subagent active",
            Self::HookTurnOpen => "Codex hook turn open",
            Self::HookTurnComplete => "Codex hook turn complete",
            Self::Unavailable => "evidence unavailable",
        }
    }
}

/// One bounded, content-free status sample.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct StatusObservation {
    pub status: SessionStatus,
    pub authority: StatusAuthority,
    pub reason: StatusReason,
    /// Unix-epoch milliseconds when this state was observed.
    pub observed_at_ms: u64,
    /// Provider connection generation, or zero for non-protocol sources.
    pub connection_generation: u64,
}

impl StatusObservation {
    pub fn new(
        status: SessionStatus,
        authority: StatusAuthority,
        reason: StatusReason,
        observed_at_ms: u64,
        connection_generation: u64,
    ) -> Self {
        Self {
            status,
            authority,
            reason,
            observed_at_ms,
            connection_generation,
        }
    }
}

/// Current status provenance plus a bounded observation history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct StatusEvidence {
    pub authority: StatusAuthority,
    pub reason: StatusReason,
    /// Unix-epoch milliseconds of the newest authoritative/heuristic sample.
    pub observed_at_ms: u64,
    /// Unix-epoch milliseconds when the current status first began.
    pub status_since_ms: u64,
    /// Provider connection generation, or zero for non-protocol sources.
    pub connection_generation: u64,
    /// Consecutive samples matching status, authority, and connection generation.
    pub consecutive_matching: u32,
    /// Content-free samples, oldest first.
    pub observations: Vec<StatusObservation>,
}

impl StatusEvidence {
    /// Record one sample and update the current evidence summary.
    pub fn observe(&mut self, observation: StatusObservation) {
        let previous = self.observations.last();
        let same_status = previous.is_some_and(|sample| {
            sample.status == observation.status
                && sample.authority == observation.authority
                && sample.connection_generation == observation.connection_generation
        });

        if same_status {
            self.consecutive_matching = self.consecutive_matching.saturating_add(1).max(1);
        } else {
            self.status_since_ms = observation.observed_at_ms;
            self.consecutive_matching = 1;
        }

        self.authority = observation.authority;
        self.reason = observation.reason;
        self.observed_at_ms = observation.observed_at_ms;
        self.connection_generation = observation.connection_generation;
        self.observations.push(observation);
        if self.observations.len() > MAX_STATUS_OBSERVATIONS {
            let excess = self.observations.len() - MAX_STATUS_OBSERVATIONS;
            self.observations.drain(..excess);
        }
    }

    /// Return a copy whose observation ledger contains only the newest samples.
    pub fn recent(&self, limit: usize) -> Self {
        let mut recent = self.clone();
        if recent.observations.len() > limit {
            let start = recent.observations.len() - limit;
            recent.observations = recent.observations.split_off(start);
        }
        recent
    }

    pub fn has_sample(&self) -> bool {
        self.observed_at_ms > 0 || !self.observations.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildProcess {
    pub pid: u32,
    pub command: String,
    pub mem_kb: u64,
    pub port: Option<u16>,
}

/// A port left open by a process whose parent session has ended.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanPort {
    pub port: u16,
    pub pid: u32,
    pub command: String,
    pub project_name: String,
}

#[derive(Debug, Clone)]
pub struct SubAgent {
    pub name: String,
    pub status: String,
    pub tokens: u64,
}

/// A single tool invocation from a session transcript.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Tool name: "Read", "Edit", "Bash", "Write", "Grep", "Glob", "Agent", etc.
    pub name: String,
    /// Short argument (file path, command prefix, pattern).
    pub arg: String,
    /// Duration in milliseconds (0 if unknown).
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatRole {
    User,
    Assistant,
}

/// A compact, redacted chat line from the session transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
}

/// Maximum chat messages kept per session to bound memory and UI noise.
pub const MAX_CHAT_MESSAGES: usize = 12;

#[derive(Debug, Clone)]
pub struct AgentSession {
    /// Which CLI tool this session belongs to: "claude", "codex", "opencode",
    /// "grok", or "kimi".
    /// Also used as the identifier for the `hidden_agents` config key
    /// (case-insensitive match).
    pub agent_cli: &'static str,
    pub pid: u32,
    /// Internal, exact opaque OS process-incarnation anchor that the collector
    /// tied to this logical row while validating ownership. Never render or
    /// serialize this value. PID actions must compare this retained value with
    /// fresh OS observations; they must never create the expected identity by
    /// resampling the PID after collection.
    ///
    /// `None` means ownership is not strong enough for kill or terminal-jump
    /// actions, even when lifecycle metadata is otherwise useful for display.
    pub action_process_incarnation: Option<String>,
    pub session_id: String,
    pub cwd: String,
    pub project_name: String,
    pub started_at: u64,
    pub status: SessionStatus,
    /// Provenance, freshness, and bounded samples supporting `status`.
    pub status_evidence: StatusEvidence,
    pub model: String,
    /// Reasoning effort setting (Codex CLI only: "minimal" | "low" | "medium" | "high").
    /// Empty string when unknown or not applicable.
    pub effort: String,
    pub context_percent: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read: u64,
    pub total_cache_create: u64,
    pub turn_count: u32,
    pub current_tasks: Vec<String>,
    pub mem_mb: u64,
    pub version: String,
    pub git_branch: String,
    pub git_added: u32,
    pub git_modified: u32,
    pub token_history: Vec<u64>,
    /// Per-turn context size (input tokens) for context evolution visualization.
    pub context_history: Vec<u64>,
    /// Number of detected compaction events (context dropped > 30% between turns).
    pub compaction_count: u32,
    /// Context window size for this session's model (e.g. 200K, 1M).
    pub context_window: u64,
    pub subagents: Vec<SubAgent>,
    pub mem_file_count: u32,
    pub mem_line_count: u32,
    pub children: Vec<ChildProcess>,
    /// First user prompt text, truncated — used as session title
    pub initial_prompt: String,
    /// First assistant response text (text blocks only) — used as summary fallback
    pub first_assistant_text: String,
    /// Recent user/assistant chat tail, excluding tool results and tool inputs.
    pub chat_messages: Vec<ChatMessage>,
    /// Timeline of tool calls extracted from transcript.
    pub tool_calls: Vec<ToolCall>,
    /// Unix-epoch ms of the assistant turn whose `tool_use` blocks are still
    /// awaiting the matching `user` response. Zero when the latest assistant
    /// turn has already been closed (no tools currently in flight).
    /// Used to animate the timeline bar for the running tool(s).
    pub pending_since_ms: u64,
    /// True when the provider exposes a pending interaction that needs a user
    /// response. Codex interaction candidates remain false here because its
    /// current hook contract cannot prove prompt resolution.
    pub awaiting_input: bool,
    /// Unix-epoch ms of the most recent `user` line (prompt or tool_result)
    /// that has not yet been followed by an assistant response. Zero when
    /// the last transcript entry was an assistant turn. Used to render a
    /// live "Thinking" row while the model is generating its next reply.
    pub thinking_since_ms: u64,
    /// File access audit log: every file read/written/edited by the agent.
    pub file_accesses: Vec<FileAccess>,
    /// Config root directory for this session's agent (home-abbreviated, e.g. "~/.claude-work").
    /// For Claude Code: the active .claude* profile folder. For Codex: "~/.codex".
    /// For OpenCode: the data directory containing opencode.db. For Grok and
    /// Kimi: the active GROK_HOME or KIMI_CODE_HOME directory.
    pub config_root: String,
}

impl AgentSession {
    /// Keep the compatibility flag exactly aligned with the lifecycle enum.
    /// Waiting is reserved for a provider-confirmed actionable interaction.
    pub fn enforce_status_contract(&mut self) {
        self.awaiting_input = matches!(self.status, SessionStatus::Waiting);
    }

    pub fn is_awaiting_input(&self) -> bool {
        matches!(self.status, SessionStatus::Waiting)
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens
            + self.total_output_tokens
            + self.total_cache_read
            + self.total_cache_create
    }

    /// Tokens that represent new work (input + output), excluding cache hits.
    /// Used for rate calculation to avoid inflated numbers from cache_read.
    pub fn active_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens + self.total_cache_create
    }

    /// Task text suitable for user-facing session summaries.
    ///
    /// Status-owned labels take precedence over stale provider task text.
    /// Only Executing may expose a provider tool/task preview: every terminal,
    /// quiescent, waiting, or uncertain state uses a canonical label so an old
    /// tool name cannot make an Idle or Unknown row look active.
    pub fn display_task(&self) -> Option<&str> {
        match self.status {
            SessionStatus::Thinking => Some("thinking"),
            SessionStatus::Executing => self
                .current_tasks
                .last()
                .map(String::as_str)
                .or(Some("executing")),
            SessionStatus::Waiting => Some("waiting for user input"),
            SessionStatus::Idle => Some("idle"),
            SessionStatus::RateLimited => Some("rate limited"),
            SessionStatus::Error => Some("error"),
            SessionStatus::Unknown => Some("status evidence unavailable"),
            SessionStatus::Done => Some("finished"),
        }
    }

    pub fn elapsed(&self) -> Duration {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.started_at))
    }

    pub fn elapsed_display(&self) -> String {
        let secs = self.elapsed().as_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m", secs / 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionFile {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "startedAt")]
    pub started_at: u64,
    /// Optional, undocumented per-PID Claude session status. Serde defaults it
    /// when absent for compatibility; the collector uses an exact decision-tool
    /// fallback when this signal is not waiting.
    #[serde(default)]
    pub status: Option<String>,
}

impl SessionFile {
    /// Truncate string fields to sane limits after deserialization.
    pub fn sanitize(&mut self) {
        truncate_string(&mut self.session_id, 256);
        truncate_string(&mut self.cwd, 4096);
    }
}

/// Truncate a string at a char boundary to avoid panics on multi-byte UTF-8.
fn truncate_string(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        // Find the last char boundary at or before max_bytes
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(input: u64, output: u64, cache_read: u64, cache_create: u64) -> AgentSession {
        AgentSession {
            agent_cli: "claude",
            pid: 0,
            action_process_incarnation: None,
            session_id: String::new(),
            cwd: String::new(),
            project_name: String::new(),
            started_at: 0,
            status: SessionStatus::Waiting,
            status_evidence: StatusEvidence::default(),
            model: String::new(),
            effort: String::new(),
            context_percent: 0.0,
            total_input_tokens: input,
            total_output_tokens: output,
            total_cache_read: cache_read,
            total_cache_create: cache_create,
            turn_count: 0,
            current_tasks: Vec::new(),
            mem_mb: 0,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: Vec::new(),
            context_history: Vec::new(),
            compaction_count: 0,
            context_window: 0,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            awaiting_input: false,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
            config_root: String::new(),
        }
    }

    #[test]
    fn test_total_tokens() {
        let session = make_session(100, 50, 200, 30);
        assert_eq!(session.total_tokens(), 380); // 100 + 50 + 200 + 30
    }

    #[test]
    fn test_active_tokens() {
        let session = make_session(100, 50, 200, 30);
        assert_eq!(session.active_tokens(), 180); // 100 + 50 + 30, excludes cache_read
    }

    #[test]
    fn only_thinking_and_executing_are_active() {
        for status in [SessionStatus::Thinking, SessionStatus::Executing] {
            assert!(status.is_active(), "{status:?}");
        }
        for status in [
            SessionStatus::Waiting,
            SessionStatus::Idle,
            SessionStatus::Unknown,
            SessionStatus::RateLimited,
            SessionStatus::Error,
            SessionStatus::Done,
        ] {
            assert!(!status.is_active(), "{status:?}");
        }
    }

    #[test]
    fn codex_active_subagent_reason_is_stable_and_content_free() {
        assert_eq!(
            StatusReason::HookSubagentActive.as_str(),
            "Codex hook subagent active"
        );
        assert_eq!(
            serde_json::to_string(&StatusReason::HookSubagentActive).unwrap(),
            r#""HookSubagentActive""#
        );
    }

    #[test]
    fn status_owned_task_labels_override_stale_provider_text() {
        let mut session = make_session(0, 0, 0, 0);
        session.current_tasks.push("Edit stale.rs".into());

        session.status = SessionStatus::Waiting;
        assert_eq!(session.display_task(), Some("waiting for user input"));

        session.status = SessionStatus::Thinking;
        assert_eq!(session.display_task(), Some("thinking"));

        session.status = SessionStatus::RateLimited;
        assert_eq!(session.display_task(), Some("rate limited"));
    }

    #[test]
    fn non_executing_task_labels_never_leak_stale_work() {
        let mut session = make_session(0, 0, 0, 0);
        session.current_tasks.push("Edit stale.rs".into());

        for (status, expected) in [
            (SessionStatus::Waiting, "waiting for user input"),
            (SessionStatus::Idle, "idle"),
            (SessionStatus::Unknown, "status evidence unavailable"),
            (SessionStatus::RateLimited, "rate limited"),
            (SessionStatus::Error, "error"),
            (SessionStatus::Done, "finished"),
        ] {
            session.status = status;
            assert_eq!(session.display_task(), Some(expected), "{status:?}");
        }
    }

    #[test]
    fn missing_status_evidence_fails_closed() {
        let evidence: StatusEvidence =
            serde_json::from_str("{}").expect("missing fields use safe defaults");
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::Unavailable);
        assert_eq!(evidence.observed_at_ms, 0);
        assert_eq!(evidence.consecutive_matching, 0);
        assert!(evidence.observations.is_empty());
        assert_eq!(
            serde_json::from_str::<SessionStatus>("\"Error\"").unwrap(),
            SessionStatus::Error
        );
    }

    #[test]
    fn status_evidence_tracks_transitions_and_bounds_samples() {
        let mut evidence = StatusEvidence::default();
        evidence.observe(StatusObservation::new(
            SessionStatus::Idle,
            StatusAuthority::Provider,
            StatusReason::ProviderIdle,
            10,
            1,
        ));
        evidence.observe(StatusObservation::new(
            SessionStatus::Idle,
            StatusAuthority::Provider,
            StatusReason::ProviderIdle,
            20,
            1,
        ));
        assert_eq!(evidence.status_since_ms, 10);
        assert_eq!(evidence.consecutive_matching, 2);

        evidence.observe(StatusObservation::new(
            SessionStatus::Thinking,
            StatusAuthority::Provider,
            StatusReason::ProviderThinking,
            30,
            1,
        ));
        assert_eq!(evidence.status_since_ms, 30);
        assert_eq!(evidence.consecutive_matching, 1);

        for observed_at_ms in 31..=(MAX_STATUS_OBSERVATIONS as u64 + 40) {
            evidence.observe(StatusObservation::new(
                SessionStatus::Thinking,
                StatusAuthority::Provider,
                StatusReason::ProviderThinking,
                observed_at_ms,
                1,
            ));
        }
        assert_eq!(evidence.observations.len(), MAX_STATUS_OBSERVATIONS);
        assert_eq!(
            evidence
                .recent(MAX_VISIBLE_STATUS_OBSERVATIONS)
                .observations
                .len(),
            MAX_VISIBLE_STATUS_OBSERVATIONS
        );
    }

    #[test]
    fn awaiting_input_is_exactly_the_waiting_status() {
        let mut session = make_session(0, 0, 0, 0);
        for status in [
            SessionStatus::Thinking,
            SessionStatus::Executing,
            SessionStatus::Idle,
            SessionStatus::Unknown,
            SessionStatus::RateLimited,
            SessionStatus::Error,
            SessionStatus::Done,
        ] {
            session.status = status;
            session.awaiting_input = true;
            session.enforce_status_contract();
            assert!(!session.awaiting_input, "{status:?}");
            assert!(!session.is_awaiting_input(), "{status:?}");
        }

        session.status = SessionStatus::Waiting;
        session.awaiting_input = false;
        session.enforce_status_contract();
        assert!(session.awaiting_input);
        assert!(session.is_awaiting_input());
    }
}
