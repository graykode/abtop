//! Collector for Factory Droid (the `~/.factory` workspace): live sessions,
//! the custom-model catalog, missions, and config validation.
//!
//! Factory Droid is a local desktop agent. It maintains `~/.factory/`:
//! - `sessions-index.json` — every session's last-update mtime, cwd, title,
//!   and orchestrator→worker relationships. The collector treats an index
//!   entry as live while its mtime is recent (the index is rewritten whenever
//!   a session makes progress), so liveness does not depend on guessing the
//!   desktop app's process name.
//! - `sessions/**/<id>.settings.json` — per-session token usage (`tokenUsage`)
//!   and the parent's view of each worker's usage
//!   (`childInclusiveTokenUsageBySessionId`).
//! - `settings.json` / `factory-settings.json` — the custom model catalog.
//! - `missions/<id>/` — per-mission state, model settings, and working dir.
//!
//! Secrets are never surfaced: the model catalog keeps `base_url` but drops
//! `apiKey`.

use crate::model::{AgentSession, SessionStatus, SubAgent};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Index entries updated more recently than this are surfaced as live.
const LIVE_WINDOW_MS: u64 = 10 * 60 * 1000;
/// An index entry updated more recently than this is "thinking".
const THINKING_MS: u64 = 15 * 1000;
/// Updated more recently than this → "executing".
const EXECUTING_MS: u64 = 120 * 1000;
/// Cap on reported config issues.
const MAX_ISSUES: usize = 50;

/// Case-insensitive tokens used to detect a running Factory Droid app.
const DROID_NAME_TOKENS: &[&str] = &["droid", "factory"];

/// A model from the Factory Droid catalog (apiKey intentionally omitted).
#[derive(Debug, Clone, Serialize)]
pub struct FactoryModel {
    /// Canonical model id, e.g. `custom:claude-opus-5-0`.
    pub id: String,
    /// Provider model name, e.g. `claude-opus-5`.
    pub model: String,
    pub display_name: String,
    pub provider: String,
    pub base_url: String,
    pub max_context_limit: u64,
    pub max_output_tokens: u64,
    pub no_image_support: bool,
    /// Sort index within the source file.
    pub index: i64,
    /// Source catalog: "droid" (settings.json) or "vibemode" (factory-settings.json).
    pub source: &'static str,
    /// True when referenced by a default / orchestrator / subagent setting.
    pub is_default: bool,
}

/// A Factory Droid mission.
#[derive(Debug, Clone, Serialize)]
pub struct FactoryMission {
    /// Stable mission id, e.g. `mis_57ffdada`.
    pub mission_id: String,
    /// Directory name under `missions/`.
    pub dir: String,
    /// Lifecycle state reported by `state.json` (paused, planning, running, …).
    pub state: String,
    /// First heading from `mission.md` (falls back to the mission_accepted title).
    pub title: String,
    pub cwd: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// `workerModel` from `model-settings.json`, if present.
    pub worker_model: String,
}

/// A config validation finding.
#[derive(Debug, Clone, Serialize)]
pub struct FactoryConfigIssue {
    /// "high" | "medium" | "low".
    pub severity: &'static str,
    /// File the issue was found in (relative to `~/.factory`).
    pub file: String,
    pub message: String,
}

/// Token usage parsed from a session's `<sessionId>.settings.json`.
#[derive(Debug, Default, Clone, Copy)]
struct SessionTokenUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
}

impl SessionTokenUsage {
    fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_create
    }
}

/// Last call token usage from a session's settings (`lastCallTokenUsage`).
#[derive(Debug, Default, Clone, Copy)]
struct LastCallUsage {
    input: u64,
    output: u64,
    cache_read: u64,
}

/// Deserialized `sessions-index.json`.
#[derive(Debug, Deserialize)]
struct IndexRoot {
    #[serde(default)]
    entries: Vec<IndexEntry>,
}

#[derive(Debug, Deserialize)]
struct IndexEntry {
    #[serde(default, rename = "sessionId")]
    session_id: String,
    #[serde(default, rename = "hostId")]
    #[allow(dead_code)]
    host_id: String,
    /// Last-update epoch-millis, stored as a float.
    #[serde(default)]
    mtime: f64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    cwd: String,
    #[serde(default, rename = "messagesCount")]
    messages_count: u64,
    #[serde(default, rename = "callingSessionId")]
    calling_session_id: Option<String>,
    #[serde(default)]
    tags: Vec<Tag>,
}

