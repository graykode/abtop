use super::{process, context_window_for_model};
use crate::model::{AgentSession, ChildProcess, SessionStatus, ChatMessage, ToolCall, ChatRole};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum sessions to fetch from the DB per query.
const MAX_SESSIONS: u32 = 50;

/// Collector for OpenCode sessions.
///
/// Discovery strategy:
/// 1. `ps` to find running opencode processes (from shared process data)
/// 2. Query SQLite DB at ~/.local/share/opencode/opencode.db via `sqlite3` CLI
/// 3. Match running PIDs to sessions by cwd
///
/// Uses `sqlite3 -readonly -json` for safe concurrent reads (WAL mode).
/// DB rows are cached and only refreshed on `shared.slow_tick` (every ~10s)
/// OR when the database file/WAL modification time changes, enabling instant updates.
pub struct OpenCodeCollector {
    db_path: PathBuf,
    /// Whether sqlite3 CLI is available (checked once).
    sqlite3_available: Option<bool>,
    /// Cached DB rows from the last slow-tick or mtime-change query. Reused on fast ticks.
    cached_db_sessions: Vec<DbSession>,
    /// Cached DB subagent rows from the last slow-tick query.
    cached_db_subagents: Vec<DbSubAgent>,
    /// Cached chat messages by session ID.
    cached_chat_messages: HashMap<String, Vec<ChatMessage>>,
    /// Cached tool calls by session ID.
    cached_tool_calls: HashMap<String, Vec<ToolCall>>,
    /// Last modification time of the DB file.
    last_db_mtime: Option<std::time::SystemTime>,
    /// Last modification time of the WAL file.
    last_wal_mtime: Option<std::time::SystemTime>,
    /// Whether the "sqlite3 missing" warning has been emitted (once).
    #[cfg(target_os = "windows")]
    warned_sqlite3_missing: bool,
}

impl OpenCodeCollector {
    pub fn new() -> Self {
        let data_dir = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
        let db_path = resolve_db_path(&data_dir);
        #[cfg(target_os = "windows")]
        let db_path = windows_db_path(db_path);
        Self {
            db_path,
            sqlite3_available: None,
            cached_db_sessions: Vec::new(),
            cached_db_subagents: Vec::new(),
            cached_chat_messages: HashMap::new(),
            cached_tool_calls: HashMap::new(),
            last_db_mtime: None,
            last_wal_mtime: None,
            #[cfg(target_os = "windows")]
            warned_sqlite3_missing: false,
        }
    }

