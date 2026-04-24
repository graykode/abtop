//! Collector for kiro-cli sessions.
//!
//! Discovery: scan `~/.kiro/sessions/cli/*.lock` (override via `KIRO_TEST_SESSIONS_DIR`).
//! Liveness: lock PID must be alive and owned by a `kiro-cli` binary.
//! Tokens: summed from `session_state.conversation_metadata.user_turn_metadatas[]`
//! in the metadata JSON — they are NOT present in the JSONL log.
//! Current task / initial prompt / first assistant text / turn count / activity
//! come from incremental JSONL parsing of `{session_id}.jsonl`.
//!
//! JSONL envelope: `{version:"v1", kind:"Prompt"|"AssistantMessage"|..., data:{...}}`.
//! Content blocks: `{kind:"text"|"toolUse"|"toolResult", data:...}`.

use super::process;
use crate::model::{AgentSession, ChildProcess, SessionStatus};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

// -------- DTOs (permissive: unknown fields ignored, missing optionals default) --------

#[derive(Debug, Deserialize)]
struct KiroLock {
    pub pid: u32,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct KiroMetadata {
    pub session_id: String,
    pub cwd: String,
    pub created_at: Option<String>,
    pub title: Option<String>,
    pub session_state: KiroSessionState,
}

/// Session state — we only pull the fields we render. Missing/unknown → defaults.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct KiroSessionState {
    pub conversation_metadata: KiroConvMeta,
    pub rts_model_state: KiroRtsState,
    pub agent_name: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct KiroConvMeta {
    pub user_turn_metadatas: Vec<KiroTurnMeta>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct KiroTurnMeta {
    pub input_token_count: u64,
    pub output_token_count: u64,
    pub metering_usage: Vec<KiroMetering>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct KiroMetering {
    pub value: f64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct KiroRtsState {
    pub model_info: Option<KiroModelInfo>,
    pub context_usage_percentage: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct KiroModelInfo {
    pub model_name: String,
}

impl KiroMetadata {
    /// Sanitize untrusted strings to bounded lengths (match claude.rs defensive posture).
    fn sanitize(&mut self) {
        truncate_string(&mut self.session_id, 256);
        truncate_string(&mut self.cwd, 4096);
        if let Some(t) = self.title.as_mut() {
            truncate_string(t, 500);
        }
    }

    fn turns(&self) -> &[KiroTurnMeta] {
        &self.session_state.conversation_metadata.user_turn_metadatas
    }

    fn model_name(&self) -> &str {
        self.session_state
            .rts_model_state
            .model_info
            .as_ref()
            .map(|m| m.model_name.as_str())
            .unwrap_or("-")
    }

    fn total_input_tokens(&self) -> u64 {
        self.turns().iter().map(|t| t.input_token_count).sum()
    }

    fn total_output_tokens(&self) -> u64 {
        self.turns().iter().map(|t| t.output_token_count).sum()
    }

    /// Per-turn credits scaled ×100 (two-decimal precision through u64).
    /// Each value is rounded before summing to avoid f64 accumulation error.
    fn credits_per_turn_scaled(&self) -> Vec<u64> {
        self.turns()
            .iter()
            .map(|t| {
                t.metering_usage
                    .iter()
                    .map(|m| (m.value * 100.0).round().max(0.0) as u64)
                    .sum()
            })
            .collect()
    }

    /// Returns 0..100. Handles the 0..1 fallback case defensively.
    fn context_percent(&self) -> f64 {
        let raw = self
            .session_state
            .rts_model_state
            .context_usage_percentage
            .unwrap_or(0.0);
        let pct = if raw > 0.0 && raw <= 1.0 { raw * 100.0 } else { raw };
        pct.clamp(0.0, 100.0)
    }
}

fn truncate_string(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

// -------- JSONL tail parser --------

#[derive(Debug, Default, Clone)]
struct KiroLogResult {
    pub turn_count: u32,
    pub current_task: String,
    pub initial_prompt: String,
    pub first_assistant_text: String,
    pub last_activity: Option<std::time::SystemTime>,
    pub new_offset: u64,
    /// (inode, mtime_ns, len) — detect file replacement/truncation. `len` acts as
    /// a tiebreaker on platforms where `inode` is unavailable (non-unix).
    pub file_identity: (u64, u64, u64),
}

/// Merge a delta parse result into an existing cached result.
fn merge_log_result(prev: &mut KiroLogResult, delta: KiroLogResult) {
    prev.turn_count += delta.turn_count;
    // Current task: empty delta means the most recent turn had no tool_use → clear.
    if delta.turn_count > 0 {
        prev.current_task = delta.current_task;
    }
    if prev.initial_prompt.is_empty() && !delta.initial_prompt.is_empty() {
        prev.initial_prompt = delta.initial_prompt;
    }
    if prev.first_assistant_text.is_empty() && !delta.first_assistant_text.is_empty() {
        prev.first_assistant_text = delta.first_assistant_text;
    }
    if let Some(ts) = delta.last_activity {
        if prev.last_activity.is_none_or(|old| ts > old) {
            prev.last_activity = Some(ts);
        }
    }
    prev.new_offset = delta.new_offset;
    prev.file_identity = delta.file_identity;
}

fn file_identity(path: &Path) -> (u64, u64, u64) {
    fs::metadata(path)
        .ok()
        .map(|m| {
            #[cfg(unix)]
            let ino = m.ino();
            #[cfg(not(unix))]
            let ino = 0u64;
            let mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            (ino, mtime_ns, m.len())
        })
        .unwrap_or((0, 0, 0))
}

/// Parse new bytes of a kiro JSONL log starting at `from_offset`.
/// Detects file replacement/truncation and resets when needed.
fn parse_kiro_log(path: &Path, from_offset: u64) -> KiroLogResult {
    let identity = file_identity(path);
    let mut result = KiroLogResult {
        new_offset: from_offset,
        file_identity: identity,
        ..Default::default()
    };

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return result,
    };
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file_len == from_offset {
        result.new_offset = file_len;
        return result;
    }
    // File shrank → reparse from start
    let from_offset = if file_len < from_offset { 0 } else { from_offset };
    result.last_activity = fs::metadata(path).ok().and_then(|m| m.modified().ok());

    let mut reader = BufReader::new(file);
    if from_offset > 0 {
        let _ = reader.seek(SeekFrom::Start(from_offset));
    }

    const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;
    let mut bytes_read = from_offset;
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        match reader
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_line(&mut line_buf)
        {
            Ok(0) => break,
            Ok(n) => {
                if line_buf.len() > MAX_LINE_BYTES && !line_buf.ends_with('\n') {
                    bytes_read = file_len;
                    break;
                }
                let has_newline = line_buf.ends_with('\n');
                let line = line_buf.trim();
                if line.is_empty() {
                    if has_newline {
                        bytes_read += n as u64;
                    }
                    continue;
                }
                let val = match serde_json::from_str::<Value>(line) {
                    Ok(v) => v,
                    Err(_) => {
                        if has_newline {
                            bytes_read += n as u64;
                        } else {
                            break; // partial write, defer
                        }
                        continue;
                    }
                };
                bytes_read += n as u64;
                apply_log_entry(&val, &mut result);
            }
            Err(_) => break,
        }
    }
    result.new_offset = bytes_read;
    result
}

fn apply_log_entry(val: &Value, result: &mut KiroLogResult) {
    let Some(kind) = val.get("kind").and_then(|k| k.as_str()) else { return };
    let data = val.get("data");
    match kind {
        "Prompt" => {
            // First prompt → initial_prompt (for fallback when metadata.title absent).
            if result.initial_prompt.is_empty() {
                if let Some(text) = first_text_block(data) {
                    result.initial_prompt = clean_prompt_text(&text);
                }
            }
        }
        "AssistantMessage" => {
            result.turn_count += 1;
            // Clear previous task each turn — if this turn has no toolUse, current_task stays empty.
            result.current_task = String::new();
            if let Some(content) = data.and_then(|d| d.get("content")).and_then(|c| c.as_array()) {
                // Latest toolUse wins (scan in reverse).
                for block in content.iter().rev() {
                    if block.get("kind").and_then(|k| k.as_str()) == Some("toolUse") {
                        let bd = block.get("data");
                        let name = bd
                            .and_then(|d| d.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        let arg = bd.and_then(|d| d.get("input")).map(extract_tool_arg).unwrap_or_default();
                        result.current_task = if arg.is_empty() {
                            name.to_string()
                        } else {
                            format!("{} {}", name, arg)
                        };
                        break;
                    }
                }
                // First assistant text (text blocks only) for summary fallback.
                if result.first_assistant_text.is_empty() {
                    let texts: Vec<&str> = content
                        .iter()
                        .filter_map(|b| {
                            if b.get("kind").and_then(|k| k.as_str()) == Some("text") {
                                b.get("data").and_then(|d| d.as_str())
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
        }
        "Clear" | "ResetTo" => {
            // Conversation reset: clear per-turn state so stale tasks don't linger.
            result.current_task = String::new();
        }
        "CancelledPrompt" => {
            // Last user prompt cancelled — keep state; counters stay accurate.
        }
        _ => {} // Compaction, ToolResults, etc. — ignore for our purposes.
    }
}

fn first_text_block(data: Option<&Value>) -> Option<String> {
    let content = data?.get("content")?.as_array()?;
    for block in content {
        if block.get("kind").and_then(|k| k.as_str()) == Some("text") {
            if let Some(s) = block.get("data").and_then(|d| d.as_str()) {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn clean_prompt_text(raw: &str) -> String {
    let cleaned: String = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Skip kiro-cli's own title-naming agent prompts so they don't leak into the UI.
    if trimmed.contains("You are a session naming agent") {
        return String::new();
    }
    truncate(trimmed, 100)
}

fn extract_tool_arg(input: &Value) -> String {
    if let Some(fp) = input.get("file_path").and_then(|f| f.as_str()) {
        return super::redact_secrets(&shorten_path(fp));
    }
    if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
        let short = cmd.lines().next().unwrap_or(cmd);
        return super::redact_secrets(&truncate(short, 40));
    }
    if let Some(pat) = input.get("pattern").and_then(|p| p.as_str()) {
        return super::redact_secrets(&truncate(pat, 40));
    }
    // ReadInternalWebsites / MCP tools: inputs[0]
    if let Some(first) = input
        .get("inputs")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    {
        return super::redact_secrets(&truncate(first, 40));
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

// -------- KiroCollector --------

pub struct KiroCollector {
    sessions_dir: PathBuf,
    /// Cached JSONL parse result keyed by session_id. Only accumulates for sessions
    /// observed alive by *this abtop run* — bounded by live-session churn, not by
    /// kiro-cli's full on-disk history.
    transcript_cache: HashMap<String, KiroLogResult>,
}

impl KiroCollector {
    pub fn new() -> Self {
        let dir = std::env::var("KIRO_TEST_SESSIONS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".kiro/sessions/cli")
            });
        Self {
            sessions_dir: dir,
            transcript_cache: HashMap::new(),
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let entries = match fs::read_dir(&self.sessions_dir) {
            Ok(e) => e,
            Err(_) => return vec![],
        };

        let mut sessions = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        for entry in entries.flatten() {
            let path = entry.path();
            // Skip symlinks (and unknowable file types) to avoid following into
            // arbitrary filesystem locations.
            if entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true) {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()) else {
                continue;
            };

            if let Some(session) = self.load_session(&stem, &path, shared) {
                seen_ids.insert(stem);
                sessions.push(session);
            }
        }

        // Evict cache entries for sessions no longer live. A transient read failure
        // above can drop a live session from `seen_ids` for one tick, causing a
        // full re-parse next tick — acceptable since kiro JSONL logs are small.
        self.transcript_cache.retain(|sid, _| seen_ids.contains(sid));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn load_session(
        &mut self,
        session_id: &str,
        lock_path: &Path,
        shared: &super::SharedProcessData,
    ) -> Option<AgentSession> {
        // 1. Parse lock → PID.
        let lock_content = fs::read_to_string(lock_path).ok()?;
        let lock: KiroLock = serde_json::from_str(&lock_content).ok()?;
        let pid = lock.pid;

        // 2. Liveness: PID alive + binary is kiro-cli or its ACP backend kiro-cli-chat.
        //    kiro-cli spawns kiro-cli-chat as a child (via bun tui.js → acp subprocess);
        //    the session lock stores the BACKEND's PID, not the frontend's.
        let proc = shared.process_info.get(&pid)?;
        if !process::cmd_has_binary(&proc.command, "kiro-cli")
            && !process::cmd_has_binary(&proc.command, "kiro-cli-chat")
        {
            return None;
        }

        // 3. Load metadata.
        let meta_path = lock_path.with_extension("json");
        let meta_content = fs::read_to_string(&meta_path).ok()?;
        let mut meta: KiroMetadata = serde_json::from_str(&meta_content).ok()?;
        meta.sanitize();

        // 4. Incremental JSONL parse.
        let jsonl_path = lock_path.with_extension("jsonl");
        let cached = self.transcript_cache.remove(session_id);
        let identity_changed = cached
            .as_ref()
            .map(|c| c.file_identity != file_identity(&jsonl_path))
            .unwrap_or(false);
        let from_offset = if identity_changed {
            0
        } else {
            cached.as_ref().map(|c| c.new_offset).unwrap_or(0)
        };
        let delta = parse_kiro_log(&jsonl_path, from_offset);
        let parse_result = match cached {
            Some(mut prev) if !identity_changed && from_offset > 0 && delta.new_offset >= from_offset => {
                merge_log_result(&mut prev, delta);
                prev
            }
            _ => delta,
        };
        self.transcript_cache
            .insert(session_id.to_string(), parse_result.clone());

        // 5. Derive fields.
        let started_at = parse_rfc3339_to_ms(meta.created_at.as_deref()).unwrap_or(0);

        let project_name = meta.cwd.rsplit('/').next().unwrap_or("?").to_string();

        // Model display: append agent_name when present.
        let model = {
            let base = meta.model_name();
            match meta.session_state.agent_name.as_deref() {
                Some(name) if !name.is_empty() => format!("{} · {}", base, name),
                _ => base.to_string(),
            }
        };

        // Title precedence: metadata.title → parsed first prompt.
        let initial_prompt = super::redact_secrets(
            &meta
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| parse_result.initial_prompt.clone()),
        );

        // Status: last_activity within 30s → Working; else check CPU/descendants.
        // CPU threshold 5.0 matches `has_active_descendant`'s threshold for consistency.
        let status = {
            let since = parse_result
                .last_activity
                .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
                .unwrap_or_else(|| std::time::Duration::from_secs(u64::MAX));
            if since.as_secs() < 30
                || proc.cpu_pct > 5.0
                || process::has_active_descendant(pid, &shared.children_map, &shared.process_info, 5.0)
            {
                SessionStatus::Executing
            } else {
                SessionStatus::Waiting
            }
        };

        let current_tasks = vec![if !parse_result.current_task.is_empty() {
            parse_result.current_task.clone()
        } else if matches!(status, SessionStatus::Waiting) {
            "waiting for input".to_string()
        } else {
            "thinking...".to_string()
        }];

        // Children (descendant walk, matching claude.rs).
        let mut children = Vec::new();
        let mut stack: Vec<u32> = shared.children_map.get(&pid).cloned().unwrap_or_default();
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
            if let Some(gc) = shared.children_map.get(&cpid) {
                stack.extend(gc);
            }
        }

        let turn_count = meta.turns().len() as u32;

        let context_percent = meta.context_percent();
        // kiro-cli persists input/output token counts as 0 in metadata, but the
        // `metering_usage[].value` credit field is authoritative and grows with real
        // activity. Route credits (x100 for 2-decimal precision) into total_input_tokens
        // so the existing UI — sessions table, token rate sparkline, active_tokens() delta —
        // all light up without any per-agent branching. If kiro ever starts emitting real
        // tokens, we preserve them preferentially and fall back to credits.
        let real_in = meta.total_input_tokens();
        let real_out = meta.total_output_tokens();
        let credits_per_turn = meta.credits_per_turn_scaled();
        let (total_input_tokens, total_output_tokens, token_history) = if real_in > 0 || real_out > 0 {
            let hist = meta
                .turns()
                .iter()
                .map(|t| t.input_token_count + t.output_token_count)
                .collect();
            (real_in, real_out, hist)
        } else {
            (credits_per_turn.iter().sum(), 0, credits_per_turn)
        };
        let cwd = meta.cwd;

        Some(AgentSession {
            agent_cli: "kiro",
            pid,
            session_id: session_id.to_string(),
            cwd,
            project_name,
            started_at,
            status,
            model,
            effort: String::new(),
            context_percent,
            total_input_tokens,
            total_output_tokens,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count,
            current_tasks,
            mem_mb: proc.rss_kb / 1024,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history,
            context_history: vec![],
            compaction_count: 0,
            context_window: 0,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children,
            initial_prompt,
            first_assistant_text: super::redact_secrets(&parse_result.first_assistant_text),
            tool_calls: vec![],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![],
        })
    }
}

impl super::AgentCollector for KiroCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

/// Parse RFC3339 timestamp to epoch milliseconds. Returns None on parse failure.
fn parse_rfc3339_to_ms(s: Option<&str>) -> Option<u64> {
    let s = s?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::process::ProcInfo;
    use crate::collector::{AgentCollector, SharedProcessData};
    use std::io::Write;

    // ---- DTO / parser tests ----

    #[test]
    fn metadata_parses_real_shape() {
        // Mirrors the real on-disk shape captured during Task 1.5.
        let json = r#"{
            "session_id": "abc-123",
            "cwd": "/tmp/proj",
            "created_at": "2026-04-16T05:30:52.620649Z",
            "updated_at": "2026-04-16T05:31:10.100588Z",
            "title": "Build a thing",
            "session_state": {
                "version": "v1",
                "conversation_metadata": {
                    "user_turn_metadatas": [
                        {"input_token_count": 100, "output_token_count": 50, "context_usage_percentage": 1.2},
                        {"input_token_count": 200, "output_token_count": 80, "context_usage_percentage": 3.4}
                    ]
                },
                "rts_model_state": {
                    "conversation_id": "abc-123",
                    "model_info": {"model_name": "claude-opus-4.6-1m"},
                    "context_usage_percentage": 4.8497
                },
                "agent_name": "kiro_default",
                "permissions": {}
            }
        }"#;
        let meta: KiroMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.session_id, "abc-123");
        assert_eq!(meta.title.as_deref(), Some("Build a thing"));
        assert_eq!(meta.model_name(), "claude-opus-4.6-1m");
        assert_eq!(meta.total_input_tokens(), 300);
        assert_eq!(meta.total_output_tokens(), 130);
        assert!((meta.context_percent() - 4.8497).abs() < 0.001);
        assert_eq!(
            meta.session_state.agent_name.as_deref(),
            Some("kiro_default")
        );
    }

    #[test]
    fn metadata_tolerates_unknown_fields_and_missing_optionals() {
        // Forward compat: unknown version tag + missing optional fields → defaults, no panic.
        let json = r#"{
            "session_id": "x",
            "cwd": "/",
            "future_field": {"anything": 1},
            "session_state": {"version": "v99", "extra": "data"}
        }"#;
        let meta: KiroMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.model_name(), "-");
        assert_eq!(meta.total_input_tokens(), 0);
        assert_eq!(meta.context_percent(), 0.0);
        assert!(meta.title.is_none());
    }

    #[test]
    fn context_percent_normalizes_fraction_scale() {
        let mut meta = KiroMetadata::default();
        meta.session_state.rts_model_state.context_usage_percentage = Some(0.42);
        assert!((meta.context_percent() - 42.0).abs() < 0.001);
        meta.session_state.rts_model_state.context_usage_percentage = Some(42.0);
        assert!((meta.context_percent() - 42.0).abs() < 0.001);
        meta.session_state.rts_model_state.context_usage_percentage = Some(150.0);
        assert_eq!(meta.context_percent(), 100.0); // clamped
    }

    #[test]
    fn lock_parses() {
        let json = r#"{"pid":12345,"started_at":"2026-04-16T05:30:52.619954Z"}"#;
        let lock: KiroLock = serde_json::from_str(json).unwrap();
        assert_eq!(lock.pid, 12345);
    }

    fn write_lines(file: &mut tempfile::NamedTempFile, lines: &[&str]) {
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    #[test]
    fn parser_extracts_first_prompt_and_tool_use() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"fix the bug"}]}}"#,
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"toolUse","data":{"toolUseId":"t1","name":"Edit","input":{"file_path":"src/main.rs"}}}]}}"#,
            ],
        );
        let r = parse_kiro_log(file.path(), 0);
        assert_eq!(r.initial_prompt, "fix the bug");
        assert_eq!(r.current_task, "Edit src/main.rs");
        assert_eq!(r.turn_count, 1);
        assert!(r.new_offset > 0);
    }

    #[test]
    fn parser_current_task_clears_on_turn_without_tool_use() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m1","content":[{"kind":"toolUse","data":{"toolUseId":"t1","name":"Edit","input":{"file_path":"a.rs"}}}]}}"#,
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"text","data":"Done."}]}}"#,
            ],
        );
        let r = parse_kiro_log(file.path(), 0);
        assert_eq!(r.turn_count, 2);
        assert_eq!(r.current_task, "");
    }

    #[test]
    fn parser_incremental_offset_accumulates() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"hello"}]}}"#,
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"text","data":"hi"}]}}"#,
            ],
        );
        let first = parse_kiro_log(file.path(), 0);
        let offset = first.new_offset;
        assert!(offset > 0);
        assert_eq!(first.turn_count, 1);

        write_lines(
            &mut file,
            &[
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m3","content":[{"kind":"toolUse","data":{"toolUseId":"t1","name":"Bash","input":{"command":"ls"}}}]}}"#,
            ],
        );
        let delta = parse_kiro_log(file.path(), offset);
        assert_eq!(delta.turn_count, 1);
        assert_eq!(delta.current_task, "Bash ls");
    }

    #[test]
    fn parser_skips_malformed_lines() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"hi"}]}}"#,
                r#"THIS IS NOT JSON"#,
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"text","data":"ok"}]}}"#,
            ],
        );
        let r = parse_kiro_log(file.path(), 0);
        assert_eq!(r.turn_count, 1);
        assert_eq!(r.initial_prompt, "hi");
    }

    #[test]
    fn parser_ignores_naming_agent_prompts() {
        // The "You are a session naming agent" prompt is kiro-cli's internal title-gen;
        // we must not surface it as a session initial_prompt.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"You are a session naming agent. Pick a title."}]}}"#,
            ],
        );
        let r = parse_kiro_log(file.path(), 0);
        assert_eq!(r.initial_prompt, "");
    }

    #[test]
    fn parser_extracts_tool_arg_variants() {
        // Verifies file_path, command, pattern, and inputs[0] all map through extract_tool_arg.
        let cases = [
            (
                r#"{"file_path":"/a/b/c/long.rs"}"#,
                "c/long.rs", // shorten_path keeps last 2 segments
            ),
            (r#"{"command":"git status"}"#, "git status"),
            (r#"{"pattern":"*.rs"}"#, "*.rs"),
            (r#"{"inputs":["https://example.com"]}"#, "https://example.com"),
        ];
        for (input, expected) in cases {
            let v: Value = serde_json::from_str(input).unwrap();
            assert_eq!(extract_tool_arg(&v), expected, "input: {}", input);
        }
    }

    // ---- Collector integration tests ----

    fn mk_proc(pid: u32, cmd: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: 1,
            rss_kb: 0,
            cpu_pct: 0.0,
            command: cmd.to_string(),
        }
    }

    fn stage_session(dir: &Path, session_id: &str, pid: u32, title: Option<&str>, agent_name: Option<&str>) {
        let lock = format!(r#"{{"pid":{},"started_at":"2026-04-16T05:30:52Z"}}"#, pid);
        fs::write(dir.join(format!("{}.lock", session_id)), lock).unwrap();

        let title_field = title.map(|t| format!(r#""title":"{}","#, t)).unwrap_or_default();
        let agent_field = agent_name
            .map(|a| format!(r#""agent_name":"{}","#, a))
            .unwrap_or_default();
        let meta = format!(
            r#"{{
                "session_id":"{sid}",
                "cwd":"/tmp/proj",
                "created_at":"2026-04-16T05:30:52Z",
                "updated_at":"2026-04-16T05:31:00Z",
                {title_field}
                "session_state":{{
                    "version":"v1",
                    "conversation_metadata":{{"user_turn_metadatas":[
                        {{"input_token_count":100,"output_token_count":50}}
                    ]}},
                    "rts_model_state":{{
                        "conversation_id":"{sid}",
                        "model_info":{{"model_name":"claude-opus-4.6"}},
                        "context_usage_percentage":42.5
                    }},
                    {agent_field}
                    "permissions":{{}}
                }}
            }}"#,
            sid = session_id,
            title_field = title_field,
            agent_field = agent_field,
        );
        fs::write(dir.join(format!("{}.json", session_id)), meta).unwrap();

        let jsonl = r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"parsed prompt"}]}}
{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"toolUse","data":{"toolUseId":"t1","name":"Edit","input":{"file_path":"src/lib.rs"}}}]}}
"#;
        fs::write(dir.join(format!("{}.jsonl", session_id)), jsonl).unwrap();
    }

    fn collector_for(dir: &Path) -> KiroCollector {
        KiroCollector {
            sessions_dir: dir.to_path_buf(),
            transcript_cache: HashMap::new(),
        }
    }

    #[test]
    fn collector_uses_credits_when_tokens_are_zero() {
        // kiro-cli's real persistence behavior: input/output token counts are 0,
        // but metering_usage[].value records real credit consumption per turn.
        // We scale credits ×100 into total_input_tokens so the existing UI/rate
        // plumbing lights up without agent-specific branching downstream.
        let dir = tempfile::tempdir().unwrap();
        let meta = r#"{
            "session_id":"credit-1",
            "cwd":"/tmp/x",
            "created_at":"2026-04-16T05:30:52Z",
            "session_state":{
                "version":"v1",
                "conversation_metadata":{"user_turn_metadatas":[
                    {"input_token_count":0,"output_token_count":0,"metering_usage":[{"value":7.42}]},
                    {"input_token_count":0,"output_token_count":0,"metering_usage":[{"value":3.25}]}
                ]},
                "rts_model_state":{
                    "conversation_id":"credit-1",
                    "model_info":{"model_name":"claude-opus-4.7"},
                    "context_usage_percentage":10.0
                },
                "permissions":{}
            }
        }"#;
        fs::write(dir.path().join("credit-1.json"), meta).unwrap();
        fs::write(
            dir.path().join("credit-1.lock"),
            r#"{"pid":2001,"started_at":"2026-04-16T05:30:52Z"}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("credit-1.jsonl"),
            r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"x"}]}}
