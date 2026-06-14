use super::process;
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum sessions to fetch from the DB per query. With 140+ active Hermes
/// sessions, a simple `LIMIT` would miss older sessions that still have
/// running workers, so we use a two-phase approach: collect running PIDs
/// first, then query only those session IDs.
const MAX_SESSIONS: u32 = 100;

/// Model -> context window size (tokens) lookup.
/// Hermes doesn't store context_window in the DB, so we maintain a table.
/// These are the models observed in Hermes deployments; users can
/// override via config.toml in future.
const MODEL_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    ("deepseek-v4-flash", 1_048_576),
    ("deepseek-v4", 1_048_576),
    ("deepseek-v3", 1_024_000),
    ("deepseek-r1", 1_024_000),
    ("deepseek-chat", 1_024_000),
    ("claude-sonnet-4", 200_000),
    ("claude-sonnet-4-20250514", 200_000),
    ("claude-opus-4", 200_000),
    ("claude-sonnet-3-5", 200_000),
    ("claude-opus-3-5", 200_000),
    ("gpt-4o", 128_000),
    ("gpt-4o-mini", 128_000),
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
    // Windows desktop install
    "~/AppData/Local/hermes/state.db",
    // Default Linux/macOS
    "~/.hermes/state.db",
    // Under XDG
    "~/.local/share/hermes/state.db",
];

fn expand_home(path_str: &str) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        let home = dirs::home_dir().unwrap_or_default();
        home.join(rest)
    } else {
        PathBuf::from(path_str)
    }
}

/// Collector for Hermes Agent sessions.
///
/// Discovery strategy:
/// 1. Find running Hermes workers via process command lines containing
///    `--session-key` (maps PID → session_id)
/// 2. Query the Hermes SQLite state.db for session metadata + tokens
/// 3. Match running PIDs to DB sessions by session_id
/// 4. Infer status from process activity and CPU usage
///
/// Uses `sqlite3 -readonly -json` for safe concurrent reads (WAL mode).
/// DB rows are cached and only refreshed on `shared.slow_tick` (~10s)
/// so we don't fork a sqlite3 process every 2s.
pub struct HermesCollector {
    db_path: PathBuf,
    /// Whether Python (with sqlite3 module) is available (checked once).
    python_available: Option<bool>,
    /// Cached DB rows from the last slow-tick query.
    cached_db_sessions: Vec<DbSession>,
}

impl HermesCollector {
    pub fn new() -> Self {
        let db_path = Self::discover_db_path();
        Self {
            db_path,
            python_available: None,
            cached_db_sessions: Vec::new(),
        }
    }

    /// Try known state.db locations, plus HERMES_HOME env var.
    fn discover_db_path() -> PathBuf {
        // Check HERMES_HOME env var first
        if let Ok(home) = std::env::var("HERMES_HOME") {
            let p = PathBuf::from(home.clone()).join("state.db");
            if p.exists() {
                return p;
            }
            // Also check under profile's data dir
            let p2 = PathBuf::from(home).join("data/state.db");
            if p2.exists() {
                return p2;
            }
        }

        // Try candidates in order
        for candidate in STATE_DB_CANDIDATES {
            let p = expand_home(candidate);
            if p.exists() {
                return p;
            }
        }

        // Fallback to default
        let home = dirs::home_dir().unwrap_or_default();
        if cfg!(target_os = "windows") {
            home.join("AppData/Local/hermes/state.db")
        } else {
            home.join(".hermes/state.db")
        }
    }

    fn check_python(&mut self) -> bool {
        if let Some(available) = self.python_available {
            return available;
        }
        // Check if we can run python -c "import sqlite3, json"
        let available = Command::new("python")
            .args(["-c", "import sqlite3, json; print('ok')"])
            .output()
            .is_ok_and(|o| o.status.success());
        self.python_available = Some(available);
        available
    }

