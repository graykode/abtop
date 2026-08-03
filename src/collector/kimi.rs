use super::{process, SharedProcessData};
use crate::model::{AgentSession, ChildProcess, SessionStatus, SubAgent};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Collector for Kimi Code (`kimi`) sessions.
///
/// Discovery strategy:
/// 1. Find running `kimi` processes via shared `ps` data
/// 2. Map each PID's cwd to a workspace under `~/.kimi-code/sessions/wd_*`
/// 3. Pick the newest session dir for that workspace (by `state.json` mtime /
///    `updatedAt`), claiming one session per live PID when several share a cwd
/// 4. Parse `agents/main/wire.jsonl` for tokens, model, current tool, etc.
///
/// Config root: `~/.kimi-code` (override with `KIMI_CODE_HOME`).
pub struct KimiCollector {
    config_root: PathBuf,
    /// Cached workspace id → root path (from workspaces.json).
    workspace_roots: HashMap<String, String>,
    /// Cached PID → cwd mapping. On macOS this avoids running one `lsof`
    /// process per Kimi session on every fast tick.
    process_cwds: HashMap<u32, String>,
    /// Cached wire parse state keyed by session id.
    wire_cache: HashMap<String, WireState>,
    /// Version string from `~/.kimi-code/updates/latest.json` (best-effort).
    version: String,
}

#[derive(Debug, Clone, Default)]
struct WireState {
    offset: u64,
    partial: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    turn_count: u32,
    model: String,
    effort: String,
    /// Last step context size (inputOther + inputCacheRead).
    last_context_tokens: u64,
    last_activity_ms: u64,
    first_prompt: String,
    current_task: String,
    /// tool.call without matching tool.result still open.
    pending_tools: u32,
    pending_since_ms: u64,
    thinking_since_ms: u64,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct StateFile {
    #[serde(default, rename = "createdAt")]
    created_at: String,
    #[serde(default, rename = "updatedAt")]
    updated_at: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "workDir")]
    work_dir: String,
    #[serde(default, rename = "lastPrompt")]
    last_prompt: String,
    #[serde(default)]
    agents: HashMap<String, AgentMeta>,
}

#[derive(Debug, Deserialize)]
struct AgentMeta {
    #[serde(default, rename = "type")]
    agent_type: String,
}