#[derive(Debug, Deserialize)]
struct Tag {
    #[serde(default)]
    name: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

impl IndexEntry {
    fn mtime_ms(&self) -> u64 {
        self.mtime.max(0.0) as u64
    }

    /// Mission id referenced by a `mission-session` tag, if any.
    fn mission_id(&self) -> Option<&str> {
        for tag in &self.tags {
            if tag.name != "mission-session" {
                continue;
            }
            if let Some(meta) = &tag.metadata {
                if let Some(id) = meta.get("missionId").and_then(serde_json::Value::as_str) {
                    if !id.is_empty() {
                        return Some(id);
                    }
                }
            }
        }
        None
    }
}

/// Collects Factory Droid sessions and metadata.
pub struct FactoryCollector {
    root: PathBuf,
    active: bool,
    cached_index: Vec<IndexEntry>,
    cached_models: Vec<FactoryModel>,
    cached_missions: Vec<FactoryMission>,
    cached_issues: Vec<FactoryConfigIssue>,
    /// Own token usage by session id (from `sessions/**/<id>.settings.json`).
    cached_tokens: HashMap<String, SessionTokenUsage>,
    /// Inclusive token usage for worker sessions as recorded by their parent
    /// (`childInclusiveTokenUsageBySessionId`).
    cached_child_tokens: HashMap<String, SessionTokenUsage>,
    /// Last call token usage by session id (from `lastCallTokenUsage` in settings).
    cached_last_call_usage: HashMap<String, LastCallUsage>,
    /// PIDs of detected droid processes on the last tick.
    last_droid_pids: Vec<u32>,
}

impl FactoryCollector {
    pub fn new() -> Self {
        let root = dirs::home_dir().map(|h| h.join(".factory"));
        let active = root.as_ref().is_some_and(|r| r.is_dir());
        Self {
            root: root.unwrap_or_default(),
            active,
            cached_index: Vec::new(),
            cached_models: Vec::new(),
            cached_missions: Vec::new(),
            cached_issues: Vec::new(),
            cached_tokens: HashMap::new(),
            cached_child_tokens: HashMap::new(),
            cached_last_call_usage: HashMap::new(),
            last_droid_pids: Vec::new(),
        }
    }

    /// True when the droid desktop app was detected on the last tick.
    pub fn app_running(&self) -> bool {
        !self.last_droid_pids.is_empty()
    }

    pub fn models(&self) -> &[FactoryModel] {
        &self.cached_models
    }

    pub fn missions(&self) -> &[FactoryMission] {
        &self.cached_missions
    }

    pub fn issues(&self) -> &[FactoryConfigIssue] {
        &self.cached_issues
    }

    fn collect_impl(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        if !self.active {
            return Vec::new();
        }
        self.last_droid_pids = Self::find_droid_pids(&shared.process_info);
        if shared.slow_tick {
            self.cached_index = read_index(&self.root);
            self.cached_models = read_models(&self.root);
            self.cached_missions = read_missions(&self.root);
            self.cached_issues = validate_config(&self.root);
            let (tokens, child_tokens, last_call) = read_session_tokens(&self.root);
            self.cached_tokens = tokens;
            self.cached_child_tokens = child_tokens;
            self.cached_last_call_usage = last_call;
        }
        self.build_sessions()
    }

