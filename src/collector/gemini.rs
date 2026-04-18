use super::process::{self, ProcInfo};
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Collector for Google Gemini CLI sessions.
///
/// Discovery strategy:
/// 1. `ps` to find running gemini processes (node …/gemini.js or …/bin/gemini)
/// 2. `lsof` to determine each process's cwd (working directory)
/// 3. Map cwd → project name via `~/.gemini/projects.json`
/// 4. Find session files at `~/.gemini/tmp/{project}/chats/session-*.json`
/// 5. Parse session JSON for metadata, token usage, model info
///
/// Session file structure (JSON):
/// - `sessionId`: unique ID
/// - `startTime`: ISO 8601 timestamp
/// - `lastUpdated`: ISO 8601 timestamp
/// - `messages`: array of message objects with types: "user", "gemini", "info", "error"
/// - `kind`: session kind ("main")
///
/// Gemini message fields (type == "gemini"):
/// - `tokens`: { input, output, cached, thoughts, tool, total }
/// - `model`: model name (e.g. "gemini-3-flash-preview")
/// - `thoughts`: array of thinking steps with timestamps
pub struct GeminiCollector {
    gemini_dir: PathBuf,
    /// Project name mapping from projects.json: cwd → project name
    project_map: HashMap<String, String>,
    /// When the project map was last loaded (for periodic refresh)
    project_map_loaded: std::time::Instant,
}

impl GeminiCollector {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        let gemini_dir = home.join(".gemini");
        let project_map = load_project_map(&gemini_dir);
        Self {
            gemini_dir,
            project_map,
            project_map_loaded: std::time::Instant::now(),
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !self.gemini_dir.exists() {
            return vec![];
        }

        // Refresh project map every 30 seconds
        if self.project_map_loaded.elapsed().as_secs() > 30 {
            self.project_map = load_project_map(&self.gemini_dir);
            self.project_map_loaded = std::time::Instant::now();
        }

        // Step 1: Find running gemini processes
        let gemini_procs = find_gemini_pids(&shared.process_info);

        // Step 2: For each running process, find its cwd and resolve the project
        let mut sessions = Vec::new();
        let mut seen_sessions = std::collections::HashSet::new();

        for (pid, cwd) in &gemini_procs {
            let project_name = self.resolve_project_name(cwd);
            let tmp_dir = self.gemini_dir.join("tmp").join(&project_name);
            let chats_dir = tmp_dir.join("chats");

            if !chats_dir.exists() {
                continue;
            }

            // Find the most recently modified session file in this project
            if let Some(session_path) = find_latest_session(&chats_dir) {
                if let Some(session) = parse_gemini_session(
                    &session_path,
                    Some(*pid),
                    cwd,
                    &project_name,
                    &shared.process_info,
                    &shared.children_map,
                    &shared.ports,
                ) {
                    seen_sessions.insert(session.session_id.clone());
                    sessions.push(session);
                }
            }
        }

        // Step 3: Also scan for recently active sessions (< 5 min) without a running process
        // This handles sessions that just finished
        let tmp_dir = self.gemini_dir.join("tmp");
        if let Ok(entries) = fs::read_dir(&tmp_dir) {
            for entry in entries.flatten() {
                let chats_dir = entry.path().join("chats");
                if !chats_dir.exists() {
                    continue;
                }
                if let Ok(chat_entries) = fs::read_dir(&chats_dir) {
                    for chat_entry in chat_entries.flatten() {
                        let path = chat_entry.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("json") {
                            continue;
                        }
                        // Only recently modified
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
                        let project_name = entry.file_name().to_string_lossy().to_string();
                        // Resolve cwd from the project map (reverse lookup)
                        let cwd = self.project_map.iter()
                            .find(|(_, v)| **v == project_name)
                            .map(|(k, _)| k.clone())
                            .unwrap_or_default();

                        if let Some(session) = parse_gemini_session(
                            &path,
                            None,
                            &cwd,
                            &project_name,
                            &shared.process_info,
                            &shared.children_map,
                            &shared.ports,
                        ) {
                            if !seen_sessions.contains(&session.session_id) {
                                seen_sessions.insert(session.session_id.clone());
                                sessions.push(session);
                            }
                        }
                    }
                }
            }
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Resolve a cwd path to a project name using the project map.
    fn resolve_project_name(&self, cwd: &str) -> String {
        if let Some(name) = self.project_map.get(cwd) {
            return name.clone();
        }
        // Fallback: last path component
        cwd.rsplit('/').next().unwrap_or("?").to_string()
    }
}

impl super::AgentCollector for GeminiCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }

    // Gemini CLI does not expose rate limit info in session files (yet).
    // This can be added when/if Google surfaces quota data.
}

/// Load the project name mapping from `~/.gemini/projects.json`.
/// Format: `{ "projects": { "/abs/path": "project-name" } }`
fn load_project_map(gemini_dir: &Path) -> HashMap<String, String> {
    let path = gemini_dir.join("projects.json");
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    let val: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let mut map = HashMap::new();
    if let Some(projects) = val["projects"].as_object() {
        for (k, v) in projects {
            if let Some(name) = v.as_str() {
                map.insert(k.clone(), name.to_string());
            }
        }
    }
    map
}

/// Find running Gemini CLI processes from shared process data.
/// Returns (pid, cwd) tuples. Gemini CLI runs as a node process with
/// `gemini` in the command (either as binary name or script path).
///
/// Uses `lsof -d cwd` to determine the working directory since ProcInfo
/// from `ps` doesn't include cwd.
fn find_gemini_pids(process_info: &HashMap<u32, ProcInfo>) -> Vec<(u32, String)> {
    // Step 1: Find candidate PIDs from ps data
    let mut candidate_pids = Vec::new();
    for (pid, info) in process_info {
        let cmd = &info.command;
        let is_gemini = (cmd.contains("/bin/gemini") || cmd.contains("gemini-cli"))
            && !cmd.contains("grep")
            && !cmd.contains("abtop");
        if is_gemini {
            candidate_pids.push(*pid);
        }
    }

    if candidate_pids.is_empty() {
        return vec![];
    }

    // Step 2: Use lsof to get cwd for each candidate PID
    let mut pid_args: Vec<String> = Vec::new();
    for pid in &candidate_pids {
        pid_args.push(format!("-p{}", pid));
    }
    // -a is required on macOS to AND the -d and -p selectors
    // (without it, lsof treats them as OR and returns all processes).
    // With multiple -p flags, lsof ORs the PIDs together then ANDs with -d.
    let mut args = vec!["-a", "-F", "pn", "-d", "cwd"];
    for pa in &pid_args {
        args.push(pa);
    }

    let mut pid_to_cwd: HashMap<u32, String> = HashMap::new();
    if let Ok(output) = std::process::Command::new("lsof").args(&args).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut current_pid: Option<u32> = None;
        for line in stdout.lines() {
            if let Some(pid_str) = line.strip_prefix('p') {
                current_pid = pid_str.parse::<u32>().ok();
            } else if let Some(name) = line.strip_prefix('n') {
                if let Some(pid) = current_pid {
                    pid_to_cwd.insert(pid, name.to_string());
                }
            }
        }
    }

    // Step 3: Deduplicate by cwd (multiple node processes for same gemini session)
    // Prefer the worker process (higher PID, usually has --max-old-space-size)
    let mut cwd_to_pid: HashMap<String, u32> = HashMap::new();
    for pid in &candidate_pids {
        if let Some(cwd) = pid_to_cwd.get(pid) {
            let replace = cwd_to_pid.get(cwd)
                .map(|existing| {
                    // Prefer the process with --max-old-space-size (the worker)
                    let existing_cmd = process_info.get(existing).map(|p| &p.command);
                    let current_cmd = process_info.get(pid).map(|p| &p.command);
                    let current_is_worker = current_cmd.is_some_and(|c| c.contains("--max-old-space-size"));
                    let existing_is_worker = existing_cmd.is_some_and(|c| c.contains("--max-old-space-size"));
                    current_is_worker && !existing_is_worker
                })
                .unwrap_or(true);
            if replace {
                cwd_to_pid.insert(cwd.clone(), *pid);
            }
        }
    }

    cwd_to_pid.into_iter().map(|(cwd, pid)| (pid, cwd)).collect()
}