    fn check_sqlite3(&mut self) -> bool {
        if let Some(available) = self.sqlite3_available {
            return available;
        }
        let available = Command::new("sqlite3").arg("--version").output().is_ok();
        self.sqlite3_available = Some(available);
        available
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // If the database path doesn't exist, try resolving it again in case it was created since startup
        if !self.db_path.exists() {
            let data_dir = std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
            let db_path = resolve_db_path(&data_dir);
            #[cfg(target_os = "windows")]
            let db_path = windows_db_path(db_path);
            self.db_path = db_path;
        }

        // Security: skip if db_path is a symlink (fail-closed)
        if is_symlink(&self.db_path) || !self.db_path.exists() {
            self.cached_db_sessions.clear();
            self.cached_db_subagents.clear();
            self.cached_chat_messages.clear();
            self.cached_tool_calls.clear();
            return vec![];
        }
        if !self.check_sqlite3() {
            // The DB exists but we can't read it: on Windows sqlite3 is
            // usually not preinstalled, so say why sessions are missing
            // instead of failing silently.
            #[cfg(target_os = "windows")]
            if !self.warned_sqlite3_missing {
                self.warned_sqlite3_missing = true;
                eprintln!(
                    "abtop: OpenCode database found at {} but the `sqlite3` CLI is not on PATH; \
                     OpenCode sessions will not appear. Install it (e.g. `winget install SQLite.SQLite`) \
                     and restart abtop.",
                    self.db_path.display()
                );
            }
            self.cached_db_sessions.clear();
            self.cached_db_subagents.clear();
            self.cached_chat_messages.clear();
            self.cached_tool_calls.clear();
            return vec![];
        }

        // Find running opencode PIDs and their commands for cwd matching
        let opencode_pids = Self::find_opencode_pids(&shared.process_info);
        let pid_commands: HashMap<u32, &str> = opencode_pids
            .iter()
            .filter_map(|&pid| {
                shared
                    .process_info
                    .get(&pid)
                    .map(|p| (pid, p.command.as_str()))
            })
            .collect();

        // Check if DB or WAL modification times changed
        let mut db_changed = false;
        if let Ok(meta) = std::fs::metadata(&self.db_path) {
            if let Ok(mtime) = meta.modified() {
                if self.last_db_mtime.is_none_or(|last| mtime > last) {
                    self.last_db_mtime = Some(mtime);
                    db_changed = true;
                }
            }
        }
        let wal_path = self.db_path.with_extension("db-wal");
        if let Ok(meta) = std::fs::metadata(&wal_path) {
            if let Ok(mtime) = meta.modified() {
                if self.last_wal_mtime.is_none_or(|last| mtime > last) {
                    self.last_wal_mtime = Some(mtime);
                    db_changed = true;
                }
            }
        }

        // Refresh DB rows on slow ticks or when the database actually changed
        if shared.slow_tick || db_changed {
            if let Some(rows) = self.query_sessions() {
                self.cached_db_sessions = rows;
            }
            if let Some(sub_rows) = self.query_subagents() {
                self.cached_db_subagents = sub_rows;
            }

            let sids: Vec<String> = self.cached_db_sessions.iter().map(|s| s.id.clone()).collect();
            if let Some(parts_map) = self.query_parts(&sids) {
                self.cached_chat_messages.clear();
                self.cached_tool_calls.clear();
                for (sid, (chat, tools)) in parts_map {
                    self.cached_chat_messages.insert(sid.clone(), chat);
                    self.cached_tool_calls.insert(sid, tools);
                }
            }
        }

        let now_ms = current_time_ms();
        let mut sessions = Vec::new();

        let mut claimed_pids = HashSet::new();
        for ds in &self.cached_db_sessions {
            let matched_pid =
                Self::match_pid_to_session_once(&pid_commands, &ds.directory, &mut claimed_pids);
            // Drop sessions whose process isn't running. (Done sessions are
            // filtered out by MultiCollector::collect anyway, so emitting
            // a Done row here would be dead code.)
            let Some(matched_pid) = matched_pid else {
                continue;
            };

            let proc = shared.process_info.get(&matched_pid);
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

            // Precise status derivation from database message history:
            // 1. If last message role is "user", model is thinking.
            // 2. If last message role is "assistant" but completed time is not set, model is thinking.
            // 3. Fallback to CPU usage check if a tool is executing but DB hasn't been committed yet.
            let status = if ds.last_role == "user"
                || (ds.last_role == "assistant" && ds.last_completed.is_none())
            {
                SessionStatus::Thinking
            } else {
                let cpu_active = proc.is_some_and(|p| p.cpu_pct > 5.0);
                let has_active_child = process::has_active_descendant(
                    matched_pid,
                    &shared.children_map,
                    &shared.process_info,
                    10.0,
                );
                if cpu_active || has_active_child {
                    SessionStatus::Thinking
                } else {
                    SessionStatus::Waiting
                }
            };

            let project_name = if !ds.project_name.is_empty() {
                ds.project_name.clone()
            } else {
                // last_path_segment also splits on `\` on Windows.
                process::last_path_segment(&ds.directory)
                    .unwrap_or("?")
                    .to_string()
            };

            let current_tasks = if matches!(status, SessionStatus::Waiting) {
                vec!["waiting for input".to_string()]
            } else {
                vec!["thinking...".to_string()]
            };

            // Collect child processes with cycle guard (visited set)
            let mut children = Vec::new();
            let mut stack: Vec<u32> = shared
                .children_map
                .get(&matched_pid)
                .cloned()
                .unwrap_or_default();
            let mut visited = std::collections::HashSet::new();
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

            let model = if !ds.provider.is_empty() && !ds.model.is_empty() {
                format!("{}/{}", ds.provider, ds.model)
            } else if !ds.model.is_empty() {
                ds.model.clone()
            } else {
                "-".to_string()
            };

            // Fetch the context limit from the local configuration file (~/.config/opencode/opencode.jsonc)
            // falling back to context_window_for_model if missing.
            let context_window = get_context_window_from_config(&ds.provider, &ds.model)
                .unwrap_or_else(|| context_window_for_model(&model, "", 0));

            // Calculate context percent on active context size (latest turn's tokens.total), not cumulative sum.
            let context_percent = if context_window > 0 {
                (ds.last_total_tokens as f64 / context_window as f64) * 100.0
            } else {
                0.0
            };

            let chat_messages = self.cached_chat_messages.get(&ds.id).cloned().unwrap_or_default();
            let tool_calls = self.cached_tool_calls.get(&ds.id).cloned().unwrap_or_default();

            sessions.push(AgentSession {
                agent_cli: "opencode",
                pid: matched_pid,
                session_id: ds.id.clone(),
                cwd: ds.directory.clone(),
                project_name,
                started_at: ds.time_created,
                status,
                model,
                effort: String::new(),
                context_percent,
                total_input_tokens: ds.total_input,
                total_output_tokens: ds.total_output,
                total_cache_read: ds.total_cache_read,
                total_cache_create: ds.total_cache_write,
                turn_count: ds.turn_count,
                current_tasks,
                mem_mb,
                version: ds.version.clone(),
                git_branch: get_git_branch(&ds.directory),
                git_added: 0,
                git_modified: 0,
                token_history: vec![],
                context_history: vec![],
                compaction_count: 0,
                context_window,
                subagents: {
                    let mut subagents = Vec::new();
                    for sub in &self.cached_db_subagents {
                        if sub.parent_id == ds.id {
                            let age_ms = now_ms.saturating_sub(sub.time_updated);
                            let since_update_secs = age_ms / 1000;
                            let status = if since_update_secs < 30 {
                                "working".to_string()
                            } else {
                                "done".to_string()
                            };
                            let mut name = sub.title.clone();
                            truncate_field(&mut name, 30);
                            subagents.push(crate::model::SubAgent {
                                name,
                                status,
                                tokens: sub.tokens,
                            });
                        }
                    }
                    subagents
                },
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt: ds.title.clone(),
                first_assistant_text: {
                    chat_messages.iter()
                        .find(|m| m.role == ChatRole::Assistant)
                        .map(|m| m.text.clone())
                        .unwrap_or_default()
                },
                chat_messages,
                tool_calls,
                pending_since_ms: 0,
                thinking_since_ms: 0,
                file_accesses: vec![],
                config_root: super::abbrev_path(
                    self.db_path.parent().unwrap_or(std::path::Path::new(".")),
                ),
            });
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Find running opencode processes, excluding subagent processes
    /// (descendants that are also binaries of name "opencode").
    fn find_opencode_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        let mut pids = Vec::new();
        for (&pid, info) in process_info {
            if process::cmd_has_binary(&info.command, "opencode") && !info.command.contains("grep") {
                // Traverse ancestor chain to verify it is the root opencode process
                let mut is_subagent = false;
                let mut curr_ppid = info.ppid;
                while curr_ppid > 1 {
                    if let Some(parent_info) = process_info.get(&curr_ppid) {
                        if process::cmd_has_binary(&parent_info.command, "opencode") {
                            is_subagent = true;
                            break;
                        }
                        curr_ppid = parent_info.ppid;
                    } else {
                        break;
                    }
                }
                if !is_subagent {
                    pids.push(pid);
                }
            }
        }
        pids
    }

    /// Match a running PID to a session by comparing its working directory
    /// with the DB session's `directory`, falling back to a command-line
    /// substring match. Returns `None` if no PID's cwd or command line ties
    /// to this session.
    #[cfg(test)]
    fn match_pid_to_session(pid_commands: &HashMap<u32, &str>, session_dir: &str) -> Option<u32> {
        Self::match_pid_to_session_excluding(pid_commands, session_dir, &HashSet::new())
    }

    fn match_pid_to_session_once(
        pid_commands: &HashMap<u32, &str>,
        session_dir: &str,
        claimed_pids: &mut HashSet<u32>,
    ) -> Option<u32> {
        let pid = Self::match_pid_to_session_excluding(pid_commands, session_dir, claimed_pids)?;
        claimed_pids.insert(pid);
        Some(pid)
    }

    fn match_pid_to_session_excluding(
        pid_commands: &HashMap<u32, &str>,
        session_dir: &str,
        claimed_pids: &HashSet<u32>,
    ) -> Option<u32> {
        // Empty / single-character `session_dir` (e.g. "" or "/") would
        // make the substring fallback match unrelated commands, so skip
        // matching entirely in that case.
        if session_dir.len() < 2 {
            return None;
        }
        for (&pid, &cmd) in pid_commands {
            if claimed_pids.contains(&pid) {
                continue;
            }
            if let Some(cwd) = get_process_cwd(pid) {
                if paths_equal(&cwd, session_dir) {
                    return Some(pid);
                }
            }
            if cmd.contains(session_dir) {
                return Some(pid);
            }
        }
        None
    }

    /// Run a single sqlite3 query and parse the JSON output.
    fn run_query(&self, sql: &str) -> Option<Vec<Value>> {
        let db = self.db_path.to_str()?;
        let output = Command::new("sqlite3")
            .args(["-readonly", "-json", db])
            .arg(sql)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Some(vec![]);
        }
        serde_json::from_str(stdout.trim()).ok()
    }