    /// Best-effort detection of a running Factory Droid desktop app.
    fn find_droid_pids(process_info: &HashMap<u32, super::process::ProcInfo>) -> Vec<u32> {
        process_info
            .iter()
            .filter(|(_, info)| {
                let lower = info.command.to_ascii_lowercase();
                DROID_NAME_TOKENS.iter().any(|tok| lower.contains(tok))
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    fn build_sessions(&self) -> Vec<AgentSession> {
        let now = now_ms();
        let mission_models: HashMap<&str, &str> = self
            .cached_missions
            .iter()
            .filter_map(|m| {
                if m.worker_model.is_empty() {
                    None
                } else {
                    Some((m.mission_id.as_str(), m.worker_model.as_str()))
                }
            })
            .collect();
        let default_model = self
            .cached_models
            .iter()
            .find(|m| m.is_default)
            .map(|m| m.model.clone())
            .unwrap_or_default();

        let mut parents: Vec<&IndexEntry> = self
            .cached_index
            .iter()
            .filter(|e| e.calling_session_id.is_none())
            .collect();
        parents.sort_by_key(|a| std::cmp::Reverse(a.mtime_ms()));
        let workers: Vec<&IndexEntry> = self
            .cached_index
            .iter()
            .filter(|e| e.calling_session_id.is_some())
            .collect();

        let config_root = super::abbrev_path(&self.root);
        let mut sessions = Vec::new();

        for parent in parents {
            let age = now.saturating_sub(parent.mtime_ms());
            if age >= LIVE_WINDOW_MS {
                continue;
            }

            let mut subagents = Vec::new();
            for worker in workers
                .iter()
                .filter(|w| w.calling_session_id.as_deref() == Some(parent.session_id.as_str()))
            {
                let worker_age = now.saturating_sub(worker.mtime_ms());
                if worker_age >= LIVE_WINDOW_MS {
                    continue;
                }
                subagents.push(SubAgent {
                    name: subagent_name(worker),
                    status: if worker_age < EXECUTING_MS {
                        "working".to_string()
                    } else {
                        "idle".to_string()
                    },
                    tokens: self
                        .cached_child_tokens
                        .get(&worker.session_id)
                        .map_or(0, |u| u.total()),
                });
            }

            let model = parent
                .mission_id()
                .and_then(|id| mission_models.get(id))
                .map(|m| (*m).to_string())
                .unwrap_or_else(|| default_model.clone());

            // Find max_context_limit for this model from the catalog.
            let context_window = self
                .cached_models
                .iter()
                .find(|m| m.id == model || m.model == model)
                .map(|m| m.max_context_limit)
                .unwrap_or(0);

            // Current context usage from last call (input + cache_read, like Claude).
            let last_call = self
                .cached_last_call_usage
                .get(&parent.session_id)
                .copied()
                .unwrap_or_default();
            let current_context = last_call.input.saturating_add(last_call.cache_read);
            let context_percent = if context_window > 0 {
                (current_context as f64 / context_window as f64 * 100.0).min(100.0)
            } else {
                0.0
            };

            let tokens = self
                .cached_tokens
                .get(&parent.session_id)
                .copied()
                .unwrap_or_default();

            sessions.push(AgentSession {
                agent_cli: "factory",
                pid: 0,
                session_id: parent.session_id.clone(),
                cwd: parent.cwd.clone(),
                project_name: base_name(&parent.cwd),
                started_at: parent.mtime_ms(),
                status: live_status(age),
                model,
                effort: String::new(),
                context_percent,
                total_input_tokens: tokens.input,
                total_output_tokens: tokens.output,
                total_cache_read: tokens.cache_read,
                total_cache_create: tokens.cache_create,
                turn_count: parent.messages_count as u32,
                current_tasks: current_tasks(parent, age),
                mem_mb: 0,
                version: String::new(),
                git_branch: String::new(),
                git_added: 0,
                git_modified: 0,
                token_history: Vec::new(),
                context_history: Vec::new(),
                compaction_count: 0,
                context_window,
                subagents,
                mem_file_count: 0,
                mem_line_count: 0,
                children: Vec::new(),
                initial_prompt: parent.title.clone(),
                first_assistant_text: String::new(),
                chat_messages: Vec::new(),
                tool_calls: Vec::new(),
                pending_since_ms: 0,
                thinking_since_ms: 0,
                file_accesses: Vec::new(),
                config_root: config_root.clone(),
            });
        }

        sessions
    }
}

impl Default for FactoryCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for FactoryCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_impl(shared)
    }

    fn discovered_config_dirs(&self) -> Vec<PathBuf> {
        if self.active {
            vec![self.root.clone()]
        } else {
            Vec::new()
        }
    }
}

fn live_status(age_ms: u64) -> SessionStatus {
    if age_ms < THINKING_MS {
        SessionStatus::Thinking
    } else if age_ms < EXECUTING_MS {
        SessionStatus::Executing
    } else {
        SessionStatus::Waiting
    }
}

fn current_tasks(e: &IndexEntry, age_ms: u64) -> Vec<String> {
    let has_tag = |name: &str| e.tags.iter().any(|t| t.name == name);
    if has_tag("mission-worker") {
        vec!["mission worker".to_string()]
    } else if has_tag("exec") || age_ms < EXECUTING_MS {
        vec!["executing".to_string()]
    } else {
        vec!["waiting for input".to_string()]
    }
}

fn subagent_name(w: &IndexEntry) -> String {
    let title = w.title.trim();
    if title.is_empty() || title.eq_ignore_ascii_case("New Session") {
        "worker".to_string()
    } else {
        truncate(title, 120)
    }
}

fn read_index(root: &Path) -> Vec<IndexEntry> {
    let path = root.join("sessions-index.json");
    if is_symlink(&path) || !path.exists() {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<IndexRoot>(&text) else {
        return Vec::new();
    };
    parsed.entries
}

/// Recursively scan `sessions/` for `<sessionId>.settings.json` files and parse
/// token usage. Returns `(own usage by session id, child inclusive usage by
/// child session id, last call usage by session id)`.
fn read_session_tokens(
    root: &Path,
) -> (
    HashMap<String, SessionTokenUsage>,
    HashMap<String, SessionTokenUsage>,
    HashMap<String, LastCallUsage>,
) {
    let mut own = HashMap::new();
    let mut children = HashMap::new();
    let mut last_call = HashMap::new();
    let mut stack = vec![root.join("sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(session_id) = name.strip_suffix(".settings.json") else {
                continue;
            };
            if session_id.is_empty() || is_symlink(&path) {
                continue;
            }
            let v = read_json(&path);
            if v.is_null() {
                continue;
            }
            if let Some(usage) = parse_token_usage(v.get("tokenUsage")) {
                own.insert(session_id.to_string(), usage);
            }
            if let Some(lc) = parse_last_call_usage(v.get("lastCallTokenUsage")) {
                last_call.insert(session_id.to_string(), lc);
            }
            if let Some(child_usage) = v
                .get("childInclusiveTokenUsageBySessionId")
                .and_then(serde_json::Value::as_object)
            {
                for (child_id, child_value) in child_usage {
                    if let Some(usage) = parse_token_usage(Some(child_value)) {
                        children.insert(child_id.clone(), usage);
                    }
                }
            }
        }
    }
    (own, children, last_call)
}

/// Parse `lastCallTokenUsage` from a session's settings.
fn parse_last_call_usage(v: Option<&serde_json::Value>) -> Option<LastCallUsage> {
    let obj = v?.as_object()?;
    let get = |key: &str| obj.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Some(LastCallUsage {
        input: get("inputTokens"),
        output: get("outputTokens"),
        cache_read: get("cacheReadTokens"),
    })
}

/// Parse a Factory Droid token usage object, tolerating missing/unknown fields.
fn parse_token_usage(v: Option<&serde_json::Value>) -> Option<SessionTokenUsage> {
    let obj = v?.as_object()?;
    let get = |key: &str| obj.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Some(SessionTokenUsage {
        input: get("inputTokens"),
        output: get("outputTokens"),
        cache_read: get("cacheReadTokens"),
        cache_create: get("cacheCreationTokens"),
    })
}

/// Collect default-referenced model ids (session default, orchestrator, and
/// subagent/mission settings) across both catalog files.
fn collect_default_ids(v: &serde_json::Value, ids: &mut HashSet<String>) {
    let mut insert = |id: &str| {
        if id.starts_with("custom:") {
            ids.insert(id.to_string());
        }
    };
    if let Some(sec) = v
        .get("sessionDefaultSettings")
        .and_then(serde_json::Value::as_object)
    {
        if let Some(id) = sec.get("model").and_then(serde_json::Value::as_str) {
            insert(id);
        }
    }
    if let Some(id) = v
        .get("missionOrchestratorModel")
        .and_then(serde_json::Value::as_str)
    {
        insert(id);
    }
    if let Some(general) = v.get("general").and_then(serde_json::Value::as_object) {
        for (_, section) in general {
            if let Some(section) = section.as_object() {
                for (_, value) in section {
                    if let Some(id) = value.as_str() {
                        insert(id);
                    }
                }
            }
        }
    }
}

fn read_models(root: &Path) -> Vec<FactoryModel> {
    let mut default_ids = HashSet::new();
    for name in ["settings.json", "factory-settings.json"] {
        if let Ok(text) = fs::read_to_string(root.join(name)) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                collect_default_ids(&v, &mut default_ids);
            }
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (name, source) in [
        ("settings.json", "droid"),
        ("factory-settings.json", "vibemode"),
    ] {
        let Ok(text) = fs::read_to_string(root.join(name)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(models) = v.get("customModels").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for m in models {
            let id = m
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if id.is_empty() || !seen.insert(id.to_string()) {
                continue;
            }
            out.push(FactoryModel {
                id: id.to_string(),
                model: m
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                display_name: m
                    .get("displayName")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                provider: m
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                base_url: m
                    .get("baseUrl")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                max_context_limit: m
                    .get("maxContextLimit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                max_output_tokens: m
                    .get("maxOutputTokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                no_image_support: m
                    .get("noImageSupport")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                index: m
                    .get("index")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
                source,
                is_default: default_ids.contains(id),
            });
        }
    }
    out.sort_by(|a, b| {
        a.source
            .cmp(b.source)
            .then(a.index.cmp(&b.index))
            .then(a.id.cmp(&b.id))
    });
    out
}

fn read_json(path: &Path) -> serde_json::Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn read_mission_title(dir: &Path) -> String {
    if let Ok(text) = fs::read_to_string(dir.join("mission.md")) {
        if let Some(first) = text.lines().next() {
            let title = first.trim_start_matches('#').trim();
            if !title.is_empty() {
                return truncate(title, 256);
            }
        }
    }
    if let Ok(text) = fs::read_to_string(dir.join("progress_log.jsonl")) {
        if let Some(first) = text.lines().next() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(first) {
                let title = v
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !title.is_empty() {
                    return truncate(title, 256);
                }
            }
        }
    }
    "mission".to_string()
}

fn read_missions(root: &Path) -> Vec<FactoryMission> {
    let mut out = Vec::new();
    let missions_dir = root.join("missions");
    let Ok(read_dir) = fs::read_dir(&missions_dir) else {
        return out;
    };
    let mut dirs: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let state = read_json(&dir.join("state.json"));
        let mission_id = state
            .get("missionId")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let state_str = state
            .get("state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let cwd = state
            .get("workingDirectory")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let created_at_ms = state
            .get("createdAt")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_iso_ms)
            .unwrap_or(0);
        let updated_at_ms = state
            .get("updatedAt")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_iso_ms)
            .unwrap_or(0);
        let worker_model = read_json(&dir.join("model-settings.json"))
            .get("workerModel")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        out.push(FactoryMission {
            mission_id,
            dir: dir_name,
            state: state_str,
            title: read_mission_title(&dir),
            cwd,
            created_at_ms,
            updated_at_ms,
            worker_model,
        });
    }
    out
}

/// Parse an RFC3339 timestamp (as written by Factory Droid) into epoch-millis.
fn parse_iso_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

fn issue(
    severity: &'static str,
    file: impl Into<String>,
    message: impl Into<String>,
) -> FactoryConfigIssue {
    FactoryConfigIssue {
        severity,
        file: file.into(),
        message: message.into(),
    }
}

/// Validate a `customModels`-style JSON body for structural problems and
/// dangling default-model references.
fn validate_models_file(v: &serde_json::Value, file: &str, issues: &mut Vec<FactoryConfigIssue>) {
    let Some(models) = v.get("customModels").and_then(serde_json::Value::as_array) else {
        return;
    };
    let mut ids: HashMap<&str, usize> = HashMap::new();
    let mut indices: HashSet<i64> = HashSet::new();
    let mut seen_indices: HashMap<i64, usize> = HashMap::new();

    for (i, m) in models.iter().enumerate() {
        let id = m
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if id.is_empty() {
            issues.push(issue("low", file, format!("customModels[{i}] missing id")));
        } else if let Some(prev) = ids.insert(id, i) {
            issues.push(issue(
                "medium",
                file,
                format!("duplicate model id '{id}' at indices {prev} and {i}"),
            ));
        }
        if m.get("model")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        {
            issues.push(issue(
                "low",
                file,
                format!("customModels[{i}] missing model name"),
            ));
        }
        if m.get("baseUrl")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        {
            issues.push(issue(
                "low",
                file,
                format!("customModels[{i}] missing baseUrl"),
            ));
        }
        if let Some(idx) = m.get("index").and_then(serde_json::Value::as_i64) {
            if !indices.insert(idx) {
                if let Some(prev) = seen_indices.get(&idx) {
                    issues.push(issue(
                        "low",
                        file,
                        format!("duplicate model index {idx} at indices {prev} and {i}"),
                    ));
                }
            }
            seen_indices.insert(idx, i);
        }
    }

    let known: HashSet<&str> = ids.keys().copied().collect();
    let mut refs = Vec::new();
    if let Some(id) = v
        .get("sessionDefaultSettings")
        .and_then(|s| s.get("model"))
        .and_then(serde_json::Value::as_str)
    {
        refs.push(id);
    }
    if let Some(id) = v
        .get("missionOrchestratorModel")
        .and_then(serde_json::Value::as_str)
    {
        refs.push(id);
    }
    if let Some(general) = v.get("general").and_then(serde_json::Value::as_object) {
        for (_, section) in general {
            if let Some(section) = section.as_object() {
                for (_, value) in section {
                    if let Some(id) = value.as_str() {
                        if id.starts_with("custom:") {
                            refs.push(id);
                        }
                    }
                }
            }
        }
    }
    for id in refs {
        if id.starts_with("custom:") && !known.contains(id) {
            issues.push(issue(
                "medium",
                file,
                format!("default model '{id}' is not in customModels"),
            ));
        }
    }
}

/// Structural validation of the whole `~/.factory` config surface.
fn validate_config(root: &Path) -> Vec<FactoryConfigIssue> {
    let mut issues = Vec::new();

    for name in ["settings.json", "factory-settings.json"] {
        match fs::read_to_string(root.join(name)) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => validate_models_file(&v, name, &mut issues),
                Err(e) => issues.push(issue("high", name, format!("invalid JSON: {e}"))),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                issues.push(issue("low", name, "missing"));
            }
            Err(e) => issues.push(issue("low", name, format!("unreadable: {e}"))),
        }
    }

