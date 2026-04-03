use super::process::{self, ProcInfo};
use crate::model::{AgentSession, ChildProcess, SessionFile, SessionStatus, SubAgent};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub struct ClaudeCollector {
    sessions_dir: PathBuf,
    projects_dir: PathBuf,
    /// Cached transcript parse results keyed by session_id.
    /// On each tick, only new bytes since `new_offset` are parsed.
    transcript_cache: HashMap<String, TranscriptResult>,
}

impl ClaudeCollector {
    pub fn new() -> Self {
        let base = std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"));
        Self {
            sessions_dir: base.join("sessions"),
            projects_dir: base.join("projects"),
            transcript_cache: HashMap::new(),
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let session_files = match fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(_) => return vec![],
        };

        let mut sessions = Vec::new();
        for entry in session_files.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            if let Some(session) = self.load_session(&path, &shared.process_info, &shared.children_map, &shared.ports) {
                sessions.push(session);
            }
        }

        // Evict transcript cache for sessions that no longer exist
        let active_ids: std::collections::HashSet<&str> =
            sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.transcript_cache.retain(|sid, _| active_ids.contains(sid.as_str()));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn load_session(
        &mut self,
        path: &Path,
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
    ) -> Option<AgentSession> {
        let content = fs::read_to_string(path).ok()?;
        let sf: SessionFile = serde_json::from_str(&content).ok()?;

        let proc_cmd = process_info.get(&sf.pid).map(|p| p.command.as_str());
        let pid_alive = proc_cmd
            .map(|c| {
                process::cmd_has_binary(c, "claude")
            })
            .unwrap_or(false);

        // Skip --print sessions (e.g. abtop's own summary generation).
        // Only filter while process is alive (command visible); dead sessions
        // are cleaned up when the session file disappears.
        if proc_cmd.map(|c| c.contains("--print")).unwrap_or(false) {
            return None;
        }

        let project_name = sf
            .cwd
            .rsplit('/')
            .next()
            .unwrap_or("?")
            .to_string();

        let proc = process_info.get(&sf.pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        let transcript_path = self.find_transcript(&sf.cwd, &sf.session_id);

        if let Some(ref tp) = transcript_path {
            let cached = self.transcript_cache.remove(&sf.session_id);
            // Detect file replacement: if inode or mtime changed, reparse from scratch
            let identity_changed = cached.as_ref()
                .map(|c| c.file_identity != file_identity(tp))
                .unwrap_or(false);
            let from_offset = if identity_changed {
                0
            } else {
                cached.as_ref().map(|c| c.new_offset).unwrap_or(0)
            };

            let delta = parse_transcript(tp, from_offset);

            if let Some(mut prev) = cached {
                // File replaced, shrank, or first parse — replace entirely
                if identity_changed || from_offset == 0 || delta.new_offset < from_offset {
                    self.transcript_cache.insert(sf.session_id.clone(), delta);
                } else {
                    // Merge delta into cached result
                    if delta.model != "-" {
                        prev.model = delta.model;
                    }
                    prev.total_input += delta.total_input;
                    prev.total_output += delta.total_output;
                    prev.total_cache_read += delta.total_cache_read;
                    prev.total_cache_create += delta.total_cache_create;
                    if delta.last_context_tokens > 0 {
                        prev.last_context_tokens = delta.last_context_tokens;
                    }
                    if delta.max_context_tokens > prev.max_context_tokens {
                        prev.max_context_tokens = delta.max_context_tokens;
                    }
                    prev.turn_count += delta.turn_count;
                    // Always update current_task from delta — empty means
                    // latest assistant turn had no tool_use (task cleared)
                    if delta.turn_count > 0 {
                        prev.current_task = delta.current_task;
                    }
                    if !delta.version.is_empty() {
                        prev.version = delta.version;
                    }
                    if !delta.git_branch.is_empty() {
                        prev.git_branch = delta.git_branch;
                    }
                    if delta.last_activity > prev.last_activity {
                        prev.last_activity = delta.last_activity;
                    }
                    prev.token_history.extend(delta.token_history);
                    if prev.initial_prompt.is_empty() && !delta.initial_prompt.is_empty() {
                        prev.initial_prompt = delta.initial_prompt;
                    }
                    prev.new_offset = delta.new_offset;
                    self.transcript_cache.insert(sf.session_id.clone(), prev);
                }
            } else {
                // First parse — store full result
                self.transcript_cache.insert(sf.session_id.clone(), delta);
            }
        }

        let empty_result = TranscriptResult {
            model: "-".to_string(),
            total_input: 0, total_output: 0, total_cache_read: 0, total_cache_create: 0,
            last_context_tokens: 0, max_context_tokens: 0, turn_count: 0, current_task: String::new(),
            version: String::new(), git_branch: String::new(),
            last_activity: std::time::UNIX_EPOCH, new_offset: 0,
            file_identity: (0, 0),
            token_history: Vec::new(), initial_prompt: String::new(),
            first_assistant_text: String::new(),
        };
        let cached = self.transcript_cache.get(&sf.session_id).unwrap_or(&empty_result);

        let model = cached.model.clone();
        let total_input = cached.total_input;
        let total_output = cached.total_output;
        let total_cache_read = cached.total_cache_read;
        let total_cache_create = cached.total_cache_create;
        let last_context_tokens = cached.last_context_tokens;
        let max_context_tokens = cached.max_context_tokens;
        let turn_count = cached.turn_count;
        let current_task = cached.current_task.clone();
        let version = cached.version.clone();
        let git_branch = cached.git_branch.clone();
        let last_activity = cached.last_activity;
        let token_history = cached.token_history.clone();
        let initial_prompt = cached.initial_prompt.clone();
        let first_assistant_text = cached.first_assistant_text.clone();

        if !pid_alive {
            return None;
        }

        let status = {
            let since_activity = std::time::SystemTime::now()
                .duration_since(last_activity)
                .unwrap_or_default();
            if since_activity.as_secs() < 30 {
                SessionStatus::Working
            } else {
                // Transcript is stale (>30s). Check CPU-based signals:
                // 1. Claude process using CPU > 1% → likely thinking/streaming
                let claude_cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
                // 2. Any descendant using significant CPU (>5%) → likely running a tool
                //    (higher threshold avoids false positives from idle watchers/servers)
                let has_active_descendant = process::has_active_descendant(
                    sf.pid, children_map, process_info, 5.0,
                );
                if claude_cpu_active || has_active_descendant {
                    SessionStatus::Working
                } else {
                    SessionStatus::Waiting
                }
            }
        };

        let context_window = context_window_for_model(&model, max_context_tokens);
        let context_percent = if context_window > 0 {
            (last_context_tokens as f64 / context_window as f64) * 100.0
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

        let mut children = Vec::new();
        // Collect all descendants (not just direct children) so we catch
        // grandchild processes that listen on ports (e.g. Claude → shell → node).
        let mut stack: Vec<u32> = children_map
            .get(&sf.pid)
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
            // Add grandchildren to the stack
            if let Some(grandchildren) = children_map.get(&cpid) {
                stack.extend(grandchildren);
            }
        }

        // Git stats: populated by MultiCollector on slow ticks
        let (git_added, git_modified) = (0, 0);

        // Derive the project directory from the transcript path (handles worktree sessions),
        // falling back to the encoded cwd.
        let project_dir = transcript_path
            .as_ref()
            .and_then(|tp| tp.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| self.projects_dir.join(encode_cwd_path(&sf.cwd)));

        // Subagent discovery
        let subagents_dir = project_dir.join(&sf.session_id).join("subagents");
        let subagents = Self::collect_subagents(&subagents_dir);

        // Memory status
        let memory_dir = project_dir.join("memory");
        let (mem_file_count, mem_line_count) = Self::collect_memory_status(&memory_dir);

        Some(AgentSession {
            agent_cli: "claude",
            pid: sf.pid,
            session_id: sf.session_id,
            cwd: sf.cwd,
            project_name,
            started_at: sf.started_at,
            status,
            model,
            context_percent,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cache_read,
            total_cache_create,
            turn_count,
            current_tasks,
            mem_mb,
            version,
            git_branch,
            git_added,
            git_modified,
            token_history,
            subagents,
            mem_file_count,
            mem_line_count,
            children,
            initial_prompt,
            first_assistant_text,
        })
    }

    fn find_transcript(&self, cwd: &str, session_id: &str) -> Option<PathBuf> {
        let jsonl_name = format!("{}.jsonl", session_id);

        // Primary: look up by encoded cwd
        let encoded = encode_cwd_path(cwd);
        let path = self.projects_dir.join(&encoded).join(&jsonl_name);
        if path.exists() {
            return Some(path);
        }

        // Fallback: scan all project directories for the session file.
        // Handles worktree (-w) sessions where the transcript directory
        // may not match the encoded cwd from the session file.
        if let Ok(entries) = fs::read_dir(&self.projects_dir) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let candidate = entry.path().join(&jsonl_name);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }

        None
    }


