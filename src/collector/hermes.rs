//! Collector for Hermes Agent (https://github.com/NousResearch/hermes-agent) sessions.
//!
//! Hermes runs one worker process per active session:
//! `python -m tui_gateway.slash_worker --session-key <id> --model <name>`, which maps
//! a live PID to a session id without any shell surface. Session metadata, token
//! counters and the transcript tail are read directly from Hermes' SQLite state DB
//! (`~/.hermes/state.db`, or `HERMES_HOME/state.db`) through an in-process,
//! read-only `rusqlite` connection — no external interpreter, no generated code,
//! no temp files, and no shell interpolation anywhere.
//!
//! Privacy follows the same model as the other collectors: every DB-sourced string
//! (title, cwd, chat tail, tool arguments) passes through
//! `sanitize_terminal_text` → `redact_secrets` → truncation before it can reach the
//! TUI or JSON snapshot. The DB path is fail-closed against symlinks, and DB rows
//! are cached and only refreshed on `shared.slow_tick` (~10s).

use super::{context_window_for_model, process};
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, SessionStatus, ToolCall, MAX_CHAT_MESSAGES,
};
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
use std::process::Command;

/// Maximum sessions fetched from the DB per query. Hermes keeps sessions for a
/// long time, so we bound the query and let process matching pick the live ones.
const MAX_SESSIONS: i64 = 100;

/// Maximum transcript rows read per session (newest first), enough to fill the
/// chat tail, initial prompt and tool-call timeline.
const MAX_TRANSCRIPT_ROWS: i64 = 60;

/// Cap on tool calls surfaced per session (matches other collectors' bounds).
const MAX_TOOL_CALLS: usize = 500;

/// Model -> context window size (tokens). Hermes does not persist the context
/// window in state.db, so we keep a table (lowercased keys). Fall back to the
/// shared heuristic for models not listed here.
const MODEL_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("deepseek-v4-flash", 1_048_576),
    ("deepseek-v4", 1_048_576),
    ("deepseek-v3", 1_024_000),
    ("deepseek-r1", 1_024_000),
    ("deepseek-chat", 1_024_000),
    ("deepseek-reasoner", 1_024_000),
    ("claude-sonnet-4", 200_000),
    ("claude-sonnet-4-20250514", 200_000),
    ("claude-opus-4", 200_000),
    ("claude-sonnet-3-5", 200_000),
    ("claude-opus-3-5", 200_000),
    ("gpt-4o", 128_000),
    ("gpt-4o-mini", 128_000),
    ("gpt-5", 400_000),
    ("gpt-5.5", 400_000),
    ("o3", 200_000),
    ("o4-mini", 200_000),
    ("gemini-2.5-pro", 1_048_576),
    ("gemini-2.0-flash", 1_048_576),
    ("qwen2.5-72b", 131_072),
    ("qwen3", 131_072),
    ("llama-3.3-70b", 131_072),
    ("llama-4", 1_000_000),
    ("llama-4-scout", 1_000_000),
    ("llama-4-maverick", 1_000_000),
    ("mistral-large", 128_000),
    ("mistral-small", 128_000),
];

/// Known state.db locations, checked in order when discovering Hermes.
const STATE_DB_CANDIDATES: &[&str] = &[
    "~/.hermes/state.db",
    "~/.local/share/hermes/state.db",
    "~/AppData/Local/hermes/state.db",
];

/// Collector for Hermes Agent sessions.
///
/// Discovery strategy:
/// 1. Map running Hermes worker PIDs to session ids from `--session-key` args
///    in already-scanned process command lines (no string building).
/// 2. Read session metadata + token counters from the Hermes SQLite state DB
///    via an in-process read-only rusqlite connection.
/// 3. Match running PIDs to DB sessions by id / session key.
/// 4. Derive status from the transcript tail (last user prompt vs. open tool
///    call) plus process CPU activity.
pub struct HermesCollector {
    db_path: PathBuf,
    /// Whether the DB opened successfully (checked once; fail-closed).
    available: Option<bool>,
    /// Cached DB rows from the last slow-tick query.
    cached_db_sessions: Vec<DbSession>,
    /// Cached transcript tails from the last slow-tick query.
    cached_chat: HashMap<String, ChatTail>,
}

/// A session row read from the Hermes state DB, sanitized at read time.
struct DbSession {
    id: String,
    title: String,
    cwd: String,
    git_repo_root: String,
    git_branch: String,
    model: String,
    started_at_ms: u64,
    turn_count: u32,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_write: u64,
    session_key: String,
}