    match fs::read_to_string(root.join("sessions-index.json")) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                if let Some(entries) = v.get("entries").and_then(serde_json::Value::as_array) {
                    let empty_cwd = entries
                        .iter()
                        .filter(|e| {
                            e.get("cwd")
                                .and_then(serde_json::Value::as_str)
                                .is_none_or(str::is_empty)
                        })
                        .count();
                    if empty_cwd > 0 {
                        issues.push(issue(
                            "low",
                            "sessions-index.json",
                            format!("{empty_cwd} entries without cwd"),
                        ));
                    }
                }
            }
            Err(e) => issues.push(issue(
                "high",
                "sessions-index.json",
                format!("invalid JSON: {e}"),
            )),
        },
        Err(_) => issues.push(issue("low", "sessions-index.json", "missing")),
    }

    if let Ok(read_dir) = fs::read_dir(root.join("missions")) {
        let mut dirs: Vec<PathBuf> = read_dir
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let dir_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let state_path = dir.join("state.json");
            if let Ok(text) = fs::read_to_string(&state_path) {
                if serde_json::from_str::<serde_json::Value>(&text).is_err() {
                    issues.push(issue(
                        "medium",
                        format!("missions/{dir_name}"),
                        "invalid state.json",
                    ));
                }
            }
            if !dir.join("working_directory.txt").exists() {
                issues.push(issue(
                    "low",
                    format!("missions/{dir_name}"),
                    "missing working_directory.txt",
                ));
            }
        }
    }

    for name in ["task-invocations.json", "background-processes.json"] {
        let path = root.join(name);
        if !path.exists() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&path) {
            if serde_json::from_str::<serde_json::Value>(&text).is_err() {
                issues.push(issue("low", name, "invalid JSON"));
            }
        }
    }

    issues.truncate(MAX_ISSUES);
    issues
}