    fn collect_subagents(subagents_dir: &Path) -> Vec<SubAgent> {
        let mut subagents = Vec::new();

        let entries = match fs::read_dir(subagents_dir) {
            Ok(e) => e,
            Err(_) => return subagents,
        };

        // Collect meta files and their corresponding jsonl files
        let mut meta_files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".meta.json") {
                    meta_files.push(path);
                }
            }
        }

        for meta_path in meta_files {
            let meta_name = match meta_path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            // Parse meta JSON
            let meta_content = match fs::read_to_string(&meta_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let meta_val: Value = match serde_json::from_str(&meta_content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let description = meta_val.get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("agent")
                .to_string();

            // Derive jsonl path: agent-{hash}.meta.json -> agent-{hash}.jsonl
            let jsonl_name = meta_name.replace(".meta.json", ".jsonl");
            let jsonl_path = meta_path.with_file_name(&jsonl_name);

            let mut tokens = 0u64;
            let mut last_activity = std::time::UNIX_EPOCH;

            if jsonl_path.exists() {
                // Get file mtime for status
                if let Ok(metadata) = fs::metadata(&jsonl_path) {
                    if let Ok(mtime) = metadata.modified() {
                        last_activity = mtime;
                    }
                }

                // Parse jsonl for token totals
                let transcript = parse_transcript(&jsonl_path, 0);
                tokens = transcript.total_input + transcript.total_output
                    + transcript.total_cache_read + transcript.total_cache_create;
            }

            let status = {
                let since = std::time::SystemTime::now()
                    .duration_since(last_activity)
                    .unwrap_or_default();
                if since.as_secs() < 30 {
                    "working".to_string()
                } else {
                    "done".to_string()
                }
            };

            // Use description as name, shorten if needed
            let name = truncate(&description, 30);

            subagents.push(SubAgent {
                name,
                status,
                tokens,
            });
        }

        subagents
    }

    fn collect_memory_status(memory_dir: &Path) -> (u32, u32) {
        let mut file_count = 0u32;
        let mut line_count = 0u32;

        if let Ok(entries) = fs::read_dir(memory_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    file_count += 1;
                }
            }
        }

        let memory_md = memory_dir.join("MEMORY.md");
        if let Ok(content) = fs::read_to_string(&memory_md) {
            line_count = content.lines().count() as u32;
        }

        (file_count, line_count)
    }
}