    fn query_sessions(&self) -> Option<Vec<DbSession>> {
        let session_sql = format!(
            r#"
SELECT
  s.id, s.title, s.directory, s.version, s.time_created, s.time_updated,
  COALESCE(p.name, '') as project_name,
  COUNT(m.id) as turn_count,
  COALESCE(SUM(json_extract(m.data, '$.tokens.input')), 0) as total_input,
  COALESCE(SUM(json_extract(m.data, '$.tokens.output')), 0) as total_output,
  COALESCE(SUM(json_extract(m.data, '$.tokens.cache.read')), 0) as total_cache_read,
  COALESCE(SUM(json_extract(m.data, '$.tokens.cache.write')), 0) as total_cache_write
FROM session s
LEFT JOIN project p ON s.project_id = p.id
LEFT JOIN message m ON m.session_id = s.id
  AND json_extract(m.data, '$.role') = 'assistant'
GROUP BY s.id
ORDER BY s.time_updated DESC
LIMIT {};"#,
            MAX_SESSIONS
        );

        let model_sql = format!(
            r#"
SELECT
  s.id,
  COALESCE((SELECT json_extract(m2.data, '$.modelID')
    FROM message m2 WHERE m2.session_id = s.id
    AND json_extract(m2.data, '$.role') = 'assistant'
    ORDER BY m2.time_created DESC LIMIT 1), '') as model,
  COALESCE((SELECT json_extract(m2.data, '$.providerID')
    FROM message m2 WHERE m2.session_id = s.id
    AND json_extract(m2.data, '$.role') = 'assistant'
    ORDER BY m2.time_created DESC LIMIT 1), '') as provider,
  COALESCE((SELECT json_extract(m2.data, '$.role')
    FROM message m2 WHERE m2.session_id = s.id
    ORDER BY m2.time_created DESC LIMIT 1), '') as last_role,
  (SELECT json_extract(m2.data, '$.time.completed')
    FROM message m2 WHERE m2.session_id = s.id
    ORDER BY m2.time_created DESC LIMIT 1) as last_completed,
  COALESCE((SELECT json_extract(m2.data, '$.tokens.total')
    FROM message m2 WHERE m2.session_id = s.id
    AND json_extract(m2.data, '$.role') = 'assistant'
    AND json_extract(m2.data, '$.tokens.total') IS NOT NULL
    ORDER BY m2.time_created DESC LIMIT 1), 0) as last_total_tokens
FROM session s
ORDER BY s.time_updated DESC
LIMIT {};"#,
            MAX_SESSIONS
        );

        // Two separate invocations to avoid fragile concatenated JSON parsing
        let rows = self.run_query(&session_sql)?;
        let model_rows = self.run_query(&model_sql).unwrap_or_default();

        // Build model lookup by session id
        let mut model_map: HashMap<String, (String, String, String, Option<u64>, u64)> = HashMap::new();
        for mr in &model_rows {
            if let Some(id) = mr["id"].as_str() {
                model_map.insert(
                    id.to_string(),
                    (
                        sanitize_db_field(mr["model"].as_str().unwrap_or(""), 256),
                        sanitize_db_field(mr["provider"].as_str().unwrap_or(""), 256),
                        sanitize_db_field(mr["last_role"].as_str().unwrap_or(""), 64),
                        mr["last_completed"].as_u64(),
                        mr["last_total_tokens"].as_u64().unwrap_or(0),
                    ),
                );
            }
        }

        let mut sessions = Vec::new();
        for row in rows {
            let id = row["id"].as_str().unwrap_or("").to_string();
            let (model, provider, last_role, last_completed, last_total_tokens) = model_map
                .remove(&id)
                .unwrap_or_else(|| ("".to_string(), "".to_string(), "".to_string(), None, 0));

            // Sanitize DB-sourced strings before they reach the TUI/JSON snapshot.
            let title = sanitize_db_title(row["title"].as_str().unwrap_or(""));
            let directory = sanitize_db_field(row["directory"].as_str().unwrap_or(""), 4096);
            let version = sanitize_db_field(row["version"].as_str().unwrap_or(""), 64);
            let project_name = sanitize_db_field(row["project_name"].as_str().unwrap_or(""), 256);

            sessions.push(DbSession {
                id,
                title,
                directory,
                version,
                // time_created is in milliseconds since epoch
                time_created: row["time_created"].as_u64().unwrap_or(0),
                project_name,
                turn_count: row["turn_count"].as_u64().unwrap_or(0) as u32,
                total_input: row["total_input"].as_u64().unwrap_or(0),
                total_output: row["total_output"].as_u64().unwrap_or(0),
                total_cache_read: row["total_cache_read"].as_u64().unwrap_or(0),
                total_cache_write: row["total_cache_write"].as_u64().unwrap_or(0),
                model,
                provider,
                last_role,
                last_completed,
                last_total_tokens,
            });
        }

        Some(sessions)
    }

