use super::{context_window_for_model, process};
use crate::model::{
    AgentSession, ChildProcess, SessionStatus, StatusAuthority, StatusEvidence, StatusObservation,
    StatusReason,
};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum sessions to fetch from the DB per query.
const MAX_SESSIONS: u32 = 20;
/// Lifecycle inference must come from the current successful query path and
/// remain very recent. Failed reads/parses never reuse cached state.
const LIFECYCLE_QUERY_FRESHNESS_MS: u64 = 5_000;
/// Allow small DB/filesystem timestamp skew when proving that a row could
/// belong to the observed process incarnation.
const OWNERSHIP_START_GRACE_MS: u64 = 5_000;

/// Collector for OpenCode sessions.
///
/// Discovery strategy:
/// 1. `ps` to find running opencode processes (from shared process data)
/// 2. Query SQLite DB at ~/.local/share/opencode/opencode.db via `sqlite3` CLI
/// 3. Match running PIDs to sessions by cwd
///
/// Uses `sqlite3 -readonly -json` for safe concurrent reads (WAL mode).
/// DB rows are cached and only refreshed on `shared.slow_tick` (every ~10s)
/// so the aggregate query does not run every 2s. A lightweight lifecycle
/// query runs on every session tick while OpenCode is live; PID matching and
/// the children walk also use the current process snapshot on every tick.
pub struct OpenCodeCollector {
    db_path: PathBuf,
    /// Whether sqlite3 CLI is available (checked once).
    sqlite3_available: Option<bool>,
    /// Cached DB rows from the last slow-tick query. Reused on fast ticks.
    cached_db_sessions: Vec<DbSession>,
    /// Per-session lifecycle rows from the newest successful query. The map is
    /// discarded immediately on failure and timestamped to bound lifecycle
    /// inference even on the successful path.
    cached_db_lifecycles: HashMap<String, CachedLifecycle>,
    /// Outcome of the newest lifecycle query attempted while a live OpenCode
    /// process existed. `false` invalidates cached lifecycle status
    /// immediately; cached Waiting must never survive a failed query.
    lifecycle_query_succeeded: bool,
    /// Whether the "sqlite3 missing" warning has been emitted (once).
    #[cfg(target_os = "windows")]
    warned_sqlite3_missing: bool,
}