impl super::AgentCollector for ClaudeCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

struct TranscriptResult {
    model: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    /// Last assistant turn's input context size (for context % calculation)
    last_context_tokens: u64,
    /// High-water mark: largest context seen in any turn (for 1M detection)
    max_context_tokens: u64,
    turn_count: u32,
    current_task: String,
    version: String,
    git_branch: String,
    last_activity: std::time::SystemTime,
    new_offset: u64,
    /// File identity: (inode, mtime_ns). Used to detect file replacement
    /// even when the new file is the same size or larger.
    file_identity: (u64, u64),
    token_history: Vec<u64>,
    initial_prompt: String,
    /// First assistant response text (text blocks only, no tool_use)
    first_assistant_text: String,
}

/// Get file identity as (inode, mtime_nanos) for detecting file replacement.
fn file_identity(path: &Path) -> (u64, u64) {
    fs::metadata(path)
        .ok()
        .map(|m| {
            let ino = m.ino();
            let mtime_ns = m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (ino, mtime_ns)
        })
        .unwrap_or((0, 0))
}

fn parse_transcript(path: &Path, from_offset: u64) -> TranscriptResult {
    let identity = file_identity(path);
    let mut result = TranscriptResult {
        model: "-".to_string(),
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        last_context_tokens: 0,
        max_context_tokens: 0,
        turn_count: 0,
        current_task: String::new(),
        version: String::new(),
        git_branch: String::new(),
        last_activity: std::time::UNIX_EPOCH,
        new_offset: from_offset,
        file_identity: identity,
        token_history: Vec::new(),
        initial_prompt: String::new(),
        first_assistant_text: String::new(),
    };

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return result,
    };

    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file_len == from_offset {
        // No new data
        result.new_offset = file_len;
        return result;
    }
    // File shrank (truncation/rotation) — reset and reparse from start
    let effective_offset = if file_len < from_offset { 0 } else { from_offset };
    let from_offset = effective_offset;

    let mut reader = BufReader::new(file);
    if from_offset > 0 {
        let _ = reader.seek(SeekFrom::Start(from_offset));
    }

    let mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .unwrap_or(std::time::UNIX_EPOCH);
    result.last_activity = mtime;

    let mut bytes_read = from_offset;
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf) {
            Ok(0) => break,
            Ok(n) => {
                let has_newline = line_buf.ends_with('\n');
                let line = line_buf.trim();
                if line.is_empty() {
                    if has_newline {
                        bytes_read += n as u64;
                    }
                    continue;
                }
                // Try to parse as JSON. If incomplete (no newline) and
                // parse fails, defer to next poll. If parse succeeds,
                // accept the record even without trailing newline.
                let val = match serde_json::from_str::<Value>(line) {
                    Ok(v) => v,
                    Err(_) => {
                        if has_newline {
                            // Complete line but invalid JSON — skip it
                            bytes_read += n as u64;
                        }
                        // Incomplete line with parse error — defer
                        if !has_newline { break; }
                        continue;
                    }
                };
                bytes_read += n as u64;
                {
                    match val.get("type").and_then(|t| t.as_str()) {
                        Some("assistant") => {
                            result.turn_count += 1;
                            // Clear previous task on each new turn so stale tasks
                            // don't persist when latest turn has no tool_use
                            result.current_task = String::new();
                            if let Some(msg) = val.get("message") {
                                if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                                    result.model = m.to_string();
                                }
                                if let Some(usage) = msg.get("usage") {
                                    let inp = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let out = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let cr = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                    let cc = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                    result.total_input += inp;
                                    result.total_output += out;
                                    result.total_cache_read += cr;
                                    result.total_cache_create += cc;
                                    // Context = last turn's total input (this is what the model "sees")
                                    result.last_context_tokens = inp + cr + cc;
                                    if result.last_context_tokens > result.max_context_tokens {
                                        result.max_context_tokens = result.last_context_tokens;
                                    }
                                    // Track per-turn total tokens for sparkline
                                    result.token_history.push(inp + out + cr + cc);
                                }
                                // Extract first assistant text (text blocks only) for summary fallback
                                if result.first_assistant_text.is_empty() {
                                    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                                        let texts: Vec<&str> = content.iter()
                                            .filter_map(|block| {
                                                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                                    block.get("text").and_then(|t| t.as_str())
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect();
                                        if !texts.is_empty() {
                                            let joined = texts.join(" ");
                                            let normalized: String = joined
                                                .lines()
                                                .map(|l| l.trim())
                                                .filter(|l| !l.is_empty())
                                                .collect::<Vec<_>>()
                                                .join(" ");
                                            result.first_assistant_text = truncate(&normalized, 200);
                                        }
                                    }
                                }
                                // Extract last tool_use from latest turn (= most recently running)
                                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                                    for item in content.iter().rev() {
                                        if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                            let tool = item.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                            let arg = extract_tool_arg(item);
                                            result.current_task = format!("{} {}", tool, arg);
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        Some("user") => {
                            if let Some(v) = val.get("version").and_then(|v| v.as_str()) {
                                result.version = v.to_string();
                            }
                            if let Some(b) = val.get("gitBranch").and_then(|b| b.as_str()) {
                                result.git_branch = b.to_string();
                            }
                            // Extract first user prompt as session title
                            if result.initial_prompt.is_empty() {
                                if let Some(msg) = val.get("message") {
                                    result.initial_prompt = extract_prompt_text(msg);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(_) => break,
        }
    }

    result.new_offset = bytes_read;
    result
}

/// Extract a short summary from the first user message content.
/// Handles both string content and array-of-blocks content.
/// Encode a cwd path to match Claude Code's project directory naming.
/// Claude Code replaces '/', '_', and '.' with '-'.
fn encode_cwd_path(cwd: &str) -> String {
    cwd.chars()
        .map(|c| match c {
            '/' | '_' | '.' => '-',
            _ => c,
        })
        .collect()
}

fn extract_prompt_text(message: &Value) -> String {
    let raw = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            // Find first text block
            arr.iter()
                .filter_map(|block| {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        block.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .next()
                .unwrap_or_default()
        }
        _ => return String::new(),
    };

    // Clean up: remove image markers, code blocks, markdown headers
    let cleaned: String = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");

    // Remove [Image #N] markers
    let mut result = cleaned;
    while let Some(start) = result.find("[Image") {
        if let Some(end) = result[start..].find(']') {
            result = format!("{}{}", &result[..start], result[start + end + 1..].trim_start());
        } else {
            break;
        }
    }

    let clean = result.trim().to_string();
    if clean.is_empty() {
        return String::new();
    }
    // Skip prompts generated by abtop's own summary generation (claude --print)
    if clean.contains("You are a conversation title generator") {
        return String::new();
    }
    truncate(&clean, 50)
}

fn extract_tool_arg(tool_use: &Value) -> String {
    if let Some(input) = tool_use.get("input") {
        // Edit/Read: file_path
        if let Some(fp) = input.get("file_path").and_then(|f| f.as_str()) {
            return shorten_path(fp);
        }
        // Bash: command (first 40 chars)
        if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
            let short = cmd.lines().next().unwrap_or(cmd);
            return truncate(short, 40);
        }
        // Grep/Glob: pattern
        if let Some(pat) = input.get("pattern").and_then(|p| p.as_str()) {
            return truncate(pat, 40);
        }
    }
    String::new()
}

fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').collect();
    if parts.len() <= 2 {
        path.to_string()
    } else {
        format!("{}/{}", parts[1], parts[0])
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{}…", truncated)
    }
}

fn context_window_for_model(model: &str, last_context_tokens: u64) -> u64 {
    if model.contains("[1m]") || last_context_tokens > 200_000 {
        1_000_000
    } else {
        200_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_lines(file: &mut tempfile::NamedTempFile, lines: &[&str]) {
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    #[test]
    fn test_parse_transcript_basic_tokens() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.86","gitBranch":"main","message":{"role":"user","content":"fix the bug"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":200,"cache_creation_input_tokens":30},"content":[{"type":"text","text":"I found the issue."}]}}"#,
        ]);
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.total_input, 100);
        assert_eq!(result.total_output, 50);
        assert_eq!(result.total_cache_read, 200);
        assert_eq!(result.total_cache_create, 30);
        assert_eq!(result.model, "claude-sonnet-4-6");
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.last_context_tokens, 330); // 100 + 200 + 30
    }

    #[test]
    fn test_parse_transcript_multiple_turns() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first prompt"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"First response."}]}}"#,
            r#"{"type":"user","timestamp":"2026-03-28T15:01:00Z","message":{"role":"user","content":"second prompt"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":200,"output_tokens":80,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Second response."}]}}"#,
        ]);
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.turn_count, 2);
        assert_eq!(result.total_input, 300); // 100 + 200
        assert_eq!(result.total_output, 130); // 50 + 80
        assert_eq!(result.token_history.len(), 2);
    }

    #[test]
    fn test_parse_transcript_tool_use_current_task() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"fix the bug"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs"}}]}}"#,
        ]);
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.current_task, "Edit src/main.rs");
    }

    #[test]
    fn test_parse_transcript_initial_prompt() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"refactor the auth module"}}"#,
        ]);
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.initial_prompt, "refactor the auth module");
    }

    #[test]
    fn test_parse_transcript_incremental_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first prompt"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"First response."}]}}"#,
        ]);
        let first = parse_transcript(file.path(), 0);
        let offset = first.new_offset;
        assert!(offset > 0);

        // Append a third line (new assistant turn)
        write_lines(&mut file, &[
            r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":40,"output_tokens":20,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Third."}]}}"#,
        ]);
        let delta = parse_transcript(file.path(), offset);
        assert_eq!(delta.turn_count, 1);
        assert_eq!(delta.total_input, 40);
        assert_eq!(delta.total_output, 20);
    }

    #[test]
    fn test_parse_transcript_empty_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.model, "-");
        assert_eq!(result.total_input, 0);
        assert_eq!(result.turn_count, 0);
    }

    #[test]
    fn test_encode_cwd_path() {
        assert_eq!(encode_cwd_path("/Users/foo/bar"), "-Users-foo-bar");
        assert_eq!(encode_cwd_path("/home/user/my_project.v2"), "-home-user-my-project-v2");
    }

    #[test]
    fn test_context_window_for_model() {
        // Base model with low token usage → 200K
        assert_eq!(context_window_for_model("claude-opus-4-6", 50_000), 200_000);
        // Explicit [1m] suffix → 1M regardless of token count
        assert_eq!(context_window_for_model("claude-opus-4-6[1m]", 0), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-4-6", 100_000), 200_000);
        assert_eq!(context_window_for_model("unknown-model", 0), 200_000);
        // Token usage exceeds 200K → must be 1M window
        assert_eq!(context_window_for_model("claude-opus-4-6", 250_000), 1_000_000);
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn test_shorten_path() {
        assert_eq!(shorten_path("src/collector/claude.rs"), "collector/claude.rs");
        assert_eq!(shorten_path("main.rs"), "main.rs");
    }

    #[test]
    fn test_parse_transcript_skips_malformed_json() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"hi"}}"#,
            r#"THIS IS NOT VALID JSON"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"response"}]}}"#,
        ]);
        let result = parse_transcript(file.path(), 0);
        // Bad line should be skipped, assistant line still parsed
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.total_input, 100);
        assert_eq!(result.initial_prompt, "hi");
    }

    #[test]
    fn test_parse_transcript_file_shrunk_resets() {
        use std::io::Seek;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","message":{"role":"user","content":"first"}}"#,
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"resp"}]}}"#,
        ]);
        let first = parse_transcript(file.path(), 0);
        let old_offset = first.new_offset;
        assert!(old_offset > 0);

        // Simulate file rotation: truncate and write shorter content
        file.as_file().set_len(0).unwrap();
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        write_lines(&mut file, &[
            r#"{"type":"assistant","timestamp":"2026-03-28T16:00:00Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"new session"}]}}"#,
        ]);
        // Pass old offset that is now beyond file length
        let result = parse_transcript(file.path(), old_offset);
        // Should reset to 0 and parse the new content
        assert_eq!(result.turn_count, 1);
        assert_eq!(result.total_input, 10);
    }

    #[test]
    fn test_parse_transcript_current_task_cleared_between_turns() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            // Turn 1: has tool_use
            r#"{"type":"assistant","timestamp":"2026-03-28T15:00:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs"}}]}}"#,
            // Turn 2: text only, no tool_use
            r#"{"type":"assistant","timestamp":"2026-03-28T15:01:05Z","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"Done, all changes applied."}]}}"#,
        ]);
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.turn_count, 2);
        // current_task should be empty because last turn had no tool_use
        assert_eq!(result.current_task, "");
    }

    #[test]
    fn test_parse_transcript_version_and_git_branch() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[
            r#"{"type":"user","timestamp":"2026-03-28T15:00:00Z","version":"2.1.90","gitBranch":"feat/payments","message":{"role":"user","content":"add stripe"}}"#,
        ]);
        let result = parse_transcript(file.path(), 0);
        assert_eq!(result.version, "2.1.90");
        assert_eq!(result.git_branch, "feat/payments");
    }
}