/// Redacted transcript tail for one session, rebuilt on slow ticks.
#[derive(Clone, Default)]
struct ChatTail {
    initial_prompt: String,
    first_assistant_text: String,
    chat_messages: Vec<ChatMessage>,
    tool_calls: Vec<ToolCall>,
    /// Newest row is a real user prompt (model is generating).
    thinking_since_ms: u64,
    /// Newest row is an assistant turn with tool calls awaiting results.
    pending_since_ms: u64,
}

/// Raw transcript row as stored by Hermes.
struct MessageRow {
    role: String,
    content: String,
    tool_calls: String,
    ts_secs: f64,
}

impl HermesCollector {
    pub fn new() -> Self {
        Self {
            db_path: Self::discover_db_path(),
            available: None,
            cached_db_sessions: Vec::new(),
            cached_chat: HashMap::new(),
        }
    }

    /// Try `HERMES_HOME` first, then known state.db locations, then a
    /// platform-appropriate default.
    fn discover_db_path() -> PathBuf {
        if let Ok(home) = std::env::var("HERMES_HOME") {
            if !home.is_empty() {
                let p = PathBuf::from(&home).join("state.db");
                if p.exists() {
                    return p;
                }
            }
        }
        for candidate in STATE_DB_CANDIDATES {
            let p = expand_home(candidate);
            if p.exists() {
                return p;
            }
        }
        let home = dirs::home_dir().unwrap_or_default();
        if cfg!(target_os = "windows") {
            home.join("AppData/Local/hermes/state.db")
        } else {
            home.join(".hermes/state.db")
        }
    }

    /// Fail-closed availability check: reject symlinked or missing DBs, and
    /// verify we can actually open the file read-only. Cached after first call.
    fn check_db(&mut self) -> bool {
        if let Some(ok) = self.available {
            return ok;
        }
        let ok = !is_symlink(&self.db_path)
            && self.db_path.exists()
            && Self::open_ro(&self.db_path).is_ok();
        self.available = Some(ok);
        ok
    }

    /// Open the Hermes state DB strictly read-only, no mutex (single-threaded
    /// use). WAL mode is handled transparently by SQLite itself.
    fn open_ro(db_path: &Path) -> rusqlite::Result<Connection> {
        Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
    }

    /// Map running Hermes worker PIDs to session ids by parsing the
    /// `--session-key <id>` argument from command lines already scanned by the
    /// shared process snapshot. No shell or codegen involved — the command
    /// strings are parsed as tokens only.
    fn find_hermes_pid_map(process_info: &HashMap<u32, process::ProcInfo>) -> HashMap<u32, String> {
        let mut pid_map = HashMap::new();
        for (&pid, info) in process_info {
            let cmd = &info.command;
            // Hermes launches one worker per active session:
            //   python -m tui_gateway.slash_worker --session-key <id> --model <name>
            // Gate on the module name to avoid matching unrelated "--session-key" users.
            if cmd.contains("tui_gateway") && cmd.contains("--session-key") {
                let after =
                    cmd[cmd.find("--session-key").unwrap() + "--session-key".len()..].trim_start();
                let key = after.split_whitespace().next().unwrap_or("").trim();
                if !key.is_empty() {
                    pid_map.insert(pid, key.to_string());
                }
            }
        }
        pid_map
    }

