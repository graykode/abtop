use super::process::{self, ProcInfo};
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Collector for `pi-coding-agent` sessions (badlogic/pi-mono).
///
/// Discovery strategy (no PID session file):
/// 1. `ps` → find running `pi` processes (binary sets `process.title = "pi"`,
///    installs as `node .../pi-coding-agent/dist/cli.js`)
/// 2. `lsof` → map PID → open `~/.pi/agent/sessions/--<cwd-encoded>--/<ts>_<uuid>.jsonl`
/// 3. Parse JSONL: SessionHeader + tree-structured entries
///
/// JSONL schema (pi session v3, docs/session.md):
/// - Line 1: `{"type":"session","version":3,"id":"uuid","timestamp":"...","cwd":"..."}`
/// - Subsequent: `{"type":"message","id":"...","parentId":"...","timestamp":"...","message":{...}}`
///   where message.role ∈ {user, assistant, toolResult, bashExecution, custom, branchSummary, compactionSummary}
/// - Also: `model_change`, `thinking_level_change`, `compaction`, `branch_summary`, `label`, `session_info`
///
/// Assistant messages carry `usage.{input,output,cacheRead,cacheWrite,totalTokens}` per-turn.
/// We accumulate across assistant messages for lifetime totals.
///
/// Pi has no rate-limit telemetry (unlike Claude/Codex) because it's provider-agnostic —
/// users bring their own Anthropic/OpenAI/Gemini keys. `live_rate_limit()` returns None.
pub struct PiCollector {
    sessions_root: PathBuf,
}

