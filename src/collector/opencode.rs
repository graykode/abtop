use super::process;
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// Maximum sessions to fetch from the DB per query.
const MAX_SESSIONS: u32 = 20;

/// Collector for OpenCode sessions.
///
/// Discovery strategy:
/// 1. `ps` to find running opencode processes (from shared process data)
/// 2. Query SQLite DB at ~/.local/share/opencode/opencode.db via `sqlite3` CLI
/// 3. Match running PIDs to sessions by cwd
///
/// Uses `sqlite3 -readonly -json` for safe concurrent reads (WAL mode).
/// Queries run on slow ticks only (every ~10s via MultiCollector gating)
/// to avoid forking a sqlite3 process every 2s.
pub struct OpenCodeCollector {
    db_path: PathBuf,
    /// Whether sqlite3 CLI is available (checked once).
    sqlite3_available: Option<bool>,
}

impl OpenCodeCollector {
    pub fn new() -> Self {
        let data_dir = std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
        Self {
            db_path: data_dir.join("opencode").join("opencode.db"),
            sqlite3_available: None,
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
        if !self.db_path.exists() || !self.check_sqlite3() {
            return vec![];
        }

        // Find running opencode PIDs and their commands for cwd matching
        let opencode_pids = Self::find_opencode_pids(&shared.process_info);
        let pid_commands: HashMap<u32, &str> = opencode_pids.iter()
            .filter_map(|&pid| {
                shared.process_info.get(&pid).map(|p| (pid, p.command.as_str()))
            })
            .collect();

        // Query sessions from SQLite
        let db_sessions = match self.query_sessions() {
            Some(s) => s,
            None => return vec![],
        };

        let now_ms = current_time_ms();
        let mut sessions = Vec::new();

        for ds in db_sessions {
            let matched_pid = Self::match_pid_to_session(&pid_commands, &ds.directory);
            let pid_alive = matched_pid.is_some();
            let display_pid = matched_pid.unwrap_or(0);

            let proc = matched_pid.and_then(|p| shared.process_info.get(&p));
            let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

            // Only show live sessions or recently finished (< 5 min)
            let age_ms = now_ms.saturating_sub(ds.time_updated);
            if !pid_alive && age_ms > 300_000 {
                continue;
            }

            let status = if !pid_alive {
                SessionStatus::Done
            } else {
                let since_update_secs = age_ms / 1000;
                if since_update_secs < 30 {
                    SessionStatus::Working
                } else {
                    let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
                    let has_active_child = matched_pid.is_some_and(|p| {
                        process::has_active_descendant(p, &shared.children_map, &shared.process_info, 5.0)
                    });
                    if cpu_active || has_active_child {
                        SessionStatus::Working
                    } else {
                        SessionStatus::Waiting
                    }
                }
            };

            let project_name = if !ds.project_name.is_empty() {
                ds.project_name
            } else {
                ds.directory.rsplit('/').next().unwrap_or("?").to_string()
            };

            let current_tasks = if matches!(status, SessionStatus::Waiting) {
                vec!["waiting for input".to_string()]
            } else if !pid_alive {
                vec!["finished".to_string()]
            } else {
                vec!["thinking...".to_string()]
            };

            // Collect child processes with cycle guard (visited set)
            let mut children = Vec::new();
            if let Some(pid) = matched_pid {
                let mut stack: Vec<u32> = shared.children_map
                    .get(&pid).cloned().unwrap_or_default();
                let mut visited = std::collections::HashSet::new();
                while let Some(cpid) = stack.pop() {
                    if !visited.insert(cpid) { continue; }
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
            }

            let model = if !ds.provider.is_empty() && !ds.model.is_empty() {
                format!("{}/{}", ds.provider, ds.model)
            } else if !ds.model.is_empty() {
                ds.model
            } else {
                "-".to_string()
            };

            sessions.push(AgentSession {
                agent_cli: "opencode",
                pid: display_pid,
                session_id: ds.id,
                cwd: ds.directory,
                project_name,
                started_at: ds.time_created,
                status,
                model,
                effort: String::new(),
                context_percent: 0.0,
                total_input_tokens: ds.total_input,
                total_output_tokens: ds.total_output,
                total_cache_read: ds.total_cache_read,
                total_cache_create: ds.total_cache_write,
                turn_count: ds.turn_count,
                current_tasks,
                mem_mb,
                version: ds.version,
                git_branch: String::new(),
                git_added: 0,
                git_modified: 0,
                token_history: vec![],
                subagents: vec![],
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt: ds.title,
                first_assistant_text: String::new(),
            });
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn find_opencode_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
        process_info.iter()
            .filter(|(_, info)| {
                process::cmd_has_binary(&info.command, "opencode")
                    && !info.command.contains("grep")
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Match a running PID to a session by checking /proc/pid/cwd,
    /// falling back to command-line substring match, then single-process match.
    fn match_pid_to_session(
        pid_commands: &HashMap<u32, &str>,
        session_dir: &str,
    ) -> Option<u32> {
        for (&pid, &cmd) in pid_commands {
            // Primary: check actual working directory via /proc
            if let Ok(cwd) = std::fs::read_link(format!("/proc/{}/cwd", pid)) {
                if cwd.to_string_lossy() == session_dir {
                    return Some(pid);
                }
            }
            // Fallback: session directory in command line
            if cmd.contains(session_dir) {
                return Some(pid);
            }
        }
        // Last resort: if only one opencode process, match it
        if pid_commands.len() == 1 {
            return pid_commands.keys().next().copied();
        }
        None
    }

    fn query_sessions(&self) -> Option<Vec<DbSession>> {
        let query = format!(r#"
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
LIMIT {};
"#, MAX_SESSIONS);

        // Model/provider require a separate correlated subquery (latest assistant msg)
        let model_query = format!(r#"
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
LIMIT {};
"#, MAX_SESSIONS);

        // Run both queries in one sqlite3 invocation
        let combined = format!("{}\n{}", query, model_query);
        let output = Command::new("sqlite3")
            .args(["-readonly", "-json", self.db_path.to_str()?])
            .arg(&combined)
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Some(vec![]);
        }

        // sqlite3 -json outputs one JSON array per query, concatenated
        // Parse the first array (session data) and second (model data)
        let arrays: Vec<&str> = stdout.trim().split("][").collect();
        let sessions_json = if arrays.len() > 1 {
            format!("{}]", arrays[0])
        } else {
            stdout.trim().to_string()
        };
        let models_json = if arrays.len() > 1 {
            format!("[{}", arrays[1])
        } else {
            String::new()
        };

        let rows: Vec<Value> = serde_json::from_str(&sessions_json).ok()?;
        let model_rows: Vec<Value> = if !models_json.is_empty() {
            serde_json::from_str(&models_json).unwrap_or_default()
        } else {
            vec![]
        };

        // Build model lookup by session id
        let mut model_map: HashMap<String, (String, String)> = HashMap::new();
        for mr in &model_rows {
            if let Some(id) = mr["id"].as_str() {
                model_map.insert(
                    id.to_string(),
                    (
                        mr["model"].as_str().unwrap_or("").to_string(),
                        mr["provider"].as_str().unwrap_or("").to_string(),
                    ),
                );
            }
        }

        let mut sessions = Vec::new();
        for row in rows {
            let id = row["id"].as_str().unwrap_or("").to_string();
            let (model, provider) = model_map.remove(&id).unwrap_or_default();
            sessions.push(DbSession {
                id,
                title: row["title"].as_str().unwrap_or("").to_string(),
                directory: row["directory"].as_str().unwrap_or("").to_string(),
                version: row["version"].as_str().unwrap_or("").to_string(),
                time_created: row["time_created"].as_u64().unwrap_or(0),
                time_updated: row["time_updated"].as_u64().unwrap_or(0),
                project_name: row["project_name"].as_str().unwrap_or("").to_string(),
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
}

impl super::AgentCollector for OpenCodeCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

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
        info.insert(100, process::ProcInfo {
            pid: 100, ppid: 1, rss_kb: 1000, cpu_pct: 0.0,
            command: "/home/user/.opencode/bin/opencode".to_string(),
        });
        info.insert(200, process::ProcInfo {
            pid: 200, ppid: 1, rss_kb: 500, cpu_pct: 0.0,
            command: "grep opencode".to_string(),
        });
        info.insert(300, process::ProcInfo {
            pid: 300, ppid: 1, rss_kb: 800, cpu_pct: 0.0,
            command: "node /usr/bin/opencode run test".to_string(),
        });
        let pids = OpenCodeCollector::find_opencode_pids(&info);
        assert!(pids.contains(&100));
        assert!(!pids.contains(&200)); // grep excluded
        assert!(pids.contains(&300));
        assert_eq!(pids.len(), 2);
    }

    #[test]
    fn test_db_path_default() {
        let collector = OpenCodeCollector::new();
        let path_str = collector.db_path.to_string_lossy();
        assert!(path_str.contains("opencode"));
        assert!(path_str.ends_with("opencode.db"));
    }
}
