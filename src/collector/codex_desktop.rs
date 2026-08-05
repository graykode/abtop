use super::process;
use super::AgentCollector;
use crate::model::{AgentSession, SessionStatus};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Collector for Codex Desktop App sessions.
///
/// Reads live session state from `~/.codex/logs_2.sqlite` and tracks
/// real-time status changes by monitoring event lifecycle events:
/// `response.created` → `response.in_progress` → `response.completed`
///
/// Key log fields (all in one structured line):
/// `event.name="codex.sse_event" event.kind=response.completed
///   input_token_count=X output_token_count=X cached_token_count=X
///   conversation.id=XXXX model=gpt-5.4`
pub struct CodexDesktopCollector {
    db_path: PathBuf,
    /// Per-conversation state, keyed by conversation ID.
    sessions: HashMap<String, DesktopSessionState>,
    /// Timestamp high-water mark — only read entries newer than this.
    last_ts: i64,
    /// PID of the Codex Desktop app process.
    codex_pid: Option<u32>,
    /// Whether the app was alive on the previous tick (for transition).
    was_alive: bool,
}

/// Per-conversation runtime state.
#[derive(Clone)]
struct DesktopSessionState {
    session_id: String,
    first_ts: i64,
    last_ts: i64,
    /// The most recent event.kind observed for this conversation.
    last_event_kind: String,
    total_input: u64,
    total_output: u64,
    total_cached: u64,
    total_reasoning: u64,
    model: String,
}

impl Default for DesktopSessionState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            first_ts: 0,
            last_ts: 0,
            last_event_kind: String::new(),
            total_input: 0,
            total_output: 0,
            total_cached: 0,
            total_reasoning: 0,
            model: String::new(),
        }
    }
}

impl CodexDesktopCollector {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            db_path: home.join(".codex").join("logs_2.sqlite"),
            sessions: HashMap::new(),
            last_ts: 0,
            codex_pid: None,
            was_alive: false,
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.codex_pid = Self::find_codex_desktop_pid(&shared.process_info);

