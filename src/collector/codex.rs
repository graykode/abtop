use super::process::{self, ProcInfo};
use crate::model::{AgentSession, ChildProcess, RateLimitInfo, SessionStatus, ToolCall};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "linux"))]
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

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !self.sessions_dir.exists() {
            self.last_rate_limit = None;
            return vec![];
        }

        // Reset live rate limit each pass — only keep it if a current session provides one
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
                    let newer = self
                        .last_rate_limit
                        .as_ref()
                        .is_none_or(|old| new_rl.updated_at > old.updated_at);
                    if newer {
                        super::rate_limit::write_codex_cache(&new_rl);
                        self.last_rate_limit = Some(new_rl);
                    }
                }
                sessions.push(session);
            }
        }

        // Recently finished sessions: scan today's JSONL files not owned by any running process.
        // This ensures Codex sessions transition to Done instead of vanishing.
        if let Some(recent_dir) = Self::today_session_dir(&self.sessions_dir) {
            if let Ok(entries) = fs::read_dir(&recent_dir) {
                for entry in entries.flatten() {
                    // Skip symlinks to avoid reading unintended files
                    if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
                        continue;
                    }
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
                            let newer = self
                                .last_rate_limit
                                .as_ref()
                                .is_none_or(|old| new_rl.updated_at > old.updated_at);
                            if newer {
                                super::rate_limit::write_codex_cache(&new_rl);
                                self.last_rate_limit = Some(new_rl);
                            }
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
        if dir.exists() {
            Some(dir)
        } else {
            None
        }
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

        let project_name = result.cwd.rsplit('/').next().unwrap_or("?").to_string();

        // Status detection
        // Note: Codex interactive sessions emit task_complete after every turn,
        // so task_complete alone does NOT mean the session is finished when PID is alive.
        // However, for exec (one-shot) sessions, task_complete means truly done.
        let pid_alive = proc.is_some();
        // Mirrors Claude: trust the trailing-event-is-user signal alone.
        // Codex tool outputs flow through response_item, not user_message,
        // so model_generating only flips on real prompts.
        let status = if !pid_alive || (is_exec && result.task_complete) {
            SessionStatus::Done
        } else {
            let has_active_child = pid.is_some_and(|p| {
                process::has_active_descendant(p, children_map, process_info, 5.0)
            });
            if has_active_child || result.pending_since_ms > 0 {
                SessionStatus::Executing
            } else if result.model_generating {
                SessionStatus::Thinking
            } else {
                SessionStatus::Waiting
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
            let mut stack: Vec<u32> = children_map.get(&p).cloned().unwrap_or_default();
            let mut visited = std::collections::HashSet::new();
            while let Some(cpid) = stack.pop() {
                if !visited.insert(cpid) {
                    continue;
                }
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

        Some((
            AgentSession {
                agent_cli: "codex",
                pid: display_pid,
                session_id: result.session_id,
                cwd: result.cwd,
                project_name,
                started_at: result.started_at,
                status,
                model: result.model,
                effort: result.effort,
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
                context_history: vec![],
                compaction_count: 0,
                context_window: result.context_window,
                subagents: vec![],
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt: result.initial_prompt,
                first_assistant_text: String::new(),
                tool_calls: result.tool_calls,
                pending_since_ms: result.pending_since_ms,
                thinking_since_ms: result.thinking_since_ms,
                file_accesses: vec![],
            },
            rate_limit,
        ))
    }

    /// Find PIDs of running codex processes from shared process data (no extra ps call).
    /// Returns (pid, is_exec) tuples — `is_exec` is true for one-shot `codex exec` runs.
    fn find_codex_pids_from_shared(process_info: &HashMap<u32, ProcInfo>) -> Vec<(u32, bool)> {
        let mut pids = Vec::new();
        for (pid, info) in process_info {
            let cmd = &info.command;
            let is_exec = cmd.contains(" exec");
            let is_codex = process::cmd_has_binary(cmd, "codex");
            if is_codex && !cmd.contains("app-server") && !cmd.contains("grep") {
                pids.push((*pid, is_exec));
            }
        }
        pids
    }

    /// Map codex PIDs to their open rollout-*.jsonl files.
    ///
    /// On Linux, scans /proc/{pid}/fd symlinks directly (no process spawn).
    /// Falls back to lsof on other platforms.
    fn map_pid_to_jsonl(pids: &[u32]) -> HashMap<u32, PathBuf> {
        let mut map = HashMap::new();
        if pids.is_empty() {
            return map;
        }

        #[cfg(target_os = "linux")]
        {
            for &pid in pids {
                for target in process::scan_proc_fds(pid) {
                    let is_rollout = target
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
                    if is_rollout {
                        map.insert(pid, target);
                        break;
                    }
                }
            }
            map
        }

        #[cfg(not(target_os = "linux"))]
        {
            // Fallback: lsof on macOS and other platforms
            let pid_args: Vec<String> = pids.iter().map(|p| format!("-p{}", p)).collect();
            let mut args = vec!["-F", "pn"];
            for pa in &pid_args {
                args.push(pa);
            }

            let output = Command::new("lsof").args(&args).output().ok();

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
}

impl super::AgentCollector for CodexCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }

    fn live_rate_limit(&self) -> Option<RateLimitInfo> {
        self.last_rate_limit
            .clone()
            .or_else(super::rate_limit::read_codex_cache)
    }
}

/// Parsed result from a Codex rollout JSONL file.
struct CodexJSONLResult {
    session_id: String,
    cwd: String,
    started_at: u64,
    model: String,
    /// Reasoning effort setting from turn_context: "minimal" | "low" | "medium" | "high".
    /// Tracks the most recent value — users can change `/effort` mid-session.
    effort: String,
    version: String,
    git_branch: String,
    context_window: u64,
    turn_count: u32,
    current_task: String,
    task_complete: bool,
    /// True iff the latest event in the rollout is a `user_message` with
    /// no `agent_message` after it — i.e. the model has been prompted
    /// but has not yet replied. Combined with recent rollout mtime this
    /// gates the Thinking status. Mirrors Claude's `last_user_ts_ms > 0`.
    model_generating: bool,
    last_activity: std::time::SystemTime,
    initial_prompt: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    last_context_tokens: u64,
    token_history: Vec<u64>,
    /// Rate limit info from the latest token_count event.
    rate_limit: Option<RateLimitInfo>,
    /// Timeline of tool calls extracted from response_item.function_call events.
    tool_calls: Vec<ToolCall>,
    /// Earliest start timestamp among currently open tool calls.
    pending_since_ms: u64,
    /// Timestamp of the latest user prompt not yet followed by assistant output.
    thinking_since_ms: u64,
}

fn event_timestamp_ms(val: &Value) -> Option<u64> {
    val["timestamp"]
        .as_str()
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
}

fn value_to_tool_arg(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    if let Some(items) = value.as_array() {
        let parts: Vec<&str> = items.iter().filter_map(|item| item.as_str()).collect();
        if parts.is_empty() {
            return None;
        }
        if parts.len() >= 3 && parts[0] == "bash" && parts[1] == "-lc" {
            return Some(parts[2].to_string());
        }
        return Some(parts.join(" "));
    }
    if value.is_number() || value.is_boolean() {
        return Some(value.to_string());
    }
    None
}

fn sanitize_tool_arg(arg: &str) -> String {
    let redacted = super::redact_secrets(arg);
    redacted.chars().take(120).collect()
}

fn parse_codex_tool_arg(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return String::new();
    };

    for key in ["file_path", "path"] {
        if let Some(raw) = value[key].as_str() {
            let short = raw.rsplit('/').next().unwrap_or(raw);
            return sanitize_tool_arg(short);
        }
    }

    for key in ["cmd", "command", "chars", "target", "session_id"] {
        if let Some(raw) = value_to_tool_arg(&value[key]) {
            return sanitize_tool_arg(&raw);
        }
    }

    if let Some(obj) = value.as_object() {
        for val in obj.values() {
            if let Some(raw) = value_to_tool_arg(val) {
                return sanitize_tool_arg(&raw);
            }
        }
    }

    String::new()
}

fn parse_codex_tool_session_id(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    let raw = &value["session_id"];
    if let Some(s) = raw.as_str() {
        return Some(s.to_string());
    }
    raw.as_u64().map(|n| n.to_string())
}

fn running_process_session_id(output: &str) -> Option<String> {
    let marker = "Process running with session ID ";
    let after = output
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(marker))?;
    let id = after.split_whitespace().next()?;
    if id.is_empty() {
        None
    } else {
        Some(
            id.trim_matches(|c: char| !c.is_ascii_alphanumeric())
                .to_string(),
        )
    }
}

fn output_reports_process_exit(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim_start().starts_with("Process exited"))
}