impl OpenCodeCollector {
    pub fn new() -> Self {
        let data_dir = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
        let db_path = data_dir.join("opencode").join("opencode.db");
        #[cfg(target_os = "windows")]
        let db_path = windows_db_path(db_path);
        Self {
            db_path,
            sqlite3_available: None,
            cached_db_sessions: Vec::new(),
            cached_db_lifecycles: HashMap::new(),
            lifecycle_query_succeeded: false,
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
        // Security: skip if db_path is a symlink (fail-closed)
        if is_symlink(&self.db_path) || !self.db_path.exists() {
            self.cached_db_sessions.clear();
            self.cached_db_lifecycles.clear();
            self.lifecycle_query_succeeded = false;
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
            self.cached_db_lifecycles.clear();
            self.lifecycle_query_succeeded = false;
            return vec![];
        }

        // Find only session-owning OpenCode processes. Generic "second token
        // is named opencode" matching accepts unrelated commands such as
        // `rg opencode`, while host/admin modes cannot own a TUI session.
        let opencode_pids = Self::find_opencode_pids(&shared.process_info);
        let mut live_processes: Vec<LiveOpenCodeProcess> = opencode_pids
            .iter()
            .filter_map(|&pid| {
                shared.process_info.get(&pid).map(|p| LiveOpenCodeProcess {
                    pid,
                    command: p.command.clone(),
                    cwd: get_process_cwd(pid),
                    started_at_ms: process::get_process_started_at_ms(pid),
                })
            })
            .collect();
        live_processes.sort_by_key(|process| process.pid);

        // Refresh DB rows on slow ticks only; reuse cache on fast ticks so
        // we don't fork sqlite3 every 2s.
        if shared.slow_tick {
            if let Some(rows) = self.query_sessions() {
                self.cached_db_sessions = rows;
            }
        }

        // Status changes are latency-sensitive (especially user questions),
        // so query just the latest message and active tool-part metadata every
        // session tick. Any failed read/parse immediately invalidates status
        // authority and discards cached rows, so Waiting (or any other status)
        // can never survive that failure.
        if !opencode_pids.is_empty() {
            match self.query_lifecycles() {
                Some(lifecycles) => {
                    let observed_at_ms = current_time_ms();
                    self.cached_db_lifecycles = lifecycles
                        .into_iter()
                        .map(|(id, lifecycle)| {
                            (
                                id,
                                CachedLifecycle {
                                    lifecycle,
                                    observed_at_ms,
                                },
                            )
                        })
                        .collect();
                    self.lifecycle_query_succeeded = true;
                }
                None => {
                    self.lifecycle_query_succeeded = false;
                    self.cached_db_lifecycles.clear();
                }
            }
        }

        let now_ms = current_time_ms();
        let ownership_plan = plan_session_ownership(&self.cached_db_sessions, &live_processes);
        let mut sessions = Vec::new();

        for planned in ownership_plan {
            let ds = &self.cached_db_sessions[planned.session_index];
            let matched_pid = planned.ownership.pid().unwrap_or(0);

            let proc = shared.process_info.get(&matched_pid);
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

            let lifecycle = resolve_session_status(
                planned.ownership,
                self.cached_db_lifecycles.get(&ds.id),
                self.lifecycle_query_succeeded,
                now_ms,
            );
            let status = lifecycle.status;

            let project_name = if !ds.project_name.is_empty() {
                ds.project_name.clone()
            } else {
                // last_path_segment also splits on `\` on Windows.
                process::last_path_segment(&ds.directory)
                    .unwrap_or("?")
                    .to_string()
            };

            let current_tasks = match &status {
                SessionStatus::Waiting => vec!["waiting for user input".to_string()],
                SessionStatus::Executing => vec![lifecycle
                    .active_tool
                    .clone()
                    .unwrap_or_else(|| "running child process".to_string())],
                SessionStatus::Thinking => vec!["thinking".to_string()],
                SessionStatus::Idle => vec!["idle".to_string()],
                SessionStatus::Error => vec!["provider error".to_string()],
                SessionStatus::Unknown => vec![lifecycle.task.clone()],
                _ => vec![],
            };
            let pending_since_ms =
                if matches!(&status, SessionStatus::Waiting | SessionStatus::Executing) {
                    lifecycle.since_ms
                } else {
                    0
                };
            let thinking_since_ms = if matches!(&status, SessionStatus::Thinking) {
                lifecycle.since_ms
            } else {
                0
            };
            let awaiting_input = matches!(&status, SessionStatus::Waiting);

            // Collect child processes with cycle guard (visited set)
            let mut children = Vec::new();
            let mut stack: Vec<u32> = if planned.ownership.is_confirmed() {
                shared
                    .children_map
                    .get(&matched_pid)
                    .cloned()
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
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

            let context_window = context_window_for_model(&model, "", 0);
            let context_percent = if context_window > 0 {
                ((ds.total_input + ds.total_output) as f64 / context_window as f64) * 100.0
            } else {
                0.0
            };

            sessions.push(AgentSession {
                agent_cli: "opencode",
                pid: matched_pid,
                // OpenCode exposes no exact PID/session registry. Cwd matching
                // is display-only and must never authorize a PID action.
                action_process_incarnation: None,
                session_id: ds.id.clone(),
                cwd: ds.directory.clone(),
                project_name,
                started_at: ds.time_created,
                status,
                status_evidence: lifecycle.evidence,
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
                pending_since_ms,
                awaiting_input,
                thinking_since_ms,
                file_accesses: vec![],
                config_root: super::abbrev_path(
                    self.db_path.parent().unwrap_or(std::path::Path::new(".")),
                ),
            });
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn find_opencode_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        process_info
            .iter()
            .filter(|(_, info)| is_session_owning_opencode_process(&info.command))
            .map(|(pid, _)| *pid)
            .collect()
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
    ORDER BY m2.time_created DESC LIMIT 1), '') as provider
FROM session s
ORDER BY s.time_updated DESC
LIMIT {};"#,
            MAX_SESSIONS
        );

        // Two separate invocations to avoid fragile concatenated JSON parsing
        let rows = self.run_query(&session_sql)?;
        let model_rows = self.run_query(&model_sql).unwrap_or_default();

        // Build model lookup by session id
        let mut model_map: HashMap<String, (String, String)> = HashMap::new();
        for mr in &model_rows {
            if let Some(id) = mr["id"].as_str() {
                model_map.insert(
                    id.to_string(),
                    (
                        sanitize_db_field(mr["model"].as_str().unwrap_or(""), 256),
                        sanitize_db_field(mr["provider"].as_str().unwrap_or(""), 256),
                    ),
                );
            }
        }

        let mut sessions = Vec::new();
        for row in rows {
            let id = row["id"].as_str().unwrap_or("").to_string();
            let (model, provider) = model_map.remove(&id).unwrap_or_default();

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
                // time_created and time_updated are in milliseconds since epoch
                time_created: row["time_created"].as_u64().unwrap_or(0),
                time_updated: row["time_updated"].as_u64().unwrap_or(0),
                project_name,
                turn_count: row["turn_count"].as_u64().unwrap_or(0) as u32,
                total_input: row["total_input"].as_u64().unwrap_or(0),
                total_output: row["total_output"].as_u64().unwrap_or(0),
                total_cache_read: row["total_cache_read"].as_u64().unwrap_or(0),
                total_cache_write: row["total_cache_write"].as_u64().unwrap_or(0),
                model,
                provider,
            });
        }

        Some(sessions)
    }

    /// Query only lifecycle metadata from the newest message and session-wide
    /// active tool parts. Raw prompts, tool inputs, outputs, and error content
    /// are deliberately excluded from the SELECT list.
    fn query_lifecycles(&self) -> Option<HashMap<String, DbLifecycle>> {
        let lifecycle_sql = format!(
            r#"
WITH recent_sessions AS (
  SELECT id
  FROM session
  ORDER BY time_updated DESC
  LIMIT {}
),
latest_messages AS (
  SELECT
    rs.id AS session_id,
    (
      SELECT m.id
      FROM message m
      WHERE m.session_id = rs.id
      ORDER BY m.time_created DESC, m.id DESC
      LIMIT 1
    ) AS message_id,
    COALESCE((
      SELECT json_extract(m.data, '$.role')
      FROM message m
      WHERE m.session_id = rs.id
      ORDER BY m.time_created DESC, m.id DESC
      LIMIT 1
    ), '') AS latest_role,
    COALESCE((
      SELECT json_extract(m.data, '$.time.created')
      FROM message m
      WHERE m.session_id = rs.id
      ORDER BY m.time_created DESC, m.id DESC
      LIMIT 1
    ), 0) AS latest_created,
    COALESCE((
      SELECT json_extract(m.data, '$.time.completed')
      FROM message m
      WHERE m.session_id = rs.id
      ORDER BY m.time_created DESC, m.id DESC
      LIMIT 1
    ), 0) AS latest_completed,
    COALESCE((
      SELECT CASE
        WHEN json_extract(m.data, '$.error.name') = 'MessageAbortedError'
        THEN 0
        WHEN (
          json_type(m.data, '$.error') IS NOT NULL
          AND json_type(m.data, '$.error') != 'null'
        ) OR json_extract(m.data, '$.finish') = 'error'
        THEN 1
        ELSE 0
      END
      FROM message m
      WHERE m.session_id = rs.id
      ORDER BY m.time_created DESC, m.id DESC
      LIMIT 1
    ), 0) AS latest_has_error
  FROM recent_sessions rs
),
ranked_active_tools AS (
  SELECT
    p.session_id,
    p.message_id,
    COALESCE(json_extract(p.data, '$.tool'), '') AS tool,
    COALESCE(json_extract(p.data, '$.state.status'), '') AS tool_status,
    COALESCE(json_extract(p.data, '$.state.time.start'), p.time_updated, 0) AS tool_started,
    ROW_NUMBER() OVER (
      PARTITION BY p.session_id
      ORDER BY
        CASE WHEN p.message_id = a.message_id THEN 0 ELSE 1 END,
        CASE
          WHEN LOWER(COALESCE(json_extract(p.data, '$.tool'), '')) = 'question'
            AND json_extract(p.data, '$.state.status') = 'running'
          THEN 0
          ELSE 1
        END,
        p.time_updated DESC,
        p.id DESC
    ) AS rank
  FROM part p
  JOIN latest_messages a
    ON a.session_id = p.session_id
  WHERE json_extract(p.data, '$.type') = 'tool'
    AND json_extract(p.data, '$.state.status') IN ('pending', 'running')
)
SELECT
  m.session_id AS id,
  COALESCE(m.message_id, '') AS latest_message_id,
  m.latest_role,
  m.latest_created,
  m.latest_completed,
  m.latest_has_error,
  COALESCE(t.message_id, '') AS active_tool_message_id,
  COALESCE(t.tool, '') AS active_tool,
  COALESCE(t.tool_status, '') AS active_tool_status,
  COALESCE(t.tool_started, 0) AS active_tool_started
FROM latest_messages m
LEFT JOIN ranked_active_tools t
  ON t.session_id = m.session_id
 AND t.rank = 1;"#,
            MAX_SESSIONS
        );

        let rows = self.run_query(&lifecycle_sql)?;
        parse_lifecycle_rows(rows)
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

#[derive(Debug, Clone)]
struct DbSession {
    id: String,
    title: String,
    directory: String,
    version: String,
    time_created: u64,
    time_updated: u64,
    project_name: String,
    turn_count: u32,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_write: u64,
    model: String,
    provider: String,
}

#[derive(Debug, Clone)]
struct LiveOpenCodeProcess {
    pid: u32,
    command: String,
    cwd: Option<String>,
    started_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOwnership {
    Confirmed { pid: u32, started_at_ms: u64 },
    Unconfirmed,
}

impl ProcessOwnership {
    fn pid(self) -> Option<u32> {
        match self {
            Self::Confirmed { pid, .. } => Some(pid),
            Self::Unconfirmed => None,
        }
    }

    fn started_at_ms(self) -> Option<u64> {
        match self {
            Self::Confirmed { started_at_ms, .. } => Some(started_at_ms),
            Self::Unconfirmed => None,
        }
    }

    fn is_confirmed(self) -> bool {
        matches!(self, Self::Confirmed { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlannedSessionOwnership {
    session_index: usize,
    ownership: ProcessOwnership,
}

/// Build a deterministic, fail-closed ownership plan.
///
/// An explicit `--session`/`-s` process argument is authoritative. Otherwise
/// a directory group is actionable only when exactly one live process and one
/// fetched DB row share that directory. Any 1:N, N:1, or N:N group emits at
/// most one row per live process, newest first, with no actionable PID.
fn plan_session_ownership(
    sessions: &[DbSession],
    processes: &[LiveOpenCodeProcess],
) -> Vec<PlannedSessionOwnership> {
    let mut plan = Vec::new();
    let mut used_sessions = HashSet::new();
    let mut used_processes = HashSet::new();

    let session_by_id: HashMap<&str, usize> = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| (session.id.as_str(), index))
        .collect();
    let mut explicit = BTreeMap::<String, Vec<usize>>::new();
    for (process_index, process) in processes.iter().enumerate() {
        if let Some(session_id) = explicit_session_id(&process.command) {
            // A process naming an uncached session must not be reassigned to a
            // different historical row merely because the cwd matches.
            used_processes.insert(process_index);
            explicit.entry(session_id).or_default().push(process_index);
        }
    }
    for (session_id, process_indices) in explicit {
        let Some(&session_index) = session_by_id.get(session_id.as_str()) else {
            continue;
        };
        used_sessions.insert(session_index);
        let ownership = if process_indices.len() == 1 {
            let process = &processes[process_indices[0]];
            process
                .started_at_ms
                .map(|started_at_ms| ProcessOwnership::Confirmed {
                    pid: process.pid,
                    started_at_ms,
                })
                .unwrap_or(ProcessOwnership::Unconfirmed)
        } else {
            ProcessOwnership::Unconfirmed
        };
        plan.push(PlannedSessionOwnership {
            session_index,
            ownership,
        });
    }

    let mut session_groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, session) in sessions.iter().enumerate() {
        if used_sessions.contains(&index) {
            continue;
        }
        if let Some(key) = path_key(&session.directory) {
            session_groups.entry(key).or_default().push(index);
        }
    }

    let mut process_groups = BTreeMap::<String, Vec<usize>>::new();
    for (process_index, process) in processes.iter().enumerate() {
        if used_processes.contains(&process_index) {
            continue;
        }
        let key = if let Some(cwd) = process.cwd.as_deref() {
            path_key(cwd).filter(|key| session_groups.contains_key(key))
        } else {
            let matching_keys: Vec<_> = session_groups
                .keys()
                .filter(|key| command_mentions_session_dir(&process.command, key))
                .cloned()
                .collect();
            (matching_keys.len() == 1).then(|| matching_keys[0].clone())
        };
        if let Some(key) = key {
            process_groups.entry(key).or_default().push(process_index);
        }
    }

    for (key, mut session_indices) in session_groups {
        let Some(mut process_indices) = process_groups.remove(&key) else {
            continue;
        };
        session_indices.sort_by(|left, right| {
            sessions[*right]
                .time_updated
                .cmp(&sessions[*left].time_updated)
                .then_with(|| {
                    sessions[*right]
                        .time_created
                        .cmp(&sessions[*left].time_created)
                })
                .then_with(|| sessions[*left].id.cmp(&sessions[*right].id))
        });
        process_indices.sort_by_key(|index| processes[*index].pid);

        if session_indices.len() == 1
            && process_indices.len() == 1
            && process_could_own_session(
                &processes[process_indices[0]],
                &sessions[session_indices[0]],
            )
        {
            let process = &processes[process_indices[0]];
            let Some(started_at_ms) = process.started_at_ms else {
                plan.push(PlannedSessionOwnership {
                    session_index: session_indices[0],
                    ownership: ProcessOwnership::Unconfirmed,
                });
                continue;
            };
            plan.push(PlannedSessionOwnership {
                session_index: session_indices[0],
                ownership: ProcessOwnership::Confirmed {
                    pid: process.pid,
                    started_at_ms,
                },
            });
            continue;
        }

        // Do not invent a PID/session permutation for an ambiguous same-cwd
        // group. Bound visible candidates by the number of live processes so
        // old DB rows do not appear as duplicate live sessions.
        for &session_index in session_indices.iter().take(process_indices.len()) {
            plan.push(PlannedSessionOwnership {
                session_index,
                ownership: ProcessOwnership::Unconfirmed,
            });
        }
    }

    plan.sort_by(|left, right| {
        sessions[right.session_index]
            .time_updated
            .cmp(&sessions[left.session_index].time_updated)
            .then_with(|| {
                sessions[left.session_index]
                    .id
                    .cmp(&sessions[right.session_index].id)
            })
    });
    plan
}

fn process_could_own_session(process: &LiveOpenCodeProcess, session: &DbSession) -> bool {
    process.started_at_ms.is_some_and(|started_at_ms| {
        session.time_updated >= started_at_ms.saturating_sub(OWNERSHIP_START_GRACE_MS)
    })
}

#[derive(Debug, Clone)]
struct CachedLifecycle {
    lifecycle: DbLifecycle,
    observed_at_ms: u64,
}

#[derive(Debug, Clone)]
struct DbLifecycle {
    status: SessionStatus,
    authority: StatusAuthority,
    reason: StatusReason,
    active_tool: Option<String>,
    since_ms: u64,
    /// Timestamp on the exact provider record used for this classification.
    /// Query time is freshness evidence, not proof that persisted lifecycle
    /// state belongs to the current process incarnation.
    source_at_ms: u64,
}

struct LifecycleDecision {
    status: SessionStatus,
    evidence: StatusEvidence,
    active_tool: Option<String>,
    since_ms: u64,
    task: String,
}

impl LifecycleDecision {
    fn unknown(reason: StatusReason, observed_at_ms: u64, task: &str) -> Self {
        Self {
            status: SessionStatus::Unknown,
            evidence: make_status_evidence(
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                reason,
                observed_at_ms,
                observed_at_ms,
            ),
            active_tool: None,
            since_ms: 0,
            task: task.to_string(),
        }
    }
}

fn resolve_lifecycle_status(
    cached: Option<&CachedLifecycle>,
    query_succeeded: bool,
    now_ms: u64,
) -> LifecycleDecision {
    if !query_succeeded {
        return LifecycleDecision::unknown(
            StatusReason::BackgroundProbeFailed,
            now_ms,
            "lifecycle query failed",
        );
    }

    let Some(cached) = cached else {
        return LifecycleDecision::unknown(
            StatusReason::ProtocolUnknown,
            now_ms,
            "lifecycle unavailable",
        );
    };
    if cached.observed_at_ms > now_ms
        || now_ms - cached.observed_at_ms > LIFECYCLE_QUERY_FRESHNESS_MS
    {
        return LifecycleDecision::unknown(StatusReason::Stale, now_ms, "lifecycle data is stale");
    }

    lifecycle_decision(
        &cached.lifecycle,
        cached.lifecycle.authority,
        cached.lifecycle.reason,
        now_ms,
    )
}

fn resolve_session_status(
    ownership: ProcessOwnership,
    cached: Option<&CachedLifecycle>,
    query_succeeded: bool,
    now_ms: u64,
) -> LifecycleDecision {
    if !ownership.is_confirmed() {
        return LifecycleDecision::unknown(
            StatusReason::OwnershipUnconfirmed,
            now_ms,
            "session ownership is ambiguous",
        );
    }
    let decision = resolve_lifecycle_status(cached, query_succeeded, now_ms);
    if decision.status == SessionStatus::Unknown {
        return decision;
    }

    let Some(process_started_at_ms) = ownership.started_at_ms() else {
        return LifecycleDecision::unknown(
            StatusReason::OwnershipUnconfirmed,
            now_ms,
            "process start is unavailable",
        );
    };
    let Some(cached) = cached else {
        return LifecycleDecision::unknown(
            StatusReason::ProtocolUnknown,
            now_ms,
            "lifecycle unavailable",
        );
    };
    if cached.lifecycle.source_at_ms == 0 || cached.lifecycle.source_at_ms < process_started_at_ms {
        return LifecycleDecision::unknown(
            StatusReason::Stale,
            now_ms,
            "lifecycle predates the process",
        );
    }
    decision
}

fn lifecycle_decision(
    lifecycle: &DbLifecycle,
    authority: StatusAuthority,
    reason: StatusReason,
    observed_at_ms: u64,
) -> LifecycleDecision {
    let task = match lifecycle.status {
        SessionStatus::Waiting => "waiting for user input",
        SessionStatus::Executing => "executing",
        SessionStatus::Thinking => "thinking",
        SessionStatus::Idle => "idle",
        SessionStatus::Error => "provider error",
        _ => "lifecycle unavailable",
    };
    LifecycleDecision {
        status: lifecycle.status,
        evidence: make_status_evidence(
            lifecycle.status,
            authority,
            reason,
            observed_at_ms,
            lifecycle.since_ms,
        ),
        active_tool: lifecycle.active_tool.clone(),
        since_ms: lifecycle.since_ms,
        task: task.to_string(),
    }
}

fn make_status_evidence(
    status: SessionStatus,
    authority: StatusAuthority,
    reason: StatusReason,
    observed_at_ms: u64,
    status_since_ms: u64,
) -> StatusEvidence {
    let mut evidence = StatusEvidence::default();
    evidence.observe(StatusObservation::new(
        status,
        authority,
        reason,
        observed_at_ms,
        0,
    ));
    if status_since_ms > 0 {
        evidence.status_since_ms = status_since_ms;
    }
    evidence
}

fn parse_lifecycle_rows(rows: Vec<Value>) -> Option<HashMap<String, DbLifecycle>> {
    let mut lifecycles = HashMap::new();
    for row in rows {
        let id = row.get("id")?.as_str().filter(|id| !id.is_empty())?;
        let latest_message_id = row.get("latest_message_id")?.as_str()?;
        let latest_role = row.get("latest_role")?.as_str()?;
        if !matches!(latest_role, "" | "user" | "assistant") {
            return None;
        }
        let latest_created = row.get("latest_created")?.as_u64()?;
        let latest_completed = row.get("latest_completed")?.as_u64()?;
        let latest_has_error = match row.get("latest_has_error")?.as_u64()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let tool_message_id = row.get("active_tool_message_id")?.as_str()?;
        let tool =
            super::redact_secrets(&sanitize_db_field(row.get("active_tool")?.as_str()?, 128));
        let tool_status = row.get("active_tool_status")?.as_str()?;
        if !matches!(tool_status, "" | "pending" | "running") {
            return None;
        }
        let tool_started = row.get("active_tool_started")?.as_u64()?;

        if (latest_role.is_empty() && (!latest_message_id.is_empty() || latest_created > 0))
            || (!latest_role.is_empty() && latest_message_id.is_empty())
            || (latest_completed > 0 && latest_completed < latest_created)
            || (latest_has_error && latest_role != "assistant")
        {
            return None;
        }

        let running_question =
            tool.eq_ignore_ascii_case("question") && tool_status.eq_ignore_ascii_case("running");
        let has_active_tool = matches!(tool_status, "pending" | "running");
        if has_active_tool != (!tool.is_empty() && !tool_message_id.is_empty()) {
            return None;
        }
        let same_message_terminal_tool = has_active_tool
            && tool_message_id == latest_message_id
            && latest_role == "assistant"
            && latest_completed > 0;
        if same_message_terminal_tool && tool_started > 0 && tool_started > latest_completed {
            return None;
        }
        let active_tool_applies = has_active_tool
            && !same_message_terminal_tool
            && (tool_message_id == latest_message_id || latest_role == "user");

        let (status, authority, reason, since_ms, source_at_ms) =
            if active_tool_applies && running_question {
                (
                    SessionStatus::Unknown,
                    StatusAuthority::Unavailable,
                    StatusReason::ProtocolUnknown,
                    0,
                    tool_started,
                )
            } else if latest_has_error {
                let source_at_ms = latest_completed.max(latest_created);
                (
                    SessionStatus::Error,
                    StatusAuthority::Heuristic,
                    StatusReason::CollectorInference,
                    source_at_ms,
                    source_at_ms,
                )
            } else if active_tool_applies {
                (
                    SessionStatus::Unknown,
                    StatusAuthority::Unavailable,
                    StatusReason::ProtocolUnknown,
                    0,
                    tool_started,
                )
            } else if latest_role == "user" {
                // OpenCode persists no marker for `noReply`, and a queued
                // prompt is also written before the existing runner resumes.
                // A lone user row therefore cannot prove model generation.
                (
                    SessionStatus::Unknown,
                    StatusAuthority::Unavailable,
                    StatusReason::ProtocolUnknown,
                    0,
                    latest_created,
                )
            } else if latest_role == "assistant" && latest_completed == 0 {
                (
                    SessionStatus::Thinking,
                    StatusAuthority::Heuristic,
                    StatusReason::CollectorInference,
                    latest_created,
                    latest_created,
                )
            } else if latest_role == "assistant" {
                (
                    SessionStatus::Idle,
                    StatusAuthority::Heuristic,
                    StatusReason::CollectorInference,
                    latest_completed,
                    latest_completed,
                )
            } else {
                (
                    SessionStatus::Unknown,
                    StatusAuthority::Unavailable,
                    StatusReason::ProtocolUnknown,
                    0,
                    0,
                )
            };

        lifecycles.insert(
            id.to_string(),
            DbLifecycle {
                status,
                authority,
                reason,
                active_tool: active_tool_applies.then_some(tool),
                since_ms,
                source_at_ms,
            },
        );
    }
    Some(lifecycles)
}

fn is_session_owning_opencode_process(command: &str) -> bool {
    let tokens = command_tokens(command);
    let Some(entry_index) = opencode_entry_index(&tokens) else {
        return false;
    };
    let first_positional = first_cli_positional(&tokens[entry_index + 1..]);
    !first_positional.is_some_and(|arg| {
        matches!(
            arg.to_ascii_lowercase().as_str(),
            "completion"
                | "acp"
                | "mcp"
                | "attach"
                | "debug"
                | "providers"
                | "auth"
                | "agent"
                | "upgrade"
                | "uninstall"
                | "serve"
                | "web"
                | "models"
                | "stats"
                | "export"
                | "import"
                | "github"
                | "session"
                | "plugin"
                | "plug"
                | "db"
        )
    })
}

fn explicit_session_id(command: &str) -> Option<String> {
    let tokens = command_tokens(command);
    let entry_index = opencode_entry_index(&tokens)?;
    let args = &tokens[entry_index + 1..];
    for (index, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        if let Some(id) = arg
            .strip_prefix("--session=")
            .or_else(|| arg.strip_prefix("-s="))
        {
            return valid_session_id(id).then(|| id.to_string());
        }
        if matches!(arg.as_str(), "--session" | "-s") {
            let id = args.get(index + 1)?;
            return valid_session_id(id).then(|| id.to_string());
        }
    }
    None
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_whitespace)
}

fn command_mentions_session_dir(command: &str, session_key: &str) -> bool {
    let tokens = command_tokens(command);
    let Some(entry_index) = opencode_entry_index(&tokens) else {
        return false;
    };
    let args = &tokens[entry_index + 1..];
    for (index, arg) in args.iter().enumerate() {
        if matches!(arg.as_str(), "--cwd" | "--directory" | "--project")
            && args
                .get(index + 1)
                .and_then(|value| path_key(value))
                .as_deref()
                == Some(session_key)
        {
            return true;
        }
        for prefix in ["--cwd=", "--directory=", "--project="] {
            if arg.strip_prefix(prefix).and_then(path_key).as_deref() == Some(session_key) {
                return true;
            }
        }
    }

    // The default TUI accepts its project directory as the first positional
    // argument. `run` and `pr` are session modes, not directory values; their
    // process cwd is the authoritative path when it is available.
    first_cli_positional(args)
        .filter(|arg| !matches!(arg.to_ascii_lowercase().as_str(), "run" | "pr"))
        .and_then(path_key)
        .as_deref()
        == Some(session_key)
}

fn opencode_entry_index(tokens: &[String]) -> Option<usize> {
    if tokens
        .first()
        .is_some_and(|token| token_has_name(token, "opencode"))
    {
        return Some(0);
    }
    let wrapper = tokens.first().and_then(|token| token_base(token));
    let recognized_wrapper = wrapper.is_some_and(|name| {
        ["node", "bun", "deno"]
            .iter()
            .any(|candidate| names_equal(name, candidate))
    });
    (recognized_wrapper
        && tokens
            .get(1)
            .is_some_and(|token| token_has_name(token, "opencode")))
    .then_some(1)
}

fn first_cli_positional(args: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            return args.get(index + 1).map(String::as_str);
        }
        if option_takes_value(arg) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(arg);
    }
    None
}

fn option_takes_value(arg: &str) -> bool {
    if arg.contains('=') {
        return false;
    }
    matches!(
        arg,
        "--log-level"
            | "--port"
            | "--hostname"
            | "--mdns-domain"
            | "--cors"
            | "-m"
            | "--model"
            | "-s"
            | "--session"
            | "--prompt"
            | "--agent"
            | "--replay-limit"
            | "--cwd"
            | "--directory"
            | "--project"
    )
}

fn command_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in command.chars() {
        match (quote, ch) {
            (Some(active), value) if value == active => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, value) if value.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn token_has_name(token: &str, expected: &str) -> bool {
    token_base(token).is_some_and(|name| names_equal(name, expected))
}

fn token_base(token: &str) -> Option<&str> {
    let base = token.rsplit(['/', '\\']).next()?;
    for suffix in [".exe", ".js", ".mjs", ".cjs", ".sh"] {
        if base.len() > suffix.len()
            && base[base.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            return base.get(..base.len() - suffix.len());
        }
    }
    Some(base)
}

#[cfg(target_os = "windows")]
fn names_equal(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[cfg(not(target_os = "windows"))]
fn names_equal(left: &str, right: &str) -> bool {
    left == right
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

/// Produce a lexical path key for exact cwd/argv-boundary comparison. Empty
/// paths and filesystem roots are deliberately unusable ownership evidence.
fn path_key(path: &str) -> Option<String> {
    let path = path.trim().trim_matches(['\'', '"']);
    if path.is_empty() {
        return None;
    }
    #[cfg(target_os = "windows")]
    let normalized = path
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase();
    #[cfg(not(target_os = "windows"))]
    let normalized = path.trim_end_matches('/').to_string();

    if normalized.is_empty() || normalized == "/" || normalized.ends_with(':') {
        None
    } else {
        Some(normalized)
    }
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
    use serde_json::json;

    fn db_session(id: &str, directory: &str, time_updated: u64) -> DbSession {
        DbSession {
            id: id.to_string(),
            title: id.to_string(),
            directory: directory.to_string(),
            version: String::new(),
            time_created: time_updated.saturating_sub(10),
            time_updated,
            project_name: String::new(),
            turn_count: 0,
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            model: String::new(),
            provider: String::new(),
        }
    }

    fn live_process(pid: u32, command: &str, cwd: Option<&str>) -> LiveOpenCodeProcess {
        LiveOpenCodeProcess {
            pid,
            command: command.to_string(),
            cwd: cwd.map(str::to_string),
            started_at_ms: Some(50),
        }
    }

    fn confirmed(pid: u32) -> ProcessOwnership {
        ProcessOwnership::Confirmed {
            pid,
            started_at_ms: 50,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lifecycle_row(
        id: &str,
        latest_role: &str,
        latest_created: u64,
        latest_completed: u64,
        latest_has_error: bool,
        active_tool: &str,
        active_tool_status: &str,
        active_tool_started: u64,
        active_tool_message_id: Option<&str>,
    ) -> Value {
        let latest_message_id = if latest_role.is_empty() {
            ""
        } else {
            "latest-message"
        };
        json!({
            "id": id,
            "latest_message_id": latest_message_id,
            "latest_role": latest_role,
            "latest_created": latest_created,
            "latest_completed": latest_completed,
            "latest_has_error": u8::from(latest_has_error),
            "active_tool_message_id": active_tool_message_id.unwrap_or(""),
            "active_tool": active_tool,
            "active_tool_status": active_tool_status,
            "active_tool_started": active_tool_started,
        })
    }

    fn db_lifecycle(status: SessionStatus, since_ms: u64) -> DbLifecycle {
        let (authority, reason) = match status {
            SessionStatus::Waiting
            | SessionStatus::Executing
            | SessionStatus::Error
            | SessionStatus::Thinking
            | SessionStatus::Idle => (StatusAuthority::Heuristic, StatusReason::CollectorInference),
            _ => (StatusAuthority::Unavailable, StatusReason::ProtocolUnknown),
        };
        DbLifecycle {
            status,
            authority,
            reason,
            active_tool: match status {
                SessionStatus::Waiting => Some("question".to_string()),
                SessionStatus::Executing => Some("bash".to_string()),
                _ => None,
            },
            since_ms,
            source_at_ms: since_ms,
        }
    }

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
                command: "rg opencode".to_string(),
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
        let pids = OpenCodeCollector::find_opencode_pids(&info);
        assert!(pids.contains(&100));
        assert!(!pids.contains(&200));
        assert!(pids.contains(&300));
        assert_eq!(pids.len(), 2);
    }

    #[test]
    fn process_classifier_rejects_unrelated_wrappers_and_non_session_modes() {
        for command in [
            "rg opencode",
            "less /tmp/opencode",
            "python /tmp/opencode",
            "opencode serve",
            "opencode web --port 3000",
            "opencode acp",
            "opencode session list",
            "opencode export ses_123",
        ] {
            assert!(
                !is_session_owning_opencode_process(command),
                "unexpected session owner: {command}"
            );
        }
        for command in [
            "/usr/local/bin/opencode",
            "node /usr/lib/node_modules/opencode/bin/opencode run fix",
            "opencode --session ses_123",
            "opencode run grep opencode",
            "opencode /work/project",
        ] {
            assert!(
                is_session_owning_opencode_process(command),
                "missed session owner: {command}"
            );
        }
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
    fn empty_and_root_paths_are_not_ownership_evidence() {
        assert_eq!(path_key(""), None);
        assert_eq!(path_key("/"), None);
    }

    #[test]
    fn command_path_matching_requires_an_exact_argument_boundary() {
        let key = path_key("/home/u/proj-a").unwrap();
        assert!(command_mentions_session_dir(
            "node /usr/bin/opencode run --cwd=/home/u/proj-a",
            &key
        ));
        assert!(command_mentions_session_dir(
            "opencode \"/home/u/proj-a\"",
            &key
        ));
        assert!(!command_mentions_session_dir(
            "opencode /home/u/proj-ab",
            &key
        ));
        assert!(!command_mentions_session_dir(
            "/home/u/proj-a/bin/opencode",
            &key
        ));
    }

    #[test]
    fn unique_directory_ownership_is_confirmed() {
        let sessions = vec![db_session("current", "/work/project", 100)];
        let processes = vec![live_process(42, "opencode", Some("/work/project"))];
        assert_eq!(
            plan_session_ownership(&sessions, &processes),
            vec![PlannedSessionOwnership {
                session_index: 0,
                ownership: confirmed(42),
            }]
        );
    }

    #[test]
    fn historical_row_cannot_own_a_new_process_incarnation() {
        let sessions = vec![db_session("stale", "/work/project", 100)];
        let mut process = live_process(42, "opencode", Some("/work/project"));
        process.started_at_ms = Some(100_000);
        assert_eq!(
            plan_session_ownership(&sessions, &[process]),
            vec![PlannedSessionOwnership {
                session_index: 0,
                ownership: ProcessOwnership::Unconfirmed,
            }]
        );
    }

    #[test]
    fn same_cwd_ambiguity_never_assigns_an_actionable_pid() {
        let sessions = vec![
            db_session("older", "/work/project", 100),
            db_session("newer", "/work/project", 200),
        ];
        let processes = vec![
            live_process(20, "opencode", Some("/work/project")),
            live_process(10, "opencode", Some("/work/project")),
        ];
        let forward = plan_session_ownership(&sessions, &processes);
        let reverse = plan_session_ownership(
            &sessions,
            &processes.iter().cloned().rev().collect::<Vec<_>>(),
        );
        assert_eq!(forward, reverse);
        assert_eq!(forward.len(), 2);
        assert!(forward
            .iter()
            .all(|row| row.ownership == ProcessOwnership::Unconfirmed));
    }

    #[test]
    fn one_pid_with_historical_rows_emits_only_newest_unknown_candidate() {
        let sessions = vec![
            db_session("old", "/work/project", 100),
            db_session("current", "/work/project", 300),
            db_session("middle", "/work/project", 200),
        ];
        let plan = plan_session_ownership(
            &sessions,
            &[live_process(42, "opencode", Some("/work/project"))],
        );
        assert_eq!(
            plan,
            vec![PlannedSessionOwnership {
                session_index: 1,
                ownership: ProcessOwnership::Unconfirmed,
            }]
        );
    }

    #[test]
    fn explicit_session_ids_disambiguate_same_cwd_processes() {
        let sessions = vec![
            db_session("ses_a", "/work/project", 100),
            db_session("ses_b", "/work/project", 200),
        ];
        let plan = plan_session_ownership(
            &sessions,
            &[
                live_process(20, "opencode --session ses_b", Some("/work/project")),
                live_process(10, "opencode -s=ses_a", Some("/work/project")),
            ],
        );
        assert_eq!(plan.len(), 2);
        assert!(plan
            .iter()
            .any(|row| { row.session_index == 0 && row.ownership == confirmed(10) }));
        assert!(plan
            .iter()
            .any(|row| { row.session_index == 1 && row.ownership == confirmed(20) }));
    }

    #[test]
    fn explicit_session_without_exact_process_start_is_not_confirmed() {
        let sessions = vec![db_session("ses_a", "/work/project", 100)];
        let mut process = live_process(10, "opencode --session ses_a", Some("/work/project"));
        process.started_at_ms = None;

        assert_eq!(
            plan_session_ownership(&sessions, &[process]),
            vec![PlannedSessionOwnership {
                session_index: 0,
                ownership: ProcessOwnership::Unconfirmed,
            }],
        );
    }

    #[test]
    fn lifecycle_parser_separates_durable_and_ambiguous_states() {
        let rows = vec![
            lifecycle_row(
                "wait",
                "assistant",
                100,
                0,
                false,
                "question",
                "running",
                110,
                Some("latest-message"),
            ),
            lifecycle_row(
                "pending-question",
                "assistant",
                200,
                0,
                false,
                "question",
                "pending",
                210,
                Some("latest-message"),
            ),
            lifecycle_row(
                "exec",
                "assistant",
                300,
                0,
                false,
                "bash",
                "running",
                310,
                Some("latest-message"),
            ),
            lifecycle_row("no-reply", "user", 400, 0, false, "", "", 0, None),
            lifecycle_row(
                "think-assistant",
                "assistant",
                500,
                0,
                false,
                "",
                "",
                0,
                None,
            ),
            lifecycle_row("idle", "assistant", 600, 650, false, "", "", 0, None),
            lifecycle_row("error", "assistant", 700, 750, true, "", "", 0, None),
            lifecycle_row("empty", "", 0, 0, false, "", "", 0, None),
        ];

        let states = parse_lifecycle_rows(rows).expect("valid lifecycle rows");
        assert_eq!(states["wait"].status, SessionStatus::Unknown);
        assert_eq!(states["wait"].authority, StatusAuthority::Unavailable);
        assert_eq!(states["wait"].source_at_ms, 110);
        assert_eq!(states["pending-question"].status, SessionStatus::Unknown);
        assert_eq!(states["exec"].status, SessionStatus::Unknown);
        assert_eq!(states["exec"].active_tool.as_deref(), Some("bash"));
        assert_eq!(states["no-reply"].status, SessionStatus::Unknown);
        assert_eq!(states["think-assistant"].status, SessionStatus::Thinking);
        assert_eq!(
            states["think-assistant"].authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(states["idle"].status, SessionStatus::Idle);
        assert_eq!(states["idle"].authority, StatusAuthority::Heuristic);
        assert_eq!(states["error"].status, SessionStatus::Error);
        assert_eq!(states["error"].reason, StatusReason::CollectorInference);
        assert_eq!(states["empty"].status, SessionStatus::Unknown);
    }

    #[test]
    fn lifecycle_parser_sanitizes_tool_name() {
        let states = parse_lifecycle_rows(vec![lifecycle_row(
            "tool",
            "assistant",
            1,
            0,
            false,
            "ba\u{202e}\u{0008}sh",
            "running",
            2,
            Some("latest-message"),
        )])
        .expect("valid lifecycle row");
        assert_eq!(states.len(), 1);
        assert_eq!(states["tool"].active_tool.as_deref(), Some("bash"));
    }

    #[test]
    fn session_wide_active_question_and_tool_override_a_queued_user_row() {
        let rows = vec![
            lifecycle_row(
                "queued-question",
                "user",
                500,
                0,
                false,
                "question",
                "running",
                400,
                Some("prior-assistant"),
            ),
            lifecycle_row(
                "queued-tool",
                "user",
                700,
                0,
                false,
                "bash",
                "running",
                600,
                Some("prior-assistant"),
            ),
        ];
        let states = parse_lifecycle_rows(rows).expect("valid queued lifecycle rows");

        assert_eq!(states["queued-question"].status, SessionStatus::Unknown);
        assert_eq!(states["queued-question"].source_at_ms, 400);
        assert_eq!(states["queued-tool"].status, SessionStatus::Unknown);
        assert_eq!(states["queued-tool"].source_at_ms, 600);
    }

    #[test]
    fn older_active_tool_does_not_override_a_newer_assistant_lifecycle() {
        let row = lifecycle_row(
            "new-assistant",
            "assistant",
            500,
            550,
            false,
            "question",
            "running",
            400,
            Some("prior-assistant"),
        );
        let states = parse_lifecycle_rows(vec![row]).expect("valid lifecycle row");

        assert_eq!(states["new-assistant"].status, SessionStatus::Idle);
        assert_eq!(states["new-assistant"].active_tool, None);
        assert_eq!(states["new-assistant"].source_at_ms, 550);
    }

    #[test]
    fn completed_assistant_supersedes_same_message_running_parts() {
        let rows = vec![
            lifecycle_row(
                "completed-question",
                "assistant",
                500,
                550,
                false,
                "question",
                "running",
                510,
                Some("latest-message"),
            ),
            lifecycle_row(
                "completed-tool",
                "assistant",
                600,
                650,
                false,
                "bash",
                "pending",
                610,
                Some("latest-message"),
            ),
            lifecycle_row(
                "failed-tool",
                "assistant",
                700,
                750,
                true,
                "bash",
                "running",
                710,
                Some("latest-message"),
            ),
        ];
        let states = parse_lifecycle_rows(rows).expect("valid terminal lifecycle rows");

        assert_eq!(states["completed-question"].status, SessionStatus::Idle);
        assert_eq!(states["completed-tool"].status, SessionStatus::Idle);
        assert_eq!(states["failed-tool"].status, SessionStatus::Error);
        assert!(states.values().all(|state| state.active_tool.is_none()));
    }

    #[test]
    fn malformed_lifecycle_row_fails_the_entire_parse_closed() {
        let valid = lifecycle_row("valid", "assistant", 1, 2, false, "", "", 0, None);
        for malformed in [
            json!({"id": ""}),
            {
                let mut row = lifecycle_row("bad-role", "assistant", 1, 0, false, "", "", 0, None);
                row["latest_role"] = json!("unexpected");
                row
            },
            {
                let mut row = lifecycle_row(
                    "bad-tool-state",
                    "assistant",
                    1,
                    0,
                    false,
                    "bash",
                    "running",
                    1,
                    Some("latest-message"),
                );
                row["active_tool_status"] = json!("mystery");
                row
            },
            {
                let mut row = valid.clone();
                row["latest_created"] = json!("now");
                row
            },
        ] {
            assert!(parse_lifecycle_rows(vec![valid.clone(), malformed]).is_none());
        }
    }

    #[test]
    fn fresh_inferred_lifecycle_reports_heuristic_authority() {
        let cached = CachedLifecycle {
            lifecycle: db_lifecycle(SessionStatus::Idle, 900),
            observed_at_ms: 1_000,
        };
        let decision = resolve_lifecycle_status(Some(&cached), true, 1_100);
        assert_eq!(decision.status, SessionStatus::Idle);
        assert_eq!(decision.evidence.authority, StatusAuthority::Heuristic);
        assert_eq!(decision.evidence.reason, StatusReason::CollectorInference);
        assert_eq!(decision.evidence.status_since_ms, 900);
    }

    #[test]
    fn failed_lifecycle_query_immediately_invalidates_every_cached_status() {
        for status in [
            SessionStatus::Waiting,
            SessionStatus::Executing,
            SessionStatus::Thinking,
            SessionStatus::Idle,
        ] {
            let cached = CachedLifecycle {
                lifecycle: db_lifecycle(status, 900),
                observed_at_ms: 1_000,
            };
            let decision = resolve_lifecycle_status(Some(&cached), false, 1_001);
            assert_eq!(decision.status, SessionStatus::Unknown, "cached={status:?}");
            assert_eq!(
                decision.evidence.authority,
                StatusAuthority::Unavailable,
                "cached={status:?}"
            );
            assert_eq!(
                decision.evidence.reason,
                StatusReason::BackgroundProbeFailed,
                "cached={status:?}"
            );
            assert_eq!(decision.active_tool, None, "cached={status:?}");
        }
    }

    #[test]
    fn successful_but_stale_lifecycle_cache_is_unavailable() {
        let cached = CachedLifecycle {
            lifecycle: db_lifecycle(SessionStatus::Waiting, 900),
            observed_at_ms: 1_000,
        };
        let decision =
            resolve_lifecycle_status(Some(&cached), true, 1_001 + LIFECYCLE_QUERY_FRESHNESS_MS);
        assert_eq!(decision.status, SessionStatus::Unknown);
        assert_eq!(decision.evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(decision.evidence.reason, StatusReason::Stale);
    }

    #[test]
    fn resolved_question_race_does_not_publish_waiting() {
        // OpenCode removes the live pending question before the tool runner
        // persists completion, so this exact DB row can also mean "already
        // answered". Fresh query time cannot disambiguate that interval.
        let lifecycle = parse_lifecycle_rows(vec![lifecycle_row(
            "question",
            "assistant",
            800,
            0,
            false,
            "question",
            "running",
            900,
            Some("latest-message"),
        )])
        .expect("valid question row")
        .remove("question")
        .unwrap();
        let cached = CachedLifecycle {
            lifecycle,
            observed_at_ms: 1_000,
        };
        let decision = resolve_lifecycle_status(Some(&cached), true, 1_001);

        assert_eq!(decision.status, SessionStatus::Unknown);
        assert_eq!(decision.evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(decision.evidence.reason, StatusReason::ProtocolUnknown);
    }

    #[test]
    fn inferred_error_uses_a_canonical_non_content_task_label() {
        let cached = CachedLifecycle {
            lifecycle: db_lifecycle(SessionStatus::Error, 900),
            observed_at_ms: 1_000,
        };
        let decision = resolve_lifecycle_status(Some(&cached), true, 1_001);

        assert_eq!(decision.status, SessionStatus::Error);
        assert_eq!(decision.task, "provider error");
        assert_eq!(decision.evidence.reason, StatusReason::CollectorInference);
    }

    #[test]
    fn ambiguous_ownership_overrides_fresh_lifecycle_status() {
        let cached = CachedLifecycle {
            lifecycle: db_lifecycle(SessionStatus::Executing, 900),
            observed_at_ms: 1_000,
        };
        let decision =
            resolve_session_status(ProcessOwnership::Unconfirmed, Some(&cached), true, 1_100);
        assert_eq!(decision.status, SessionStatus::Unknown);
        assert_eq!(decision.evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(decision.evidence.reason, StatusReason::OwnershipUnconfirmed);
    }

    #[test]
    fn persisted_lifecycle_from_before_exact_process_start_is_unknown() {
        for status in [
            SessionStatus::Waiting,
            SessionStatus::Executing,
            SessionStatus::Thinking,
            SessionStatus::Idle,
            SessionStatus::Error,
        ] {
            let cached = CachedLifecycle {
                lifecycle: db_lifecycle(status, 1_000),
                observed_at_ms: 20_000,
            };
            let decision = resolve_session_status(
                ProcessOwnership::Confirmed {
                    pid: 42,
                    started_at_ms: 10_000,
                },
                Some(&cached),
                true,
                20_001,
            );
            assert_eq!(decision.status, SessionStatus::Unknown, "status={status:?}");
            assert_eq!(
                decision.evidence.authority,
                StatusAuthority::Unavailable,
                "status={status:?}",
            );
            assert_eq!(
                decision.evidence.reason,
                StatusReason::Stale,
                "status={status:?}",
            );
        }
    }

    #[test]
    fn lifecycle_at_or_after_exact_process_start_remains_available() {
        let cached = CachedLifecycle {
            lifecycle: db_lifecycle(SessionStatus::Error, 10_000),
            observed_at_ms: 10_100,
        };
        let decision = resolve_session_status(
            ProcessOwnership::Confirmed {
                pid: 42,
                started_at_ms: 10_000,
            },
            Some(&cached),
            true,
            10_101,
        );

        assert_eq!(decision.status, SessionStatus::Error);
        assert_eq!(decision.evidence.authority, StatusAuthority::Heuristic);
    }

    #[test]
    fn missing_lifecycle_after_query_failure_is_unavailable() {
        let decision = resolve_lifecycle_status(None, false, 1_000);
        assert_eq!(decision.status, SessionStatus::Unknown);
        assert_eq!(decision.evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(
            decision.evidence.reason,
            StatusReason::BackgroundProbeFailed
        );
    }

    #[test]
    fn lifecycle_query_reads_session_wide_active_state_without_private_content() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }

        let temp = tempfile::tempdir().expect("create temp directory");
        let db_path = temp.path().join("opencode.db");
        let schema_and_rows = r#"
CREATE TABLE session (id TEXT PRIMARY KEY, time_updated INTEGER NOT NULL);
CREATE TABLE message (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  data TEXT NOT NULL
);
CREATE TABLE part (
  id TEXT PRIMARY KEY,
  message_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);
INSERT INTO session VALUES
  ('waiting', 1000), ('executing', 900), ('queued', 850), ('no-reply', 825),
  ('error', 810), ('aborted', 805), ('idle', 800);
INSERT INTO message VALUES
  ('w-old', 'waiting', 10, '{"role":"assistant","time":{"created":10,"completed":20}}'),
  ('w-new', 'waiting', 30, '{"role":"assistant","time":{"created":30}}'),
  ('e-new', 'executing', 40, '{"role":"assistant","time":{"created":40}}'),
  ('q-old', 'queued', 41, '{"role":"assistant","time":{"created":41}}'),
  ('q-new', 'queued', 42, '{"role":"user","time":{"created":42}}'),
  ('n-new', 'no-reply', 45, '{"role":"user","time":{"created":45}}'),
  ('err-new', 'error', 48, '{"role":"assistant","time":{"created":48,"completed":49},"finish":"error","error":{"name":"APIError","data":{"message":"must-not-be-read"}}}'),
  ('abort-new', 'aborted', 49, '{"role":"assistant","time":{"created":49,"completed":50},"finish":"error","error":{"name":"MessageAbortedError","data":{"message":"must-not-be-read"}}}'),
  ('i-new', 'idle', 50, '{"role":"assistant","time":{"created":50,"completed":60}}');
INSERT INTO part VALUES
  ('stale', 'w-old', 'waiting', 20, '{"type":"tool","tool":"bash","state":{"status":"running","time":{"start":11},"input":{"secret":"must-not-be-read"}}}'),
  ('question', 'w-new', 'waiting', 31, '{"type":"tool","tool":"question","state":{"status":"running","time":{"start":31},"input":{"secret":"must-not-be-read"}}}'),
  ('bash', 'e-new', 'executing', 41, '{"type":"tool","tool":"bash","state":{"status":"pending","raw":"must-not-be-read"}}'),
  ('queued-question', 'q-old', 'queued', 41, '{"type":"tool","tool":"question","state":{"status":"running","time":{"start":41}}}');
"#;
        let created = Command::new("sqlite3")
            .arg(&db_path)
            .arg(schema_and_rows)
            .output()
            .expect("create SQLite fixture");
        assert!(
            created.status.success(),
            "sqlite3 fixture failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        let collector = OpenCodeCollector {
            db_path,
            sqlite3_available: Some(true),
            cached_db_sessions: vec![],
            cached_db_lifecycles: HashMap::new(),
            lifecycle_query_succeeded: false,
            #[cfg(target_os = "windows")]
            warned_sqlite3_missing: false,
        };
        let states = collector.query_lifecycles().expect("query lifecycle");

        assert_eq!(states["waiting"].status, SessionStatus::Unknown);
        assert_eq!(states["waiting"].active_tool.as_deref(), Some("question"));
        assert_eq!(states["executing"].status, SessionStatus::Unknown);
        assert_eq!(states["executing"].active_tool.as_deref(), Some("bash"));
        assert_eq!(states["queued"].status, SessionStatus::Unknown);
        assert_eq!(states["queued"].active_tool.as_deref(), Some("question"));
        assert_eq!(states["no-reply"].status, SessionStatus::Unknown);
        assert_eq!(states["error"].status, SessionStatus::Error);
        assert_eq!(states["aborted"].status, SessionStatus::Idle);
        assert_eq!(states["idle"].status, SessionStatus::Idle);
    }
}