"#,
        )
        .unwrap();

        let mut process_info = HashMap::new();
        process_info.insert(2001, mk_proc(2001, "kiro-cli-chat acp"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let s = &c.collect(&shared)[0];
        // 7.42 + 3.25 = 10.67 credits → scaled x100 = 1067
        assert_eq!(s.total_input_tokens, 1067);
        assert_eq!(s.total_output_tokens, 0);
    }

    #[test]
    fn collector_preserves_real_tokens_when_present() {
        // If kiro ever starts emitting real token counts, we must not
        // overwrite them with credits.
        let dir = tempfile::tempdir().unwrap();
        let meta = r#"{
            "session_id":"real-1","cwd":"/tmp/x",
            "created_at":"2026-04-16T05:30:52Z",
            "session_state":{
                "version":"v1",
                "conversation_metadata":{"user_turn_metadatas":[
                    {"input_token_count":500,"output_token_count":200,"metering_usage":[{"value":9.99}]}
                ]},
                "rts_model_state":{
                    "conversation_id":"real-1",
                    "model_info":{"model_name":"claude-opus-4.7"},
                    "context_usage_percentage":10.0
                },
                "permissions":{}
            }
        }"#;
        fs::write(dir.path().join("real-1.json"), meta).unwrap();
        fs::write(
            dir.path().join("real-1.lock"),
            r#"{"pid":2002,"started_at":"2026-04-16T05:30:52Z"}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("real-1.jsonl"),
            r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"x"}]}}
