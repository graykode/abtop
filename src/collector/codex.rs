use super::process::{self, ProcInfo};
use crate::model::{AgentSession, ChildProcess, RateLimitInfo, SessionStatus};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Collector for OpenAI Codex CLI sessions.
///
/// Discovery strategy (no PID session file like Claude):
/// 1. `ps` to find running codex processes
/// 2. `lsof` to map PID → open rollout-*.jsonl file
/// 3. Parse JSONL for session metadata, tokens, tool usage
///
/// JSONL event types:
/// - `session_meta`: session ID, cwd, cli_version, model_provider, git info
/// - `event_msg` subtypes: task_started, user_message, token_count, agent_message, task_complete
/// - `response_item`: assistant messages (commentary/final), function_call, function_call_output
/// - `turn_context`: model, cwd, effort, context window size
pub struct CodexCollector {
    sessions_dir: PathBuf,
    /// Latest rate limit info parsed from Codex JSONL token_count events.
    pub last_rate_limit: Option<RateLimitInfo>,
}

impl CodexCollector {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            sessions_dir: home.join(".codex").join("sessions"),
            last_rate_limit: None,
        }
    }

    pub fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !self.sessions_dir.exists() {
            self.last_rate_limit = None;
            return vec![];
        }

        // Reset rate limit each pass — only keep it if a current session provides one
        self.last_rate_limit = None;

        // Step 1: Find running codex processes from shared ps data (no extra ps call)
        let codex_pids = Self::find_codex_pids_from_shared(&shared.process_info);
        let just_pids: Vec<u32> = codex_pids.iter().map(|(p, _)| *p).collect();
        let pid_to_jsonl = Self::map_pid_to_jsonl(&just_pids);
        let pid_is_exec: HashMap<u32, bool> = codex_pids.into_iter().collect();

        let mut sessions = Vec::new();
        let mut seen_jsonl = std::collections::HashSet::new();

        // Active sessions: running codex processes with open JSONL files
        for (pid, jsonl_path) in &pid_to_jsonl {
            let is_exec = pid_is_exec.get(pid).copied().unwrap_or(false);
            if let Some((session, rl)) = self.load_session_with_rate_limit(
                Some(*pid),
                is_exec,
                jsonl_path,
                &shared.process_info,
                &shared.children_map,
                &shared.ports,
            ) {
                seen_jsonl.insert(jsonl_path.clone());
                if let Some(new_rl) = rl {
                    let newer = self.last_rate_limit.as_ref()
                        .is_none_or(|old| new_rl.updated_at > old.updated_at);
                    if newer { self.last_rate_limit = Some(new_rl); }
                }
                sessions.push(session);
            }
        }

        // Recently finished sessions: scan today's JSONL files not owned by any running process.
        // This ensures Codex sessions transition to Done instead of vanishing.
        if let Some(recent_dir) = Self::today_session_dir(&self.sessions_dir) {
            if let Ok(entries) = fs::read_dir(&recent_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if seen_jsonl.contains(&path) {
                        continue;
                    }
                    // Only show recently finished sessions (< 5 min old)
                    if let Ok(meta) = fs::metadata(&path) {
                        if let Ok(modified) = meta.modified() {
                            let age = std::time::SystemTime::now()
                                .duration_since(modified)
                                .unwrap_or_default();
                            if age.as_secs() > 300 {
                                continue;
                            }
                        }
                    }
                    if let Some((session, rl)) = self.load_session_with_rate_limit(
                        None,
                        false,
                        &path,
                        &shared.process_info,
                        &shared.children_map,
                        &shared.ports,
                    ) {
                        if let Some(new_rl) = rl {
                            let newer = self.last_rate_limit.as_ref()
                                .is_none_or(|old| new_rl.updated_at > old.updated_at);
                            if newer { self.last_rate_limit = Some(new_rl); }
                        }
                        sessions.push(session);
                    }
                }
            }
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Get today's session directory path: ~/.codex/sessions/YYYY/MM/DD
    fn today_session_dir(sessions_dir: &Path) -> Option<PathBuf> {
        let now = chrono::Local::now();
        let dir = sessions_dir
            .join(now.format("%Y").to_string())
            .join(now.format("%m").to_string())
            .join(now.format("%d").to_string());
        if dir.exists() { Some(dir) } else { None }
    }

    fn load_session_with_rate_limit(
        &self,
        pid: Option<u32>,
        is_exec: bool,
        jsonl_path: &Path,
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
    ) -> Option<(AgentSession, Option<RateLimitInfo>)> {
        let result = parse_codex_jsonl(jsonl_path)?;

        let proc = pid.and_then(|p| process_info.get(&p));
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);
        let display_pid = pid.unwrap_or(0);

        let project_name = result
            .cwd
            .rsplit('/')
            .next()
            .unwrap_or("?")
            .to_string();

        // Status detection
        // Note: Codex interactive sessions emit task_complete after every turn,
        // so task_complete alone does NOT mean the session is finished when PID is alive.
        // However, for exec (one-shot) sessions, task_complete means truly done.
        let pid_alive = proc.is_some();
        let status = if !pid_alive || (is_exec && result.task_complete) {
            SessionStatus::Done
        } else {
            let since_activity = std::time::SystemTime::now()
                .duration_since(result.last_activity)
                .unwrap_or_default();
            if since_activity.as_secs() < 30 {
                SessionStatus::Working
            } else {
                let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
                let has_active_child = pid.is_some_and(|p| {
                    process::has_active_descendant(p, children_map, process_info, 5.0)
                });
                if cpu_active || has_active_child {
                    SessionStatus::Working
                } else {
                    SessionStatus::Waiting
                }
            }
        };

        // Current task from last tool use
        // For exec (one-shot) sessions, task_complete means truly finished.
        // For interactive sessions, task_complete fires after every turn — ignore it.
        let current_tasks = if !result.current_task.is_empty() {
            vec![result.current_task]
        } else if !pid_alive || (is_exec && result.task_complete) {
            vec!["finished".to_string()]
        } else if matches!(status, SessionStatus::Waiting) {
            vec!["waiting for input".to_string()]
        } else {
            vec!["thinking...".to_string()]
        };

        // Context window percentage from token usage
        let context_percent = if result.context_window > 0 && result.last_context_tokens > 0 {
            (result.last_context_tokens as f64 / result.context_window as f64) * 100.0
        } else {
            0.0
        };

        // Children: collect all descendants recursively (not just direct children)
        // so we catch grandchild processes that listen on ports.
        let mut children = Vec::new();
        if let Some(p) = pid {
            let mut stack: Vec<u32> = children_map
                .get(&p)
                .cloned()
                .unwrap_or_default();
            while let Some(cpid) = stack.pop() {
                if let Some(cproc) = process_info.get(&cpid) {
                    let port = ports.get(&cpid).and_then(|v| v.first().copied());
                    children.push(ChildProcess {
                        pid: cpid,
                        command: cproc.command.clone(),
                        mem_kb: cproc.rss_kb,
                        port,
                    });
                }
                if let Some(grandchildren) = children_map.get(&cpid) {
                    stack.extend(grandchildren);
                }
            }
        }

        // Git stats: populated by MultiCollector on slow ticks
        let (git_added, git_modified) = (0, 0);
        let rate_limit = result.rate_limit.clone();

        Some((AgentSession {
            agent_cli: "codex",
            pid: display_pid,
            session_id: result.session_id,
            cwd: result.cwd,
            project_name,
            started_at: result.started_at,
            status,
            model: result.model,
            context_percent,
            total_input_tokens: result.total_input,
            total_output_tokens: result.total_output,
            total_cache_read: result.total_cache_read,
            total_cache_create: 0, // Codex doesn't report cache write
            turn_count: result.turn_count,
            current_tasks,
            mem_mb,
            version: result.version,
            git_branch: result.git_branch,
            git_added,
            git_modified,
            token_history: result.token_history,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children,
            transcript_offset: 0,
            initial_prompt: result.initial_prompt,
        }, rate_limit))
    }

    /// Find PIDs of running codex processes from shared process data (no extra ps call).
    /// Returns (pid, is_exec) tuples — `is_exec` is true for one-shot `codex exec` runs.
    fn find_codex_pids_from_shared(process_info: &HashMap<u32, ProcInfo>) -> Vec<(u32, bool)> {
        let mut pids = Vec::new();
        for (pid, info) in process_info {
            let cmd = &info.command;
            let is_exec = cmd.contains(" exec");
            if cmd.contains("/codex")
                && !cmd.contains("app-server")
                && !cmd.contains("grep")
                && !cmd.starts_with("node ")
            {
                pids.push((*pid, is_exec));
            }
        }
        pids
    }

    /// Map codex PIDs to their open rollout-*.jsonl files via lsof.
    fn map_pid_to_jsonl(pids: &[u32]) -> HashMap<u32, PathBuf> {
        let mut map = HashMap::new();
        if pids.is_empty() {
            return map;
        }

        // Build lsof command for all PIDs at once
        let pid_args: Vec<String> = pids.iter().map(|p| format!("-p{}", p)).collect();
        let mut args = vec!["-F", "pn"];
        for pa in &pid_args {
            args.push(pa);
        }

        let output = Command::new("lsof")
            .args(&args)
            .output()
            .ok();

        if let Some(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut current_pid: Option<u32> = None;
            for line in stdout.lines() {
                if let Some(pid_str) = line.strip_prefix('p') {
                    current_pid = pid_str.parse::<u32>().ok();
                } else if let Some(name) = line.strip_prefix('n') {
                    if let Some(pid) = current_pid {
                        if name.contains("rollout-") && name.ends_with(".jsonl") {
                            map.insert(pid, PathBuf::from(name));
                        }
                    }
                }
            }
        }
        map
    }

}