    fn query_subagents(&self) -> Option<Vec<DbSubAgent>> {
        let sql = format!(
            r#"
SELECT 
  id, parent_id, title,
  (tokens_input + tokens_output + tokens_cache_read + tokens_cache_write) as tokens,
  time_updated
FROM session
WHERE parent_id IS NOT NULL AND parent_id != ''
LIMIT {};"#,
            MAX_SESSIONS
        );
        let rows = self.run_query(&sql)?;
        let mut subagents = Vec::new();
        for row in rows {
            let parent_id = row["parent_id"].as_str().unwrap_or("").to_string();
            let title = sanitize_db_title(row["title"].as_str().unwrap_or(""));
            let tokens = row["tokens"].as_u64().unwrap_or(0);
            let time_updated = row["time_updated"].as_u64().unwrap_or(0);
            subagents.push(DbSubAgent {
                parent_id,
                title,
                tokens,
                time_updated,
            });
        }
        Some(subagents)
    }

    /// Query dialogue text messages and tool calls for active sessions.
    #[allow(clippy::type_complexity)]
    fn query_parts(&self, session_ids: &[String]) -> Option<HashMap<String, (Vec<ChatMessage>, Vec<ToolCall>)>> {
        if session_ids.is_empty() {
            return Some(HashMap::new());
        }
        let formatted_ids: Vec<String> = session_ids.iter().map(|id| format!("'{}'", id)).collect();
        let sql = format!(
            r#"
SELECT
  p.session_id,
  json_extract(m.data, '$.role') as role,
  p.data as part_data
FROM part p
JOIN message m ON p.message_id = m.id
WHERE p.session_id IN ({})
ORDER BY p.time_created ASC;"#,
            formatted_ids.join(",")
        );

        let rows = self.run_query(&sql)?;
        let mut map: HashMap<String, (Vec<ChatMessage>, Vec<ToolCall>)> = HashMap::new();
        for id in session_ids {
            map.insert(id.clone(), (Vec::new(), Vec::new()));
        }

        for row in rows {
            let session_id = row["session_id"].as_str().unwrap_or("").to_string();
            let role_str = row["role"].as_str().unwrap_or("");
            let part_data_str = row["part_data"].as_str().unwrap_or("");

            if let Some((chat, tools)) = map.get_mut(&session_id) {
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(part_data_str) {
                    let part_type = obj["type"].as_str().unwrap_or("");
                    if part_type == "text" || part_type == "reasoning" {
                        if let Some(text) = obj["text"].as_str() {
                            if !text.trim().is_empty() {
                                let role = if role_str == "user" {
                                    ChatRole::User
                                } else {
                                    ChatRole::Assistant
                                };
                                let redacted_text = super::redact_secrets(&super::sanitize_terminal_text(text));
                                chat.push(ChatMessage {
                                    role,
                                    text: redacted_text,
                                });
                            }
                        }
                    } else if part_type == "tool" {
                        let name = obj["tool"].as_str().unwrap_or("").to_string();
                        if !name.is_empty() {
                            let mut arg = String::new();
                            if let Some(input) = obj["state"]["input"].as_object() {
                                if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                                    arg = cmd.to_string();
                                } else if let Some(path) = input.get("filePath").and_then(|v| v.as_str()) {
                                    arg = path.to_string();
                                } else if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                                    arg = path.to_string();
                                } else if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
                                    arg = pattern.to_string();
                                } else if let Some(desc) = input.get("description").and_then(|v| v.as_str()) {
                                    arg = desc.to_string();
                                } else if !input.is_empty() {
                                    if let Some(first_val) = input.values().next().and_then(|v| v.as_str()) {
                                        arg = first_val.to_string();
                                    }
                                }
                            }
                            let start = obj["state"]["time"]["start"].as_u64().unwrap_or(0);
                            let end = obj["state"]["time"]["end"].as_u64().unwrap_or(0);
                            let duration_ms = end.saturating_sub(start);
                            tools.push(ToolCall {
                                name,
                                arg,
                                duration_ms,
                            });
                        }
                    }
                }
            }
        }

        Some(map)
    }
}