"#,
        )
        .unwrap();

        let mut process_info = HashMap::new();
        process_info.insert(2002, mk_proc(2002, "kiro-cli-chat"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let s = &c.collect(&shared)[0];
        // Real counts win; credits are ignored when tokens are present.
        assert_eq!(s.total_input_tokens, 500);
        assert_eq!(s.total_output_tokens, 200);
    }

    #[test]
    fn collector_accepts_kiro_cli_chat_backend_binary() {
        // Real-world: kiro-cli spawns kiro-cli-chat as the ACP backend; the lock
        // stores the backend's PID, not the frontend's. We must accept both names.
        let dir = tempfile::tempdir().unwrap();
        stage_session(dir.path(), "live-1", 1001, Some("Real Session"), None);

        let mut process_info = HashMap::new();
        process_info.insert(
            1001,
            mk_proc(
                1001,
                "/Users/x/tools/kiro-cli/Kiro CLI.app/Contents/MacOS/kiro-cli-chat acp --trust-tools ...",
            ),
        );
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let sessions = c.collect(&shared);
        assert_eq!(sessions.len(), 1, "kiro-cli-chat backend PID should be recognized");
        assert_eq!(sessions[0].session_id, "live-1");
    }

    #[test]
    fn collector_surfaces_live_sessions_only() {
        let dir = tempfile::tempdir().unwrap();
        stage_session(dir.path(), "live-1", 1001, Some("My Task"), Some("planner"));
        stage_session(dir.path(), "stale-1", 1002, None, None); // PID not in process_info → stale
        stage_session(dir.path(), "wrong-bin-1", 1003, None, None); // PID exists but wrong binary

        // Simulate ps output: live-1's PID owns kiro-cli; wrong-bin-1's PID is bash.
        let mut process_info = HashMap::new();
        process_info.insert(1001, mk_proc(1001, "kiro-cli chat"));
        process_info.insert(1003, mk_proc(1003, "bash -l"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let sessions = c.collect(&shared);

        assert_eq!(sessions.len(), 1, "only the live session should be returned");
        let s = &sessions[0];
        assert_eq!(s.agent_cli, "kiro");
        assert_eq!(s.session_id, "live-1");
        assert_eq!(s.pid, 1001);
        assert_eq!(s.total_input_tokens, 100);
        assert_eq!(s.total_output_tokens, 50);
        assert!((s.context_percent - 42.5).abs() < 0.001);
    }

    #[test]
    fn collector_prefers_metadata_title_over_parsed_prompt() {
        let dir = tempfile::tempdir().unwrap();
        stage_session(dir.path(), "live-1", 1001, Some("Metadata Title Wins"), None);

        let mut process_info = HashMap::new();
        process_info.insert(1001, mk_proc(1001, "/opt/homebrew/bin/kiro-cli chat"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let sessions = c.collect(&shared);
        assert_eq!(sessions[0].initial_prompt, "Metadata Title Wins");
    }

    #[test]
    fn collector_falls_back_to_parsed_prompt_when_title_absent() {
        let dir = tempfile::tempdir().unwrap();
        stage_session(dir.path(), "live-1", 1001, None, None);

        let mut process_info = HashMap::new();
        process_info.insert(1001, mk_proc(1001, "kiro-cli"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let sessions = c.collect(&shared);
        assert_eq!(sessions[0].initial_prompt, "parsed prompt");
    }

    #[test]
    fn collector_embeds_agent_name_in_model_column() {
        let dir = tempfile::tempdir().unwrap();
        stage_session(dir.path(), "live-1", 1001, None, Some("planner-agent"));

        let mut process_info = HashMap::new();
        process_info.insert(1001, mk_proc(1001, "kiro-cli chat"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let sessions = c.collect(&shared);
        assert_eq!(sessions[0].model, "claude-opus-4.6 · planner-agent");
    }

    #[test]
    fn collector_omits_agent_name_separator_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        stage_session(dir.path(), "live-1", 1001, None, None);

        let mut process_info = HashMap::new();
        process_info.insert(1001, mk_proc(1001, "kiro-cli"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        let sessions = c.collect(&shared);
        assert_eq!(sessions[0].model, "claude-opus-4.6");
    }

    #[test]
    fn collector_evicts_cache_when_session_disappears() {
        let dir = tempfile::tempdir().unwrap();
        stage_session(dir.path(), "live-1", 1001, None, None);

        let mut process_info = HashMap::new();
        process_info.insert(1001, mk_proc(1001, "kiro-cli"));
        let shared = SharedProcessData {
            process_info,
            children_map: HashMap::new(),
            ports: HashMap::new(),
        slow_tick: false,
        };

        let mut c = collector_for(dir.path());
        c.collect(&shared);
        assert_eq!(c.transcript_cache.len(), 1);

        // Simulate session ending: remove lock file → no longer live.
        fs::remove_file(dir.path().join("live-1.lock")).unwrap();
        c.collect(&shared);
        assert_eq!(c.transcript_cache.len(), 0, "cache should evict dropped sessions");
    }
}