fn close_codex_tool_call(
    call_id: &str,
    end_ms: u64,
    tool_calls: &mut [ToolCall],
    call_indices: &HashMap<String, usize>,
    call_starts: &mut HashMap<String, u64>,
    pending_tasks: &mut Vec<(String, String)>,
) {
    if let Some(start_ms) = call_starts.remove(call_id) {
        if let Some(idx) = call_indices.get(call_id).copied() {
            if let Some(tool_call) = tool_calls.get_mut(idx) {
                tool_call.duration_ms = end_ms.saturating_sub(start_ms);
            }
        }
    }
    pending_tasks.retain(|(id, _)| id != call_id);
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
    let mut reader = BufReader::new(file);

    let mut result = CodexJSONLResult {
        session_id: String::new(),
        cwd: String::new(),
        started_at: 0,
        model: String::from("-"),
        effort: String::new(),
        version: String::new(),
        git_branch: String::new(),
        context_window: 0,
        turn_count: 0,
        current_task: String::new(),
        task_complete: false,
        model_generating: false,
        last_activity: std::time::UNIX_EPOCH,
        initial_prompt: String::new(),
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        last_context_tokens: 0,
        token_history: Vec::new(),
        rate_limit: None,
        tool_calls: Vec::new(),
        pending_since_ms: 0,
        thinking_since_ms: 0,
    };
    let mut call_indices: HashMap<String, usize> = HashMap::new();
    let mut call_starts: HashMap<String, u64> = HashMap::new();
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut write_stdin_targets: HashMap<String, String> = HashMap::new();
    let mut running_exec_by_session: HashMap<String, String> = HashMap::new();
    let mut pending_tasks: Vec<(String, String)> = Vec::new();

    // Match Claude transcript cap: a malformed/hostile line beyond this size
    // aborts the scan to prevent OOM. take(MAX+1) physically bounds the read.
    const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_line(&mut line_buf)
        {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        // Cap hit without a newline — skip this file's remainder.
        if line_buf.len() > MAX_LINE_BYTES && !line_buf.ends_with('\n') {
            break;
        }
        let line = line_buf.trim();
        if line.is_empty() {
            continue;
        }

        let val: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // partial line at EOF or malformed
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
                        result.model_generating = true;
                        result.thinking_since_ms = event_timestamp_ms(&val).unwrap_or(0);
                        if result.initial_prompt.is_empty() {
                            if let Some(msg) = payload["message"].as_str() {
                                let truncated: String = msg.chars().take(120).collect();
                                result.initial_prompt = super::redact_secrets(&truncated);
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
                            let cache = total["cached_input_tokens"]
                                .as_u64()
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
                            let cache = last["cached_input_tokens"]
                                .as_u64()
                                .or_else(|| last["cache_read_input_tokens"].as_u64())
                                .unwrap_or(0);
                            result.last_context_tokens = inp + cache;
                            if result.token_history.len() < 10_000 {
                                result.token_history.push(inp + out + cache);
                            }
                        }
                        // Context window may also appear inside info
                        if let Some(cw) = info["model_context_window"].as_u64() {
                            result.context_window = cw;
                        }
                        // Rate limits: assign to 5h/7d slots based on window_minutes.
                        // Plus plans: primary=5h(300min), secondary=7d(10080min).
                        // Free plans: primary=7d(10080min), secondary=null.
                        let rl = &payload["rate_limits"];
                        if rl.is_object() {
                            let event_secs = val["timestamp"]
                                .as_str()
                                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                                .map(|dt| dt.timestamp() as u64);
                            let mut info = RateLimitInfo {
                                source: "codex".to_string(),
                                updated_at: event_secs,
                                ..Default::default()
                            };
                            for slot in &["primary", "secondary"] {
                                let w = &rl[slot];
                                if !w.is_object() {
                                    continue;
                                }
                                let mins = w["window_minutes"].as_u64().unwrap_or(0);
                                let pct = w["used_percent"].as_f64();
                                let resets = w["resets_at"].as_u64();
                                if mins <= 300 {
                                    info.five_hour_pct = pct;
                                    info.five_hour_resets_at = resets;
                                } else {
                                    info.seven_day_pct = pct;
                                    info.seven_day_resets_at = resets;
                                }
                            }
                            result.rate_limit = Some(info);
                        }
                    }
                    Some("agent_message") => {
                        result.turn_count += 1;
                        result.model_generating = false;
                        result.thinking_since_ms = 0;
                    }
                    Some("task_complete") => {
                        result.task_complete = true;
                        result.model_generating = false;
                        result.thinking_since_ms = 0;
                    }
                    Some(event_type) if event_type.ends_with("_end") => {
                        if let Some(call_id) = payload["call_id"].as_str() {
                            let end_ms = event_timestamp_ms(&val).unwrap_or(0);
                            close_codex_tool_call(
                                call_id,
                                end_ms,
                                &mut result.tool_calls,
                                &call_indices,
                                &mut call_starts,
                                &mut pending_tasks,
                            );
                        }
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
                            .map(parse_codex_tool_arg)
                            .unwrap_or_default();

                        let task = if arg.is_empty() {
                            name.to_string()
                        } else {
                            format!("{} {}", name, arg)
                        };

                        result.model_generating = false;
                        result.thinking_since_ms = 0;

                        if let Some(call_id) = payload["call_id"].as_str() {
                            let start_ms = event_timestamp_ms(&val).unwrap_or(0);
                            call_names.insert(call_id.to_string(), name.to_string());
                            if name == "write_stdin" {
                                if let Some(session_id) = payload["arguments"]
                                    .as_str()
                                    .and_then(parse_codex_tool_session_id)
                                {
                                    write_stdin_targets.insert(call_id.to_string(), session_id);
                                }
                            }
                            call_starts.insert(call_id.to_string(), start_ms);
                            pending_tasks.retain(|(id, _)| id != call_id);
                            pending_tasks.push((call_id.to_string(), task));
                            if result.tool_calls.len() < 500 {
                                let idx = result.tool_calls.len();
                                result.tool_calls.push(ToolCall {
                                    name: name.to_string(),
                                    arg,
                                    duration_ms: 0,
                                });
                                call_indices.insert(call_id.to_string(), idx);
                            }
                        }
                    }
                } else if payload["type"].as_str() == Some("function_call_output") {
                    if let Some(call_id) = payload["call_id"].as_str() {
                        let end_ms = event_timestamp_ms(&val).unwrap_or(0);
                        let output = payload["output"].as_str().unwrap_or_default();
                        match call_names.get(call_id).map(String::as_str) {
                            Some("exec_command") => {
                                if let Some(session_id) = running_process_session_id(output) {
                                    running_exec_by_session.insert(session_id, call_id.to_string());
                                } else {
                                    close_codex_tool_call(
                                        call_id,
                                        end_ms,
                                        &mut result.tool_calls,
                                        &call_indices,
                                        &mut call_starts,
                                        &mut pending_tasks,
                                    );
                                }
                            }
                            Some("write_stdin") => {
                                close_codex_tool_call(
                                    call_id,
                                    end_ms,
                                    &mut result.tool_calls,
                                    &call_indices,
                                    &mut call_starts,
                                    &mut pending_tasks,
                                );
                                if output_reports_process_exit(output) {
                                    if let Some(exec_call_id) =
                                        write_stdin_targets.get(call_id).and_then(|session_id| {
                                            running_exec_by_session.remove(session_id)
                                        })
                                    {
                                        close_codex_tool_call(
                                            &exec_call_id,
                                            end_ms,
                                            &mut result.tool_calls,
                                            &call_indices,
                                            &mut call_starts,
                                            &mut pending_tasks,
                                        );
                                    }
                                }
                            }
                            _ => {
                                close_codex_tool_call(
                                    call_id,
                                    end_ms,
                                    &mut result.tool_calls,
                                    &call_indices,
                                    &mut call_starts,
                                    &mut pending_tasks,
                                );
                            }
                        }
                    }
                }
            }

            Some("turn_context") => {
                let payload = &val["payload"];
                if let Some(m) = payload["model"].as_str() {
                    result.model = m.to_string();
                }
                // Effort may change mid-session via /effort — always take the latest.
                if let Some(e) = payload["effort"].as_str() {
                    result.effort = e.to_string();
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

    result.current_task = pending_tasks
        .last()
        .map(|(_, task)| task.clone())
        .unwrap_or_default();
    result.pending_since_ms = call_starts.values().copied().min().unwrap_or(0);
    if !result.model_generating {
        result.thinking_since_ms = 0;
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SESSION_META: &str = r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"sess-123","cwd":"/home/user/project","cli_version":"0.1.5","timestamp":"2026-03-28T15:00:00Z","git":{"branch":"feature/x"}}}"#;

    fn write_lines(file: &mut tempfile::NamedTempFile, lines: &[&str]) {
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    #[test]
    fn test_parse_codex_session_meta() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[SESSION_META]);
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.session_id, "sess-123");
        assert_eq!(result.cwd, "/home/user/project");
        assert_eq!(result.version, "0.1.5");
        assert_eq!(result.git_branch, "feature/x");
    }

    #[test]
    fn test_parse_codex_token_count() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"output_tokens":200,"cached_input_tokens":100},"last_token_usage":{"input_tokens":50,"output_tokens":20,"cached_input_tokens":10},"model_context_window":128000}}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.total_input, 500);
        assert_eq!(result.total_output, 200);
        assert_eq!(result.total_cache_read, 100);
        assert_eq!(result.last_context_tokens, 60); // 50 + 10
        assert_eq!(result.context_window, 128000);
        assert_eq!(result.token_history.len(), 1);
        assert_eq!(result.token_history[0], 80); // 50 + 20 + 10
    }

    #[test]
    fn test_parse_codex_rate_limits() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":1},"last_token_usage":{"input_tokens":1,"output_tokens":1}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":9.0,"window_minutes":300,"resets_at":1774686045},"secondary":{"used_percent":14.0,"window_minutes":10080,"resets_at":1775186466},"plan_type":"plus"}}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        let rl = result.rate_limit.expect("rate_limit should be Some");
        assert_eq!(rl.five_hour_pct, Some(9.0));
        assert_eq!(rl.seven_day_pct, Some(14.0));
    }

    #[test]
    fn test_parse_codex_cache_read_fallback_field_name() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                // Uses cache_read_input_tokens instead of cached_input_tokens
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":30},"last_token_usage":{"input_tokens":20,"output_tokens":10,"cache_read_input_tokens":5},"model_context_window":200000}}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.total_cache_read, 30);
        assert_eq!(result.last_context_tokens, 25); // 20 + 5
    }

    #[test]
    fn test_parse_codex_skips_malformed_lines() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"NOT VALID JSON AT ALL"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"agent_message"}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        // Bad line skipped, agent_message still counted
        assert_eq!(result.turn_count, 1);
    }

    #[test]
    fn test_parse_codex_model_generating_after_user_message() {
        // Latest event is a user_message → the model has not replied yet.
        // Combined with recent rollout mtime this drives the Thinking
        // status branch in CodexCollector::collect_sessions.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"agent_message","message":"hi"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:02:00Z","payload":{"type":"user_message","message":"do a thing"}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert!(
            result.model_generating,
            "trailing user_message must mark model as generating"
        );
    }

    #[test]
    fn test_parse_codex_model_generating_cleared_by_agent_message() {
        // user_message followed by agent_message → reply landed, the
        // session is idle. Without the reset Thinking would misfire on
        // every just-finished turn while mtime is still fresh.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"user_message","message":"do a thing"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:02:00Z","payload":{"type":"agent_message","message":"done"}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert!(
            !result.model_generating,
            "agent_message must close the thinking window"
        );
    }

    #[test]
    fn test_parse_codex_turn_context_effort() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"turn_context","timestamp":"2026-03-28T15:01:00Z","payload":{"cwd":"/home/user/project","model":"gpt-5-codex","effort":"low","summary":"auto"}}"#,
                // Later turn_context overrides — /effort can change mid-session
                r#"{"type":"turn_context","timestamp":"2026-03-28T15:02:00Z","payload":{"cwd":"/home/user/project","model":"gpt-5-codex","effort":"high","summary":"auto"}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.model, "gpt-5-codex");
        assert_eq!(result.effort, "high");
    }

    #[test]
    fn test_parse_codex_missing_effort_is_empty() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                // turn_context without effort field
                r#"{"type":"turn_context","timestamp":"2026-03-28T15:01:00Z","payload":{"cwd":"/home/user/project","model":"gpt-5-codex"}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.effort, "");
    }

    #[test]
    fn test_codex_pending_function_call_marks_session_executing_and_timeline() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"user_message","message":"run tests"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"agent_message","message":"I'll run them."}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let mut process_info = HashMap::new();
        process_info.insert(
            42,
            ProcInfo {
                pid: 42,
                ppid: 1,
                rss_kb: 1024,
                cpu_pct: 0.0,
                command: "codex".to_string(),
            },
        );

        let (session, _) = collector
            .load_session_with_rate_limit(
                Some(42),
                false,
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
            )
            .unwrap();

        assert_eq!(session.status, SessionStatus::Executing);
        assert_eq!(
            session.current_tasks,
            vec!["exec_command cargo test".to_string()]
        );
        assert_eq!(session.tool_calls.len(), 1);
        assert_eq!(session.tool_calls[0].name, "exec_command");
        assert_eq!(session.tool_calls[0].arg, "cargo test");
        assert_eq!(session.tool_calls[0].duration_ms, 0);
        assert!(session.pending_since_ms > 0);
        assert_eq!(session.thinking_since_ms, 0);
    }

    #[test]
    fn test_codex_exec_command_end_closes_task_and_records_duration() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"exec_command_end","call_id":"call_1"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let mut process_info = HashMap::new();
        process_info.insert(
            42,
            ProcInfo {
                pid: 42,
                ppid: 1,
                rss_kb: 1024,
                cpu_pct: 0.0,
                command: "codex".to_string(),
            },
        );

        let (session, _) = collector
            .load_session_with_rate_limit(
                Some(42),
                false,
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
            )
            .unwrap();

        assert_eq!(session.status, SessionStatus::Waiting);
        assert_eq!(session.current_tasks, vec!["waiting for input".to_string()]);
        assert_eq!(session.tool_calls.len(), 1);
        assert_eq!(session.tool_calls[0].duration_ms, 3_000);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn test_codex_exec_command_output_closes_task_without_end_event() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"function_call_output","call_id":"call_1","output":"Chunk ID: abc\nWall time: 0.1000 seconds\nProcess exited with code 0\nOutput:\nok"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let mut process_info = HashMap::new();
        process_info.insert(
            42,
            ProcInfo {
                pid: 42,
                ppid: 1,
                rss_kb: 1024,
                cpu_pct: 0.0,
                command: "codex".to_string(),
            },
        );

        let (session, _) = collector
            .load_session_with_rate_limit(
                Some(42),
                false,
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
            )
            .unwrap();

        assert_eq!(session.status, SessionStatus::Waiting);
        assert_eq!(session.current_tasks, vec!["waiting for input".to_string()]);
        assert_eq!(session.tool_calls.len(), 1);
        assert_eq!(session.tool_calls[0].duration_ms, 3_000);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn test_codex_running_exec_closes_when_write_stdin_reports_exit() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:07Z","payload":{"type":"function_call_output","call_id":"call_1","output":"Chunk ID: abc\nWall time: 1.0000 seconds\nProcess running with session ID 12345\nOutput:\ncompiling"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:08Z","payload":{"type":"function_call","name":"write_stdin","arguments":"{\"session_id\":12345,\"chars\":\"\"}","call_id":"call_2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:12Z","payload":{"type":"function_call_output","call_id":"call_2","output":"Chunk ID: abc\nWall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\nok"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let mut process_info = HashMap::new();
        process_info.insert(
            42,
            ProcInfo {
                pid: 42,
                ppid: 1,
                rss_kb: 1024,
                cpu_pct: 0.0,
                command: "codex".to_string(),
            },
        );

        let (session, _) = collector
            .load_session_with_rate_limit(
                Some(42),
                false,
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
            )
            .unwrap();

        assert_eq!(session.status, SessionStatus::Waiting);
        assert_eq!(session.current_tasks, vec!["waiting for input".to_string()]);
        assert_eq!(session.tool_calls.len(), 2);
        assert_eq!(session.tool_calls[0].name, "exec_command");
        assert_eq!(session.tool_calls[0].duration_ms, 6_000);
        assert_eq!(session.tool_calls[1].name, "write_stdin");
        assert_eq!(session.tool_calls[1].duration_ms, 4_000);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn test_parse_codex_empty_returns_none() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(parse_codex_jsonl(file.path()).is_none());
    }
}