impl Default for OpenCodeCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for OpenCodeCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

struct DbSubAgent {
    parent_id: String,
    title: String,
    tokens: u64,
    time_updated: u64,
}

fn resolve_db_path(data_dir: &Path) -> PathBuf {
    let base_dir = data_dir.join("opencode");
    let default = base_dir.join("opencode.db");
    if default.exists() {
        return default;
    }
    if let Ok(entries) = fs::read_dir(&base_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "db") {
                return path;
            }
        }
    }
    default
}

fn get_git_branch(cwd: &str) -> String {
    let git_dir = Path::new(cwd).join(".git");
    if !git_dir.exists() {
        return String::new();
    }
    let head_file = git_dir.join("HEAD");
    if let Ok(content) = fs::read_to_string(head_file) {
        let content = content.trim();
        if let Some(ref_path) = content.strip_prefix("ref: ") {
            if let Some(branch_name) = ref_path.rsplit('/').next() {
                return branch_name.to_string();
            }
        } else if content.len() >= 7 {
            return content[..7].to_string();
        }
    }
    String::new()
}

/// Strip jsonc single-line and multi-line comments.
fn strip_jsonc_comments(content: &str) -> String {
    let mut clean = String::new();
    let mut in_block_comment = false;
    let mut in_string = false;
    let mut escaped = false;
    
    for line in content.lines() {
        let mut line_clean = String::new();
        let mut chars = line.chars().peekable();
        
        while let Some(c) = chars.next() {
            if in_block_comment {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_comment = false;
                }
            } else if in_string {
                line_clean.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    in_string = false;
                }
            } else if c == '"' {
                in_string = true;
                line_clean.push(c);
            } else if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                in_block_comment = true;
            } else if c == '/' && chars.peek() == Some(&'/') {
                break;
            } else {
                line_clean.push(c);
            }
        }
        
        if !in_block_comment {
            clean.push_str(&line_clean);
            clean.push('\n');
        }
    }
    clean
}