impl PiCollector {
    pub fn new() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            sessions_root: home.join(".pi").join("agent").join("sessions"),
        }
    }

    fn collect_sessions(&self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !self.sessions_root.exists() {
            return vec![];
        }

        // Step 1: find running pi processes from shared ps data
        let pi_pids = Self::find_pi_pids_from_shared(&shared.process_info);
        if pi_pids.is_empty() {
            return vec![];
        }

        // Step 2: map PID → open session JSONL file via lsof
        let pid_to_jsonl = Self::map_pid_to_jsonl(&pi_pids);

        let mut sessions = Vec::new();
        for (pid, jsonl_path) in &pid_to_jsonl {
            if let Some(session) = self.load_session(
                *pid,
                jsonl_path,
                &shared.process_info,
                &shared.children_map,
                &shared.ports,
            ) {
                sessions.push(session);
            }
        }

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    /// Find PIDs of running pi processes.
    ///
    /// Pi binary invocations look like one of:
    ///   - `pi [args]`                                  (process.title set, Linux comm)
    ///   - `node /usr/lib/node_modules/@mariozechner/pi-coding-agent/dist/cli.js [args]`
    ///   - `/usr/bin/pi [args]`                         (native wrapper)
    fn find_pi_pids_from_shared(process_info: &HashMap<u32, ProcInfo>) -> Vec<u32> {
        let mut pids = Vec::new();
        for (pid, info) in process_info {
            let cmd = &info.command;
            // Path-based match is more precise than bare "pi" (which would collide
            // with e.g. `pip`, `pipewire`, `pinentry`). `pi-coding-agent` in the argv
            // path is unambiguous. Fall back to binary name match for the native path.
            let is_pi = cmd.contains("pi-coding-agent") || process::cmd_has_binary(cmd, "pi");
            if is_pi && !cmd.contains("grep") && !cmd.contains("pip ") {
                pids.push(*pid);
            }
        }
        pids
    }

    /// Map pi PIDs to their open session JSONL files via lsof.
    ///
    /// Pi writes to `~/.pi/agent/sessions/--<cwd>--/<ts>_<uuid>.jsonl`.
    /// Match on `.pi/agent/sessions/` as the disambiguator.
    fn map_pid_to_jsonl(pids: &[u32]) -> HashMap<u32, PathBuf> {
        let mut map = HashMap::new();
        if pids.is_empty() {
            return map;
        }

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
                        // Match pi session files specifically — avoid catching
                        // unrelated .jsonl files the process might have open.
                        if name.contains("/.pi/agent/sessions/") && name.ends_with(".jsonl") {
                            map.insert(pid, PathBuf::from(name));
                        }
                    }
                }
            }
        }
        map
    }

    fn load_session(
        &self,
        pid: u32,
        jsonl_path: &Path,
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
    ) -> Option<AgentSession> {
        // Skip symlinks (fail-closed) — matches claude.rs / opencode.rs hardening.
        if is_symlink(jsonl_path) {
            return None;
        }

        let result = parse_pi_jsonl(jsonl_path)?;

        let proc = process_info.get(&pid);
        let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

        let project_name = result
            .cwd
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("?")
            .to_string();

        // Status: pid is alive by construction (we found it via ps). Distinguish
        // Working (recent activity OR active descendant) vs Waiting.
        let since_activity = std::time::SystemTime::now()
            .duration_since(result.last_activity)
            .unwrap_or_default();
        let status = if since_activity.as_secs() < 30 {
            SessionStatus::Working
        } else {
            let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);
            let has_active_child =
                process::has_active_descendant(pid, children_map, process_info, 5.0);
            if cpu_active || has_active_child {
                SessionStatus::Working
            } else {
                SessionStatus::Waiting
            }
        };

        let current_tasks = if !result.current_task.is_empty() {
            vec![result.current_task]
        } else if matches!(status, SessionStatus::Waiting) {
            vec!["waiting for input".to_string()]
        } else {
            vec!["thinking...".to_string()]
        };

        // Pi does not emit context-window size in session data. We infer it from
        // the model name using the same hardcoded table abtop already uses for
        // Claude (see CLAUDE.md §"Context Window Calculation"). Unknown models
        // get 200k as a safe default.
        let context_window = context_window_for_model(&result.model);
        let context_percent = if context_window > 0 && result.last_context_tokens > 0 {
            (result.last_context_tokens as f64 / context_window as f64) * 100.0
        } else {
            0.0
        };

        // Collect descendant children (same pattern as Codex).
        let mut children = Vec::new();
        let mut stack: Vec<u32> = children_map.get(&pid).cloned().unwrap_or_default();
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

        // Redact + truncate the initial prompt before it reaches the TUI.
        let initial_prompt = super::redact_secrets(&truncate_field(&result.initial_prompt, 1024));

        Some(AgentSession {
            agent_cli: "pi",
            pid,
            session_id: truncate_field(&result.session_id, 256),
            cwd: truncate_field(&result.cwd, 4096),
            project_name: truncate_field(&project_name, 256),
            started_at: result.started_at,
            status,
            model: truncate_field(&result.model, 128),
            effort: truncate_field(&result.thinking_level, 16),
            context_percent,
            total_input_tokens: result.total_input,
            total_output_tokens: result.total_output,
            total_cache_read: result.total_cache_read,
            total_cache_create: result.total_cache_write,
            turn_count: result.turn_count,
            current_tasks,
            mem_mb,
            version: String::new(), // pi session header has no version field
            git_branch: String::new(), // pi doesn't record git branch in session
            git_added: 0,
            git_modified: 0,
            token_history: result.token_history,
            subagents: vec![], // pi has no sub-agent concept
            mem_file_count: 0, // pi has no MEMORY.md equivalent
            mem_line_count: 0,
            children,
            initial_prompt,
            first_assistant_text: String::new(),
        })
    }
}

impl super::AgentCollector for PiCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
    // live_rate_limit() defaults to None — pi is provider-agnostic.
}

/// Parsed result from a pi session JSONL file.
struct PiJSONLResult {
    session_id: String,
    cwd: String,
    started_at: u64,
    model: String,
    thinking_level: String,
    turn_count: u32,
    current_task: String,
    last_activity: std::time::SystemTime,
    initial_prompt: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_write: u64,
    /// Last observed total-context-tokens value. Pi's `usage.totalTokens` per
    /// assistant turn is cumulative-like for context-window accounting.
    last_context_tokens: u64,
    /// Per-turn token delta history for the sparkline.
    token_history: Vec<u64>,
}

