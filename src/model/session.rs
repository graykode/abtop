use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Account-level rate limit info (shared across all sessions).
#[derive(Debug, Clone, Default)]
pub struct RateLimitInfo {
    /// "claude" or "codex"
    pub source: String,
    /// 5-hour window usage percentage (0-100)
    pub five_hour_pct: Option<f64>,
    /// 5-hour window reset timestamp (epoch seconds)
    pub five_hour_resets_at: Option<u64>,
    /// 7-day window usage percentage (0-100)
    pub seven_day_pct: Option<f64>,
    /// 7-day window reset timestamp (epoch seconds)
    pub seven_day_resets_at: Option<u64>,
    /// When this data was last updated
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Working,
    Waiting,
    Done,
}

#[derive(Debug, Clone)]
pub struct ChildProcess {
    pub pid: u32,
    pub command: String,
    pub mem_kb: u64,
    pub port: Option<u16>,
}

/// A port left open by a process whose parent session has ended.
#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct AgentSession {
    /// Which CLI tool this session belongs to: "claude", "codex", etc.
    pub agent_cli: &'static str,
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub project_name: String,
    pub started_at: u64,
    pub status: SessionStatus,
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
    pub subagents: Vec<SubAgent>,
    pub mem_file_count: u32,
    pub mem_line_count: u32,
    pub children: Vec<ChildProcess>,
    /// First user prompt text, truncated — used as session title
    pub initial_prompt: String,
    /// First assistant response text (text blocks only) — used as summary fallback
    pub first_assistant_text: String,
}

impl AgentSession {
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens + self.total_cache_read + self.total_cache_create
    }

    /// Tokens that represent new work (input + output), excluding cache hits.
    /// Used for rate calculation to avoid inflated numbers from cache_read.
    pub fn active_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens + self.total_cache_create
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
            session_id: String::new(),
            cwd: String::new(),
            project_name: String::new(),
            started_at: 0,
            status: SessionStatus::Waiting,
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
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
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
}