/// Resolve context limit from opencode.jsonc config file.
fn get_context_window_from_config(provider: &str, model_id: &str) -> Option<u64> {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .map(|h| h.join(".config"))
                .unwrap_or_default()
        });
    #[allow(unused_mut)]
    let mut config_path = config_dir.join("opencode").join("opencode.jsonc");
    
    #[cfg(target_os = "windows")]
    if !config_path.exists() {
        for var in ["LOCALAPPDATA", "APPDATA"] {
            if let Ok(base) = std::env::var(var) {
                if !base.is_empty() {
                    let candidate = PathBuf::from(base).join("opencode").join("opencode.jsonc");
                    if candidate.exists() {
                        config_path = candidate;
                        break;
                    }
                }
            }
        }
    }

    if !config_path.exists() {
        return None;
    }
    let content = fs::read_to_string(&config_path).ok()?;
    let clean_json = strip_jsonc_comments(&content);
    let val: serde_json::Value = serde_json::from_str(&clean_json).ok()?;
    
    if let Some(context_limit) = val["provider"][provider]["models"][model_id]["limit"]["context"].as_u64() {
        return Some(context_limit);
    }
    
    None
}

struct DbSession {
    id: String,
    title: String,
    directory: String,
    version: String,
    time_created: u64,
    project_name: String,
    turn_count: u32,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_write: u64,
    model: String,
    provider: String,
    last_role: String,
    last_completed: Option<u64>,
    last_total_tokens: u64,
}