    /// Query session metadata and token counters (slow tick only).
    fn query_sessions(&self) -> Option<Vec<DbSession>> {
        let conn = Self::open_ro(&self.db_path).ok()?;
        let mut stmt = conn
            .prepare(
                "SELECT
                   id,
                   COALESCE(title, ''),
                   COALESCE(cwd, ''),
                   COALESCE(git_repo_root, ''),
                   COALESCE(git_branch, ''),
                   COALESCE(model, ''),
                   COALESCE(started_at, 0),
                   COALESCE(message_count, 0),
                   COALESCE(input_tokens, 0),
                   COALESCE(output_tokens, 0),
                   COALESCE(cache_read_tokens, 0),
                   COALESCE(cache_write_tokens, 0),
                   COALESCE(session_key, '')
                 FROM sessions
                 WHERE ended_at IS NULL
                   AND (archived IS NULL OR archived = 0)
                 ORDER BY started_at DESC
                 LIMIT ?1",
            )
            .ok()?;
        let rows = stmt
            .query_map([MAX_SESSIONS], |row| {
                Ok(DbSession {
                    id: row.get::<_, String>(0)?,
                    title: sanitize_db_title(&row.get::<_, String>(1)?),
                    cwd: sanitize_db_field(&row.get::<_, String>(2)?, 4096),
                    git_repo_root: sanitize_db_field(&row.get::<_, String>(3)?, 4096),
                    git_branch: sanitize_db_field(&row.get::<_, String>(4)?, 256),
                    model: sanitize_db_field(&row.get::<_, String>(5)?, 256),
                    started_at_ms: secs_float_to_ms(row.get::<_, f64>(6)?),
                    turn_count: row.get::<_, i64>(7)? as u32,
                    total_input: row.get::<_, i64>(8)? as u64,
                    total_output: row.get::<_, i64>(9)? as u64,
                    total_cache_read: row.get::<_, i64>(10)? as u64,
                    total_cache_write: row.get::<_, i64>(11)? as u64,
                    session_key: sanitize_db_field(&row.get::<_, String>(12)?, 512),
                })
            })
            .ok()?;
        rows.collect::<Result<Vec<_>, _>>().ok()
    }

