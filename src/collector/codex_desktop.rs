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
/// Codex Desktop stores session data in `~/.codex/logs_2.sqlite` instead of
/// the JSONL files used by Codex CLI. This collector reads the SQLite DB
/// directly to extract session metadata and token usage.
///
/// Data sources:
/// 1. `ps` to find the Codex Desktop process (Codex.app)
/// 2. The `logs_2.sqlite` database for per-session token counts, model info
///
/// Key log event pattern:
/// ```text
/// event.name="codex.sse_event" event.kind=response.completed
///   input_token_count=335696 output_token_count=316 cached_token_count=332672
///   reasoning_token_count=66 conversation.id=019dddc9-...
///   model=gpt-5.4
/// ```
pub struct CodexDesktopCollector {
    db_path: PathBuf,
    /// Cached per-session data keyed by conversation ID.
    sessions: HashMap<String, DesktopSessionState>,
    /// Timestamp (ms) of the most recent log entry we've processed.
    last_ts: i64,
    /// PID of the Codex Desktop process, if running.
    codex_pid: Option<u32>,
}

/// Internal state tracked per conversation.
#[derive(Clone, Default)]
struct DesktopSessionState {
    session_id: String,
    first_ts: i64,
    last_ts: i64,
    total_input: u64,
    total_output: u64,
    total_cached: u64,
    total_reasoning: u64,
    model: String,
}

impl CodexDesktopCollector {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            db_path: home.join(".codex").join("logs_2.sqlite"),
            sessions: HashMap::new(),
            last_ts: 0,
            codex_pid: None,
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // Step 1: Find Codex Desktop PID
        self.codex_pid = Self::find_codex_desktop_pid(&shared.process_info);

        // No desktop process and no cached sessions — nothing to show
        if self.codex_pid.is_none() && self.sessions.is_empty() {
            return vec![];
        }

        // Step 2: Read new data from SQLite
        if self.db_path.exists() {
            if let Ok(new_sessions) = self.read_new_sessions() {
                for s in new_sessions {
                    self.sessions.insert(s.session_id.clone(), s);
                }
            }
        }

        // Step 3: Build AgentSession vec
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut result: Vec<AgentSession> = self
            .sessions
            .values()
            .map(|s| {
                let model = if s.model.is_empty() {
                    "Codex Desktop".to_string()
                } else {
                    s.model.clone()
                };

                // Estimate context window — common for Codex models
                let context_window = if model.contains("gpt-5") || model.contains("o5") {
                    200_000
                } else {
                    128_000
                };

                // Context percent: use last turn's input tokens as proxy
                // We don't have per-turn breakdown from the aggregated SQLite data,
                // so use a simple heuristic
                let avg_input_per_call = if s.total_input > 0 && s.total_input > s.total_cached {
                    (s.total_input - s.total_cached) / s.total_input.max(1)
                } else {
                    0
                };
                let context_percent = if context_window > 0 {
                    (avg_input_per_call as f64 / context_window as f64 * 100.0)
                        .min(100.0)
                } else {
                    0.0
                };

                AgentSession {
                    agent_cli: "codex-desktop",
                    pid: self.codex_pid.unwrap_or(0),
                    session_id: s.session_id.clone(),
                    cwd: String::new(),
                    project_name: "Codex Desktop".to_string(),
                    started_at: if s.first_ts > 0 {
                        s.first_ts as u64 / 1000
                    } else {
                        now_ms / 1000
                    },
                    status: SessionStatus::Waiting,
                    model,
                    effort: String::new(),
                    context_percent,
                    total_input_tokens: s.total_input,
                    total_output_tokens: s.total_output,
                    total_cache_read: s.total_cached,
                    total_cache_create: 0,
                    turn_count: 1,
                    current_tasks: vec!["Codex Desktop".to_string()],
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
                    pending_since_ms: 0,
                    thinking_since_ms: 0,
                    file_accesses: vec![],
                }
            })
            .collect();

        // Mark sessions as Done if the desktop app is no longer running
        if self.codex_pid.is_none() {
            for session in &mut result {
                session.status = SessionStatus::Done;
            }
        }

        result.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        result
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

    /// Read new log entries from the SQLite database that have been
    /// appended since `self.last_ts`.
    fn read_new_sessions(&mut self) -> Result<Vec<DesktopSessionState>, String> {
        let db_path_str = self.db_path.to_string_lossy();

        // Query for session token events newer than our last read timestamp
        let query = format!(
            "SELECT ts, feedback_log_body FROM logs \
             WHERE ts > {} \
             AND feedback_log_body LIKE '%input_token_count%' \
             ORDER BY ts",
            self.last_ts
        );

        let output = Command::new("sqlite3")
            .args(["-json", &db_path_str, &query])
            .output()
            .map_err(|e| format!("Failed to run sqlite3: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("no such table") {
                log_error(&format!("sqlite3 error: {}", stderr));
            }
            return Ok(vec![]);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(vec![]);
        }

        #[derive(Deserialize)]
        struct LogRow {
            ts: i64,
            feedback_log_body: String,
        }

        let rows: Vec<LogRow> =
            serde_json::from_str(&stdout).map_err(|e| format!("JSON parse error: {}", e))?;

        let mut session_map: HashMap<String, DesktopSessionState> = HashMap::new();

        for row in &rows {
            let body = &row.feedback_log_body;
            let ts = row.ts;

            // Extract conversation ID
            let conv_id = extract_field(body, "conversation.id");
            let Some(conv_id) = conv_id else { continue };

            // Extract token counts
            let input_tok = extract_int(body, "input_token_count").unwrap_or(0);
            let output_tok = extract_int(body, "output_token_count").unwrap_or(0);
            let cached_tok = extract_int(body, "cached_token_count").unwrap_or(0);
            let reasoning_tok = extract_int(body, "reasoning_token_count").unwrap_or(0);
            let model = extract_field(body, "model").unwrap_or_default();

            let state = session_map.entry(conv_id.clone()).or_insert_with(|| {
                let sid = if conv_id.len() > 18 {
                    format!("desktop-{}", &conv_id[..18])
                } else {
                    format!("desktop-{}", conv_id)
                };
                DesktopSessionState {
                    session_id: sid,
                    first_ts: ts,
                    model: model.clone(),
                    ..Default::default()
                }
            });

            state.last_ts = ts;
            state.total_input += input_tok as u64;
            state.total_output += output_tok as u64;
            state.total_cached += cached_tok as u64;
            state.total_reasoning += reasoning_tok as u64;
            if !model.is_empty() {
                state.model = model;
            }
        }

        // Update our high-water mark
        if let Some(max_ts) = rows.iter().map(|r| r.ts).max() {
            if max_ts > self.last_ts {
                self.last_ts = max_ts;
            }
        }

        Ok(session_map.into_values().collect())
    }
}

impl AgentCollector for CodexDesktopCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

/// Extract a field from a structured log body.
/// Format: `field.name=value` or `field.name="value"` (space-separated key=value pairs)
/// Returns None if the field is not found or its value is empty/null.
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

/// Extract an integer field from a structured log body.
fn extract_int(body: &str, field: &str) -> Option<i64> {
    let val = extract_field(body, field)?;
    val.parse::<i64>().ok()
}

fn log_error(msg: &str) {
    // Log to stderr — abtop currently doesn't have a logging facility
    eprintln!("[codex-desktop] {}", msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_field_basic() {
        let body = r#"event.name="codex.sse_event" input_token_count=335696 conversation.id=019dddc9-23fd-7c62 model=gpt-5.4"#;
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
}