#[derive(Debug, Deserialize)]
struct WorkspacesFile {
    #[serde(default)]
    workspaces: HashMap<String, WorkspaceEntry>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceEntry {
    #[serde(default)]
    root: String,
}

impl KimiCollector {
    pub fn new() -> Self {
        let config_root = std::env::var("KIMI_CODE_HOME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".kimi-code"));
        Self {
            config_root,
            workspace_roots: HashMap::new(),
            process_cwds: HashMap::new(),
            wire_cache: HashMap::new(),
            version: String::new(),
        }
    }

    fn refresh_meta(&mut self) {
        self.workspace_roots = load_workspace_roots(&self.config_root);
        if self.version.is_empty() {
            self.version = load_version(&self.config_root);
        }
    }

    fn collect_sessions(&mut self, shared: &SharedProcessData) -> Vec<AgentSession> {
        if !self.config_root.is_dir() {
            self.process_cwds.clear();
            return vec![];
        }

        // Refresh workspace map on slow ticks (or first run).
        if shared.slow_tick || self.workspace_roots.is_empty() {
            self.refresh_meta();
        }

        let kimi_pids = find_kimi_pids(&shared.process_info);
        if kimi_pids.is_empty() {
            self.process_cwds.clear();
            return vec![];
        }

        self.refresh_process_cwds(&kimi_pids, shared.slow_tick);

        // Group live PIDs by cwd.
        let mut pids_by_cwd: HashMap<String, Vec<u32>> = HashMap::new();
        for pid in kimi_pids {
            let Some(cwd) = self.process_cwds.get(&pid).cloned() else {
                continue;
            };
            if cwd.len() < 2 {
                continue;
            }
            pids_by_cwd.entry(cwd).or_default().push(pid);
        }

        // Invert workspace map: root path → workspace id.
        let mut root_to_wd: HashMap<String, String> = HashMap::new();
        for (wd, root) in &self.workspace_roots {
            root_to_wd.insert(root.clone(), wd.clone());
        }

        let now_ms = current_time_ms();
        let mut sessions = Vec::new();
        let mut live_session_ids: HashSet<String> = HashSet::new();

        for (cwd, mut pids) in pids_by_cwd {
            pids.sort_unstable();
            let wd_id = root_to_wd
                .get(&cwd)
                .cloned()
                .or_else(|| find_workspace_id_by_scan(&self.config_root, &cwd));

            let Some(wd_id) = wd_id else {
                // Live process but no known workspace — still show a stub row.
                for pid in pids {
                    sessions.push(stub_session(
                        pid,
                        &cwd,
                        &shared.process_info,
                        &shared.children_map,
                        &shared.ports,
                        &self.config_root,
                        &self.version,
                    ));
                }
                continue;
            };

            let mut candidates = list_sessions_for_workspace(&self.config_root, &wd_id);
            // Newest first so the first N map to the N live PIDs.
            candidates.sort_by_key(|b| std::cmp::Reverse(b.updated_ms));

            for (i, pid) in pids.into_iter().enumerate() {
                let Some(meta) = candidates.get(i) else {
                    sessions.push(stub_session(
                        pid,
                        &cwd,
                        &shared.process_info,
                        &shared.children_map,
                        &shared.ports,
                        &self.config_root,
                        &self.version,
                    ));
                    continue;
                };

                live_session_ids.insert(meta.session_id.clone());
                let wire_path = meta
                    .session_dir
                    .join("agents")
                    .join("main")
                    .join("wire.jsonl");

                let wire = self.parse_wire_incremental(&meta.session_id, &wire_path);
                let state = read_state_file(&meta.session_dir.join("state.json"));

                let proc = shared.process_info.get(&pid);
                let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);

                let model = if !wire.model.is_empty() {
                    wire.model.clone()
                } else {
                    "-".to_string()
                };
                let context_window = context_window_for_kimi(&model);
                let context_percent = if context_window > 0 && wire.last_context_tokens > 0 {
                    (wire.last_context_tokens as f64 / context_window as f64) * 100.0
                } else {
                    0.0
                };

                let activity_ms = wire
                    .last_activity_ms
                    .max(meta.updated_ms)
                    .max(wire_mtime_ms(&wire_path));
                let age_secs = now_ms.saturating_sub(activity_ms) / 1000;
                let has_active_child = process::has_active_descendant(
                    pid,
                    &shared.children_map,
                    &shared.process_info,
                    5.0,
                );
                let cpu_active = proc.is_some_and(|p| p.cpu_pct > 1.0);

                let status = if wire.pending_tools > 0 || has_active_child {
                    SessionStatus::Executing
                } else if age_secs < 30 || cpu_active || wire.thinking_since_ms > 0 {
                    SessionStatus::Thinking
                } else {
                    SessionStatus::Waiting
                };

                let current_tasks = if !wire.current_task.is_empty()
                    && matches!(status, SessionStatus::Executing)
                {
                    vec![wire.current_task.clone()]
                } else if matches!(status, SessionStatus::Waiting) {
                    vec!["waiting for input".to_string()]
                } else if matches!(status, SessionStatus::Thinking) {
                    vec!["thinking...".to_string()]
                } else {
                    vec![]
                };

                let project_name = process::last_path_segment(&cwd).unwrap_or("?").to_string();

                let started_at =
                    parse_iso_ms(state.as_ref().map(|s| s.created_at.as_str()).unwrap_or(""))
                        .unwrap_or(meta.updated_ms);

                let initial_prompt = {
                    let from_wire = wire.first_prompt.clone();
                    let from_state = state
                        .as_ref()
                        .map(|s| {
                            if !s.title.is_empty() {
                                s.title.clone()
                            } else {
                                s.last_prompt.clone()
                            }
                        })
                        .unwrap_or_default();
                    if !from_wire.is_empty() {
                        from_wire
                    } else {
                        from_state
                    }
                };

                let subagents: Vec<SubAgent> = state
                    .as_ref()
                    .map(|s| {
                        s.agents
                            .iter()
                            .filter(|(name, a)| *name != "main" && a.agent_type != "main")
                            .map(|(name, _)| SubAgent {
                                name: name.clone(),
                                // Kimi subagent wire files are not polled yet;
                                // surface presence only.
                                status: String::new(),
                                tokens: 0,
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let children = collect_children(pid, shared);

                sessions.push(AgentSession {
                    agent_cli: "kimi",
                    pid,
                    session_id: meta.session_id.clone(),
                    cwd: cwd.clone(),
                    project_name,
                    started_at,
                    status,
                    model,
                    effort: wire.effort.clone(),
                    context_percent,
                    total_input_tokens: wire.total_input,
                    total_output_tokens: wire.total_output,
                    total_cache_read: wire.total_cache_read,
                    total_cache_create: wire.total_cache_create,
                    turn_count: wire.turn_count,
                    current_tasks,
                    mem_mb,
                    version: self.version.clone(),
                    git_branch: String::new(),
                    git_added: 0,
                    git_modified: 0,
                    token_history: wire.token_history.clone(),
                    context_history: wire.context_history.clone(),
                    compaction_count: 0,
                    context_window,
                    subagents,
                    mem_file_count: 0,
                    mem_line_count: 0,
                    children,
                    initial_prompt: super::redact_secrets(&super::sanitize_terminal_text(
                        &truncate_str(&initial_prompt, 200),
                    )),
                    first_assistant_text: String::new(),
                    chat_messages: vec![],
                    tool_calls: vec![],
                    pending_since_ms: wire.pending_since_ms,
                    thinking_since_ms: wire.thinking_since_ms,
                    file_accesses: vec![],
                    config_root: super::abbrev_path(&self.config_root),
                });
            }
        }

        // Drop wire cache entries for sessions no longer live.
        self.wire_cache
            .retain(|id, _| live_session_ids.contains(id));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn refresh_process_cwds(&mut self, pids: &[u32], force: bool) {
        let live: HashSet<u32> = pids.iter().copied().collect();
        self.process_cwds.retain(|pid, _| live.contains(pid));

        for &pid in pids {
            if force || !self.process_cwds.contains_key(&pid) {
                match process::get_process_cwd(pid) {
                    Some(cwd) => {
                        self.process_cwds.insert(pid, cwd);
                    }
                    None => {
                        self.process_cwds.remove(&pid);
                    }
                }
            }
        }
    }

    fn parse_wire_incremental(&mut self, session_id: &str, path: &Path) -> WireState {
        let mut state = self.wire_cache.get(session_id).cloned().unwrap_or_default();

        let Ok(meta) = fs::metadata(path) else {
            return state;
        };
        let file_len = meta.len();

        // File rotated / rewritten smaller — full rescan.
        if file_len < state.offset {
            state = WireState::default();
        }

        // Nothing new on disk (incomplete trailing line waits for more bytes).
        if file_len == state.offset {
            return state;
        }

        let Ok(mut file) = File::open(path) else {
            return state;
        };
        let start_offset = state.offset;
        if start_offset > 0 && file.seek(SeekFrom::Start(start_offset)).is_err() {
            return state;
        }

        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return state;
        }

        // Prepend any incomplete trailing line from the previous read.
        // Those bytes were already counted in `offset`; only `buf` is new.
        let combined = if state.partial.is_empty() {
            buf
        } else {
            let mut s = std::mem::take(&mut state.partial);
            s.push_str(&buf);
            s
        };

        for line in combined.split_inclusive('\n') {
            if !line.ends_with('\n') {
                state.partial = line.to_string();
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                apply_wire_event(&mut state, &value);
            }
        }
        // All bytes through EOF were either parsed or buffered in `partial`.
        state.offset = file_len;

        self.wire_cache
            .insert(session_id.to_string(), state.clone());
        state
    }
}

impl Default for KimiCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for KimiCollector {
    fn collect(&mut self, shared: &SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

// ── Wire event application ──────────────────────────────────────────────────

fn apply_wire_event(state: &mut WireState, value: &Value) {
    let Some(event_type) = value.get("type").and_then(|v| v.as_str()) else {
        return;
    };
    let time_ms = value.get("time").and_then(|v| v.as_u64()).unwrap_or(0);
    if time_ms > state.last_activity_ms {
        state.last_activity_ms = time_ms;
    }

    match event_type {
        "usage.record" => {
            let scope = value
                .get("usageScope")
                .and_then(|v| v.as_str())
                .unwrap_or("turn");
            if scope != "turn" {
                return;
            }
            if let Some(model) = value.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    state.model = model.to_string();
                }
            }
            if let Some(usage) = value.get("usage") {
                let input = usage_u64(usage, "inputOther");
                let output = usage_u64(usage, "output");
                let cache_read = usage_u64(usage, "inputCacheRead");
                let cache_create = usage_u64(usage, "inputCacheCreation");
                state.total_input = state.total_input.saturating_add(input);
                state.total_output = state.total_output.saturating_add(output);
                state.total_cache_read = state.total_cache_read.saturating_add(cache_read);
                state.total_cache_create = state.total_cache_create.saturating_add(cache_create);
                state.turn_count = state.turn_count.saturating_add(1);
                let turn_tokens = input
                    .saturating_add(output)
                    .saturating_add(cache_read)
                    .saturating_add(cache_create);
                state.token_history.push(turn_tokens);
                if state.token_history.len() > 200 {
                    let drain = state.token_history.len() - 200;
                    state.token_history.drain(0..drain);
                }
                // Context usage excludes cache_create (same rationale as Claude #54).
                state.last_context_tokens = input.saturating_add(cache_read);
                state.context_history.push(state.last_context_tokens);
                if state.context_history.len() > 200 {
                    let drain = state.context_history.len() - 200;
                    state.context_history.drain(0..drain);
                }
            }
            // A usage record closes a thinking/generation phase.
            state.thinking_since_ms = 0;
        }
        "llm.request" => {
            if let Some(model) = value
                .get("modelAlias")
                .and_then(|v| v.as_str())
                .or_else(|| value.get("model").and_then(|v| v.as_str()))
            {
                if !model.is_empty() {
                    state.model = model.to_string();
                }
            }
            if let Some(effort) = value.get("thinkingEffort").and_then(|v| v.as_str()) {
                state.effort = effort.to_string();
            }
            if state.thinking_since_ms == 0 {
                state.thinking_since_ms = time_ms;
            }
        }
        "turn.prompt" => {
            if state.first_prompt.is_empty() {
                if let Some(text) = first_text_from_input(value.get("input")) {
                    state.first_prompt = text;
                }
            }
            state.thinking_since_ms = time_ms;
        }
        "context.append_loop_event" => {
            let Some(event) = value.get("event") else {
                return;
            };
            let Some(ev_type) = event.get("type").and_then(|v| v.as_str()) else {
                return;
            };
            match ev_type {
                "tool.call" => {
                    state.pending_tools = state.pending_tools.saturating_add(1);
                    if state.pending_since_ms == 0 {
                        state.pending_since_ms = time_ms;
                    }
                    state.thinking_since_ms = 0;
                    state.current_task = format_tool_task(event);
                }
                "tool.result" => {
                    state.pending_tools = state.pending_tools.saturating_sub(1);
                    if state.pending_tools == 0 {
                        state.pending_since_ms = 0;
                        state.current_task.clear();
                    }
                }
                "step.end" => {
                    if let Some(usage) = event.get("usage") {
                        let input = usage_u64(usage, "inputOther");
                        let cache_read = usage_u64(usage, "inputCacheRead");
                        state.last_context_tokens = input.saturating_add(cache_read);
                    }
                    state.thinking_since_ms = 0;
                }
                "step.begin" if state.pending_tools == 0 && state.thinking_since_ms == 0 => {
                    state.thinking_since_ms = time_ms;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn usage_u64(usage: &Value, key: &str) -> u64 {
    usage
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .unwrap_or(0)
}

fn first_text_from_input(input: Option<&Value>) -> Option<String> {
    let arr = input?.as_array()?;
    for item in arr {
        if item.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                let t = text.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn format_tool_task(event: &Value) -> String {
    let name = event.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
    let args = event.get("args").cloned().unwrap_or(Value::Null);
    let arg = match name {
        "Bash" | "bash" => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .to_string(),
        "Read" | "Write" | "Edit" | "read" | "write" | "edit" => args
            .get("file_path")
            .or_else(|| args.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Grep" | "Glob" => args
            .get("pattern")
            .or_else(|| args.get("glob_pattern"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Agent" => args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => {
            // Prefer description when present (Kimi often provides a short one).
            if let Some(d) = event.get("description").and_then(|v| v.as_str()) {
                d.to_string()
            } else {
                String::new()
            }
        }
    };
    let arg = super::sanitize_terminal_text(&arg);
    let arg = super::redact_secrets(&arg);
    let arg = truncate_str(&arg, 40);
    if arg.is_empty() {
        name.to_string()
    } else {
        format!("{name} {arg}")
    }
}

// ── Discovery helpers ───────────────────────────────────────────────────────

fn find_kimi_pids(process_info: &HashMap<u32, process::ProcInfo>) -> Vec<u32> {
    process_info
        .iter()
        .filter(|(_, info)| {
            process::cmd_has_binary(&info.command, "kimi") && !info.command.contains("grep")
        })
        .map(|(pid, _)| *pid)
        .collect()
}

#[derive(Debug, Clone)]
struct SessionMeta {
    session_id: String,
    session_dir: PathBuf,
    updated_ms: u64,
}

fn list_sessions_for_workspace(config_root: &Path, wd_id: &str) -> Vec<SessionMeta> {
    let dir = config_root.join("sessions").join(wd_id);
    let Ok(entries) = fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("session_") {
            continue;
        }
        let state_path = path.join("state.json");
        let updated_ms = fs::metadata(&state_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // Prefer state.updatedAt when parseable (more accurate than mtime).
        let updated_ms = read_state_file(&state_path)
            .and_then(|s| parse_iso_ms(&s.updated_at))
            .unwrap_or(updated_ms);
        out.push(SessionMeta {
            session_id: name,
            session_dir: path,
            updated_ms,
        });
    }
    out
}

fn load_workspace_roots(config_root: &Path) -> HashMap<String, String> {
    let path = config_root.join("workspaces.json");
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(parsed) = serde_json::from_str::<WorkspacesFile>(&text) else {
        return HashMap::new();
    };
    parsed
        .workspaces
        .into_iter()
        .filter(|(_, e)| !e.root.is_empty())
        .map(|(id, e)| (id, e.root))
        .collect()
}

/// Fallback when workspaces.json is missing/stale: scan state.json files for workDir.
fn find_workspace_id_by_scan(config_root: &Path, cwd: &str) -> Option<String> {
    let sessions = config_root.join("sessions");
    let entries = fs::read_dir(sessions).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let wd_id = entry.file_name().to_string_lossy().into_owned();
        if !wd_id.starts_with("wd_") {
            continue;
        }
        // Check any session's state.json under this workspace.
        if let Ok(subs) = fs::read_dir(&path) {
            for sub in subs.flatten() {
                let state_path = sub.path().join("state.json");
                if let Some(state) = read_state_file(&state_path) {
                    if process::paths_equal(&state.work_dir, cwd) {
                        return Some(wd_id);
                    }
                }
            }
        }
    }
    None
}

fn read_state_file(path: &Path) -> Option<StateFile> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn load_version(config_root: &Path) -> String {
    let path = config_root.join("updates").join("latest.json");
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return String::new();
    };
    value
        .pointer("/lastSuccess/version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn wire_mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn stub_session(
    pid: u32,
    cwd: &str,
    process_info: &HashMap<u32, process::ProcInfo>,
    children_map: &HashMap<u32, Vec<u32>>,
    ports: &HashMap<u32, Vec<u16>>,
    config_root: &Path,
    version: &str,
) -> AgentSession {
    let proc = process_info.get(&pid);
    let mem_mb = proc.map(|p| p.rss_kb / 1024).unwrap_or(0);
    let project_name = process::last_path_segment(cwd).unwrap_or("?").to_string();
    let children = collect_children_from_maps(pid, process_info, children_map, ports);
    AgentSession {
        agent_cli: "kimi",
        pid,
        session_id: format!("kimi-{pid}"),
        cwd: cwd.to_string(),
        project_name,
        started_at: current_time_ms(),
        status: SessionStatus::Unknown,
        model: "-".to_string(),
        effort: String::new(),
        context_percent: 0.0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_cache_read: 0,
        total_cache_create: 0,
        turn_count: 0,
        current_tasks: vec!["session metadata unavailable".to_string()],
        mem_mb,
        version: version.to_string(),
        git_branch: String::new(),
        git_added: 0,
        git_modified: 0,
        token_history: vec![],
        context_history: vec![],
        compaction_count: 0,
        context_window: 200_000,
        subagents: vec![],
        mem_file_count: 0,
        mem_line_count: 0,
        children,
        initial_prompt: String::new(),
        first_assistant_text: String::new(),
        chat_messages: vec![],
        tool_calls: vec![],
        pending_since_ms: 0,
        thinking_since_ms: 0,
        file_accesses: vec![],
        config_root: super::abbrev_path(config_root),
    }
}

fn collect_children(pid: u32, shared: &SharedProcessData) -> Vec<ChildProcess> {
    collect_children_from_maps(
        pid,
        &shared.process_info,
        &shared.children_map,
        &shared.ports,
    )
}

fn collect_children_from_maps(
    pid: u32,
    process_info: &HashMap<u32, process::ProcInfo>,
    children_map: &HashMap<u32, Vec<u32>>,
    ports: &HashMap<u32, Vec<u16>>,
) -> Vec<ChildProcess> {
    let mut children = Vec::new();
    let mut stack: Vec<u32> = children_map.get(&pid).cloned().unwrap_or_default();
    let mut visited = HashSet::new();
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
        if let Some(gc) = children_map.get(&cpid) {
            stack.extend(gc);
        }
    }
    children
}

/// Context window for Kimi models. Values mirror `~/.kimi-code/config.toml`.
pub(crate) fn context_window_for_kimi(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("k3") && !m.contains("256") {
        1_048_576
    } else if m.contains("256") || m.contains("kimi-for-coding") || m.contains("k2") {
        262_144
    } else {
        200_000
    }
}

fn parse_iso_ms(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    // chrono parses RFC3339 with fractional seconds.
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis() as u64)
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn context_window_k3_is_1m() {
        assert_eq!(context_window_for_kimi("kimi-code/k3"), 1_048_576);
        assert_eq!(context_window_for_kimi("k3"), 1_048_576);
    }

    #[test]
    fn context_window_256k_variants() {
        assert_eq!(context_window_for_kimi("kimi-code/k3-256k"), 262_144);
        assert_eq!(
            context_window_for_kimi("kimi-code/kimi-for-coding"),
            262_144
        );
    }

    #[test]
    fn apply_wire_usage_and_tool() {
        let mut state = WireState::default();
        let usage = serde_json::json!({
            "type": "usage.record",
            "model": "kimi-code/k3",
            "usage": {
                "inputOther": 100,
                "output": 50,
                "inputCacheRead": 1000,
                "inputCacheCreation": 0
            },
            "usageScope": "turn",
            "time": 1000
        });
        apply_wire_event(&mut state, &usage);
        assert_eq!(state.total_input, 100);
        assert_eq!(state.total_output, 50);
        assert_eq!(state.total_cache_read, 1000);
        assert_eq!(state.turn_count, 1);
        assert_eq!(state.token_history, vec![1150]);
        assert_eq!(state.last_context_tokens, 1100);
        assert_eq!(state.model, "kimi-code/k3");

        let tool = serde_json::json!({
            "type": "context.append_loop_event",
            "event": {
                "type": "tool.call",
                "name": "Bash",
                "args": {"command": "cargo test"}
            },
            "time": 2000
        });
        apply_wire_event(&mut state, &tool);
        assert_eq!(state.pending_tools, 1);
        assert!(state.current_task.contains("Bash"));
        assert!(state.current_task.contains("cargo test"));

        let result = serde_json::json!({
            "type": "context.append_loop_event",
            "event": {"type": "tool.result", "toolCallId": "x"},
            "time": 3000
        });
        apply_wire_event(&mut state, &result);
        assert_eq!(state.pending_tools, 0);
        assert!(state.current_task.is_empty());
    }

    #[test]
    fn tool_task_redacts_secrets_and_uses_only_first_command_line() {
        let tool = serde_json::json!({
            "name": "Bash",
            "args": {"command": "curl -H 'Authorization: Bearer secret-token' example.com\necho leaked"}
        });

        let task = format_tool_task(&tool);
        assert!(task.starts_with("Bash curl"));
        assert!(task.contains("[REDACTED]"));
        assert!(!task.contains("secret-token"));
        assert!(!task.contains("echo leaked"));
    }

    #[test]
    fn agent_task_never_falls_back_to_prompt_text() {
        let tool = serde_json::json!({
            "name": "Agent",
            "args": {"prompt": "private prompt contents"}
        });

        assert_eq!(format_tool_task(&tool), "Agent");
    }

    #[test]
    fn apply_wire_first_prompt() {
        let mut state = WireState::default();
        let prompt = serde_json::json!({
            "type": "turn.prompt",
            "input": [{"type": "text", "text": "fix the bug"}],
            "time": 1
        });
        apply_wire_event(&mut state, &prompt);
        assert_eq!(state.first_prompt, "fix the bug");
    }

    #[test]
    fn incremental_wire_parse_across_chunks() {
        let dir = tempdir().unwrap();
        let wire = dir.path().join("wire.jsonl");
        {
            let mut f = File::create(&wire).unwrap();
            writeln!(
                f,
                r#"{{"type":"usage.record","model":"kimi-code/k3","usage":{{"inputOther":10,"output":5,"inputCacheRead":100,"inputCacheCreation":0}},"usageScope":"turn","time":1}}"#
            )
            .unwrap();
        }

        let mut collector = KimiCollector {
            config_root: dir.path().to_path_buf(),
            workspace_roots: HashMap::new(),
            process_cwds: HashMap::new(),
            wire_cache: HashMap::new(),
            version: String::new(),
        };
        let s1 = collector.parse_wire_incremental("sess1", &wire);
        assert_eq!(s1.turn_count, 1);
        assert_eq!(s1.total_input, 10);

        // Append another usage line
        {
            let mut f = fs::OpenOptions::new().append(true).open(&wire).unwrap();
            writeln!(
                f,
                r#"{{"type":"usage.record","model":"kimi-code/k3","usage":{{"inputOther":20,"output":7,"inputCacheRead":200,"inputCacheCreation":0}},"usageScope":"turn","time":2}}"#
            )
            .unwrap();
        }
        let s2 = collector.parse_wire_incremental("sess1", &wire);
        assert_eq!(s2.turn_count, 2);
        assert_eq!(s2.total_input, 30);
        assert_eq!(s2.total_output, 12);
        assert_eq!(s2.last_context_tokens, 220);
    }

    #[test]
    fn list_sessions_sorted_by_updated() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let wd = root.join("sessions").join("wd_demo");
        fs::create_dir_all(wd.join("session_old")).unwrap();
        fs::create_dir_all(wd.join("session_new")).unwrap();
        fs::write(
            wd.join("session_old").join("state.json"),
            r#"{"createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-01T00:00:00.000Z","title":"old","workDir":"/tmp/demo"}"#,
        )
        .unwrap();
        fs::write(
            wd.join("session_new").join("state.json"),
            r#"{"createdAt":"2026-02-01T00:00:00.000Z","updatedAt":"2026-02-01T00:00:00.000Z","title":"new","workDir":"/tmp/demo"}"#,
        )
        .unwrap();

        let mut sessions = list_sessions_for_workspace(root, "wd_demo");
        sessions.sort_by_key(|b| std::cmp::Reverse(b.updated_ms));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "session_new");
        assert_eq!(sessions[1].session_id, "session_old");
    }

    #[test]
    fn load_workspace_roots_parses() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("workspaces.json"),
            r#"{"version":1,"workspaces":{"wd_x":{"root":"/home/u/proj","name":"proj"}}}"#,
        )
        .unwrap();
        let map = load_workspace_roots(dir.path());
        assert_eq!(map.get("wd_x").map(String::as_str), Some("/home/u/proj"));
    }

    #[test]
    fn collects_live_session_from_local_process_and_wire_state() {
        let dir = tempdir().unwrap();
        let cwd = "/tmp/kimi-project";
        let session_dir = dir
            .path()
            .join("sessions")
            .join("wd_demo")
            .join("session_live");
        let agent_dir = session_dir.join("agents").join("main");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            dir.path().join("workspaces.json"),
            format!(r#"{{"workspaces":{{"wd_demo":{{"root":"{cwd}"}}}}}}"#),
        )
        .unwrap();
        fs::write(
            session_dir.join("state.json"),
            format!(
                r#"{{"createdAt":"2026-08-03T00:00:00Z","updatedAt":"2026-08-03T00:00:01Z","title":"Local Kimi session","workDir":"{cwd}"}}"#
            ),
        )
        .unwrap();
        fs::write(
            agent_dir.join("wire.jsonl"),
            concat!(
                "{\"type\":\"usage.record\",\"model\":\"kimi-code/k3\",\"usage\":{\"inputOther\":10,\"output\":5,\"inputCacheRead\":100,\"inputCacheCreation\":2},\"usageScope\":\"turn\",\"time\":1}\n",
                "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"tool.call\",\"name\":\"Read\",\"args\":{\"file_path\":\"src/main.rs\"}},\"time\":2}\n"
            ),
        )
        .unwrap();

        let pid = 42;
        let mut collector = KimiCollector {
            config_root: dir.path().to_path_buf(),
            workspace_roots: HashMap::new(),
            process_cwds: HashMap::from([(pid, cwd.to_string())]),
            wire_cache: HashMap::new(),
            version: "0.6.0".to_string(),
        };
        let shared = SharedProcessData {
            process_info: HashMap::from([(
                pid,
                process::ProcInfo {
                    pid,
                    ppid: 1,
                    rss_kb: 2048,
                    cpu_pct: 0.0,
                    command: "/usr/local/bin/kimi".to_string(),
                },
            )]),
            children_map: HashMap::new(),
            ports: HashMap::new(),
            slow_tick: false,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        };

        let sessions = collector.collect_sessions(&shared);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.pid, pid);
        assert_eq!(session.session_id, "session_live");
        assert_eq!(session.cwd, cwd);
        assert_eq!(session.model, "kimi-code/k3");
        assert_eq!(session.status, SessionStatus::Executing);
        assert_eq!(session.current_tasks, vec!["Read src/main.rs"]);
        assert_eq!(session.total_tokens(), 117);
        assert_eq!(session.token_history, vec![117]);
    }
}