/// Parsed result from a Codex rollout JSONL file.
struct CodexJSONLResult {
    session_id: String,
    cwd: String,
    started_at: u64,
    model: String,
    version: String,
    git_branch: String,
    context_window: u64,
    turn_count: u32,
    current_task: String,
    task_complete: bool,
    last_activity: std::time::SystemTime,
    initial_prompt: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    last_context_tokens: u64,
    token_history: Vec<u64>,
    /// Rate limit info from the latest token_count event.
    rate_limit: Option<RateLimitInfo>,
}

/// Parse a Codex rollout-*.jsonl file.
///
/// Event types:
/// - session_meta: session ID, cwd, version, git
/// - event_msg.task_started: context window size
/// - event_msg.token_count: rate limits (handled at app level)
/// - event_msg.user_message: user prompt
/// - event_msg.agent_message: turn count
/// - event_msg.task_complete: session done
/// - response_item (function_call): current tool use
/// - turn_context: model, effort
fn parse_codex_jsonl(path: &Path) -> Option<CodexJSONLResult> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut result = CodexJSONLResult {
        session_id: String::new(),
        cwd: String::new(),
        started_at: 0,
        model: String::from("-"),
        version: String::new(),
        git_branch: String::new(),
        context_window: 0,
        turn_count: 0,
        current_task: String::new(),
        task_complete: false,
        last_activity: std::time::UNIX_EPOCH,
        initial_prompt: String::new(),
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        last_context_tokens: 0,
        token_history: Vec::new(),
        rate_limit: None,
    };

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }

        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // partial line at EOF
        };

        // Update last_activity from timestamp
        if let Some(ts_str) = val["timestamp"].as_str() {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts_str) {
                let sys_time = std::time::UNIX_EPOCH
                    + std::time::Duration::from_millis(dt.timestamp_millis() as u64);
                if sys_time > result.last_activity {
                    result.last_activity = sys_time;
                }
            }
        }

        match val["type"].as_str() {
            Some("session_meta") => {
                let payload = &val["payload"];
                if let Some(id) = payload["id"].as_str() {
                    result.session_id = id.to_string();
                }
                if let Some(cwd) = payload["cwd"].as_str() {
                    result.cwd = cwd.to_string();
                }
                if let Some(ver) = payload["cli_version"].as_str() {
                    result.version = ver.to_string();
                }
                // started_at from timestamp
                if let Some(ts) = payload["timestamp"].as_str() {
                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                        result.started_at = dt.timestamp_millis() as u64;
                    }
                }
                // Git branch
                if let Some(branch) = payload["git"]["branch"].as_str() {
                    result.git_branch = branch.to_string();
                }
            }

            Some("event_msg") => {
                let payload = &val["payload"];
                match payload["type"].as_str() {
                    Some("task_started") => {
                        if let Some(cw) = payload["model_context_window"].as_u64() {
                            result.context_window = cw;
                        }
                    }
                    Some("user_message") => {
                        if result.initial_prompt.is_empty() {
                            if let Some(msg) = payload["message"].as_str() {
                                result.initial_prompt = msg.chars().take(120).collect();
                            }
                        }
                    }
                    Some("token_count") => {
                        let info = &payload["info"];
                        // Use total_token_usage as cumulative snapshot for totals
                        let total = &info["total_token_usage"];
                        if total.is_object() {
                            let inp = total["input_tokens"].as_u64().unwrap_or(0);
                            let out = total["output_tokens"].as_u64().unwrap_or(0);
                            let cache = total["cached_input_tokens"].as_u64()
                                .or_else(|| total["cache_read_input_tokens"].as_u64())
                                .unwrap_or(0);
                            result.total_input = inp;
                            result.total_output = out;
                            result.total_cache_read = cache;
                        }
                        // Use last_token_usage for context % and sparkline
                        let last = &info["last_token_usage"];
                        if last.is_object() {
                            let inp = last["input_tokens"].as_u64().unwrap_or(0);
                            let out = last["output_tokens"].as_u64().unwrap_or(0);
                            let cache = last["cached_input_tokens"].as_u64()
                                .or_else(|| last["cache_read_input_tokens"].as_u64())
                                .unwrap_or(0);
                            result.last_context_tokens = inp + cache;
                            result.token_history.push(inp + out + cache);
                        }
                        // Context window may also appear inside info
                        if let Some(cw) = info["model_context_window"].as_u64() {
                            result.context_window = cw;
                        }
                        // Rate limits: primary = 5h window, secondary = 7d window
                        // Lives at payload.rate_limits (NOT inside info)
                        let rl = &payload["rate_limits"];
                        if rl.is_object() {
                            let primary = &rl["primary"];
                            let secondary = &rl["secondary"];
                            // Use event timestamp so newer events always win;
                            // leave updated_at unset if no timestamp is available.
                            let event_secs = val["timestamp"].as_str()
                                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                                .map(|dt| dt.timestamp() as u64);
                            result.rate_limit = Some(RateLimitInfo {
                                source: "codex".to_string(),
                                five_hour_pct: primary["used_percent"].as_f64(),
                                five_hour_resets_at: primary["resets_at"].as_u64(),
                                seven_day_pct: secondary["used_percent"].as_f64(),
                                seven_day_resets_at: secondary["resets_at"].as_u64(),
                                updated_at: event_secs,
                            });
                        }
                    }
                    Some("agent_message") => {
                        result.turn_count += 1;
                    }
                    Some("task_complete") => {
                        result.task_complete = true;
                    }
                    _ => {}
                }
            }

            Some("response_item") => {
                let payload = &val["payload"];
                // Track current tool use
                if payload["type"].as_str() == Some("function_call") {
                    if let Some(name) = payload["name"].as_str() {
                        // Extract first arg (typically file path or command)
                        let arg = payload["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .and_then(|v| {
                                v["file_path"]
                                    .as_str()
                                    .or_else(|| v["cmd"].as_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_default();

                        if arg.is_empty() {
                            result.current_task = name.to_string();
                        } else {
                            // Shorten path: just filename
                            let short = arg.rsplit('/').next().unwrap_or(&arg);
                            result.current_task = format!("{} {}", name, short);
                        }
                    }
                }
            }

            Some("turn_context") => {
                let payload = &val["payload"];
                if let Some(m) = payload["model"].as_str() {
                    result.model = m.to_string();
                }
                if let Some(cw) = payload["model_context_window"].as_u64() {
                    result.context_window = cw;
                }
            }

            _ => {}
        }
    }

    if result.session_id.is_empty() {
        return None;
    }

    Some(result)
}