/// Check if a path is a symlink (fail-closed: returns true on error).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

fn base_name(p: &str) -> String {
    p.rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("?")
        .to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample_index() -> Vec<IndexEntry> {
        let text = r#"{"version":2,"entries":[
          {"sessionId":"parent-1","hostId":"h1","mtime":<RECENT>,"title":"Orchestrator","cwd":"C:\\project","messagesCount":5},
          {"sessionId":"worker-1","hostId":"h1","mtime":<RECENT>,"title":"Worker: fix x","cwd":"C:\\project","messagesCount":2,"callingSessionId":"parent-1","tags":[{"name":"mission-worker"},{"name":"mission-session","metadata":{"role":"worker","missionId":"mis_1"}}]},
          {"sessionId":"old-1","hostId":"h1","mtime":1000,"title":"Stale","cwd":"C:\\old","messagesCount":1}
        ]}"#;
        let now = now_ms();
        let json = text.replace("<RECENT>", &format!("{}", now - 5000));
        let root: IndexRoot = serde_json::from_str(&json).unwrap();
        root.entries
    }

    fn collector_with(root: PathBuf) -> FactoryCollector {
        FactoryCollector {
            root,
            active: true,
            cached_index: Vec::new(),
            cached_models: Vec::new(),
            cached_missions: Vec::new(),
            cached_issues: Vec::new(),
            cached_tokens: HashMap::new(),
            cached_child_tokens: HashMap::new(),
            cached_last_call_usage: HashMap::new(),
            last_droid_pids: Vec::new(),
        }
    }

    #[test]
    fn build_sessions_keeps_live_parents_and_attaches_workers() {
        let root = dirs::home_dir().unwrap().join(".factory");
        let mut collector = collector_with(root);
        collector.cached_index = sample_index();
        let sessions = collector.build_sessions();

        assert_eq!(sessions.len(), 1, "stale parent dropped");
        let s = &sessions[0];
        assert_eq!(s.agent_cli, "factory");
        assert_eq!(s.session_id, "parent-1");
        assert_eq!(s.project_name, "project");
        assert_eq!(s.subagents.len(), 1);
        assert_eq!(s.subagents[0].name, "Worker: fix x");
        assert_eq!(s.config_root, "~/.factory");
    }

    #[test]
    fn build_sessions_populates_token_usage_and_subagent_tokens() {
        let root = dirs::home_dir().unwrap().join(".factory");
        let mut collector = collector_with(root);
        collector.cached_index = sample_index();
        collector.cached_tokens.insert(
            "parent-1".to_string(),
            SessionTokenUsage {
                input: 1000,
                output: 100,
                cache_read: 9000,
                cache_create: 0,
            },
        );
        collector.cached_child_tokens.insert(
            "worker-1".to_string(),
            SessionTokenUsage {
                input: 50,
                output: 5,
                cache_read: 400,
                cache_create: 0,
            },
        );

        let sessions = collector.build_sessions();
        let s = &sessions[0];
        assert_eq!(s.total_input_tokens, 1000);
        assert_eq!(s.total_output_tokens, 100);
        assert_eq!(s.total_cache_read, 9000);
        assert_eq!(s.total_cache_create, 0);
        assert_eq!(s.total_tokens(), 10100);
        assert_eq!(s.subagents[0].tokens, 455);
    }

    #[test]
    fn read_session_tokens_parses_own_and_child_usage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sessions = root.join("sessions").join("-C-project-Miros");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("parent-1.settings.json"),
            r#"{
              "model": "custom:x",
              "tokenUsage": {
                "inputTokens": 100,
                "outputTokens": 20,
                "cacheCreationTokens": 0,
                "cacheReadTokens": 300,
                "thinkingTokens": 5
              },
              "childInclusiveTokenUsageBySessionId": {
                "worker-1": {
                  "inputTokens": 10,
                  "outputTokens": 2,
                  "cacheCreationTokens": 0,
                  "cacheReadTokens": 30
                }
              }
            }"#,
        )
        .unwrap();
        std::fs::write(
            sessions.join("worker-1.settings.json"),
            r#"{"tokenUsage": {"inputTokens": 10, "outputTokens": 2}}"#,
        )
        .unwrap();
        // Non-JSON and non-settings files are ignored.
        std::fs::write(sessions.join("notes.txt"), "not json").unwrap();
        std::fs::write(sessions.join("misc.settings.json.bak"), "{").unwrap();

        let (own, children, _last_call) = read_session_tokens(root);
        assert_eq!(own.get("parent-1").unwrap().total(), 420);
        assert_eq!(own.get("worker-1").unwrap().total(), 12);
        assert_eq!(children.get("worker-1").unwrap().total(), 42);
        assert!(!children.contains_key("parent-1"));
        assert_eq!(own.len(), 2, "notes.txt and .bak ignored");
    }

    #[test]
    fn parse_token_usage_tolerates_partial_objects() {
        let v: serde_json::Value = serde_json::from_str(r#"{"inputTokens": 7}"#).unwrap();
        let usage = parse_token_usage(Some(&v)).unwrap();
        assert_eq!(usage.input, 7);
        assert_eq!(usage.output, 0);
        assert!(parse_token_usage(Some(&serde_json::Value::Null)).is_none());
        assert!(parse_token_usage(None).is_none());
    }

    #[test]
    fn status_windows_map_age_to_thinking_executing_waiting() {
        assert_eq!(live_status(5_000), SessionStatus::Thinking);
        assert_eq!(live_status(60_000), SessionStatus::Executing);
        assert_eq!(live_status(5 * 60_000), SessionStatus::Waiting);
    }

    #[test]
    fn base_name_handles_windows_and_posix_paths() {
        assert_eq!(base_name("C:\\project"), "project");
        assert_eq!(base_name("C:\\project\\sub"), "sub");
        assert_eq!(base_name("/home/u/proj"), "proj");
        assert_eq!(base_name("C:\\"), "C:");
        assert_eq!(base_name(""), "?");
    }

    #[test]
    fn parse_iso_ms_accepts_rfc3339_zulu() {
        assert_eq!(
            parse_iso_ms("2026-06-24T00:31:45.533Z"),
            Some(1_782_261_105_533)
        );
    }

    #[test]
    fn read_models_dedupes_by_id_and_marks_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut settings = std::fs::File::create(root.join("settings.json")).unwrap();
        write!(
            settings,
            r#"{{
              "sessionDefaultSettings": {{ "model": "custom:a-1" }},
              "customModels": [
                {{ "id": "custom:a-1", "model": "a", "index": 0, "baseUrl": "http://127.0.0.1:3000/v1", "provider": "generic" }},
                {{ "id": "custom:b-2", "model": "b", "index": 1, "baseUrl": "http://127.0.0.1:3000/v1", "provider": "openai", "maxContextLimit": 1048576 }}
              ]
            }}"#
        )
        .unwrap();
        let mut vibemode = std::fs::File::create(root.join("factory-settings.json")).unwrap();
        write!(
            vibemode,
            r#"{{ "customModels": [
              {{ "id": "custom:a-1", "model": "a-vibe", "index": 0, "baseUrl": "https://r-api.vibemod.pro/v1" }},
              {{ "id": "custom:c-3", "model": "c", "index": 1, "baseUrl": "https://r-api.vibemod.pro/v1" }}
            ] }}"#
        )
        .unwrap();

        let models = read_models(root);
        assert_eq!(models.len(), 3, "duplicate id custom:a-1 deduped");
        let a = models.iter().find(|m| m.id == "custom:a-1").unwrap();
        assert!(a.is_default);
        assert_eq!(a.source, "droid");
        let c = models.iter().find(|m| m.id == "custom:c-3").unwrap();
        assert_eq!(c.source, "vibemode");
        assert!(!c.is_default);
    }

    #[test]
    fn validate_config_flags_dup_id_and_dangling_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut settings = std::fs::File::create(root.join("settings.json")).unwrap();
        write!(
            settings,
            r#"{{ "sessionDefaultSettings": {{ "model": "custom:ghost" }},
              "customModels": [
                {{ "id": "custom:a", "model": "a", "index": 0, "baseUrl": "http://x" }},
                {{ "id": "custom:a", "model": "a2", "index": 1, "baseUrl": "http://x" }}
              ] }}"#
        )
        .unwrap();

        let issues = validate_config(root);
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("duplicate model id")),
            "issues: {issues:?}"
        );
        assert!(
            issues.iter().any(|i| i.message.contains("custom:ghost")),
            "issues: {issues:?}"
        );
    }

    #[test]
    fn validate_config_reports_broken_settings_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("settings.json"), r#"{"customModels": ["#).unwrap();
        let issues = validate_config(root);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == "high" && i.file == "settings.json"),
            "issues: {issues:?}"
        );
    }

    #[test]
    fn find_droid_pids_matches_by_command_token() {
        use super::super::process::ProcInfo;
        let mut info = HashMap::new();
        info.insert(
            10,
            ProcInfo {
                pid: 10,
                ppid: 1,
                rss_kb: 1000,
                cpu_pct: 0.0,
                command: "C:\\Program Files\\Factory Droid\\Droid.exe".to_string(),
            },
        );
        info.insert(
            20,
            ProcInfo {
                pid: 20,
                ppid: 1,
                rss_kb: 1000,
                cpu_pct: 0.0,
                command: "node server.js".to_string(),
            },
        );
        let pids = FactoryCollector::find_droid_pids(&info);
        assert_eq!(pids, vec![10]);
    }
}