/// Check if a path is a symlink (fail-closed: returns true on error).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

fn sanitize_db_title(raw: &str) -> String {
    super::redact_secrets(&sanitize_db_field(raw, 512))
}

fn sanitize_db_field(raw: &str, max_bytes: usize) -> String {
    let mut value = super::sanitize_terminal_text(raw);
    truncate_field(&mut value, max_bytes);
    value
}

/// Truncate a string at a char boundary to avoid panics on multi-byte UTF-8.
fn truncate_field(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Compare a process cwd with a DB session directory.
/// On Windows paths are case-insensitive and may mix `/` and `\`, so
/// normalize before comparing; elsewhere keep the exact comparison.
#[cfg(target_os = "windows")]
fn paths_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

#[cfg(not(target_os = "windows"))]
fn paths_equal(a: &str, b: &str) -> bool {
    a == b
}

/// On Windows, OpenCode builds (e.g. installed via npm) have been observed to
/// keep the XDG-style `~/.local/share/opencode` layout, so prefer the same
/// path as unix; fall back to probing `%LOCALAPPDATA%` / `%APPDATA%` in case
/// a build stores the DB there instead.
#[cfg(target_os = "windows")]
fn windows_db_path(default: PathBuf) -> PathBuf {
    if default.exists() {
        return default;
    }
    for var in ["LOCALAPPDATA", "APPDATA"] {
        if let Ok(base) = std::env::var(var) {
            if base.is_empty() {
                continue;
            }
            let candidate = PathBuf::from(base).join("opencode").join("opencode.db");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    default
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
    // `lsof` does not exist on Windows; sysinfo reads the cwd from the
    // process PEB. Refresh just this one PID — this runs only for the
    // handful of opencode PIDs, once per tick.
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
    // lsof -Fn output: lines starting with 'n' contain the path
    stdout
        .lines()
        .find(|l| l.starts_with('n') && l.len() > 1)
        .map(|l| l[1..].to_string())
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_opencode_pids() {
        let mut info = HashMap::new();
        info.insert(
            100,
            process::ProcInfo {
                pid: 100,
                ppid: 1,
                rss_kb: 1000,
                cpu_pct: 0.0,
                command: "/home/user/.opencode/bin/opencode".to_string(),
            },
        );
        info.insert(
            200,
            process::ProcInfo {
                pid: 200,
                ppid: 1,
                rss_kb: 500,
                cpu_pct: 0.0,
                command: "grep opencode".to_string(),
            },
        );
        info.insert(
            300,
            process::ProcInfo {
                pid: 300,
                ppid: 1,
                rss_kb: 800,
                cpu_pct: 0.0,
                command: "node /usr/bin/opencode run test".to_string(),
            },
        );
        // Add a subagent process of PID 100 (should be filtered out)
        info.insert(
            400,
            process::ProcInfo {
                pid: 400,
                ppid: 100,
                rss_kb: 300,
                cpu_pct: 0.0,
                command: "/home/user/.opencode/bin/opencode subagent".to_string(),
            },
        );

        let pids = OpenCodeCollector::find_opencode_pids(&info);
        assert!(pids.contains(&100));
        assert!(!pids.contains(&200)); // grep excluded
        assert!(pids.contains(&300));
        assert!(!pids.contains(&400)); // subagent excluded (parent is opencode)
        assert_eq!(pids.len(), 2);
    }

    #[test]
    fn test_db_path_default() {
        let collector = OpenCodeCollector::new();
        let path_str = collector.db_path.to_string_lossy();
        assert!(path_str.contains("opencode"));
        assert!(path_str.ends_with("opencode.db"));
    }

    #[test]
    fn sanitize_db_field_removes_terminal_control_chars() {
        assert_eq!(
            sanitize_db_field("proj\u{202E}\u{0008}name", 512),
            "projname"
        );
    }

    #[test]
    fn sanitize_db_title_redacts_known_secret_prefixes() {
        assert_eq!(
            sanitize_db_title("debug sk-ant-secret-value now"),
            "debug [REDACTED] now"
        );
    }

    #[test]
    fn match_pid_short_session_dir_never_matches() {
        // Regression: previously, an empty `directory` made `cmd.contains("")`
        // always true, and `/` matched every absolute path. Both should fail
        // the length guard now, regardless of how many opencode procs run.
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        pid_commands.insert(100, "/usr/local/bin/opencode");
        pid_commands.insert(200, "/usr/local/bin/opencode --foo");
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, ""),
            None
        );
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, "/"),
            None
        );
    }

    #[test]
    fn match_pid_no_last_resort_when_cwd_and_cmdline_disagree() {
        // Regression: previously, when exactly one opencode process was
        // running, the `pid_commands.len() == 1` last-resort branch matched
        // every DB session to it, even when neither the cwd nor the command
        // line matched. With the fallback removed, this returns None.
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        // u32::MAX is a synthetic PID that has no /proc/<pid>/cwd entry,
        // so the cwd branch can't accidentally succeed.
        pid_commands.insert(u32::MAX, "/usr/local/bin/opencode");
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, "/home/u/proj-a"),
            None
        );
    }

    #[test]
    fn match_pid_substring_fallback_still_works() {
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        pid_commands.insert(u32::MAX, "node /usr/bin/opencode run --cwd=/home/u/proj-a");
        assert_eq!(
            OpenCodeCollector::match_pid_to_session(&pid_commands, "/home/u/proj-a"),
            Some(u32::MAX)
        );
    }

    #[test]
    fn match_pid_to_session_once_does_not_reuse_pid_for_old_rows() {
        let mut pid_commands: HashMap<u32, &str> = HashMap::new();
        pid_commands.insert(u32::MAX, "node /usr/bin/opencode run --cwd=/home/u/proj-a");
        let mut claimed_pids = std::collections::HashSet::new();

        assert_eq!(
            OpenCodeCollector::match_pid_to_session_once(
                &pid_commands,
                "/home/u/proj-a",
                &mut claimed_pids,
            ),
            Some(u32::MAX)
        );
        assert_eq!(
            OpenCodeCollector::match_pid_to_session_once(
                &pid_commands,
                "/home/u/proj-a",
                &mut claimed_pids,
            ),
            None
        );
    }
}