    /// Query the redacted transcript tail for the active session ids
    /// (slow tick only).
    fn query_chat_tail(&self, active_ids: &[String]) -> HashMap<String, ChatTail> {
        let mut out = HashMap::new();
        let Ok(conn) = Self::open_ro(&self.db_path) else {
            return out;
        };
        for sid in active_ids {
            let rows = match (|| {
                let mut stmt = conn
                    .prepare(
                        "SELECT
                           COALESCE(role, ''),
                           COALESCE(content, ''),
                           COALESCE(tool_calls, ''),
                           COALESCE(timestamp, 0)
                         FROM messages
                         WHERE session_id = ?1
                         ORDER BY timestamp DESC
                         LIMIT ?2",
                    )
                    .ok()?;
                let rows = stmt
                    .query_map(rusqlite::params![sid, MAX_TRANSCRIPT_ROWS], |row| {
                        Ok(MessageRow {
                            role: row.get(0)?,
                            content: row.get(1)?,
                            tool_calls: row.get(2)?,
                            ts_secs: row.get(3)?,
                        })
                    })
                    .ok()?;
                rows.collect::<Result<Vec<_>, _>>().ok()
            })() {
                Some(rows) => rows,
                None => continue,
            };
            out.insert(sid.clone(), build_chat_tail(rows));
        }
        out
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // Fail-closed: symlinked/missing/unopenable DB => no Hermes sessions.
        if !self.check_db() {
            self.cached_db_sessions.clear();
            self.cached_chat.clear();
            return vec![];
        }

        let pid_map = Self::find_hermes_pid_map(&shared.process_info);

        // Refresh DB rows + transcript tails on slow ticks only; reuse the
        // cache on fast ticks so we don't open/re-query SQLite every 2s.
        if shared.slow_tick {
            if let Some(rows) = self.query_sessions() {
                self.cached_db_sessions = rows;
            }
            let active_ids: Vec<String> = self
                .cached_db_sessions
                .iter()
                .filter(|ds| {
                    pid_map
                        .values()
                        .any(|key| key.as_str() == ds.id || key.as_str() == ds.session_key)
                })
                .map(|ds| ds.id.clone())
                .collect();
            if !active_ids.is_empty() {
                self.cached_chat = self.query_chat_tail(&active_ids);
            } else {
                self.cached_chat.clear();
            }
        }

        let mut sessions = Vec::new();

        for ds in &self.cached_db_sessions {
            // Match this DB session to a live worker PID by id or session key.
            let matched_pid = pid_map
                .iter()
                .find(|(_, key)| key.as_str() == ds.id || key.as_str() == ds.session_key)
                .map(|(&pid, _)| pid);
            let Some(matched_pid) = matched_pid else {
                // No running worker: session is idle/finished. MultiCollector
                // drops Done rows anyway, so just skip it (like OpenCode).
                continue;
            };

            let proc = shared.process_info.get(&matched_pid);
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

            let tail = self.cached_chat.get(&ds.id).cloned().unwrap_or_default();
            // Fall back to the DB title (itself the first user prompt) when
            // the transcript tail has no user message yet.
            let initial_prompt = if tail.initial_prompt.is_empty() {
                ds.title.clone()
            } else {
                tail.initial_prompt.clone()
            };

            let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
            let has_active_child = process::has_active_descendant(
                matched_pid,
                &shared.children_map,
                &shared.process_info,
                5.0,
            );
            let status = if tail.thinking_since_ms > 0 {
                SessionStatus::Thinking
            } else if tail.pending_since_ms > 0 {
                SessionStatus::Executing
            } else if cpu_active || has_active_child {
                SessionStatus::Thinking
            } else {
                SessionStatus::Waiting
            };

            let current_tasks = match status {
                SessionStatus::Executing => vec!["running tool".to_string()],
                SessionStatus::Thinking => vec!["thinking...".to_string()],
                _ => vec!["waiting for input".to_string()],
            };

            // Project directory: DB cwd wins, then git root, then the live
            // worker's cwd (covers TUI sessions where cwd is not persisted).
            let cwd = if !ds.cwd.is_empty() {
                ds.cwd.clone()
            } else if !ds.git_repo_root.is_empty() {
                ds.git_repo_root.clone()
            } else if let Some(cwd) = get_process_cwd(matched_pid) {
                sanitize_db_field(&cwd, 4096)
            } else if let Some(home) = dirs::home_dir() {
                home.to_string_lossy().into_owned()
            } else {
                String::new()
            };
            let project_name = if !cwd.is_empty() {
                process::last_path_segment(&cwd)
                    .filter(|seg| seg.len() >= 2)
                    .unwrap_or("hermes")
                    .to_string()
            } else {
                "hermes".to_string()
            };

            // Collect child processes with a cycle guard (visited set).
            let mut children = Vec::new();
            let mut stack: Vec<u32> = shared
                .children_map
                .get(&matched_pid)
                .cloned()
                .unwrap_or_default();
            let mut visited = HashSet::new();
            while let Some(cpid) = stack.pop() {
                if !visited.insert(cpid) {
                    continue;
                }
                if let Some(cproc) = shared.process_info.get(&cpid) {
                    let port = shared.ports.get(&cpid).and_then(|v| v.first().copied());
                    children.push(ChildProcess {
                        pid: cpid,
                        command: cproc.command.clone(),
                        mem_kb: cproc.rss_kb,
                        port,
                    });
                }
                if let Some(grandchildren) = shared.children_map.get(&cpid) {
                    stack.extend(grandchildren);
                }
            }

            let context_window =
                context_window_for_model(&ds.model, "", 0).max(model_context_window(&ds.model));
            let context_percent = if context_window > 0 {
                ((ds.total_input + ds.total_output) as f64 / context_window as f64) * 100.0
            } else {
                0.0
            };

            sessions.push(AgentSession {
                agent_cli: "hermes",
                pid: matched_pid,
                session_id: ds.id.clone(),
                cwd,
                project_name,
                started_at: ds.started_at_ms,
                status,
                model: ds.model.clone(),
                effort: String::new(),
                context_percent,
                total_input_tokens: ds.total_input,
                total_output_tokens: ds.total_output,
                total_cache_read: ds.total_cache_read,
                total_cache_create: ds.total_cache_write,
                turn_count: ds.turn_count,
                current_tasks,
                mem_mb,
                version: String::new(),
                git_branch: ds.git_branch.clone(),
                git_added: 0,
                git_modified: 0,
                token_history: vec![],
                context_history: vec![],
                compaction_count: 0,
                context_window,
                subagents: vec![],
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt,
                first_assistant_text: tail.first_assistant_text,
                chat_messages: tail.chat_messages,
                tool_calls: tail.tool_calls,
                pending_since_ms: tail.pending_since_ms,
                thinking_since_ms: tail.thinking_since_ms,
                file_accesses: vec![],
                config_root: super::abbrev_path(self.db_path.parent().unwrap_or(Path::new("."))),
            });
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }
}

impl Default for HermesCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for HermesCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

/// Build the redacted chat tail from newest-first transcript rows.
///
/// Everything that can carry user or tool data goes through the same
/// sanitize → redact → truncate pipeline used by the other collectors.
fn build_chat_tail(mut rows: Vec<MessageRow>) -> ChatTail {
    let mut tail = ChatTail::default();
    if rows.is_empty() {
        return tail;
    }

    // Newest row drives status hints.
    let newest = &rows[0];
    let last_ts_ms = secs_float_to_ms(newest.ts_secs);
    if newest.role == "user" {
        tail.thinking_since_ms = last_ts_ms;
    } else if newest.role == "assistant" && !newest.tool_calls.trim().is_empty() {
        tail.pending_since_ms = last_ts_ms;
    }

    // Oldest-first walk to pick the first prompt / first assistant text.
    rows.reverse();
    let mut chat: Vec<ChatMessage> = Vec::new();
    for row in &rows {
        match row.role.as_str() {
            "user" => {
                let text = sanitize_chat_text(&row.content);
                if !text.is_empty() {
                    if tail.initial_prompt.is_empty() {
                        tail.initial_prompt = truncate_copy(&text, 512);
                    }
                    chat.push(ChatMessage {
                        role: ChatRole::User,
                        text,
                    });
                }
            }
            "assistant" => {
                let text = sanitize_chat_text(&row.content);
                if !text.is_empty() {
                    if tail.first_assistant_text.is_empty() {
                        tail.first_assistant_text = truncate_copy(&text, 2000);
                    }
                    chat.push(ChatMessage {
                        role: ChatRole::Assistant,
                        text,
                    });
                }
                if !row.tool_calls.trim().is_empty() && tail.tool_calls.len() < MAX_TOOL_CALLS {
                    tail.tool_calls.extend(parse_tool_calls(&row.tool_calls));
                }
            }
            // "tool" rows are tool results, "session_meta" is internal:
            // neither belongs in the chat tail.
            _ => {}
        }
    }
    // Keep only the most recent MAX_CHAT_MESSAGES entries.
    let len = chat.len();
    if len > MAX_CHAT_MESSAGES {
        chat.drain(..len - MAX_CHAT_MESSAGES);
    }
    tail.chat_messages = chat;
    tail
}

/// Parse a Hermes `tool_calls` JSON array (stored on assistant rows) into
/// redacted ToolCall entries. Handles both the normalized
/// `{"function":{"name","arguments"}}` shape and bare `{"name","input"}`.
fn parse_tool_calls(raw: &str) -> Vec<ToolCall> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return vec![];
    };
    let Some(items) = value.as_array() else {
        return vec![];
    };
    let mut calls = Vec::new();
    for item in items {
        let obj = match item.as_object() {
            Some(o) => o,
            None => continue,
        };
        let name = obj
            .get("function")
            .and_then(|f| f.get("name"))
            .or_else(|| obj.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("tool");
        // Arguments come either as a JSON string (function.arguments) or as an
        // inline object (input); normalize both to a short redacted string.
        let arg = if let Some(s) = obj
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
        {
            s.to_string()
        } else if let Some(s) = obj.get("input").and_then(|i| i.as_str()) {
            s.to_string()
        } else if let Some(o) = obj.get("input").and_then(|i| i.as_object()) {
            serde_json::to_string(o).unwrap_or_default()
        } else {
            String::new()
        };
        calls.push(ToolCall {
            name: sanitize_db_field(name, 128),
            arg: truncate_copy(&sanitize_chat_text(&arg), 200),
            duration_ms: 0,
        });
    }
    calls
}