#[derive(Deserialize)]
struct SessionHeader {
    #[serde(default)]
    id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    timestamp: String,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default, rename = "cacheRead")]
    cache_read: u64,
    #[serde(default, rename = "cacheWrite")]
    cache_write: u64,
    #[serde(default, rename = "totalTokens")]
    total_tokens: u64,
}

/// Parse a pi session JSONL file.
///
/// Defensive parsing: `serde(default)` everywhere, unknown fields ignored.
/// The session format is not a stable API (undocumented internals), so any
/// parse failure on a single line is recoverable — we skip it and continue.
fn parse_pi_jsonl(path: &Path) -> Option<PiJSONLResult> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut result = PiJSONLResult {
        session_id: String::new(),
        cwd: String::new(),
        started_at: 0,
        model: String::from("-"),
        thinking_level: String::new(),
        turn_count: 0,
        current_task: String::new(),
        last_activity: std::time::UNIX_EPOCH,
        initial_prompt: String::new(),
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        total_cache_write: 0,
        last_context_tokens: 0,
        token_history: Vec::new(),
    };

    let mut header_seen = false;

    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // skip malformed lines
        };

        let entry_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match entry_type {
            "session" => {
                if let Ok(hdr) = serde_json::from_value::<SessionHeader>(v.clone()) {
                    result.session_id = hdr.id;
                    result.cwd = hdr.cwd;
                    result.started_at = parse_iso_timestamp_ms(&hdr.timestamp);
                    header_seen = true;
                }
            }
            "message" => {
                let Some(msg) = v.get("message") else {
                    continue;
                };
                let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

                // Timestamp on the entry wrapper is ISO, on the inner message is ms epoch.
                let entry_ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
                let ts_ms = parse_iso_timestamp_ms(entry_ts);
                if ts_ms > 0 {
                    if let Some(t) =
                        std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ts_ms))
                    {
                        result.last_activity = t;
                    }
                }

                match role {
                    "user" => {
                        result.turn_count = result.turn_count.saturating_add(1);
                        if result.initial_prompt.is_empty() {
                            result.initial_prompt = extract_user_text(msg);
                        }
                    }
                    "assistant" => {
                        // Model: latest wins (user can /model mid-session).
                        if let Some(m) = msg.get("model").and_then(|m| m.as_str()) {
                            if !m.is_empty() {
                                result.model = m.to_string();
                            }
                        }
                        // Token accounting.
                        if let Some(u) = msg.get("usage") {
                            if let Ok(u) = serde_json::from_value::<Usage>(u.clone()) {
                                let prev_total = result.total_input
                                    + result.total_output
                                    + result.total_cache_write;
                                result.total_input = result.total_input.saturating_add(u.input);
                                result.total_output = result.total_output.saturating_add(u.output);
                                result.total_cache_read =
                                    result.total_cache_read.saturating_add(u.cache_read);
                                result.total_cache_write =
                                    result.total_cache_write.saturating_add(u.cache_write);
                                if u.total_tokens > 0 {
                                    result.last_context_tokens = u.total_tokens;
                                }
                                let new_total = result.total_input
                                    + result.total_output
                                    + result.total_cache_write;
                                let delta = new_total.saturating_sub(prev_total);
                                if delta > 0 {
                                    result.token_history.push(delta);
                                    if result.token_history.len() > 200 {
                                        result.token_history.remove(0);
                                    }
                                }
                            }
                        }
                        // Current task: latest toolCall in content array, if any.
                        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                            for block in content {
                                if block.get("type").and_then(|t| t.as_str()) == Some("toolCall") {
                                    let name =
                                        block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                    if !name.is_empty() {
                                        result.current_task =
                                            format_tool_summary(name, block.get("arguments"));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "model_change" => {
                if let Some(m) = v.get("modelId").and_then(|m| m.as_str()) {
                    if !m.is_empty() {
                        result.model = m.to_string();
                    }
                }
            }
            "thinking_level_change" => {
                if let Some(t) = v.get("thinkingLevel").and_then(|t| t.as_str()) {
                    result.thinking_level = t.to_string();
                }
            }
            _ => {}
        }
    }

    if !header_seen {
        return None;
    }
    Some(result)
}

/// Extract a displayable user-prompt string from a pi user message's `content` field.
/// `content` may be either a string or an array of TextContent/ImageContent blocks.
fn extract_user_text(msg: &Value) -> String {
    let Some(content) = msg.get("content") else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    return t.to_string();
                }
            }
        }
    }
    String::new()
}