        // Step 1: Read latest log entries from SQLite (always read recent data)
        if self.db_path.exists() {
            if let Err(e) = self.read_recent_logs() {
                log_error(&format!("read error: {}", e));
            }
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let codex_running = self.codex_pid.is_some();
        let mut result: Vec<AgentSession> = Vec::new();

        // Filter: only show sessions that have had activity recently or are active
        let now_sec = (now_ms / 1000) as u64;

        for s in self.sessions.values() {
            let is_active_window = s.last_ts > 0 && (now_ms as i64 / 1000 - s.last_ts / 1000) < 300;
            // Show: codex is running AND this session is recently active
            if !codex_running || !is_active_window {
                continue;
            }

            let model = if s.model.is_empty() {
                "Codex Desktop".to_string()
            } else {
                s.model.clone()
            };

            // Determine real-time status from last event kind
            let is_thinking = codex_running
                && (s.last_event_kind == "response.created"
                    || s.last_event_kind == "response.in_progress");

            let status = if !codex_running {
                SessionStatus::Done
            } else if s.last_event_kind == "response.created"
                || s.last_event_kind == "response.in_progress"
            {
                // Model is currently generating a response
                SessionStatus::Thinking
            } else if s.last_event_kind == "response.output_item.done"
                || s.last_event_kind == "response.function_call_arguments.done"
            {
                // Tool is executing
                SessionStatus::Executing
            } else if s.last_event_kind == "response.failed" {
                SessionStatus::Done
            } else {
                // response.completed or idle — waiting for next input
                SessionStatus::Waiting
            };

            // Estimate context window
            let context_window = if model.contains("gpt-5") || model.contains("o5") {
                200_000
            } else {
                128_000
            };

            // Context percentage based on last-turn input tokens
            let context_percent = if context_window > 0 {
                let last_input = s.total_input.saturating_sub(
                    self.sessions
                        .values()
                        .filter(|o| o.session_id != s.session_id)
                        .map(|o| o.total_input)
                        .sum::<u64>(),
                );
                ((last_input.min(context_window)) as f64 / context_window as f64 * 100.0).min(100.0)
            } else {
                0.0
            };

            // Current task description from event kind
            let current_tasks = match s.last_event_kind.as_str() {
                "response.created" | "response.in_progress" => vec!["generating...".to_string()],
                "response.completed" => vec!["idle".to_string()],
                "response.failed" => vec!["error".to_string()],
                _ => vec!["working...".to_string()],
            };

            let thinking_since = if is_thinking { s.last_ts as u64 } else { 0 };

            result.push(AgentSession {
                agent_cli: "codex-desktop",
                pid: self.codex_pid.unwrap_or(0),
                session_id: s.session_id.clone(),
                cwd: String::new(),
                project_name: "Codex Desktop".to_string(),
                started_at: if s.first_ts > 0 {
                    s.first_ts as u64 / 1000
                } else {
                    now_sec
                },
                status,
                model,
                effort: String::new(),
                context_percent,
                total_input_tokens: s.total_input,
                total_output_tokens: s.total_output,
                total_cache_read: s.total_cached,
                total_cache_create: 0,
                turn_count: 1,
                current_tasks,
                mem_mb: 0,
                version: String::new(),
                git_branch: String::new(),
                git_added: 0,
                git_modified: 0,
                token_history: vec![s.total_input + s.total_output],
                context_history: vec![],
                compaction_count: 0,
                context_window,
                subagents: vec![],
                mem_file_count: 0,
                mem_line_count: 0,
                children: vec![],
                initial_prompt: String::new(),
                first_assistant_text: String::new(),
                chat_messages: vec![],
                tool_calls: vec![],
                pending_since_ms: if s.last_event_kind == "response.function_call_arguments.done" {
                    s.last_ts as u64
                } else {
                    0
                },
                thinking_since_ms: thinking_since,
                file_accesses: vec![],
            });
        }

        // If codex is running but we have no sessions yet, show a placeholder session
        if codex_running && result.is_empty() {
            result.push(AgentSession {
                agent_cli: "codex-desktop",
                pid: self.codex_pid.unwrap_or(0),
                session_id: "desktop-active".to_string(),
                cwd: String::new(),
                project_name: "Codex Desktop".to_string(),
                started_at: now_sec,
                status: SessionStatus::Waiting,
                model: "Codex Desktop".to_string(),
                effort: String::new(),
                context_percent: 0.0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cache_read: 0,
                total_cache_create: 0,
                turn_count: 0,
                current_tasks: vec!["waiting for activity".to_string()],
                mem_mb: 0,
                version: String::new(),
                git_branch: String::new(),
                git_added: 0,
                git_modified: 0,
                token_history: vec![],
                context_history: vec![],
                compaction_count: 0,
                context_window: 200_000,
                subagents: vec![],
                mem_file_count: 0,
                mem_line_count: 0,
                children: vec![],
                initial_prompt: String::new(),
                first_assistant_text: String::new(),
                chat_messages: vec![],
                tool_calls: vec![],
                pending_since_ms: 0,
                thinking_since_ms: 0,
                file_accesses: vec![],
            });
        }

        result.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        self.was_alive = codex_running;
        result
    }

    /// Read the most recent log entries (last 60 seconds) to track live state.
    fn read_recent_logs(&mut self) -> Result<(), String> {
        let db = self.db_path.to_string_lossy();

        // Always read the most recent 60 seconds of events to track state transitions.
        // This ensures we catch `response.created` → `response.completed` lifecycles.
        let recent_query = format!(
            "SELECT ts, feedback_log_body FROM logs \
             WHERE feedback_log_body LIKE '%event.kind=%' \
             ORDER BY ts DESC LIMIT 500"
        );

        let output = Command::new("sqlite3")
            .args(["-json", &db, &recent_query])
            .output()
            .map_err(|e| format!("sqlite3 failed: {}", e))?;

        if !output.status.success() {
            return Ok(());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(());
        }

        #[derive(Deserialize)]
        struct LogRow {
            ts: i64,
            feedback_log_body: String,
        }

        let rows: Vec<LogRow> =
            serde_json::from_str(&stdout).map_err(|e| format!("JSON parse: {}", e))?;

        for row in &rows {
            let body = &row.feedback_log_body;
            let ts = row.ts;

            // Extract event kind
            let Some(event_kind) = extract_field(body, "event.kind") else { continue };

            // Update high-water mark
            if ts > self.last_ts {
                self.last_ts = ts;
            }

            // For completion/failure events, also capture token counts
            let is_token_event = event_kind == "response.completed"
                || event_kind == "response.failed";

            let conv_id = if is_token_event || event_kind == "response.created"
                || event_kind == "response.in_progress"
            {
                extract_field(body, "conversation.id")
            } else {
                None
            };

            let Some(conv_id) = conv_id else { continue };

            let sid = if conv_id.len() > 18 {
                format!("desktop-{}", &conv_id[..18])
            } else {
                format!("desktop-{}", conv_id)
            };

            let state = self.sessions.entry(conv_id).or_insert_with(|| DesktopSessionState {
                session_id: sid,
                first_ts: ts,
                ..Default::default()
            });

            state.last_ts = ts;
            state.last_event_kind = event_kind.clone();

            if is_token_event {
                let input_tok = extract_int(body, "input_token_count").unwrap_or(0) as u64;
                let output_tok = extract_int(body, "output_token_count").unwrap_or(0) as u64;
                let cached_tok = extract_int(body, "cached_token_count").unwrap_or(0) as u64;
                let reasoning_tok = extract_int(body, "reasoning_token_count").unwrap_or(0) as u64;
                let model = extract_field(body, "model").unwrap_or_default();

                state.total_input += input_tok;
                state.total_output += output_tok;
                state.total_cached += cached_tok;
                state.total_reasoning += reasoning_tok;
                if !model.is_empty() {
                    state.model = model;
                }
            }
        }

        Ok(())
    }