/// Context window from the Hermes model table (case-insensitive).
fn model_context_window(model: &str) -> u64 {
    let key = model.to_lowercase();
    MODEL_CONTEXT_WINDOWS
        .iter()
        .find(|(name, _)| key.contains(name))
        .map(|(_, size)| *size)
        .unwrap_or(0)
}

fn expand_home(path_str: &str) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else {
        PathBuf::from(path_str)
    }
}

/// Check if a path is a symlink (fail-closed: returns true on error).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

/// Title-style sanitization: strip control chars, truncate, then redact
/// known secret prefixes.
fn sanitize_db_title(raw: &str) -> String {
    super::redact_secrets(&sanitize_db_field(raw, 512))
}

fn sanitize_db_field(raw: &str, max_bytes: usize) -> String {
    let mut value = super::sanitize_terminal_text(raw);
    truncate_str(&mut value, max_bytes);
    value
}

/// Chat text: same pipeline, with secret redaction applied after control
/// char stripping (mirrors claude.rs transcript handling).
fn sanitize_chat_text(raw: &str) -> String {
    super::redact_secrets(&super::sanitize_terminal_text(raw.trim()))
}

fn truncate_str(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Truncate a copy of `s` at a char boundary to avoid panics on multi-byte
/// UTF-8, returning the truncated string.
fn truncate_copy(s: &str, max_bytes: usize) -> String {
    let mut value = s.to_string();
    truncate_str(&mut value, max_bytes);
    value
}

/// Hermes stores timestamps as epoch seconds (f64); the model wants ms.
fn secs_float_to_ms(secs: f64) -> u64 {
    (secs * 1000.0) as u64
}

/// Get the current working directory of a process.
/// Uses /proc on Linux, sysinfo (PEB) on Windows, lsof on macOS/other Unix.
#[cfg(target_os = "linux")]
fn get_process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{}/cwd", pid))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(target_os = "windows")]
fn get_process_cwd(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
    );
    sys.process(pid)
        .and_then(|p| p.cwd())
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
fn get_process_cwd(pid: u32) -> Option<String> {
    // -a ANDs the selection terms; without it, lsof ORs `-p <pid>` with
    // `-d cwd` and returns cwd entries for unrelated processes too.
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|l| l.starts_with('n') && l.len() > 1)
        .map(|l| l[1..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn proc(pid: u32, command: &str) -> process::ProcInfo {
        process::ProcInfo {
            pid,
            ppid: 1,
            rss_kb: 1000,
            cpu_pct: 0.0,
            command: command.to_string(),
        }
    }

    #[test]
    fn find_pid_map_parses_session_key() {
        let mut info = HashMap::new();
        info.insert(
            100,
            proc(
                100,
                "/usr/bin/python -m tui_gateway.slash_worker --session-key 20260806_123953_abcd --model deepseek-v4-flash",
            ),
        );
        info.insert(200, proc(200, "grep --session-key foo"));
        info.insert(300, proc(300, "/usr/local/bin/hermes --tui"));
        info.insert(400, proc(400, "node entry.js --session-key 123"));
        let map = HermesCollector::find_hermes_pid_map(&info);
        assert_eq!(
            map.get(&100).map(String::as_str),
            Some("20260806_123953_abcd")
        );
        assert!(!map.contains_key(&200));
        assert!(!map.contains_key(&300));
        assert!(!map.contains_key(&400));
    }

    #[test]
    fn model_context_lookup_is_case_insensitive() {
        assert_eq!(model_context_window("deepseek-v4-flash"), 1_048_576);
        assert_eq!(model_context_window("DeepSeek-V4"), 1_048_576);
        assert_eq!(model_context_window("claude-sonnet-4"), 200_000);
        assert_eq!(model_context_window("unknown-model-xyz"), 0);
    }

    #[test]
    fn sanitize_db_title_redacts_known_secret_prefixes() {
        assert_eq!(
            sanitize_db_title("debug sk-ant-api03-secret now"),
            "debug [REDACTED] now"
        );
    }

    #[test]
    fn sanitize_db_field_removes_terminal_control_chars() {
        assert_eq!(
            sanitize_db_field("proj\u{202E}\u{0008}name", 512),
            "projname"
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let mut s = "你好世界".to_string();
        truncate_str(&mut s, 5);
        // 5 bytes can only hold the first 3-byte char ("你"); never splits UTF-8.
        assert_eq!(s, "你");
    }

    #[test]
    fn query_sessions_reads_and_sanitizes_rows() {
        // Point the collector at a temp DB.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let conn_path = Connection::open(&db_path).unwrap();
        conn_path
            .execute_batch(
                "CREATE TABLE sessions (
                   id TEXT PRIMARY KEY, title TEXT, cwd TEXT, git_repo_root TEXT, git_branch TEXT,
                   model TEXT, profile_name TEXT, started_at REAL, message_count INTEGER,
                   input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER,
                   cache_write_tokens INTEGER, session_key TEXT, parent_session_id TEXT,
                   ended_at REAL, archived INTEGER
                 );",
            )
            .unwrap();
        conn_path
            .execute(
                "INSERT INTO sessions (id, title, cwd, git_branch, model, started_at, message_count,
                                       input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, session_key, ended_at, archived)
                 VALUES ('s1', 'hello sk-ant-api03-x', '/proj/a', 'main', 'deepseek-v4-flash',
                         1785991221.3, 42, 1000, 500, 200, 10, 'agent:main:email:x', NULL, 0)",
                [],
            )
            .unwrap();
        conn_path
            .execute(
                "INSERT INTO sessions (id, title, cwd, started_at, ended_at)
                 VALUES ('s2', 'finished', '/proj/b', 1000.0, 2000.0)",
                [],
            )
            .unwrap();
        drop(conn_path);

        let mut collector = HermesCollector {
            db_path,
            available: None,
            cached_db_sessions: Vec::new(),
            cached_chat: HashMap::new(),
        };
        assert!(collector.check_db());
        let rows = collector.query_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        let s = &rows[0];
        assert_eq!(s.id, "s1");
        // Redaction applied to the title at read time.
        assert_eq!(s.title, "hello [REDACTED]");
        assert_eq!(s.cwd, "/proj/a");
        assert_eq!(s.model, "deepseek-v4-flash");
        assert_eq!(s.started_at_ms, 1785991221300);
        assert_eq!(s.turn_count, 42);
        assert_eq!(s.total_input, 1000);
        assert_eq!(s.total_cache_write, 10);
    }

    #[test]
    fn chat_tail_extracts_prompt_tools_and_status_hints() {
        let rows = vec![
            // newest first
            MessageRow {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: r#"[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"/etc/passwd\"}"}}]"#.into(),
                ts_secs: 300.0,
            },
            MessageRow {
                role: "tool".into(),
                content: "file contents...".into(),
                tool_calls: String::new(),
                ts_secs: 200.0,
            },
            MessageRow {
                role: "assistant".into(),
                content: "Here is the summary sk-ant-api03-secret".into(),
                tool_calls: String::new(),
                ts_secs: 100.0,
            },
            MessageRow {
                role: "user".into(),
                content: "  read this file  ".into(),
                tool_calls: String::new(),
                ts_secs: 50.0,
            },
        ];
        let tail = build_chat_tail(rows);
        assert_eq!(tail.initial_prompt, "read this file");
        assert_eq!(tail.first_assistant_text, "Here is the summary [REDACTED]");
        assert_eq!(tail.thinking_since_ms, 0);
        assert_eq!(tail.pending_since_ms, 300_000);
        assert_eq!(tail.chat_messages.len(), 2);
        assert_eq!(tail.tool_calls.len(), 1);
        assert_eq!(tail.tool_calls[0].name, "read_file");
        assert!(tail.tool_calls[0].arg.contains("/etc/passwd"));
    }

    #[test]
    fn chat_tail_user_prompt_sets_thinking() {
        let rows = vec![MessageRow {
            role: "user".into(),
            content: "do the thing".into(),
            tool_calls: String::new(),
            ts_secs: 500.0,
        }];
        let tail = build_chat_tail(rows);
        assert_eq!(tail.thinking_since_ms, 500_000);
        assert_eq!(tail.pending_since_ms, 0);
    }

    #[test]
    fn chat_tail_empty_rows_yield_default() {
        let tail = build_chat_tail(vec![]);
        assert_eq!(tail.initial_prompt, "");
        assert_eq!(tail.thinking_since_ms, 0);
        assert!(tail.chat_messages.is_empty());
    }

    #[test]
    fn chat_tail_skips_tool_results_and_meta() {
        let rows = vec![
            MessageRow {
                role: "user".into(),
                content: "prompt".into(),
                tool_calls: String::new(),
                ts_secs: 10.0,
            },
            MessageRow {
                role: "tool".into(),
                content: "42".into(),
                tool_calls: String::new(),
                ts_secs: 9.0,
            },
            MessageRow {
                role: "session_meta".into(),
                content: "meta".into(),
                tool_calls: String::new(),
                ts_secs: 8.0,
            },
        ];
        let tail = build_chat_tail(rows);
        assert_eq!(tail.chat_messages.len(), 1);
        assert_eq!(tail.chat_messages[0].role, ChatRole::User);
    }

    #[test]
    fn parse_tool_calls_handles_both_shapes() {
        let calls = parse_tool_calls(
            r#"[{"function":{"name":"search_files","arguments":"{\"pattern\":\"*.rs\"}"}},
                 {"name":"bash","input":{"command":"ls"}}]"#,
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "search_files");
        assert_eq!(calls[1].name, "bash");
        assert!(calls[1].arg.contains("ls"));
    }

    #[test]
    fn expand_home_replaces_tilde() {
        let p = expand_home("~/.hermes/state.db");
        assert!(p.to_string_lossy().contains(".hermes"));
        assert!(p.is_absolute());
        let plain = expand_home("/var/lib/state.db");
        assert_eq!(plain.to_string_lossy(), "/var/lib/state.db");
    }
}