/// Summarise a toolCall block for the "current task" line: `{name} {first-path-arg}`.
fn format_tool_summary(name: &str, args: Option<&Value>) -> String {
    let arg_str = args
        .and_then(|a| a.as_object())
        .and_then(|obj| {
            // Prefer path-like args, else fall back to first scalar.
            for key in ["file_path", "path", "command", "cmd"] {
                if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
                    return Some(v.to_string());
                }
            }
            obj.values().find_map(|v| v.as_str().map(str::to_string))
        })
        .unwrap_or_default();
    if arg_str.is_empty() {
        name.to_string()
    } else {
        let short: String = arg_str.chars().take(60).collect();
        format!("{} {}", name, short)
    }
}

/// Parse an ISO-8601 timestamp to milliseconds since epoch.
/// Pi uses `"2024-12-03T14:00:00.000Z"` format.
fn parse_iso_timestamp_ms(s: &str) -> u64 {
    if s.is_empty() {
        return 0;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

/// Context-window size (tokens) for a given model name. Mirrors the logic in
/// claude.rs — when Anthropic/OpenAI/Gemini add new models, this table falls
/// back to 200k rather than reporting 0% / inflated percentages.
fn context_window_for_model(model: &str) -> u64 {
    let m = model.to_lowercase();
    if m.contains("[1m]") || m.contains("1m") && m.contains("claude") {
        return 1_000_000;
    }
    if m.contains("gemini") && (m.contains("1.5") || m.contains("2.0") || m.contains("2.5")) {
        return 1_000_000;
    }
    if m.contains("gpt-4")
        || m.contains("gpt-5")
        || m.contains("o1")
        || m.contains("o3")
        || m.contains("o4")
    {
        return 128_000;
    }
    200_000 // safe default
}

/// Truncate a string at a UTF-8 char boundary to `max_bytes` bytes.
/// Matches the hardening pattern in `model/session.rs::truncate_string` and
/// `opencode.rs`.
fn truncate_field(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Check if a path is a symlink without following it.
/// Defaults to `true` on error (fail-closed).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_pi_pids_from_shared() {
        let mut procs = HashMap::new();
        procs.insert(
            1001,
            ProcInfo {
                pid: 1001,
                ppid: 1,
                rss_kb: 50_000,
                cpu_pct: 0.5,
                command: "node /usr/lib/node_modules/@mariozechner/pi-coding-agent/dist/cli.js"
                    .to_string(),
            },
        );
        procs.insert(
            1002,
            ProcInfo {
                pid: 1002,
                ppid: 1,
                rss_kb: 10_000,
                cpu_pct: 0.0,
                command: "pip install requests".to_string(), // must NOT match
            },
        );
        procs.insert(
            1003,
            ProcInfo {
                pid: 1003,
                ppid: 1,
                rss_kb: 5_000,
                cpu_pct: 0.0,
                command: "grep pi-coding-agent".to_string(), // must NOT match
            },
        );
        procs.insert(
            1004,
            ProcInfo {
                pid: 1004,
                ppid: 1,
                rss_kb: 20_000,
                cpu_pct: 0.0,
                command: "pi --resume".to_string(), // binary-name match
            },
        );

        let mut found = PiCollector::find_pi_pids_from_shared(&procs);
        found.sort();
        assert_eq!(found, vec![1001, 1004]);
    }

    #[test]
    fn test_sessions_root_default() {
        let c = PiCollector::new();
        let p = c.sessions_root.to_string_lossy().to_string();
        assert!(
            p.ends_with(".pi/agent/sessions") || p.ends_with(".pi\\agent\\sessions"),
            "unexpected sessions_root: {}",
            p
        );
    }

    #[test]
    fn test_truncate_field_char_boundary() {
        // 4-byte emoji — truncating at byte 3 must back up to 0.
        let s = "😀abc";
        assert_eq!(truncate_field(s, 3), "");
        assert_eq!(truncate_field(s, 4), "😀");
        assert_eq!(truncate_field(s, 7), "😀abc");
    }

    #[test]
    fn test_context_window_for_model() {
        assert_eq!(context_window_for_model("claude-sonnet-4-5"), 200_000);
        assert_eq!(context_window_for_model("claude-opus-4-6[1m]"), 1_000_000);
        assert_eq!(context_window_for_model("gemini-2.5-pro"), 1_000_000);
        assert_eq!(context_window_for_model("gpt-4o"), 128_000);
        assert_eq!(context_window_for_model("o3-mini"), 128_000);
        assert_eq!(context_window_for_model("unknown-model-v99"), 200_000);
    }

    #[test]
    fn test_parse_iso_timestamp_ms() {
        assert_eq!(
            parse_iso_timestamp_ms("2024-12-03T14:00:00.000Z"),
            1_733_234_400_000
        );
        assert_eq!(parse_iso_timestamp_ms(""), 0);
        assert_eq!(parse_iso_timestamp_ms("not-a-date"), 0);
    }

    #[test]
    fn test_format_tool_summary() {
        let args: Value =
            serde_json::from_str(r#"{"file_path":"src/main.rs","old":"x","new":"y"}"#).unwrap();
        assert_eq!(format_tool_summary("edit", Some(&args)), "edit src/main.rs");

        let bash_args: Value = serde_json::from_str(r#"{"command":"cargo test"}"#).unwrap();
        assert_eq!(
            format_tool_summary("bash", Some(&bash_args)),
            "bash cargo test"
        );

        assert_eq!(format_tool_summary("read", None), "read");
    }

    #[test]
    fn test_parse_pi_jsonl_minimal() {
        // Build a minimal valid session file in a tempdir.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let content = r#"{"type":"session","version":3,"id":"abc-123","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/home/user/proj"}
{"type":"message","id":"a1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"Fix the bug in src/main.rs"}}
{"type":"message","id":"a2","parentId":"a1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Looking at it now"},{"type":"toolCall","id":"t1","name":"read","arguments":{"file_path":"src/main.rs"}}],"model":"claude-sonnet-4-5","provider":"anthropic","usage":{"input":100,"output":50,"cacheRead":200,"cacheWrite":10,"totalTokens":360},"stopReason":"toolUse"}}
{"type":"thinking_level_change","id":"a3","parentId":"a2","timestamp":"2024-12-03T14:00:03.000Z","thinkingLevel":"high"}
"#;
        fs::write(&path, content).unwrap();

        let r = parse_pi_jsonl(&path).expect("parse ok");
        assert_eq!(r.session_id, "abc-123");
        assert_eq!(r.cwd, "/home/user/proj");
        assert_eq!(r.model, "claude-sonnet-4-5");
        assert_eq!(r.thinking_level, "high");
        assert_eq!(r.turn_count, 1);
        assert_eq!(r.total_input, 100);
        assert_eq!(r.total_output, 50);
        assert_eq!(r.total_cache_read, 200);
        assert_eq!(r.total_cache_write, 10);
        assert_eq!(r.last_context_tokens, 360);
        assert_eq!(r.initial_prompt, "Fix the bug in src/main.rs");
        assert_eq!(r.current_task, "read src/main.rs");
        assert_eq!(r.token_history, vec![160]); // input + output + cacheWrite
    }

    #[test]
    fn test_parse_pi_jsonl_malformed_line_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let content = r#"{"type":"session","version":3,"id":"s","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/x"}
this is not json
{"type":"message","id":"m1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"hi"}}
"#;
        fs::write(&path, content).unwrap();

        let r = parse_pi_jsonl(&path).expect("parse recovers");
        assert_eq!(r.session_id, "s");
        assert_eq!(r.turn_count, 1);
    }

    #[test]
    fn test_parse_pi_jsonl_rejects_file_without_header() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        fs::write(
            &path,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"hi"}}
"#,
        )
        .unwrap();
        assert!(parse_pi_jsonl(&path).is_none());
    }
}