/// Find the most recently modified session file in a chats directory.
fn find_latest_session(chats_dir: &Path) -> Option<PathBuf> {
    let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;

    if let Ok(entries) = fs::read_dir(chats_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(meta) = fs::metadata(&path) {
                if let Ok(modified) = meta.modified() {
                    if latest.as_ref().is_none_or(|(_, t)| modified > *t) {
                        latest = Some((path, modified));
                    }
                }
            }
        }
    }
    latest.map(|(p, _)| p)
}

/// Context window sizes for known Gemini models.
fn context_window_for_model(model: &str) -> u64 {
    if model.contains("gemini-2.5-pro") || model.contains("gemini-3-pro") {
        1_048_576 // 1M
    } else if model.contains("gemini-2.5-flash") || model.contains("gemini-3-flash") {
        1_048_576 // 1M
    } else if model.contains("gemini-2.0") {
        1_048_576
    } else {
        1_048_576 // Default to 1M for unknown Gemini models
    }
}

/// Parse a Gemini CLI session JSON file into an AgentSession.
fn parse_gemini_session(
    path: &Path,
    pid: Option<u32>,
    cwd: &str,
    project_name: &str,
    process_info: &HashMap<u32, ProcInfo>,
    children_map: &HashMap<u32, Vec<u32>>,
    ports: &HashMap<u32, Vec<u16>>,
) -> Option<AgentSession> {
    let content = fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;

    let session_id = val["sessionId"].as_str()?.to_string();

    // Parse start time
    let start_time = val["startTime"].as_str().unwrap_or_default();
    let started_at = chrono::DateTime::parse_from_rfc3339(start_time)
        .map(|dt| dt.timestamp_millis() as u64)
        .unwrap_or(0);

    // Parse last updated
    let last_updated = val["lastUpdated"].as_str().unwrap_or_default();
    let last_activity = chrono::DateTime::parse_from_rfc3339(last_updated)
        .map(|dt| {
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(dt.timestamp_millis() as u64)
        })
        .unwrap_or(std::time::UNIX_EPOCH);

    let messages = val["messages"].as_array()?;

    // Aggregate token data from gemini-type messages
    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_cached: u64 = 0;
    let mut total_thoughts: u64 = 0;
    let mut turn_count: u32 = 0;
    let mut model = String::from("-");
    let mut initial_prompt = String::new();
    let mut first_assistant_text = String::new();
    let mut current_task = String::new();
    let mut token_history: Vec<u64> = Vec::new();

    for msg in messages {
        match msg["type"].as_str() {
            Some("user") => {
                if initial_prompt.is_empty() {
                    if let Some(content) = msg["content"].as_array() {
                        if let Some(first) = content.first() {
                            if let Some(text) = first["text"].as_str() {
                                initial_prompt = text.chars().take(120).collect();
                            }
                        }
                    }
                }
            }
            Some("gemini") => {
                turn_count += 1;
                if let Some(tokens) = msg.get("tokens") {
                    let inp = tokens["input"].as_u64().unwrap_or(0);
                    let out = tokens["output"].as_u64().unwrap_or(0);
                    let cached = tokens["cached"].as_u64().unwrap_or(0);
                    let thoughts = tokens["thoughts"].as_u64().unwrap_or(0);
                    total_input += inp;
                    total_output += out;
                    total_cached += cached;
                    total_thoughts += thoughts;
                    token_history.push(inp + out + cached);
                }
                if let Some(m) = msg["model"].as_str() {
                    model = m.to_string();
                }
                // First assistant text (for summary fallback)
                if first_assistant_text.is_empty() {
                    if let Some(content) = msg["content"].as_str() {
                        first_assistant_text = content.chars().take(200).collect();
                    }
                }
                // Track current task from thinking steps
                if let Some(thoughts) = msg["thoughts"].as_array() {
                    if let Some(last) = thoughts.last() {
                        if let Some(subject) = last["subject"].as_str() {
                            current_task = subject.chars().take(60).collect();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Process info
    let display_pid = pid.unwrap_or(0);
    let proc = pid.and_then(|p| process_info.get(&p));
    let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);
    let pid_alive = proc.is_some();

    // Status detection
    let status = if !pid_alive {
        SessionStatus::Done
    } else {
        let since_activity = std::time::SystemTime::now()
            .duration_since(last_activity)
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

    // Context window
    let context_window = context_window_for_model(&model);
    // Approximate context usage from cumulative input + cached tokens
    // (Gemini doesn't report per-turn context size, so use total as estimate)
    let last_turn_tokens = token_history.last().copied().unwrap_or(0);
    let context_percent = if context_window > 0 && last_turn_tokens > 0 {
        // Use the latest turn's input tokens as a proxy for context usage
        (last_turn_tokens as f64 / context_window as f64) * 100.0
    } else {
        0.0
    };

    let current_tasks = if !current_task.is_empty() {
        vec![current_task]
    } else if !pid_alive {
        vec!["finished".to_string()]
    } else if matches!(status, SessionStatus::Waiting) {
        vec!["waiting for input".to_string()]
    } else {
        vec!["thinking...".to_string()]
    };

    // Children
    let mut children = Vec::new();
    if let Some(p) = pid {
        let mut stack: Vec<u32> = children_map.get(&p).cloned().unwrap_or_default();
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

    // Gemini CLI version: read from the package if possible
    let version = read_gemini_version().unwrap_or_default();

    Some(AgentSession {
        agent_cli: "gemini",
        pid: display_pid,
        session_id,
        cwd: cwd.to_string(),
        project_name: project_name.to_string(),
        started_at,
        status,
        model,
        effort: String::new(), // Gemini CLI doesn't have an effort setting
        context_percent,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_cache_read: total_cached,
        total_cache_create: total_thoughts, // Map thought tokens to cache_create slot
        turn_count,
        current_tasks,
        mem_mb,
        version,
        git_branch: String::new(), // Populated by MultiCollector
        git_added: 0,
        git_modified: 0,
        token_history,
        subagents: vec![], // Gemini CLI doesn't have subagents
        mem_file_count: 0,
        mem_line_count: 0,
        children,
        initial_prompt,
        first_assistant_text,
    })
}

/// Try to read the Gemini CLI version from the installed package.
fn read_gemini_version() -> Option<String> {
    // Look for package.json in the gemini-cli installation
    let home = dirs::home_dir()?;
    // Common paths for npm global installs
    let candidates = [
        home.join(".nvm/versions/node").to_path_buf(),
    ];
    for base in &candidates {
        if let Ok(entries) = fs::read_dir(base) {
            for entry in entries.flatten() {
                let pkg = entry.path()
                    .join("lib/node_modules/@google/gemini-cli/package.json");
                if pkg.exists() {
                    if let Ok(content) = fs::read_to_string(&pkg) {
                        if let Ok(val) = serde_json::from_str::<Value>(&content) {
                            if let Some(ver) = val["version"].as_str() {
                                return Some(ver.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(dir: &Path, session_json: &str) -> PathBuf {
        let path = dir.join("session-test.json");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(session_json.as_bytes()).unwrap();
        path
    }

    #[test]
    fn test_parse_gemini_session_basic() {
        let dir = tempfile::tempdir().unwrap();
        let session = r#"{
            "sessionId": "test-123",
            "startTime": "2026-04-17T12:00:00.000Z",
            "lastUpdated": "2026-04-17T12:30:00.000Z",
            "messages": [
                {
                    "id": "m1",
                    "timestamp": "2026-04-17T12:00:05.000Z",
                    "type": "user",
                    "content": [{"text": "Hello world"}]
                },
                {
                    "id": "m2",
                    "timestamp": "2026-04-17T12:00:10.000Z",
                    "type": "gemini",
                    "content": "Hi there!",
                    "tokens": {"input": 100, "output": 50, "cached": 20, "thoughts": 10, "tool": 0, "total": 180},
                    "model": "gemini-3-flash-preview",
                    "thoughts": [{"subject": "Greeting", "description": "Responding to user", "timestamp": "2026-04-17T12:00:08.000Z"}]
                }
            ],
            "kind": "main"
        }"#;
        let path = write_session(dir.path(), session);
        let process_info = HashMap::new();
        let children_map = HashMap::new();
        let ports = HashMap::new();

        let result = parse_gemini_session(
            &path, None, "/home/user/project", "project",
            &process_info, &children_map, &ports,
        ).unwrap();

        assert_eq!(result.session_id, "test-123");
        assert_eq!(result.agent_cli, "gemini");
        assert_eq!(result.total_input_tokens, 100);
        assert_eq!(result.total_output_tokens, 50);
        assert_eq!(result.total_cache_read, 20);
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.model, "gemini-3-flash-preview");
        assert_eq!(result.initial_prompt, "Hello world");
        assert_eq!(result.first_assistant_text, "Hi there!");
    }

    #[test]
    fn test_parse_gemini_session_multiple_turns() {
        let dir = tempfile::tempdir().unwrap();
        let session = r#"{
            "sessionId": "test-456",
            "startTime": "2026-04-17T12:00:00.000Z",
            "lastUpdated": "2026-04-17T12:30:00.000Z",
            "messages": [
                {"id": "m1", "timestamp": "2026-04-17T12:00:05.000Z", "type": "user", "content": [{"text": "First message"}]},
                {"id": "m2", "timestamp": "2026-04-17T12:00:10.000Z", "type": "gemini", "content": "Response 1",
                 "tokens": {"input": 500, "output": 100, "cached": 0, "thoughts": 20, "tool": 0, "total": 620},
                 "model": "gemini-3-flash-preview", "thoughts": []},
                {"id": "m3", "timestamp": "2026-04-17T12:01:00.000Z", "type": "user", "content": [{"text": "Second message"}]},
                {"id": "m4", "timestamp": "2026-04-17T12:01:10.000Z", "type": "gemini", "content": "Response 2",
                 "tokens": {"input": 1000, "output": 200, "cached": 400, "thoughts": 30, "tool": 50, "total": 1680},
                 "model": "gemini-3-flash-preview", "thoughts": [{"subject": "Deep analysis", "description": "Analyzing code", "timestamp": "2026-04-17T12:01:08.000Z"}]}
            ],
            "kind": "main"
        }"#;
        let path = write_session(dir.path(), session);
        let process_info = HashMap::new();
        let children_map = HashMap::new();
        let ports = HashMap::new();

        let result = parse_gemini_session(
            &path, None, "/home/user/project", "project",
            &process_info, &children_map, &ports,
        ).unwrap();

        assert_eq!(result.turn_count, 2);
        assert_eq!(result.total_input_tokens, 1500); // 500 + 1000
        assert_eq!(result.total_output_tokens, 300); // 100 + 200
        assert_eq!(result.total_cache_read, 400); // 0 + 400
        assert_eq!(result.token_history.len(), 2);
        assert_eq!(result.initial_prompt, "First message");
    }

    #[test]
    fn test_parse_gemini_session_empty_messages() {
        let dir = tempfile::tempdir().unwrap();
        let session = r#"{
            "sessionId": "test-empty",
            "startTime": "2026-04-17T12:00:00.000Z",
            "lastUpdated": "2026-04-17T12:00:00.000Z",
            "messages": [],
            "kind": "main"
        }"#;
        let path = write_session(dir.path(), session);
        let process_info = HashMap::new();
        let children_map = HashMap::new();
        let ports = HashMap::new();

        let result = parse_gemini_session(
            &path, None, "/home/user/project", "project",
            &process_info, &children_map, &ports,
        );

        // Should still return a session (just with zero tokens)
        assert!(result.is_some());
        let s = result.unwrap();
        assert_eq!(s.total_input_tokens, 0);
        assert_eq!(s.turn_count, 0);
    }

    #[test]
    fn test_load_project_map() {
        let dir = tempfile::tempdir().unwrap();
        let projects = r#"{"projects": {"/home/user/project": "my-project", "/home/user/other": "other-proj"}}"#;
        fs::write(dir.path().join("projects.json"), projects).unwrap();

        let map = load_project_map(dir.path());
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("/home/user/project").unwrap(), "my-project");
    }

    #[test]
    fn test_context_window_for_model() {
        assert_eq!(context_window_for_model("gemini-3-flash-preview"), 1_048_576);
        assert_eq!(context_window_for_model("gemini-2.5-pro"), 1_048_576);
        assert_eq!(context_window_for_model("unknown-model"), 1_048_576);
    }
}