    /// Find the Codex Desktop app process (not the CLI `codex` binary).
    fn find_codex_desktop_pid(
        process_info: &HashMap<u32, process::ProcInfo>,
    ) -> Option<u32> {
        for (pid, info) in process_info {
            if info.command.contains("Codex.app/Contents/MacOS/Codex")
                && !info.command.contains("Helper")
                && !info.command.contains("app-server")
            {
                return Some(*pid);
            }
        }
        None
    }
}

impl AgentCollector for CodexDesktopCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

/// Extract a field from a structured log body.
/// Format: `field.name=value` or `field.name="value"` (space-separated).
fn extract_field(body: &str, field: &str) -> Option<String> {
    let search = format!("{}=", field);
    let pos = body.find(&search)?;
    let start = pos + search.len();
    let rest = &body[start..];
    let end = rest.find(' ').unwrap_or(rest.len());
    let val = rest[..end].trim().to_string();
    let val = val.trim_matches('"').to_string();
    if val.is_empty() || val == "null" || val == "undefined" {
        return None;
    }
    Some(val)
}

fn extract_int(body: &str, field: &str) -> Option<i64> {
    let val = extract_field(body, field)?;
    val.parse::<i64>().ok()
}

fn log_error(msg: &str) {
    eprintln!("[codex-desktop] {}", msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_field_basic() {
        let body = r#"event.name="codex.sse_event" event.kind=response.completed input_token_count=335696 conversation.id=019dddc9-23fd-7c62 model=gpt-5.4"#;
        assert_eq!(extract_field(body, "event.kind"), Some("response.completed".into()));
        assert_eq!(extract_field(body, "conversation.id"), Some("019dddc9-23fd-7c62".into()));
        assert_eq!(extract_field(body, "input_token_count"), Some("335696".into()));
        assert_eq!(extract_field(body, "model"), Some("gpt-5.4".into()));
    }

    #[test]
    fn test_extract_int() {
        let body = r#"input_token_count=335696 output_token_count=316"#;
        assert_eq!(extract_int(body, "input_token_count"), Some(335696));
        assert_eq!(extract_int(body, "output_token_count"), Some(316));
    }

    #[test]
    fn test_extract_field_missing() {
        let body = r#"event.name="codex.sse_event""#;
        assert_eq!(extract_field(body, "nonexistent"), None);
    }

    #[test]
    fn test_extract_field_created_event() {
        let body = r#"event.name="codex.sse_event" event.kind=response.created conversation.id=abc123"#;
        assert_eq!(extract_field(body, "event.kind"), Some("response.created".into()));
        assert_eq!(extract_field(body, "conversation.id"), Some("abc123".into()));
    }

    #[test]
    fn test_extract_field_in_progress_event() {
        let body = r#"event.name="codex.sse_event" event.kind=response.in_progress conversation.id=abc123"#;
        assert_eq!(extract_field(body, "event.kind"), Some("response.in_progress".into()));
    }
}