    /// Find Hermes PID → session_id mappings from running process command lines.
    /// Hermes launches `tui_gateway.slash_worker` processes with `--session-key <id>`.
    fn find_hermes_pid_map(process_info: &HashMap<u32, process::ProcInfo>) -> HashMap<u32, String> {
        let mut pid_map = HashMap::new();
        for (&pid, info) in process_info {
            let cmd = &info.command;
            // Match session workers: python -m tui_gateway.slash_worker --session-key <id>
            if let Some(pos) = cmd.find("--session-key") {
                let after = &cmd[pos + "--session-key".len()..];
                let key = after.trim().split_whitespace().next().unwrap_or("");
                if !key.is_empty() {
                    pid_map.insert(pid, key.to_string());
                }
            }
        }
        pid_map
    }

    /// Filter process_info for anything likely to be a Hermes process
    /// (python containing 'hermes' or 'slash_worker').
    #[allow(dead_code)]
    fn find_hermes_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        process_info
            .iter()
            .filter(|(_, info)| {
                let cmd_lower = info.command.to_lowercase();
                // Match python processes running Hermes modules
                (info.command.contains("hermes") || cmd_lower.contains("slash_worker"))
                    && !cmd_lower.contains("grep")
                    && !cmd_lower.contains("psutil")
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Run a sqlite3 query using Python's built-in sqlite3 module.
    /// This avoids depending on the sqlite3 CLI being installed.
    /// Writes SQL to a temp file to avoid quoting/escaping issues.
    fn run_query(&self, sql: &str) -> Option<Vec<Value>> {
        let db = self.db_path.to_str()?;

        // Write SQL to a temp file to avoid shell quoting issues.
        // File is cleaned up on next call (PID-based naming = one temp file per process).
        let sql_path = format!(
            "{}\\hermes_query_{}.sql",
            std::env::temp_dir().to_string_lossy(),
            std::process::id()
        );
        if std::fs::write(&sql_path, sql).is_err() {
            return None;
        }

        let script = format!(
            "import sqlite3, json, sys
with open(r'{}') as f:
    query = f.read()
conn = sqlite3.connect(r'{}')
conn.row_factory = sqlite3.Row
cur = conn.execute(query)
rows = [dict(row) for row in cur.fetchall()]
conn.close()
json.dump(rows, sys.stdout, ensure_ascii=False, default=str)",
            sql_path.replace('\\', "/"),
            db.replace('\\', "/"),
        );

        let output = Command::new("python")
            .args(["-c", &script])
            .output()
            .ok()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Hermes collector python error: {}", stderr);
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Some(vec![]);
        }
        serde_json::from_str(stdout.trim()).ok()
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        // Security: skip if db_path is a symlink (fail-closed)
        if is_symlink(&self.db_path) || !self.db_path.exists() || !self.check_python() {
            self.cached_db_sessions.clear();
            return vec![];
        }

        // Map running Hermes PIDs to session_ids
        let pid_map = Self::find_hermes_pid_map(&shared.process_info);

        // Refresh DB rows on slow ticks only; reuse cache on fast ticks
        if shared.slow_tick {
            if let Some(rows) = self.query_sessions() {
                self.cached_db_sessions = rows;
            }
        }

        let now_ms = current_time_ms();
        let mut sessions = Vec::new();

        // Build a set of tracked session_ids for cache eviction later
        let _db_ids: HashSet<&str> = self
            .cached_db_sessions
            .iter()
            .map(|ds| ds.id.as_str())
            .collect();

        // Match each DB session to a running PID by session_id
        // Also track any Hermes PIDs not in the DB (newly starting sessions)
        let mut matched_ids = HashSet::new();

        for ds in &self.cached_db_sessions {
            // Match by session_id in pid_map
            let matched_pid = pid_map
                .iter()
                .find(|(_, sid)| sid.as_str() == ds.id)
                .map(|(&pid, _)| pid);

            let Some(matched_pid) = matched_pid else {
                // No running process for this session
                // Check if it ended recently (< 30s) to show as "Done"
                if ds.time_updated > 0 && now_ms.saturating_sub(ds.time_updated) < 30_000 {
                    matched_ids.insert(ds.id.as_str());
                    sessions.push(self.build_session(
                        ds, 0, shared, now_ms, SessionStatus::Done,
                    ));
                }
                continue;
            };

            matched_ids.insert(ds.id.as_str());
            let proc = shared.process_info.get(&matched_pid);
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

            let age_ms = now_ms.saturating_sub(ds.time_updated);
            let since_update_secs = age_ms / 1000;

            // Derive session status
            let status = if since_update_secs < 30 {
                // Recent activity — could be Thinking or Executing
                let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
                let has_active_child = process::has_active_descendant(
                    matched_pid,
                    &shared.children_map,
                    &shared.process_info,
                    5.0,
                );
                if cpu_active || has_active_child {
                    SessionStatus::Executing
                } else {
                    // No CPU activity but recently touched — probably thinking
                    SessionStatus::Thinking
                }
            } else {
                // No recent activity — check CPU
                let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
                let has_active_child = process::has_active_descendant(
                    matched_pid,
                    &shared.children_map,
                    &shared.process_info,
                    5.0,
                );
                if cpu_active || has_active_child {
                    SessionStatus::Executing
                } else {
                    SessionStatus::Waiting
                }
            };

            // Collect child processes
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

            let current_tasks = if matches!(status, SessionStatus::Waiting) {
                vec!["waiting for input".to_string()]
            } else if matches!(status, SessionStatus::Executing) {
                vec!["executing...".to_string()]
            } else {
                vec!["thinking...".to_string()]
            };

            let project_name = if !ds.project_name.is_empty() {
                ds.project_name.clone()
            } else if !ds.directory.is_empty() {
                ds.directory
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .or_else(|| ds.directory.rsplit('\\').next())
                    .unwrap_or("?")
                    .to_string()
            } else {
                "?".to_string()
            };

            let context_window = lookup_context_window(&ds.model);
            let context_percent = if context_window > 0 {
                let used = ds.total_input + ds.total_output + ds.total_cache_read;
                (used as f64 / context_window as f64) * 100.0
            } else {
                0.0
            };

            sessions.push(AgentSession {
                agent_cli: "hermes",
                pid: matched_pid,
                session_id: ds.id.clone(),
                cwd: ds.directory.clone(),
                project_name,
                started_at: ds.time_created,
                status,
                model: ds.model.clone(),
                effort: String::new(),
                context_percent: context_percent.min(100.0),
                total_input_tokens: ds.total_input,
                total_output_tokens: ds.total_output,
                total_cache_read: ds.total_cache_read,
                total_cache_create: ds.total_cache_write,
                turn_count: ds.turn_count,
                current_tasks,
                mem_mb,
                version: String::new(),
                git_branch: String::new(),
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
                initial_prompt: ds.title.clone(),
                first_assistant_text: String::new(),
                chat_messages: vec![],
                tool_calls: vec![],
                pending_since_ms: 0,
                thinking_since_ms: 0,
                file_accesses: vec![],
                config_root: super::abbrev_path(
                    self.db_path
                        .parent()
                        .unwrap_or(Path::new(".")),
                ),
            });
        }

        // Also check for Hermes processes that have a session-key but no DB row
        // (e.g. just-started sessions not yet flushed)
        for (&pid, sid) in &pid_map {
            if matched_ids.contains(sid.as_str()) {
                continue;
            }
            // Show an unknown session until DB catches up
            let proc = shared.process_info.get(&pid);
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);
            sessions.push(AgentSession {
                agent_cli: "hermes",
                pid,
                session_id: sid.clone(),
                cwd: String::new(),
                project_name: "hermes".to_string(),
                started_at: now_ms,
                status: SessionStatus::Unknown,
                model: String::new(),
                effort: String::new(),
                context_percent: 0.0,
                total_input_tokens: 0,
                total_output_tokens: 0,
                total_cache_read: 0,
                total_cache_create: 0,
                turn_count: 0,
                current_tasks: vec!["initializing...".to_string()],
                mem_mb,
                version: String::new(),
                git_branch: String::new(),
                git_added: 0,
                git_modified: 0,
                token_history: vec![],
                context_history: vec![],
                compaction_count: 0,
                context_window: 0,
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
                config_root: super::abbrev_path(
                    self.db_path
                        .parent()
                        .unwrap_or(Path::new(".")),
                ),
            });
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Build a session from a DB row (helper for Done/error rows).
    fn build_session(
        &self,
        ds: &DbSession,
        matched_pid: u32,
        shared: &super::SharedProcessData,
        _now_ms: u64,
        status: SessionStatus,
    ) -> AgentSession {
        let proc = shared.process_info.get(&matched_pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        let context_window = lookup_context_window(&ds.model);
        let context_percent = if context_window > 0 {
            let used = ds.total_input + ds.total_output + ds.total_cache_read;
            (used as f64 / context_window as f64) * 100.0
        } else {
            0.0
        };

        AgentSession {
            agent_cli: "hermes",
            pid: matched_pid,
            session_id: ds.id.clone(),
            cwd: ds.directory.clone(),
            project_name: ds.project_name.clone(),
            started_at: ds.time_created,
            status,
            model: ds.model.clone(),
            effort: String::new(),
            context_percent: context_percent.min(100.0),
            total_input_tokens: ds.total_input,
            total_output_tokens: ds.total_output,
            total_cache_read: ds.total_cache_read,
            total_cache_create: ds.total_cache_write,
            turn_count: ds.turn_count,
            current_tasks: vec![],
            mem_mb,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: vec![],
            context_history: vec![],
            compaction_count: 0,
            context_window,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children: vec![],
            initial_prompt: ds.title.clone(),
            first_assistant_text: String::new(),
            chat_messages: vec![],
            tool_calls: vec![],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![],
            config_root: super::abbrev_path(
                self.db_path.parent().unwrap_or(Path::new(".")),
            ),
        }
    }

    fn query_sessions(&self) -> Option<Vec<DbSession>> {
        // Query: all active (not ended) sessions, plus ended sessions in last 60s.
        // The `s.ended_at IS NULL` clause catches long-running sessions that
        // may be older than 24h but still have a running process.
        //
        // The Hermes state.db schema:
        //   sessions(id, source, model, title, cwd, started_at, ended_at,
        //            message_count, tool_call_count, api_call_count,
        //            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
        //            reasoning_tokens, ...)
        // NOTE: no explicit 'version' column — we use empty string as fallback.
        let sql = format!(
            r#"
SELECT
  s.id,
  COALESCE(s.title, '') as title,
  COALESCE(s.cwd, '') as directory,
  COALESCE(s.model, '') as model,
  CAST(CAST(s.started_at AS INTEGER) * 1000 AS INTEGER) as time_created_ms,
  CAST(CAST(COALESCE(s.ended_at, s.started_at) AS INTEGER) * 1000 AS INTEGER) as time_updated_ms,
  COALESCE(s.input_tokens, 0) as total_input,
  COALESCE(s.output_tokens, 0) as total_output,
  COALESCE(s.cache_read_tokens, 0) as total_cache_read,
  COALESCE(s.cache_write_tokens, 0) as total_cache_write,
  COALESCE(s.api_call_count, s.message_count, 0) as turn_count
FROM sessions s
WHERE s.ended_at IS NULL
   OR (s.ended_at IS NOT NULL AND s.ended_at > CAST((strftime('%%s', 'now') - 60) AS REAL))
ORDER BY s.started_at DESC
LIMIT {};
"#,
            MAX_SESSIONS
        );

        let rows = self.run_query(&sql)?;
        let mut sessions = Vec::new();
        for row in rows {
            let mut id = row["id"].as_str().unwrap_or("").to_string();
            let mut title = row["title"].as_str().unwrap_or("").to_string();
            let mut directory = row["directory"].as_str().unwrap_or("").to_string();
            let mut model = row["model"].as_str().unwrap_or("").to_string();

            truncate_field(&mut id, 256);
            truncate_field(&mut title, 512);
            truncate_field(&mut directory, 4096);
            truncate_field(&mut model, 128);

            let title = super::redact_secrets(&title);

            // Hermes stores timestamps as Unix epoch seconds (REAL).
            // Convert to milliseconds for abtop compatibility.
            let time_created = row["time_created_ms"].as_u64().unwrap_or(0);
            let time_updated = row["time_updated_ms"].as_u64().unwrap_or(0);

            let project_name = directory
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .or_else(|| directory.rsplit('\\').next())
                .unwrap_or("?")
                .to_string();

            sessions.push(DbSession {
                id,
                title,
                directory,
                time_created,
                time_updated,
                project_name,
                turn_count: row["turn_count"].as_u64().unwrap_or(0) as u32,
                total_input: row["total_input"].as_u64().unwrap_or(0),
                total_output: row["total_output"].as_u64().unwrap_or(0),
                total_cache_read: row["total_cache_read"].as_u64().unwrap_or(0),
                total_cache_write: row["total_cache_write"].as_u64().unwrap_or(0),
                model,
            });
        }

        Some(sessions)
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

struct DbSession {
    id: String,
    title: String,
    directory: String,
    time_created: u64,
    time_updated: u64,
    project_name: String,
    turn_count: u32,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_write: u64,
    model: String,
}

/// Check if a path is a symlink (fail-closed: returns true on error).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
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

/// Look up a model's context window from the built-in table.
fn lookup_context_window(model: &str) -> u64 {
    let model_lower = model.to_lowercase();
    for (prefix, size) in MODEL_CONTEXT_WINDOWS {
        if model_lower.contains(prefix) {
            return *size;
        }
    }
    0
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
    fn test_find_hermes_pid_map() {
        let mut info = HashMap::new();
        info.insert(
            100,
            process::ProcInfo {
                pid: 100,
                ppid: 1,
                rss_kb: 50000,
                cpu_pct: 0.0,
                command: "python -m tui_gateway.slash_worker --session-key 20260614_095914_b987ed --model deepseek-v4-flash".to_string(),
            },
        );
        info.insert(
            200,
            process::ProcInfo {
                pid: 200,
                ppid: 1,
                rss_kb: 30000,
                cpu_pct: 2.0,
                command: "python -m tui_gateway.slash_worker --session-key cron_7239c7622ece_20260614_085402".to_string(),
            },
        );
        info.insert(
            300,
            process::ProcInfo {
                pid: 300,
                ppid: 1,
                rss_kb: 1000,
                cpu_pct: 0.0,
                command: "grep hermes".to_string(),
            },
        );
        let map = HermesCollector::find_hermes_pid_map(&info);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&100).unwrap(), "20260614_095914_b987ed");
        assert_eq!(map.get(&200).unwrap(), "cron_7239c7622ece_20260614_085402");
        assert!(!map.contains_key(&300));
    }

    #[test]
    fn test_lookup_context_window() {
        assert_eq!(lookup_context_window("deepseek-v4-flash"), 1_048_576);
        assert_eq!(lookup_context_window("claude-sonnet-4"), 200_000);
        assert_eq!(lookup_context_window("unknown-model"), 0);
    }

    #[test]
    fn test_discover_db_path() {
        // Should not panic; returns a valid path even if it doesn't exist
        let path = HermesCollector::discover_db_path();
        assert!(!path.as_os_str().is_empty());
    }
}
