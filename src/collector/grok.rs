//! Collector for xAI Grok Build sessions (`~/.grok`).

use super::{
    abbrev_path, process, redact_secrets, sanitize_terminal_text, AgentCollector, SharedProcessData,
};
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, FileAccess, FileOp, SessionStatus,
    StatusAuthority, StatusEvidence, StatusObservation, StatusReason, SubAgent, ToolCall,
    MAX_CHAT_MESSAGES, MAX_FILE_ACCESSES,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_UPDATE_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_UPDATE_READ_BYTES: u64 = 32 * 1024 * 1024;
const MAX_EVENT_LINE_BYTES: usize = 64 * 1024;
const MAX_EVENT_READ_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SESSION_GROUPS: usize = 2_000;
const MAX_UPDATE_CACHES: usize = 128;
const MAX_EVENT_CACHES: usize = 128;
const MAX_PENDING_PERMISSION_NAMES: usize = 256;
const MAX_PENDING_PERMISSIONS_PER_NAME: usize = 64;
const MAX_TOOL_CALLS: usize = 500;
const MAX_LEADER_LOCK_BYTES: u64 = 32;
const FILE_PREFIX_BYTES: u64 = 256;

pub struct GrokCollector {
    roots: Vec<PathBuf>,
    session_dirs: HashMap<(PathBuf, String), PathBuf>,
    updates: HashMap<PathBuf, UpdateCache>,
    events: HashMap<PathBuf, EventCache>,
}

impl GrokCollector {
    pub fn new() -> Self {
        Self {
            roots: default_roots(),
            session_dirs: HashMap::new(),
            updates: HashMap::new(),
            events: HashMap::new(),
        }
    }

    fn refresh_roots(&mut self, shared: &SharedProcessData) {
        let mut roots = default_roots();
        for (&pid, info) in &shared.process_info {
            if !is_grok_process(&info.command) {
                continue;
            }
            if let Some(root) = process_grok_root(pid) {
                roots.push(root);
            }
        }
        roots.sort();
        roots.dedup();
        self.roots = roots;
    }

    fn find_session_dir(&mut self, root: &Path, session_id: &str) -> Option<PathBuf> {
        let key = (root.to_path_buf(), session_id.to_string());
        if let Some(path) = self.session_dirs.get(&key) {
            if valid_session_dir(root, path, session_id) {
                return Some(path.clone());
            }
            self.session_dirs.remove(&key);
        }

        let sessions_root = root.join("sessions");
        if is_symlink(&sessions_root) {
            return None;
        }
        let groups = fs::read_dir(&sessions_root).ok()?;
        for group in groups.flatten().take(MAX_SESSION_GROUPS) {
            let group_path = group.path();
            if is_symlink(&group_path) || !group_path.is_dir() {
                continue;
            }
            let candidate = group_path.join(session_id);
            if valid_session_dir(root, &candidate, session_id) {
                self.session_dirs.insert(key, candidate.clone());
                return Some(candidate);
            }
        }
        None
    }

    fn parse_updates(&mut self, path: &Path) -> UpdateState {
        if is_symlink(path) {
            self.updates.remove(path);
            return UpdateState {
                lifecycle_failure: Some(StatusReason::ProtocolMalformed),
                ..UpdateState::default()
            };
        }
        let cache = self.updates.entry(path.to_path_buf()).or_default();
        let availability = cache.refresh(path);
        let mut state = cache.state.clone();
        if let EventAvailability::Failed(reason) = availability {
            state.lifecycle_failure = Some(reason);
        }
        state
    }

    fn parse_events(&mut self, path: &Path) -> (EventState, EventAvailability) {
        if is_symlink(path) {
            return (
                EventState::default(),
                EventAvailability::Failed(StatusReason::ProtocolMalformed),
            );
        }
        let cache = self.events.entry(path.to_path_buf()).or_default();
        let availability = cache.refresh(path);
        (cache.state.clone(), availability)
    }

    fn build_row(
        &mut self,
        active: &ActiveSession,
        owns_process_resources: bool,
        shared: &SharedProcessData,
    ) -> Option<AgentSession> {
        let state = self.parse_updates(&active.dir.join("updates.jsonl"));
        let (events, event_availability) = self.parse_events(&active.dir.join("events.jsonl"));
        let (awaiting_plan_approval, plan_availability) =
            read_awaiting_plan_approval(&active.dir.join("plan_mode.json"));
        let signals = read_signals(&active.dir.join("signals.json"));
        let context_window = signals.context_window.unwrap_or(0);
        let context_tokens = signals
            .context_tokens
            .or(state.meta_context_tokens)
            .unwrap_or(0);
        let context_percent = if context_window > 0 {
            signals
                .context_percent
                .unwrap_or(context_tokens as f64 * 100.0 / context_window as f64)
        } else {
            0.0
        };
        let lifecycle = grok_status_decision(
            &state,
            &events,
            event_availability,
            plan_availability,
            active.opened_at,
            awaiting_plan_approval,
        );
        let status = lifecycle.status;
        let awaiting_input = lifecycle.awaiting_input;
        let current_tasks = grok_current_tasks(&state, status, active.opened_at);
        let pending_since_ms = match &status {
            SessionStatus::Waiting | SessionStatus::Executing => lifecycle.status_since_ms,
            _ => 0,
        };
        let thinking_since_ms = if status == SessionStatus::Thinking {
            lifecycle.status_since_ms
        } else {
            0
        };
        let proc_info = shared.process_info.get(&active.pid);
        let children = if owns_process_resources {
            collect_children(active.pid, shared)
        } else {
            Vec::new()
        };

        Some(AgentSession {
            agent_cli: "grok",
            pid: active.pid,
            action_process_incarnation: Some(active.action_process_incarnation.clone()),
            session_id: active.meta.id.clone(),
            cwd: active.meta.cwd.clone(),
            project_name: process::last_path_segment(&active.meta.cwd)
                .unwrap_or("?")
                .to_string(),
            started_at: active.meta.created_at,
            status,
            status_evidence: evidence_for(lifecycle),
            model: if active.meta.model.is_empty() {
                state.model.clone()
            } else {
                active.meta.model.clone()
            },
            effort: active.meta.effort.clone(),
            context_percent,
            total_input_tokens: state.total_input,
            total_output_tokens: state.total_output,
            total_cache_read: state.total_cache_read,
            total_cache_create: state.total_cache_create,
            turn_count: signals.turn_count.unwrap_or(state.turn_count),
            current_tasks,
            mem_mb: if owns_process_resources {
                proc_info.map_or(0, |info| info.rss_kb / 1024)
            } else {
                0
            },
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: state.token_history.clone(),
            context_history: state.context_history.clone(),
            compaction_count: signals.compaction_count.unwrap_or(state.compaction_count),
            context_window,
            subagents: state
                .subagents
                .values()
                .map(SubagentState::to_model)
                .collect(),
            mem_file_count: 0,
            mem_line_count: 0,
            children,
            initial_prompt: active.meta.title.clone(),
            first_assistant_text: state.first_assistant_text.clone(),
            chat_messages: state.chat_messages.clone(),
            tool_calls: state.tool_calls.clone(),
            pending_since_ms,
            awaiting_input,
            thinking_since_ms,
            file_accesses: state.file_accesses.clone(),
            config_root: abbrev_path(&active.root),
        })
    }
}

fn grok_current_tasks(state: &UpdateState, status: SessionStatus, opened_at: u64) -> Vec<String> {
    let mut tasks = match status {
        SessionStatus::Waiting => vec!["waiting for user input".to_string()],
        SessionStatus::Executing => state.current_work_labels(opened_at),
        SessionStatus::Thinking => vec!["thinking".to_string()],
        SessionStatus::Error => vec!["error".to_string()],
        SessionStatus::Idle => vec!["idle".to_string()],
        SessionStatus::Unknown => vec!["status evidence unavailable".to_string()],
        _ => Vec::new(),
    };
    if status == SessionStatus::Executing && tasks.is_empty() {
        tasks.push("executing".to_string());
    }
    tasks
}

#[cfg(test)]
fn grok_session_status(
    state: &UpdateState,
    events: &EventState,
    opened_at: u64,
    awaiting_plan_approval: bool,
) -> (SessionStatus, bool) {
    let decision = grok_status_decision(
        state,
        events,
        EventAvailability::Available,
        EventAvailability::Missing,
        opened_at,
        awaiting_plan_approval,
    );
    (decision.status, decision.awaiting_input)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GrokStatusDecision {
    status: SessionStatus,
    awaiting_input: bool,
    authority: StatusAuthority,
    reason: StatusReason,
    status_since_ms: u64,
}

impl GrokStatusDecision {
    fn provider(status: SessionStatus, reason: StatusReason, status_since_ms: u64) -> Self {
        Self {
            status,
            awaiting_input: status == SessionStatus::Waiting,
            authority: StatusAuthority::Provider,
            reason,
            status_since_ms,
        }
    }

    fn unavailable(reason: StatusReason) -> Self {
        Self {
            status: SessionStatus::Unknown,
            awaiting_input: false,
            authority: StatusAuthority::Unavailable,
            reason,
            status_since_ms: 0,
        }
    }
}

fn grok_status_decision(
    state: &UpdateState,
    events: &EventState,
    event_availability: EventAvailability,
    plan_availability: EventAvailability,
    opened_at: u64,
    awaiting_plan_approval: bool,
) -> GrokStatusDecision {
    if let Some(reason) = state.lifecycle_failure {
        return GrokStatusDecision::unavailable(reason);
    }
    if let EventAvailability::Failed(reason) = plan_availability {
        return GrokStatusDecision::unavailable(reason);
    }
    if let EventAvailability::Failed(reason) = event_availability {
        return GrokStatusDecision::unavailable(reason);
    }

    if awaiting_plan_approval {
        return GrokStatusDecision::provider(
            SessionStatus::Waiting,
            StatusReason::ProviderWaitingApproval,
            opened_at,
        );
    }

    if let Some(since) = state.waiting_since(opened_at) {
        return GrokStatusDecision::provider(
            SessionStatus::Waiting,
            StatusReason::ProviderWaitingUserInput,
            since,
        );
    }

    match event_availability {
        EventAvailability::Available => {
            if let Some(since) = events.waiting_since(opened_at) {
                return GrokStatusDecision::provider(
                    SessionStatus::Waiting,
                    StatusReason::ProviderWaitingApproval,
                    since,
                );
            }
        }
        EventAvailability::Missing => {}
        EventAvailability::Failed(_) => unreachable!("failed event evidence was rejected above"),
    }

    if state.rate_limited_since > 0 && state.rate_limited_since >= opened_at {
        return GrokStatusDecision::provider(
            SessionStatus::RateLimited,
            StatusReason::ProviderRateLimit,
            state.rate_limited_since,
        );
    }

    if state.fatal_error_since > 0 && state.fatal_error_since >= opened_at {
        return GrokStatusDecision::provider(
            SessionStatus::Error,
            StatusReason::ProviderError,
            state.fatal_error_since,
        );
    }

    let event_executing = (event_availability == EventAvailability::Available)
        .then(|| events.executing_since(opened_at))
        .flatten();
    if let Some(since) = min_nonzero(state.executing_since(opened_at), event_executing) {
        return GrokStatusDecision::provider(
            SessionStatus::Executing,
            StatusReason::ProviderExecuting,
            since,
        );
    }

    let event_thinking = (event_availability == EventAvailability::Available)
        .then(|| events.thinking_since(opened_at))
        .flatten();
    if let Some(since) = min_nonzero(state.thinking_since(opened_at), event_thinking) {
        return GrokStatusDecision::provider(
            SessionStatus::Thinking,
            StatusReason::ProviderThinking,
            since,
        );
    }

    GrokStatusDecision::provider(SessionStatus::Idle, StatusReason::ProviderIdle, 0)
}

fn evidence_for(decision: GrokStatusDecision) -> StatusEvidence {
    let observed_at_ms = current_time_ms();
    let mut evidence = StatusEvidence::default();
    evidence.observe(StatusObservation::new(
        decision.status,
        decision.authority,
        decision.reason,
        observed_at_ms,
        0,
    ));
    if decision.status_since_ms > 0 {
        evidence.status_since_ms = decision.status_since_ms;
    }
    evidence
}

impl Default for GrokCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentCollector for GrokCollector {
    fn collect(&mut self, shared: &SharedProcessData) -> Vec<AgentSession> {
        if shared.slow_tick || self.roots.is_empty() {
            self.refresh_roots(shared);
        }

        let mut active = Vec::new();
        for root in self.roots.clone() {
            let leader_pids = read_leader_pids(&root);
            for entry in read_active_registry(&root) {
                let Some(info) = shared.process_info.get(&entry.pid) else {
                    continue;
                };
                if !is_grok_process(&info.command) || leader_pids.contains(&entry.pid) {
                    continue;
                }
                let Some(action_process_incarnation) = process::get_process_incarnation(entry.pid)
                else {
                    continue;
                };
                let Some(action_process_tokens) = process::get_process_tokens(entry.pid) else {
                    continue;
                };
                if !is_grok_process_tokens(&action_process_tokens) {
                    continue;
                }
                let Some(process_started_at) = process::get_process_started_at_ms(entry.pid) else {
                    continue;
                };
                if entry.opened_at == 0 || process_started_at > entry.opened_at {
                    continue;
                }
                let Some(dir) = self.find_session_dir(&root, &entry.session_id) else {
                    continue;
                };
                let Some(meta) = read_summary(&dir.join("summary.json"), &entry) else {
                    continue;
                };
                if is_hidden_grok_session(&meta) {
                    continue;
                }
                let current_incarnation = process::get_process_incarnation(entry.pid);
                if !grok_process_observation_is_exact(
                    &action_process_incarnation,
                    current_incarnation.as_deref(),
                    &action_process_tokens,
                ) {
                    continue;
                }
                active.push(ActiveSession {
                    root: root.clone(),
                    dir,
                    pid: entry.pid,
                    opened_at: entry.opened_at,
                    action_process_incarnation,
                    meta,
                });
            }
        }

        let mut built = Vec::new();
        let mut keep_updates = HashSet::new();
        let mut keep_events = HashSet::new();
        for session in &active {
            keep_updates.insert(session.dir.join("updates.jsonl"));
            keep_events.insert(session.dir.join("events.jsonl"));
            if let Some(row) = self.build_row(session, false, shared) {
                built.push((session.clone(), row));
            }
        }
        self.updates.retain(|path, _| keep_updates.contains(path));
        if self.updates.len() > MAX_UPDATE_CACHES {
            self.updates.clear();
        }
        self.events.retain(|path, _| keep_events.contains(path));
        if self.events.len() > MAX_EVENT_CACHES {
            self.events.clear();
        }

        // Pick a resource owner only from rows that were built successfully.
        // A transiently unreadable newest session must not strip process
        // memory/children from every other row sharing the same Grok PID.
        let successful: Vec<_> = built.iter().map(|(session, _)| session.clone()).collect();
        let resource_owner = resource_owner_indices(&successful);
        // Status ambiguity applies to every validated registry entry, even if
        // one row could not be built during this poll. Dropping an unreadable
        // sibling from this count could make a stale survivor look exactly
        // Idle merely because the competing row failed to render.
        let session_counts = active.iter().fold(HashMap::new(), |mut counts, session| {
            *counts.entry(session.pid).or_insert(0usize) += 1;
            counts
        });
        for (index, (session, row)) in built.iter_mut().enumerate() {
            if resource_owner.get(&session.pid) == Some(&index) {
                row.mem_mb = shared
                    .process_info
                    .get(&session.pid)
                    .map_or(0, |info| info.rss_kb / 1024);
                row.children = collect_children(session.pid, shared);
            }

            let logical_sessions = session_counts.get(&session.pid).copied().unwrap_or(1);
            if let Some(override_status) = shared_pid_idle_uncertainty(
                row.status,
                row.status_evidence.authority,
                logical_sessions,
            ) {
                apply_status_override(row, override_status);
            }
        }
        built.into_iter().map(|(_, row)| row).collect()
    }
}

fn resource_owner_indices(active: &[ActiveSession]) -> HashMap<u32, usize> {
    let mut resource_owner = HashMap::<u32, usize>::new();
    for (index, session) in active.iter().enumerate() {
        resource_owner
            .entry(session.pid)
            .and_modify(|current| {
                if active[*current].meta.updated_at < session.meta.updated_at {
                    *current = index;
                }
            })
            .or_insert(index);
    }
    resource_owner
}

fn shared_pid_idle_uncertainty(
    lifecycle_status: SessionStatus,
    lifecycle_authority: StatusAuthority,
    logical_sessions: usize,
) -> Option<GrokStatusDecision> {
    // Grok's active-session registry is best-effort: unregister may be skipped
    // under lock contention. When one process owns multiple logical rows, the
    // absence of lifecycle work cannot prove that any particular row is still
    // open. Preserve positive provider lifecycle evidence, but do not claim a
    // provider-authoritative Idle state for an ownership-ambiguous row.
    (logical_sessions > 1
        && lifecycle_status == SessionStatus::Idle
        && lifecycle_authority == StatusAuthority::Provider)
        .then(|| GrokStatusDecision::unavailable(StatusReason::OwnershipUnconfirmed))
}

fn apply_status_override(row: &mut AgentSession, decision: GrokStatusDecision) {
    row.status = decision.status;
    row.status_evidence = evidence_for(decision);
    row.awaiting_input = decision.awaiting_input;
    row.pending_since_ms = 0;
    row.thinking_since_ms = 0;
    row.current_tasks = vec!["shared process ownership is ambiguous".to_string()];
}

#[derive(Clone)]
struct ActiveSession {
    root: PathBuf,
    dir: PathBuf,
    pid: u32,
    opened_at: u64,
    /// Exact identity of the process validated against this registry entry.
    action_process_incarnation: String,
    meta: SessionMeta,
}

#[derive(Clone)]
struct RegistryEntry {
    session_id: String,
    pid: u32,
    cwd: String,
    opened_at: u64,
}

#[derive(Clone, Default)]
struct SessionMeta {
    id: String,
    cwd: String,
    title: String,
    model: String,
    effort: String,
    session_kind: String,
    hidden: Option<bool>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Clone, Default)]
struct Signals {
    context_tokens: Option<u64>,
    context_window: Option<u64>,
    context_percent: Option<f64>,
    turn_count: Option<u32>,
    compaction_count: Option<u32>,
}

#[derive(Default)]
struct UpdateCache {
    offset: u64,
    prefix: Vec<u8>,
    partial: Vec<u8>,
    dropping_long_line: bool,
    integrity_failure: Option<StatusReason>,
    state: UpdateState,
}

impl UpdateCache {
    fn refresh(&mut self, path: &Path) -> EventAvailability {
        let Ok(meta) = fs::metadata(path) else {
            return EventAvailability::Failed(StatusReason::Unavailable);
        };
        if !meta.is_file() {
            return EventAvailability::Failed(StatusReason::ProtocolMalformed);
        }
        let Ok(mut file) = File::open(path) else {
            return EventAvailability::Failed(StatusReason::Unavailable);
        };
        let mut prefix = Vec::new();
        if file
            .by_ref()
            .take(meta.len().min(FILE_PREFIX_BYTES))
            .read_to_end(&mut prefix)
            .is_err()
        {
            return EventAvailability::Failed(StatusReason::Unavailable);
        }
        let replaced = !self.prefix.is_empty()
            && (prefix.len() < self.prefix.len() || !prefix.starts_with(&self.prefix));
        if meta.len() < self.offset || replaced {
            *self = Self::default();
        }
        self.prefix = prefix;
        if meta.len() == self.offset {
            return self.availability(meta.len());
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return EventAvailability::Failed(StatusReason::Unavailable);
        }
        let mut bytes = Vec::new();
        if file
            .take(MAX_UPDATE_READ_BYTES)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return EventAvailability::Failed(StatusReason::Unavailable);
        }
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.consume(&bytes);
        self.availability(meta.len())
    }

    fn consume(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.dropping_long_line {
                if byte == b'\n' {
                    self.dropping_long_line = false;
                }
                continue;
            }
            if byte == b'\n' {
                match std::str::from_utf8(&self.partial).map(str::trim) {
                    Ok("") => {}
                    Ok(line) => match serde_json::from_str::<Value>(line) {
                        Ok(value) => match validate_update_record(&value) {
                            Ok(()) => self.state.apply(&value),
                            Err(reason) => self.integrity_failure = Some(reason),
                        },
                        Err(_) => self.integrity_failure = Some(StatusReason::ProtocolMalformed),
                    },
                    Err(_) => self.integrity_failure = Some(StatusReason::ProtocolMalformed),
                }
                self.partial.clear();
            } else if self.partial.len() < MAX_UPDATE_LINE_BYTES {
                self.partial.push(byte);
            } else {
                self.partial.clear();
                self.dropping_long_line = true;
                self.integrity_failure = Some(StatusReason::ProtocolMalformed);
            }
        }
    }

    fn availability(&self, file_len: u64) -> EventAvailability {
        if let Some(reason) = self.integrity_failure {
            EventAvailability::Failed(reason)
        } else if self.offset < file_len || !self.partial.is_empty() || self.dropping_long_line {
            EventAvailability::Failed(StatusReason::Stale)
        } else {
            EventAvailability::Available
        }
    }
}

#[derive(Default)]
struct EventCache {
    offset: u64,
    prefix: Vec<u8>,
    partial: Vec<u8>,
    dropping_long_line: bool,
    state: EventState,
    seen_source: bool,
    failure_reason: Option<StatusReason>,
}

impl EventCache {
    fn refresh(&mut self, path: &Path) -> EventAvailability {
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !self.seen_source => {
                return EventAvailability::Missing;
            }
            Err(_) => return self.fail(StatusReason::Unavailable),
        };
        self.seen_source = true;
        if !meta.is_file() {
            return self.fail(StatusReason::ProtocolMalformed);
        }
        let Ok(mut file) = File::open(path) else {
            return self.fail(StatusReason::Unavailable);
        };
        let mut prefix = Vec::new();
        if file
            .by_ref()
            .take(meta.len().min(FILE_PREFIX_BYTES))
            .read_to_end(&mut prefix)
            .is_err()
        {
            return self.fail(StatusReason::Unavailable);
        }
        let replaced = !self.prefix.is_empty()
            && (prefix.len() < self.prefix.len() || !prefix.starts_with(&self.prefix));
        if meta.len() < self.offset || replaced {
            *self = Self::default();
            self.seen_source = true;
        }
        self.prefix = prefix;
        if meta.len() == self.offset {
            return self.availability(meta.len());
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return self.fail(StatusReason::Unavailable);
        }
        let mut bytes = Vec::new();
        if file
            .take(MAX_EVENT_READ_BYTES)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return self.fail(StatusReason::Unavailable);
        }
        self.offset = self.offset.saturating_add(bytes.len() as u64);
        self.consume(&bytes);
        self.availability(meta.len())
    }

    fn consume(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.dropping_long_line {
                if byte == b'\n' {
                    self.dropping_long_line = false;
                }
                continue;
            }
            if byte == b'\n' {
                match std::str::from_utf8(&self.partial).map(str::trim) {
                    Ok("") => {}
                    Ok(line) => match serde_json::from_str::<Value>(line) {
                        Ok(value) => match validate_event_record(&value) {
                            Ok(()) => self.state.apply(&value),
                            Err(reason) => self.failure_reason = Some(reason),
                        },
                        Err(_) => self.failure_reason = Some(StatusReason::ProtocolMalformed),
                    },
                    Err(_) => self.failure_reason = Some(StatusReason::ProtocolMalformed),
                }
                self.partial.clear();
            } else if self.partial.len() < MAX_EVENT_LINE_BYTES {
                self.partial.push(byte);
            } else {
                self.partial.clear();
                self.dropping_long_line = true;
                self.failure_reason = Some(StatusReason::ProtocolMalformed);
            }
        }
    }

    fn availability(&self, file_len: u64) -> EventAvailability {
        if let Some(reason) = self.failure_reason {
            EventAvailability::Failed(reason)
        } else if self.offset < file_len || !self.partial.is_empty() || self.dropping_long_line {
            EventAvailability::Failed(StatusReason::Stale)
        } else {
            EventAvailability::Available
        }
    }

    fn fail(&mut self, reason: StatusReason) -> EventAvailability {
        if reason == StatusReason::ProtocolMalformed {
            self.failure_reason = Some(reason);
        }
        EventAvailability::Failed(reason)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventAvailability {
    Available,
    Missing,
    Failed(StatusReason),
}

fn validate_update_record(record: &Value) -> Result<(), StatusReason> {
    let params_value = record.get("params").unwrap_or(record);
    let Some(params) = params_value.as_object() else {
        return Err(StatusReason::ProtocolMalformed);
    };
    let update_value = params.get("update").unwrap_or(params_value);
    let Some(update) = update_value.as_object() else {
        return Err(StatusReason::ProtocolMalformed);
    };
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))
        .or_else(|| update.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind.is_empty() {
        return Err(StatusReason::ProtocolMalformed);
    }
    if is_lifecycle_update_kind(kind) && event_time_ms(record, params_value) == 0 {
        return Err(StatusReason::ProtocolMalformed);
    }
    match kind {
        "tool_call" | "tool_call_update"
            if string_field(update_value, &["toolCallId", "tool_call_id", "id"]).is_empty() =>
        {
            Err(StatusReason::ProtocolMalformed)
        }
        "turn_completed"
            if string_field(
                update_value,
                &["prompt_id", "turn_id", "promptId", "turnId", "id"],
            )
            .is_empty()
                || string_field(update_value, &["stop_reason", "stopReason", "reason"])
                    .is_empty() =>
        {
            Err(StatusReason::ProtocolMalformed)
        }
        _ => Ok(()),
    }
}

fn is_lifecycle_update_kind(kind: &str) -> bool {
    matches!(
        kind,
        "user_message_chunk"
            | "agent_message_chunk"
            | "agent_thought_chunk"
            | "tool_call"
            | "tool_call_update"
            | "response_started"
            | "response_completed"
            | "turn_completed"
            | "rewind_marker"
            | "subagent_spawned"
            | "subagent_progress"
            | "subagent_finished"
            | "task_backgrounded"
            | "task_completed"
            | "monitor_event"
            | "retry_state"
            | "auto_compact_failed"
            | "auto_recovery_exhausted"
    )
}

fn validate_event_record(event: &Value) -> Result<(), StatusReason> {
    let Some(event_object) = event.as_object() else {
        return Err(StatusReason::ProtocolMalformed);
    };
    let kind = event_object
        .get("type")
        .or_else(|| event.get("event_type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind.is_empty() {
        return Err(StatusReason::ProtocolMalformed);
    }
    if is_lifecycle_event_kind(kind)
        && parse_time(
            event_object
                .get("ts")
                .or_else(|| event.get("timestamp"))
                .unwrap_or(&Value::Null),
        ) == 0
    {
        return Err(StatusReason::ProtocolMalformed);
    }
    match kind {
        "permission_requested"
        | "permission_resolved"
        | "tool_started"
        | "tool_call_started"
        | "tool_completed"
        | "tool_call_completed"
            if event_tool_name(event).is_none() =>
        {
            Err(StatusReason::ProtocolMalformed)
        }
        "phase_changed"
            if !matches!(
                event_object.get("phase").and_then(Value::as_str),
                Some(
                    "waiting_for_model"
                        | "streaming_text"
                        | "streaming_reasoning"
                        | "sampling"
                        | "tool_execution"
                        | "permission_prompt"
                        | "idle"
                )
            ) =>
        {
            Err(StatusReason::ProtocolUnknown)
        }
        _ => Ok(()),
    }
}

fn is_lifecycle_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "turn_started"
            | "turn_ended"
            | "phase_changed"
            | "tool_started"
            | "tool_call_started"
            | "tool_completed"
            | "tool_call_completed"
            | "permission_requested"
            | "permission_resolved"
            | "first_token"
            | "loop_started"
    )
}

#[derive(Clone, Default)]
struct EventState {
    pending_permissions: HashMap<String, VecDeque<u64>>,
    pending_tools: HashMap<String, VecDeque<u64>>,
    active_turn_started_at: u64,
    active_turn_last_activity_at: u64,
    phase: EventPhase,
    phase_since: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum EventPhase {
    #[default]
    None,
    Thinking,
    Executing,
    Waiting,
}

impl EventState {
    fn apply(&mut self, event: &Value) {
        let timestamp = parse_time(
            event
                .get("ts")
                .or_else(|| event.get("timestamp"))
                .unwrap_or(&Value::Null),
        );
        match event
            .get("type")
            .or_else(|| event.get("event_type"))
            .and_then(Value::as_str)
            .unwrap_or("")
        {
            "turn_started" => {
                self.pending_permissions.clear();
                self.pending_tools.clear();
                self.active_turn_started_at = timestamp;
                self.active_turn_last_activity_at = timestamp;
                self.phase = EventPhase::Thinking;
                self.phase_since = timestamp;
            }
            "turn_ended" => {
                self.pending_permissions.clear();
                self.pending_tools.clear();
                self.active_turn_started_at = 0;
                self.active_turn_last_activity_at = 0;
                self.phase = EventPhase::None;
                self.phase_since = 0;
            }
            "phase_changed" => {
                self.phase = match event.get("phase").and_then(Value::as_str).unwrap_or("") {
                    "waiting_for_model" | "streaming_text" | "streaming_reasoning" | "sampling" => {
                        EventPhase::Thinking
                    }
                    "tool_execution" => EventPhase::Executing,
                    "permission_prompt" => EventPhase::Waiting,
                    "idle" => EventPhase::None,
                    _ => return,
                };
                if self.phase == EventPhase::None {
                    self.phase_since = 0;
                    self.active_turn_started_at = 0;
                    self.active_turn_last_activity_at = 0;
                } else {
                    self.phase_since = timestamp;
                    self.active_turn_last_activity_at = timestamp;
                    if self.active_turn_started_at == 0 {
                        self.active_turn_started_at = timestamp;
                    }
                }
            }
            "tool_started" | "tool_call_started" => {
                let Some(tool_name) = event_tool_name(event) else {
                    return;
                };
                if !self.pending_tools.contains_key(&tool_name)
                    && self.pending_tools.len() >= MAX_PENDING_PERMISSION_NAMES
                {
                    return;
                }
                let pending = self.pending_tools.entry(tool_name).or_default();
                if pending.len() < MAX_PENDING_PERMISSIONS_PER_NAME {
                    pending.push_back(timestamp);
                }
                self.phase = EventPhase::Executing;
                self.phase_since = timestamp;
                self.active_turn_last_activity_at = timestamp;
            }
            "tool_completed" | "tool_call_completed" => {
                let Some(tool_name) = event_tool_name(event) else {
                    return;
                };
                remove_oldest_named_event(&mut self.pending_tools, &tool_name);
                self.active_turn_last_activity_at = timestamp;
            }
            "permission_requested" => {
                let Some(tool_name) = event_tool_name(event) else {
                    return;
                };
                if timestamp == 0 {
                    return;
                }
                if !self.pending_permissions.contains_key(&tool_name)
                    && self.pending_permissions.len() >= MAX_PENDING_PERMISSION_NAMES
                {
                    return;
                }
                let pending = self.pending_permissions.entry(tool_name).or_default();
                if pending.len() < MAX_PENDING_PERMISSIONS_PER_NAME {
                    pending.push_back(timestamp);
                }
                self.phase = EventPhase::Waiting;
                self.phase_since = timestamp;
                self.active_turn_last_activity_at = timestamp;
            }
            "permission_resolved" => {
                let Some(tool_name) = event_tool_name(event) else {
                    return;
                };
                remove_oldest_named_event(&mut self.pending_permissions, &tool_name);
                self.phase = EventPhase::Executing;
                self.phase_since = timestamp;
                self.active_turn_last_activity_at = timestamp;
            }
            "first_token" | "loop_started" => self.active_turn_last_activity_at = timestamp,
            _ => {}
        }
    }

    fn waiting_since(&self, opened_at: u64) -> Option<u64> {
        let permission = self
            .pending_permissions
            .values()
            .flatten()
            .copied()
            .filter(|timestamp| *timestamp >= opened_at)
            .min();
        let phase = (self.phase == EventPhase::Waiting && self.phase_since >= opened_at)
            .then_some(self.phase_since);
        min_nonzero(permission, phase)
    }

    fn executing_since(&self, opened_at: u64) -> Option<u64> {
        let tool = self
            .pending_tools
            .values()
            .flatten()
            .copied()
            .filter(|timestamp| *timestamp >= opened_at)
            .min();
        let phase = (self.phase == EventPhase::Executing && self.phase_since >= opened_at)
            .then_some(self.phase_since);
        min_nonzero(tool, phase)
    }

    fn thinking_since(&self, opened_at: u64) -> Option<u64> {
        let phase = (self.phase == EventPhase::Thinking && self.phase_since >= opened_at)
            .then_some(self.phase_since);
        let active_turn = observed_since(
            self.active_turn_started_at,
            self.active_turn_last_activity_at,
            opened_at,
        )
        .then_some(if self.active_turn_started_at >= opened_at {
            self.active_turn_started_at
        } else {
            self.active_turn_last_activity_at
        });
        min_nonzero(phase, active_turn)
    }
}

fn remove_oldest_named_event(events: &mut HashMap<String, VecDeque<u64>>, name: &str) {
    let should_remove = events.get_mut(name).is_some_and(|pending| {
        pending.pop_front();
        pending.is_empty()
    });
    if should_remove {
        events.remove(name);
    }
}

fn event_tool_name(event: &Value) -> Option<String> {
    let tool_name = event.get("tool_name")?.as_str()?.trim();
    if tool_name.is_empty() {
        return None;
    }
    Some(
        tool_name
            .chars()
            .take(256)
            .collect::<String>()
            .to_ascii_lowercase(),
    )
}

#[derive(Clone, Default)]
struct UpdateState {
    model: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    turn_count: u32,
    active_turn: bool,
    turn_started_at: u64,
    turn_last_activity_at: u64,
    pending_tools: HashMap<String, PendingTool>,
    background_tasks: HashMap<String, BackgroundTask>,
    last_error: Option<String>,
    fatal_error_since: u64,
    rate_limited_since: u64,
    lifecycle_failure: Option<StatusReason>,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
    compaction_count: u32,
    meta_context_tokens: Option<u64>,
    pending_response_usage: Usage,
    seen_event_ids: HashSet<String>,
    seen_turn_ids: HashSet<String>,
    seen_tool_ids: HashSet<String>,
    subagents: HashMap<String, SubagentState>,
    first_assistant_text: String,
    chat_messages: Vec<ChatMessage>,
    tool_calls: Vec<ToolCall>,
    tool_indices: HashMap<String, usize>,
    file_accesses: Vec<FileAccess>,
}

#[derive(Clone, Copy, Default)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
}

impl Usage {
    fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_create
    }

    fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_create = self.cache_create.saturating_add(other.cache_create);
    }
}

#[derive(Clone)]
struct PendingTool {
    name: String,
    arg: String,
    started_at: u64,
    last_update_at: u64,
    waits_for_user: bool,
}

impl PendingTool {
    fn label(&self) -> String {
        if self.arg.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.name, self.arg)
        }
    }

    fn observed_since(&self, opened_at: u64) -> bool {
        observed_since(self.started_at, self.last_update_at, opened_at)
    }

    fn current_since(&self, opened_at: u64) -> u64 {
        if self.started_at >= opened_at {
            self.started_at
        } else if self.last_update_at >= opened_at {
            self.last_update_at
        } else {
            0
        }
    }
}

#[derive(Clone)]
struct BackgroundTask {
    label: String,
    started_at: u64,
    last_update_at: u64,
}

impl BackgroundTask {
    fn observed_since(&self, opened_at: u64) -> bool {
        observed_since(self.started_at, self.last_update_at, opened_at)
    }

    fn current_since(&self, opened_at: u64) -> u64 {
        if self.started_at >= opened_at {
            self.started_at
        } else if self.last_update_at >= opened_at {
            self.last_update_at
        } else {
            0
        }
    }
}

#[derive(Clone, Default)]
struct SubagentState {
    name: String,
    status: String,
    tokens: u64,
    started_at: u64,
    last_update_at: u64,
}

impl SubagentState {
    fn to_model(&self) -> SubAgent {
        SubAgent {
            name: self.name.clone(),
            status: self.status.clone(),
            tokens: self.tokens,
        }
    }

    fn is_running_since(&self, opened_at: u64) -> bool {
        matches!(
            self.status.to_ascii_lowercase().as_str(),
            "working" | "running" | "in_progress" | "pending" | "starting"
        ) && observed_since(self.started_at, self.last_update_at, opened_at)
    }

    fn current_since(&self, opened_at: u64) -> u64 {
        if self.started_at >= opened_at {
            self.started_at
        } else if self.last_update_at >= opened_at {
            self.last_update_at
        } else {
            0
        }
    }

    fn label(&self) -> String {
        if self.name.is_empty() {
            "Subagent".to_string()
        } else {
            format!("Subagent {}", self.name)
        }
    }
}

impl UpdateState {
    fn waiting_since(&self, opened_at: u64) -> Option<u64> {
        self.pending_tools
            .values()
            .filter(|tool| tool.waits_for_user)
            .map(|tool| tool.current_since(opened_at))
            .filter(|timestamp| *timestamp > 0)
            .min()
    }

    fn executing_since(&self, opened_at: u64) -> Option<u64> {
        let tool = self
            .pending_tools
            .values()
            .filter(|tool| !tool.waits_for_user && tool.observed_since(opened_at))
            .map(|tool| tool.current_since(opened_at))
            .filter(|timestamp| *timestamp > 0)
            .min();
        let task = self
            .background_tasks
            .values()
            .filter(|task| task.observed_since(opened_at))
            .map(|task| task.current_since(opened_at))
            .filter(|timestamp| *timestamp > 0)
            .min();
        let subagent = self
            .subagents
            .values()
            .filter(|subagent| subagent.is_running_since(opened_at))
            .map(|subagent| subagent.current_since(opened_at))
            .filter(|timestamp| *timestamp > 0)
            .min();
        min_nonzero(min_nonzero(tool, task), subagent)
    }

    fn thinking_since(&self, opened_at: u64) -> Option<u64> {
        (self.active_turn
            && observed_since(self.turn_started_at, self.turn_last_activity_at, opened_at))
        .then_some(if self.turn_started_at >= opened_at {
            self.turn_started_at
        } else {
            self.turn_last_activity_at
        })
    }

    fn current_work_labels(&self, opened_at: u64) -> Vec<String> {
        let mut labels = Vec::new();
        labels.extend(
            self.pending_tools
                .values()
                .filter(|tool| !tool.waits_for_user && tool.observed_since(opened_at))
                .map(|tool| (tool.current_since(opened_at), tool.label())),
        );
        labels.extend(
            self.background_tasks
                .values()
                .filter(|task| task.observed_since(opened_at))
                .map(|task| (task.current_since(opened_at), task.label.clone())),
        );
        labels.extend(
            self.subagents
                .values()
                .filter(|subagent| subagent.is_running_since(opened_at))
                .map(|subagent| (subagent.current_since(opened_at), subagent.label())),
        );
        labels.sort_by_key(|(timestamp, label)| (*timestamp, label.clone()));
        labels.into_iter().map(|(_, label)| label).collect()
    }

    fn apply(&mut self, record: &Value) {
        let params = record.get("params").unwrap_or(record);
        if let Some(total) = params
            .get("_meta")
            .and_then(|meta| meta.get("totalTokens"))
            .and_then(Value::as_u64)
        {
            self.meta_context_tokens = Some(total);
        }
        if let Some(event_id) = params
            .get("_meta")
            .and_then(|meta| meta.get("eventId"))
            .and_then(Value::as_str)
        {
            if !self.seen_event_ids.insert(event_id.to_string()) {
                return;
            }
        }
        let update = params.get("update").unwrap_or(params);
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("type"))
            .or_else(|| update.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let timestamp = event_time_ms(record, params);

        match kind {
            "user_message_chunk" => {
                self.last_error = None;
                self.mark_turn_activity(timestamp);
                push_chat(
                    &mut self.chat_messages,
                    ChatRole::User,
                    clean_text(&event_text(update), 500),
                );
            }
            "agent_message_chunk" => {
                self.mark_turn_activity(timestamp);
                let text = clean_text(&event_text(update), 500);
                if self.first_assistant_text.is_empty() {
                    self.first_assistant_text = text.clone();
                }
                push_chat(&mut self.chat_messages, ChatRole::Assistant, text);
            }
            "agent_thought_chunk" => self.mark_turn_activity(timestamp),
            "tool_call" => self.apply_tool_call(update, timestamp),
            "tool_call_update" => self.apply_tool_update(update, timestamp),
            "response_started" => {
                self.last_error = None;
                self.mark_turn_activity(timestamp);
                if let Some(model) = update.get("model").and_then(Value::as_str) {
                    self.model = clean_text(model, 120);
                }
            }
            "response_completed" => {
                self.mark_turn_activity(timestamp);
                self.pending_response_usage
                    .add(parse_response_usage(update.get("usage").unwrap_or(update)));
                // Older Grok builds included the model on this event.
                if let Some(model) = update.get("model").and_then(Value::as_str) {
                    self.model = clean_text(model, 120);
                }
            }
            "model_changed" => {
                if let Some(model) = update
                    .get("model_id")
                    .or_else(|| update.get("modelId"))
                    .and_then(Value::as_str)
                {
                    self.model = clean_text(model, 120);
                }
            }
            "turn_completed" => self.apply_turn_completed(update, timestamp),
            "rewind_marker" => self.reset_logical_branch(
                u32::try_from(u64_field(
                    update,
                    &["target_prompt_index", "targetPromptIndex"],
                ))
                .unwrap_or(u32::MAX),
            ),
            "auto_compact_completed" | "context_compacted" => {
                self.compaction_count = self.compaction_count.saturating_add(1);
                let tokens_after = u64_field(update, &["tokens_after", "tokensAfter"]);
                if tokens_after > 0 {
                    self.context_history.push(tokens_after);
                    trim_history(&mut self.context_history);
                }
            }
            "subagent_spawned" | "subagent_progress" | "subagent_finished" => {
                self.apply_subagent(kind, update, timestamp)
            }
            "task_backgrounded" => self.apply_task_backgrounded(update, timestamp),
            "task_completed" => self.apply_task_completed(update),
            "monitor_event" => self.apply_task_progress(update, timestamp),
            "retry_state" | "auto_compact_failed" | "auto_recovery_exhausted" => {
                let retry_kind = string_field(update, &["type", "status"]);
                if matches!(retry_kind.as_str(), "failed" | "exhausted")
                    || matches!(kind, "auto_compact_failed" | "auto_recovery_exhausted")
                {
                    self.last_error = Some(clean_text(
                        update
                            .get("error")
                            .and_then(value_text)
                            .or_else(|| update.get("message").and_then(value_text))
                            .or_else(|| update.get("reason").and_then(value_text))
                            .unwrap_or("Grok turn failed"),
                        160,
                    ));
                    let exhausted_rate_limit = kind == "retry_state"
                        && retry_kind == "exhausted"
                        && bool_field(update, &["isRateLimited", "is_rate_limited"]);
                    if exhausted_rate_limit {
                        self.rate_limited_since = timestamp;
                        self.fatal_error_since = 0;
                    } else if kind == "auto_recovery_exhausted"
                        || (kind == "retry_state"
                            && matches!(retry_kind.as_str(), "failed" | "exhausted"))
                    {
                        self.fatal_error_since = timestamp;
                        self.rate_limited_since = 0;
                    }
                }
            }
            _ => {}
        }
    }

    fn mark_turn_activity(&mut self, timestamp: u64) {
        self.last_error = None;
        self.fatal_error_since = 0;
        self.rate_limited_since = 0;
        self.active_turn = true;
        if self.turn_started_at == 0 {
            self.turn_started_at = timestamp;
        }
        self.turn_last_activity_at = timestamp;
    }

    fn apply_tool_call(&mut self, update: &Value, timestamp: u64) {
        let id = string_field(update, &["toolCallId", "tool_call_id", "id"]);
        if id.is_empty() {
            return;
        }
        self.seen_tool_ids.insert(id.clone());
        self.mark_turn_activity(timestamp);
        let (kind, identifier) = tool_identity(update);
        let name = safe_tool_display_name(&kind, &identifier);
        let arg = safe_tool_location(update);
        let waits_for_user = tool_waits_for_user(update, &kind, &identifier);
        let status = string_field(update, &["status", "state"]);
        let terminal = matches!(status.as_str(), "completed" | "failed" | "cancelled");
        if terminal {
            if let Some(pending) = self.pending_tools.remove(&id) {
                if let Some(&index) = self.tool_indices.get(&id) {
                    if let Some(tool) = self.tool_calls.get_mut(index) {
                        tool.duration_ms = timestamp.saturating_sub(pending.started_at);
                    }
                }
            }
        } else {
            self.pending_tools.insert(
                id.clone(),
                PendingTool {
                    name: name.clone(),
                    arg: arg.clone(),
                    started_at: timestamp,
                    last_update_at: timestamp,
                    waits_for_user,
                },
            );
        }
        if !self.tool_indices.contains_key(&id) && self.tool_calls.len() < MAX_TOOL_CALLS {
            self.tool_indices.insert(id, self.tool_calls.len());
            self.tool_calls.push(ToolCall {
                name: name.clone(),
                arg: arg.clone(),
                duration_ms: 0,
            });
        }
        if !arg.is_empty() && self.file_accesses.len() < MAX_FILE_ACCESSES {
            if let Some(operation) = file_op(&kind) {
                self.file_accesses.push(FileAccess {
                    path: arg,
                    operation,
                    turn_index: self.turn_count,
                });
            }
        }
        if status == "failed" {
            self.last_error = Some("Grok tool failed".to_string());
        }
    }

    fn apply_tool_update(&mut self, update: &Value, timestamp: u64) {
        let id = string_field(update, &["toolCallId", "tool_call_id", "id"]);
        if !self.seen_tool_ids.contains(&id) {
            // Grok registers every tool with a canonical ToolCall before
            // emitting any ToolCallUpdate. Without that opener we cannot
            // reconstruct either an active or terminal lifecycle exactly.
            self.lifecycle_failure = Some(StatusReason::ProtocolMalformed);
            return;
        }
        let status = string_field(update, &["status", "state"]);
        if !matches!(status.as_str(), "completed" | "failed" | "cancelled") {
            // Background processes keep publishing output updates under the
            // original tool-call ID after that foreground call has completed.
            // Those records may enrich an exact still-open call, but they must
            // not reopen a model turn once its terminal lifecycle was observed.
            if !self.pending_tools.contains_key(&id) {
                if let Some(&index) = self.tool_indices.get(&id) {
                    // A known completed call can still receive delayed display
                    // metadata. Preserve it without changing lifecycle state.
                    let (kind, identifier) = tool_identity(update);
                    let arg = safe_tool_location(update);
                    if let Some(tool) = self.tool_calls.get_mut(index) {
                        if !kind.is_empty() || !identifier.is_empty() {
                            tool.name = safe_tool_display_name(&kind, &identifier);
                        }
                        if !arg.is_empty() {
                            tool.arg = arg;
                        }
                    }
                }
                return;
            }
            self.mark_turn_activity(timestamp);
            let (kind, identifier) = tool_identity(update);
            let waits_for_user = tool_waits_for_user(update, &kind, &identifier);
            let arg = safe_tool_location(update);
            let enriched = self.pending_tools.get_mut(&id).map(|pending| {
                if !kind.is_empty() || !identifier.is_empty() {
                    pending.name = safe_tool_display_name(&kind, &identifier);
                }
                if !arg.is_empty() {
                    pending.arg = arg;
                }
                pending.last_update_at = timestamp;
                pending.waits_for_user |= waits_for_user;
                (pending.name.clone(), pending.arg.clone())
            });
            if let Some((name, arg)) = enriched {
                if let Some(&index) = self.tool_indices.get(&id) {
                    if let Some(tool) = self.tool_calls.get_mut(index) {
                        tool.name = name;
                        tool.arg = arg;
                    }
                }
            }
            return;
        }
        if let Some(pending) = self.pending_tools.remove(&id) {
            if let Some(&index) = self.tool_indices.get(&id) {
                if let Some(tool) = self.tool_calls.get_mut(index) {
                    tool.duration_ms = timestamp.saturating_sub(pending.started_at);
                }
            }
        }
        if status == "failed" {
            self.last_error = Some(clean_text(
                update
                    .get("error")
                    .and_then(value_text)
                    .unwrap_or("Grok tool failed"),
                160,
            ));
        }
    }

    fn apply_task_backgrounded(&mut self, update: &Value, timestamp: u64) {
        self.fatal_error_since = 0;
        self.rate_limited_since = 0;
        let id = id_field(update, &["task_id", "taskId", "id"]);
        if id.is_empty() {
            return;
        }
        let monitor = string_field(update, &["monitor_description", "monitorDescription"]);
        let description = string_field(update, &["description"]);
        let label = if !monitor.trim().is_empty() {
            format!("Monitor {}", clean_text(&monitor, 120))
        } else if !description.trim().is_empty() {
            format!("Background {}", clean_text(&description, 120))
        } else {
            "Background task".to_string()
        };
        self.background_tasks.insert(
            id,
            BackgroundTask {
                label,
                started_at: timestamp,
                last_update_at: timestamp,
            },
        );
    }

    fn apply_task_progress(&mut self, update: &Value, timestamp: u64) {
        let id = id_field(update, &["task_id", "taskId", "id"]);
        if let Some(task) = self.background_tasks.get_mut(&id) {
            task.last_update_at = timestamp;
        }
    }

    fn apply_task_completed(&mut self, update: &Value) {
        let snapshot = update
            .get("task_snapshot")
            .or_else(|| update.get("taskSnapshot"))
            .unwrap_or(update);
        let id = id_field(snapshot, &["task_id", "taskId", "id"]);
        if id.is_empty() {
            return;
        }
        self.background_tasks.remove(&id);
        let exit_code = snapshot
            .get("exit_code")
            .or_else(|| snapshot.get("exitCode"))
            .and_then(Value::as_i64);
        if let Some(exit_code) = exit_code.filter(|code| *code != 0) {
            self.last_error = Some(format!("Background task failed (exit {exit_code})"));
        }
    }

    fn apply_turn_completed(&mut self, update: &Value, timestamp: u64) {
        let turn_id = string_field(
            update,
            &["prompt_id", "turn_id", "promptId", "turnId", "id"],
        );
        if !turn_id.is_empty() && !self.seen_turn_ids.insert(turn_id) {
            self.pending_response_usage = Usage::default();
            return;
        }
        let usage_value = update.get("usage").unwrap_or(&Value::Null);
        let mut usage = parse_turn_usage(usage_value);
        if usage.total() == 0 {
            usage = self.pending_response_usage;
        }
        self.pending_response_usage = Usage::default();
        self.total_input = self.total_input.saturating_add(usage.input);
        self.total_output = self.total_output.saturating_add(usage.output);
        self.total_cache_read = self.total_cache_read.saturating_add(usage.cache_read);
        self.total_cache_create = self.total_cache_create.saturating_add(usage.cache_create);
        self.token_history.push(usage.total());
        trim_history(&mut self.token_history);
        let context = inclusive_input_tokens(usage_value);
        if context > 0 {
            self.context_history.push(context);
            trim_history(&mut self.context_history);
        }
        self.turn_count = self.turn_count.saturating_add(1);
        self.active_turn = false;
        self.turn_started_at = 0;
        self.turn_last_activity_at = 0;
        self.pending_tools.clear();
        let stop_reason = string_field(update, &["stop_reason", "stopReason", "reason"]);
        self.last_error = None;
        self.fatal_error_since = 0;
        self.rate_limited_since = 0;
        if stop_reason == "error" || stop_reason == "failed" {
            self.last_error = Some(clean_text(
                update
                    .get("error")
                    .and_then(value_text)
                    .unwrap_or("Grok turn failed"),
                160,
            ));
            self.fatal_error_since = timestamp;
        } else if stop_reason == "rate_limit" || stop_reason == "rate_limited" {
            self.rate_limited_since = timestamp;
        }
    }

    fn apply_subagent(&mut self, kind: &str, update: &Value, timestamp: u64) {
        let id = string_field(
            update,
            &[
                "subagent_id",
                "child_session_id",
                "sessionId",
                "childSessionId",
                "subagentId",
                "id",
            ],
        );
        if id.is_empty() {
            return;
        }
        let incoming_name = string_field(
            update,
            &[
                "description",
                "subagent_type",
                "agent_type",
                "agentType",
                "name",
            ],
        );
        let status = match kind {
            "subagent_spawned" => "working".to_string(),
            "subagent_finished" => {
                let terminal = string_field(update, &["status", "result"]);
                if terminal.is_empty() {
                    "completed".to_string()
                } else {
                    terminal
                }
            }
            _ => string_field(update, &["status"]),
        };
        let tokens = u64_field(update, &["tokens_used", "tokens", "totalTokens"]);
        let previous = self.subagents.get(&id).cloned().unwrap_or_default();
        self.subagents.insert(
            id.clone(),
            SubagentState {
                name: if incoming_name.is_empty() {
                    if previous.name.is_empty() {
                        clean_text(&id, 80)
                    } else {
                        previous.name.clone()
                    }
                } else {
                    clean_text(&incoming_name, 80)
                },
                status: clean_text(
                    if status.is_empty() {
                        if previous.status.is_empty() {
                            "working"
                        } else {
                            &previous.status
                        }
                    } else {
                        &status
                    },
                    40,
                ),
                tokens: previous.tokens.max(tokens),
                started_at: if previous.started_at == 0 {
                    timestamp
                } else {
                    previous.started_at
                },
                last_update_at: timestamp,
            },
        );
        if kind == "subagent_finished" && matches!(status.as_str(), "failed" | "error") {
            self.last_error = Some(clean_text(
                update
                    .get("error")
                    .and_then(value_text)
                    .unwrap_or("Grok subagent failed"),
                160,
            ));
        }
    }

    fn reset_logical_branch(&mut self, target_prompt_index: u32) {
        // Rewinding changes the logical conversation, but it does not refund
        // tokens already spent. Keep lifetime usage and history intact while
        // clearing transient state from the abandoned branch.
        self.turn_count = self.turn_count.min(target_prompt_index);
        self.active_turn = false;
        self.turn_started_at = 0;
        self.turn_last_activity_at = 0;
        self.pending_tools.clear();
        self.background_tasks.clear();
        self.pending_response_usage = Usage::default();
        self.chat_messages.clear();
        self.tool_calls.clear();
        self.tool_indices.clear();
        self.file_accesses
            .retain(|access| access.turn_index < target_prompt_index);
        self.subagents.clear();
        self.last_error = None;
        self.fatal_error_since = 0;
        self.rate_limited_since = 0;
        if target_prompt_index == 0 {
            self.first_assistant_text.clear();
        }
    }
}

fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".grok"));
    }
    if let Some(root) = std::env::var_os("GROK_HOME").map(PathBuf::from) {
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .join(root)
        };
        roots.push(root);
    }
    roots.sort();
    roots.dedup();
    roots
}

fn process_grok_root(pid: u32) -> Option<PathBuf> {
    let configured = process::read_process_env_var(pid, "GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("GROK_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })?;
    if configured.is_absolute() {
        Some(configured)
    } else {
        process::get_process_cwd(pid).map(|cwd| PathBuf::from(cwd).join(configured))
    }
}

fn read_leader_pids(root: &Path) -> HashSet<u32> {
    let Ok(entries) = fs::read_dir(root) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .take(1_000)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with("leader") || !name.ends_with(".lock") || is_symlink(&path) {
                return None;
            }
            let meta = fs::metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > MAX_LEADER_LOCK_BYTES {
                return None;
            }
            fs::read_to_string(path).ok()?.trim().parse().ok()
        })
        .filter(|pid| *pid > 0)
        .collect()
}

pub(crate) fn is_grok_leader_pid(pid: u32) -> bool {
    let mut roots = default_roots();
    if let Some(root) = process_grok_root(pid) {
        roots.push(root);
    }
    roots.sort();
    roots.dedup();
    roots
        .iter()
        .any(|root| read_leader_pids(root).contains(&pid))
}

fn read_active_registry(root: &Path) -> Vec<RegistryEntry> {
    let path = root.join("active_sessions.json");
    if is_symlink(&path) {
        return Vec::new();
    }
    let Ok(meta) = fs::metadata(&path) else {
        return Vec::new();
    };
    if !meta.is_file() || meta.len() > MAX_REGISTRY_BYTES {
        return Vec::new();
    }
    let Ok(value) = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .ok_or(())
    else {
        return Vec::new();
    };
    value
        .as_array()
        .into_iter()
        .flat_map(|entries| entries.iter())
        .filter_map(|entry| {
            let session_id = entry.get("session_id")?.as_str()?;
            let pid = u32::try_from(entry.get("pid")?.as_u64()?).ok()?;
            if session_id.is_empty() || session_id.len() > 256 || pid == 0 {
                return None;
            }
            Some(RegistryEntry {
                session_id: session_id.to_string(),
                pid,
                cwd: entry
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                opened_at: parse_time(entry.get("opened_at").unwrap_or(&Value::Null)),
            })
        })
        .collect()
}

fn read_summary(path: &Path, registry: &RegistryEntry) -> Option<SessionMeta> {
    let value = read_json_file(path, MAX_JSON_BYTES)?;
    let info = value.get("info").unwrap_or(&Value::Null);
    let id = info
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(&registry.session_id);
    if id != registry.session_id {
        return None;
    }
    let cwd = info
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or(&registry.cwd);
    if cwd.is_empty() {
        return None;
    }
    let title = value
        .get("generated_title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .or_else(|| value.get("session_summary").and_then(Value::as_str))
        .unwrap_or("");
    let created_at = parse_time(value.get("created_at").unwrap_or(&Value::Null));
    let last_active_at = parse_time(value.get("last_active_at").unwrap_or(&Value::Null));
    let updated_at = if last_active_at > 0 {
        last_active_at
    } else {
        parse_time(value.get("updated_at").unwrap_or(&Value::Null))
    }
    .max(created_at);
    Some(SessionMeta {
        id: id.to_string(),
        cwd: cwd.to_string(),
        title: clean_text(title, 120),
        model: clean_text(
            value
                .get("current_model_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
            120,
        ),
        effort: clean_text(
            value
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .unwrap_or(""),
            40,
        ),
        session_kind: value
            .get("session_kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        hidden: value.get("hidden").and_then(Value::as_bool),
        created_at,
        updated_at,
    })
}

fn read_signals(path: &Path) -> Signals {
    let Some(value) = read_json_file(path, MAX_JSON_BYTES) else {
        return Signals::default();
    };
    Signals {
        context_tokens: optional_u64_field(&value, &["contextTokensUsed", "context_tokens_used"]),
        context_window: optional_u64_field(
            &value,
            &["contextWindowTokens", "context_window_tokens"],
        ),
        context_percent: optional_f64_field(
            &value,
            &["contextWindowUsage", "context_window_usage"],
        ),
        turn_count: optional_u64_field(&value, &["turnCount", "turn_count"])
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX)),
        compaction_count: optional_u64_field(&value, &["compactionCount", "compaction_count"])
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX)),
    }
}

fn read_awaiting_plan_approval(path: &Path) -> (bool, EventAvailability) {
    if is_symlink(path) {
        return (
            false,
            EventAvailability::Failed(StatusReason::ProtocolMalformed),
        );
    }
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (false, EventAvailability::Missing)
        }
        Err(_) => return (false, EventAvailability::Failed(StatusReason::Unavailable)),
    };
    if !meta.is_file() || meta.len() > MAX_JSON_BYTES {
        return (
            false,
            EventAvailability::Failed(StatusReason::ProtocolMalformed),
        );
    }
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return (false, EventAvailability::Failed(StatusReason::Unavailable)),
    };
    let mut bytes = Vec::new();
    if file
        .by_ref()
        .take(MAX_JSON_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return (false, EventAvailability::Failed(StatusReason::Unavailable));
    }
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return (
            false,
            EventAvailability::Failed(StatusReason::ProtocolMalformed),
        );
    }
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(_) => {
            return (
                false,
                EventAvailability::Failed(StatusReason::ProtocolMalformed),
            )
        }
    };
    match value.get("awaiting_plan_approval").and_then(Value::as_bool) {
        Some(awaiting) => (awaiting, EventAvailability::Available),
        None => (
            false,
            EventAvailability::Failed(StatusReason::ProtocolMalformed),
        ),
    }
}

fn read_json_file(path: &Path, max_bytes: u64) -> Option<Value> {
    if is_symlink(path) {
        return None;
    }
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > max_bytes {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn valid_session_dir(root: &Path, path: &Path, session_id: &str) -> bool {
    let sessions_root = root.join("sessions");
    path.is_dir()
        && path.starts_with(&sessions_root)
        && path.file_name().is_some_and(|name| name == session_id)
        && !has_symlink_component(&sessions_root, path)
}

pub(crate) fn is_grok_process(command: &str) -> bool {
    let tokens = process::command_tokens(command);
    is_grok_process_tokens(&tokens)
}

pub(crate) fn is_grok_process_tokens(tokens: &[String]) -> bool {
    let recognized = tokens.first().is_some_and(|token| {
        process::token_has_binary(token, "grok")
            || process::token_has_binary(token, "xai-grok-pager")
            || process::token_has_binary(token, "agent")
            || is_versioned_grok_binary(token)
    });
    recognized && !is_grok_leader_command(tokens)
}

fn grok_process_observation_is_exact(
    expected_incarnation: &str,
    current_incarnation: Option<&str>,
    tokens: &[String],
) -> bool {
    current_incarnation == Some(expected_incarnation) && is_grok_process_tokens(tokens)
}

fn is_grok_leader_command(tokens: &[String]) -> bool {
    let Some(agent_index) = grok_agent_command_index(tokens) else {
        return false;
    };
    grok_agent_subcommand(tokens, agent_index + 1)
        .is_some_and(|command| command.eq_ignore_ascii_case("leader"))
}

/// Return the index of the root `agent` command. Only the first positional can
/// select a command: once ordinary prompt text is seen, later words such as
/// `agent leader` belong to that prompt and must not hide a user session.
fn grok_agent_command_index(tokens: &[String]) -> Option<usize> {
    let mut index = 1;
    while let Some(token) = tokens.get(index) {
        if token == "--" {
            return None;
        }

        let option = option_name(token);
        if is_grok_prompt_option(option) {
            // These options consume opaque user input. Process listings on
            // some platforms flatten an argv value containing spaces, so no
            // later word can safely be interpreted as a host command.
            return None;
        }
        if is_inline_option(token) || is_grok_root_flag(option) {
            index += 1;
            continue;
        }
        if is_grok_root_value_option(option) {
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            // Unknown options will be rejected by Grok. Treat them as flags so
            // an arbitrary following value does not gain executable identity.
            index += 1;
            continue;
        }

        return token.eq_ignore_ascii_case("agent").then_some(index);
    }
    None
}

fn grok_agent_subcommand(tokens: &[String], mut index: usize) -> Option<&str> {
    while let Some(token) = tokens.get(index) {
        if token == "--" {
            return tokens.get(index + 1).map(String::as_str);
        }
        let option = option_name(token);
        if is_inline_option(token) || is_grok_agent_flag(option) {
            index += 1;
            continue;
        }
        if is_grok_agent_value_option(option) {
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(token);
    }
    None
}

fn option_name(value: &str) -> &str {
    value.split_once('=').map_or(value, |(name, _)| name)
}

fn is_inline_option(value: &str) -> bool {
    value.starts_with('-') && value.contains('=')
}

fn is_grok_prompt_option(value: &str) -> bool {
    matches!(
        value,
        "-p" | "--single"
            | "--prompt-file"
            | "--prompt-json"
            | "--json-schema"
            | "--system-prompt"
            | "--system-prompt-override"
    )
}

fn is_grok_root_value_option(value: &str) -> bool {
    matches!(
        value,
        "--agent"
            | "--agents"
            | "--allow"
            | "--allowedTools"
            | "--cwd"
            | "--debug-file"
            | "--deny"
            | "--disallowed-tools"
            | "--disallowedTools"
            | "--leader-socket"
            | "-m"
            | "--model"
            | "--max-turns"
            | "--output-format"
            | "--permission-mode"
            | "-r"
            | "--resume"
            | "--reasoning-effort"
            | "--effort"
            | "--rules"
            | "-s"
            | "--session-id"
            | "--sandbox"
            | "--tools"
            | "-w"
            | "--worktree"
            | "--worktree-ref"
            | "--ref"
    )
}

fn is_grok_root_flag(value: &str) -> bool {
    matches!(
        value,
        "--always-approve"
            | "-c"
            | "--continue"
            | "--debug"
            | "--disable-web-search"
            | "--experimental-memory"
            | "--fork-session"
            | "--fullscreen"
            | "-h"
            | "--help"
            | "--include-partial-messages"
            | "--minimal"
            | "--no-alt-screen"
            | "--no-memory"
            | "--no-plan"
            | "--no-subagents"
            | "--oauth"
            | "--restore-code"
            | "-v"
            | "--version"
            | "--verbatim"
    )
}

fn is_grok_agent_value_option(value: &str) -> bool {
    matches!(
        value,
        "-m" | "--model"
            | "--reasoning-effort"
            | "--effort"
            | "--agent-profile"
            | "--plugin-dir"
            | "--grok-ws-origin"
            | "--grok-ws-url"
            | "--cli-chat-proxy-base-url"
            | "--xai-api-base-url"
            | "--debug-file"
            | "--leader-socket"
    )
}

fn is_grok_agent_flag(value: &str) -> bool {
    matches!(
        value,
        "--reauth"
            | "----reauthenticate"
            | "--always-approve"
            | "--leader"
            | "--no-leader"
            | "--debug"
            | "-h"
            | "--help"
    )
}

fn is_hidden_grok_session(meta: &SessionMeta) -> bool {
    meta.hidden.unwrap_or(false) || is_subagent_session_kind(&meta.session_kind)
}

fn is_subagent_session_kind(kind: &str) -> bool {
    kind.trim_start()
        .get(.."subagent".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("subagent"))
}

fn is_versioned_grok_binary(token: &str) -> bool {
    let normalized = token.trim_matches(['\'', '"']).replace('\\', "/");
    let base = normalized
        .rsplit('/')
        .next()
        .unwrap_or(&normalized)
        .to_ascii_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    let Some(suffix) = base.strip_prefix("grok-") else {
        return false;
    };
    let mut parts = suffix.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    let platform = if valid_grok_version(first) {
        parts.next()
    } else {
        Some(first)
    };
    match platform {
        None => true,
        Some("macos" | "linux" | "windows") => {
            parts
                .next()
                .is_some_and(|arch| matches!(arch, "aarch64" | "arm64" | "x64" | "x86_64"))
                && parts.next().is_none()
        }
        _ => false,
    }
}

fn valid_grok_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|component| !component.is_empty() && component.chars().all(|c| c.is_ascii_digit()))
}

fn parse_turn_usage(value: &Value) -> Usage {
    let inclusive_input = inclusive_input_tokens(value);
    let cache_read = u64_field(
        value,
        &[
            "cachedReadTokens",
            "cache_read_input_tokens",
            "cached_read_tokens",
        ],
    );
    let cache_create = u64_field(
        value,
        &[
            "cacheCreationTokens",
            "cache_creation_input_tokens",
            "cache_create_tokens",
        ],
    );
    Usage {
        input: inclusive_input.saturating_sub(cache_read.saturating_add(cache_create)),
        output: u64_field(value, &["outputTokens", "output_tokens"]),
        cache_read,
        cache_create,
    }
}

fn parse_response_usage(value: &Value) -> Usage {
    Usage {
        input: u64_field(value, &["input_tokens", "inputTokens"]),
        output: u64_field(value, &["output_tokens", "outputTokens"]),
        cache_read: u64_field(value, &["cache_read_input_tokens", "cachedReadTokens"]),
        cache_create: u64_field(
            value,
            &["cache_creation_input_tokens", "cacheCreationTokens"],
        ),
    }
}

fn inclusive_input_tokens(value: &Value) -> u64 {
    u64_field(value, &["inputTokens", "input_tokens"])
}

fn event_time_ms(record: &Value, params: &Value) -> u64 {
    let from_meta = params
        .get("_meta")
        .and_then(|meta| meta.get("agentTimestampMs"))
        .and_then(Value::as_u64);
    from_meta.unwrap_or_else(|| {
        let value = record.get("timestamp").unwrap_or(&Value::Null);
        parse_time(value)
    })
}

fn event_text(value: &Value) -> String {
    value
        .get("content")
        .or_else(|| value.get("text"))
        .and_then(value_text)
        .unwrap_or("")
        .to_string()
}

fn tool_identity(value: &Value) -> (String, String) {
    let metadata = value.get("_meta").and_then(|meta| meta.get("x.ai/tool"));
    let mut kind = metadata
        .map(|tool| string_field(tool, &["kind", "toolKind", "tool_kind"]))
        .unwrap_or_default();
    if kind.is_empty() {
        kind = string_field(value, &["kind", "toolKind", "tool_kind"]);
    }
    let mut identifier = metadata
        .map(|tool| string_field(tool, &["name", "toolName", "tool_name"]))
        .unwrap_or_default();
    if identifier.is_empty() {
        identifier = string_field(value, &["name", "toolName", "tool_name"]);
    }
    (clean_text(&kind, 120), clean_text(&identifier, 120))
}

fn tool_waits_for_user(_value: &Value, kind: &str, identifier: &str) -> bool {
    if kind.eq_ignore_ascii_case("ask_user") || identifier.eq_ignore_ascii_case("ask_user_question")
    {
        return true;
    }
    false
}

fn safe_tool_display_name(kind: &str, identifier: &str) -> String {
    let by_kind = safe_tool_name(kind);
    if by_kind != "Tool" {
        return by_kind;
    }
    match identifier.to_ascii_lowercase().as_str() {
        "ask_user_question" => "Ask User",
        "run_terminal_command" | "run_terminal_cmd" => "Execute",
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" | "apply_patch" => "Edit",
        "list_dir" | "search" => "Search",
        "web_fetch" => "Fetch",
        _ => "Tool",
    }
    .to_string()
}

fn safe_tool_location(value: &Value) -> String {
    let path = value
        .get("locations")
        .and_then(Value::as_array)
        .and_then(|locations| locations.first())
        .and_then(|location| location.get("path"))
        .and_then(Value::as_str)
        .or_else(|| value.get("path").and_then(Value::as_str))
        .unwrap_or("");
    clean_text(path, 160)
}

fn safe_tool_name(kind: &str) -> String {
    match kind {
        "read" => "Read",
        "edit" => "Edit",
        "write" => "Write",
        "move" => "Move",
        "delete" => "Delete",
        "search" => "Search",
        "fetch" => "Fetch",
        "execute" => "Execute",
        "think" => "Think",
        _ => "Tool",
    }
    .to_string()
}

fn file_op(kind: &str) -> Option<FileOp> {
    match kind {
        "read" | "search" | "fetch" => Some(FileOp::Read),
        "edit" | "move" | "delete" => Some(FileOp::Edit),
        "write" => Some(FileOp::Write),
        _ => None,
    }
}

fn collect_children(pid: u32, shared: &SharedProcessData) -> Vec<ChildProcess> {
    let mut out = Vec::new();
    let mut stack = shared.children_map.get(&pid).cloned().unwrap_or_default();
    let mut visited = HashSet::new();
    while let Some(child_pid) = stack.pop() {
        if !visited.insert(child_pid) {
            continue;
        }
        if let Some(info) = shared.process_info.get(&child_pid) {
            out.push(ChildProcess {
                pid: child_pid,
                command: info.command.clone(),
                mem_kb: info.rss_kb,
                port: shared
                    .ports
                    .get(&child_pid)
                    .and_then(|ports| ports.first().copied()),
            });
        }
        if let Some(children) = shared.children_map.get(&child_pid) {
            stack.extend(children);
        }
    }
    out
}

fn string_field(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .to_string()
}

fn bool_field(value: &Value, keys: &[&str]) -> bool {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
        .unwrap_or(false)
}

fn id_field(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            let value = value.get(*key)?;
            value
                .as_str()
                .map(ToString::to_string)
                .or_else(|| value.as_u64().map(|number| number.to_string()))
                .or_else(|| value.as_i64().map(|number| number.to_string()))
        })
        .unwrap_or_default()
}

fn observed_since(started_at: u64, last_activity_at: u64, opened_at: u64) -> bool {
    (started_at > 0 && started_at >= opened_at)
        || (last_activity_at > 0 && last_activity_at >= opened_at)
}

fn min_nonzero(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (
        left.filter(|value| *value > 0),
        right.filter(|value| *value > 0),
    ) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn u64_field(value: &Value, keys: &[&str]) -> u64 {
    optional_u64_field(value, keys).unwrap_or(0)
}

fn optional_u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

fn optional_f64_field(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_f64))
}

fn value_text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("text").and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
}

fn parse_time(value: &Value) -> u64 {
    if let Some(raw) = value.as_u64() {
        return if raw < 10_000_000_000 {
            raw * 1_000
        } else {
            raw
        };
    }
    value
        .as_f64()
        .map(|raw| {
            if raw < 10_000_000_000.0 {
                (raw * 1_000.0) as u64
            } else {
                raw as u64
            }
        })
        .or_else(|| {
            value
                .as_str()
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        })
        .unwrap_or(0)
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn clean_text(text: &str, max_chars: usize) -> String {
    let sanitized = sanitize_terminal_text(text);
    let redacted = redact_secrets(&sanitized);
    redacted
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn push_chat(messages: &mut Vec<ChatMessage>, role: ChatRole, text: String) {
    if text.is_empty() {
        return;
    }
    messages.push(ChatMessage { role, text });
    if messages.len() > MAX_CHAT_MESSAGES {
        messages.remove(0);
    }
}

fn trim_history(history: &mut Vec<u64>) {
    const MAX_HISTORY: usize = 10_000;
    if history.len() > MAX_HISTORY {
        history.drain(..history.len() - MAX_HISTORY);
    }
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn has_symlink_component(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let mut current = root.to_path_buf();
    if is_symlink(&current) {
        return true;
    }
    for component in relative.components() {
        current.push(component);
        if is_symlink(&current) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn recognizes_user_grok_processes_and_excludes_leader_hosts() {
        assert!(is_grok_process("/usr/local/bin/grok"));
        assert!(is_grok_process("/Users/test/.local/bin/agent"));
        assert!(is_grok_process("xai-grok-pager --resume abc"));
        assert!(is_grok_process("~/.grok/bin/grok-1.2.3 -p hi"));
        assert!(is_grok_process("~/.grok/downloads/grok-macos-aarch64"));
        assert!(is_grok_process("GROK-0.2.118-WINDOWS-X64.EXE"));
        assert!(!is_grok_process("grok agent leader"));
        assert!(!is_grok_process("grok --debug agent --model code leader"));
        assert!(!is_grok_process(
            "grok agent --plugin-dir /tmp/company-plugin --debug leader"
        ));
        assert!(is_grok_process("grok -p agent leader"));
        assert!(is_grok_process("grok --single=agent leader"));
        assert!(is_grok_process("grok fix agent leader handling"));
        assert!(is_grok_process("grok \"fix agent leader handling\""));
        assert!(!is_grok_process("cat ~/.grok/bin/grok-1.2.3"));
        assert!(!is_grok_process("node /tmp/grok-1-not-the-cli.js"));
        assert!(!is_grok_process("node server.js"));
    }

    #[test]
    fn exact_registry_binding_rejects_pid_reuse_and_leader_role() {
        let session_tokens = vec![
            "/usr/local/bin/grok".to_string(),
            "--resume".to_string(),
            "session-a".to_string(),
        ];
        let leader_tokens = vec![
            "/usr/local/bin/grok".to_string(),
            "agent".to_string(),
            "leader".to_string(),
        ];

        assert!(grok_process_observation_is_exact(
            "process-a",
            Some("process-a"),
            &session_tokens,
        ));
        assert!(!grok_process_observation_is_exact(
            "process-a",
            Some("process-b"),
            &session_tokens,
        ));
        assert!(!grok_process_observation_is_exact(
            "process-a",
            Some("process-a"),
            &leader_tokens,
        ));
    }

    #[test]
    fn subagent_session_kinds_are_always_hidden() {
        let mut meta = SessionMeta {
            session_kind: "interactive".to_string(),
            hidden: Some(false),
            ..SessionMeta::default()
        };
        assert!(!is_hidden_grok_session(&meta));

        meta.session_kind = "subagent".to_string();
        assert!(is_hidden_grok_session(&meta));
        meta.session_kind = "subagent_worker".to_string();
        assert!(is_hidden_grok_session(&meta));
        meta.session_kind = "SubAgentFork".to_string();
        assert!(is_hidden_grok_session(&meta));

        meta.session_kind = "interactive".to_string();
        meta.hidden = Some(true);
        assert!(is_hidden_grok_session(&meta));
    }

    #[test]
    fn leader_lock_files_identify_host_pids() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("leader.lock"), "42").unwrap();
        fs::write(root.path().join("leader-custom.lock"), "43\n").unwrap();
        fs::write(root.path().join("leader.sock"), "44").unwrap();
        fs::write(root.path().join("not-leader.lock"), "45").unwrap();

        let pids = read_leader_pids(root.path());
        assert_eq!(pids, HashSet::from([42, 43]));
    }

    #[test]
    fn event_state_pairs_permissions_and_clears_at_turn_boundaries() {
        let mut state = EventState::default();
        state.apply(&serde_json::json!({
            "type":"permission_requested",
            "tool_name":"run_terminal_command",
            "ts":"2026-08-01T08:00:00Z"
        }));
        let first = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        assert_eq!(state.waiting_since(first), Some(first));
        assert_eq!(state.waiting_since(first + 1), None);

        state.apply(&serde_json::json!({
            "type":"permission_requested",
            "tool_name":"run_terminal_command",
            "ts":"2026-08-01T08:00:01Z"
        }));
        state.apply(&serde_json::json!({
            "type":"permission_resolved",
            "tool_name":"run_terminal_command",
            "ts":"2026-08-01T08:00:02Z"
        }));
        assert!(state.waiting_since(first + 1).is_some());
        state.apply(&serde_json::json!({
            "type":"permission_resolved",
            "tool_name":"run_terminal_command",
            "ts":"2026-08-01T08:00:03Z"
        }));
        assert_eq!(state.waiting_since(first), None);

        state.apply(&serde_json::json!({
            "type":"permission_requested",
            "tool_name":"edit_file",
            "ts":"2026-08-01T08:00:02Z"
        }));
        state.apply(&serde_json::json!({
            "type":"turn_ended",
            "ts":"2026-08-01T08:00:04Z"
        }));
        assert_eq!(state.waiting_since(first), None);
        state.apply(&serde_json::json!({
            "type":"permission_requested",
            "tool_name":"edit_file",
            "ts":"2026-08-01T08:00:03Z"
        }));
        state.apply(&serde_json::json!({
            "type":"turn_started",
            "ts":"2026-08-01T08:00:05Z"
        }));
        assert_eq!(state.waiting_since(first), None);
    }

    #[test]
    fn event_source_failure_does_not_drop_a_pending_permission_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        fs::write(
            &path,
            "{\"type\":\"permission_requested\",\"tool_name\":\"shell\",\"ts\":\"2026-08-01T08:00:01Z\"}\n",
        )
        .unwrap();
        let mut cache = EventCache::default();
        assert_eq!(cache.refresh(&path), EventAvailability::Available);
        let waiting = grok_status_decision(
            &UpdateState::default(),
            &cache.state,
            EventAvailability::Available,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(waiting.status, SessionStatus::Waiting);
        assert_eq!(waiting.authority, StatusAuthority::Provider);
        assert_eq!(waiting.reason, StatusReason::ProviderWaitingApproval);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{not-json}\n").unwrap();
        file.flush().unwrap();
        let failed = cache.refresh(&path);
        assert_eq!(
            failed,
            EventAvailability::Failed(StatusReason::ProtocolMalformed)
        );
        let unavailable = grok_status_decision(
            &UpdateState::default(),
            &cache.state,
            failed,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(unavailable.status, SessionStatus::Unknown);
        assert_eq!(unavailable.authority, StatusAuthority::Unavailable);
        assert_eq!(unavailable.reason, StatusReason::ProtocolMalformed);

        file.write_all(
            b"{\"type\":\"permission_resolved\",\"tool_name\":\"shell\",\"ts\":\"2026-08-01T08:00:02Z\"}\n",
        )
        .unwrap();
        file.flush().unwrap();
        assert_eq!(
            cache.refresh(&path),
            EventAvailability::Failed(StatusReason::ProtocolMalformed),
            "a skipped malformed lifecycle record permanently prevents exact reconstruction"
        );
        let resolved = grok_status_decision(
            &UpdateState::default(),
            &cache.state,
            EventAvailability::Failed(StatusReason::ProtocolMalformed),
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(resolved.status, SessionStatus::Unknown);
        assert_eq!(resolved.authority, StatusAuthority::Unavailable);
        assert_eq!(resolved.reason, StatusReason::ProtocolMalformed);
    }

    #[test]
    fn invalid_event_timestamps_make_lifecycle_status_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        for (index, invalid_resolution) in [
            "{\"type\":\"permission_resolved\",\"tool_name\":\"shell\"}\n",
            "{\"type\":\"permission_resolved\",\"tool_name\":\"shell\",\"ts\":\"not-a-time\"}\n",
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.path().join(format!("events-{index}.jsonl"));
            fs::write(
                &path,
                "{\"type\":\"permission_requested\",\"tool_name\":\"shell\",\"ts\":\"2026-08-01T08:00:01Z\"}\n",
            )
            .unwrap();
            let mut cache = EventCache::default();
            assert_eq!(cache.refresh(&path), EventAvailability::Available);

            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(invalid_resolution.as_bytes()).unwrap();
            file.flush().unwrap();
            let availability = cache.refresh(&path);
            assert_eq!(
                availability,
                EventAvailability::Failed(StatusReason::ProtocolMalformed)
            );
            assert!(cache.state.waiting_since(opened_at).is_some());

            let decision = grok_status_decision(
                &UpdateState::default(),
                &cache.state,
                availability,
                EventAvailability::Missing,
                opened_at,
                false,
            );
            assert_eq!(decision.status, SessionStatus::Unknown);
            assert_eq!(decision.authority, StatusAuthority::Unavailable);
            assert_eq!(decision.reason, StatusReason::ProtocolMalformed);
        }
    }

    #[test]
    fn missing_optional_events_preserve_provider_idle_but_stale_waits_do_not_leak() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut stale_events = EventState::default();
        stale_events.apply(&serde_json::json!({
            "type":"permission_requested",
            "tool_name":"shell",
            "ts":"2026-08-01T07:59:59Z"
        }));
        let stale = grok_status_decision(
            &UpdateState::default(),
            &stale_events,
            EventAvailability::Available,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(stale.status, SessionStatus::Idle);
        assert_eq!(stale.authority, StatusAuthority::Provider);
        assert_eq!(stale.reason, StatusReason::ProviderIdle);

        let missing = grok_status_decision(
            &UpdateState::default(),
            &EventState::default(),
            EventAvailability::Missing,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(missing.status, SessionStatus::Idle);
        assert_eq!(missing.authority, StatusAuthority::Provider);
        assert_eq!(missing.reason, StatusReason::ProviderIdle);
    }

    #[test]
    fn interaction_waits_override_execution_without_aging_ordinary_tools() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut updates = UpdateState::default();
        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"terminal",
                "_meta":{"x.ai/tool":{"kind":"execute","name":"run_terminal_command"}}
            }}
        }));
        let mut events = EventState::default();
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Executing, false)
        );

        events.apply(&serde_json::json!({
            "type":"permission_requested",
            "tool_name":"run_terminal_command",
            "ts":"2026-08-01T08:00:02Z"
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Waiting, true)
        );
        events.apply(&serde_json::json!({
            "type":"permission_resolved",
            "tool_name":"run_terminal_command",
            "ts":"2026-08-01T08:00:03Z"
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Executing, false)
        );
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, true),
            (SessionStatus::Waiting, true)
        );
    }

    #[test]
    fn exact_lifecycle_distinguishes_thinking_executing_waiting_and_idle() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut updates = UpdateState::default();
        let events = EventState::default();
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Idle, false)
        );

        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T07:59:59Z",
            "params":{"update":{"sessionUpdate":"response_started"}}
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Idle, false),
            "pre-open response state must not leak into the current registry interval"
        );

        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{"sessionUpdate":"agent_thought_chunk","content":"work"}}
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Thinking, false)
        );

        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:02Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"read",
                "kind":"read",
                "locations":[{"path":"/tmp/input"}]
            }}
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Executing, false)
        );

        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:03Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"question",
                "_meta":{"x.ai/tool":{"kind":"ask_user","name":"ask_user_question"}}
            }}
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Waiting, true),
            "an actionable wait must win over simultaneous work"
        );

        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:04Z",
            "params":{"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"question",
                "status":"completed"
            }}
        }));
        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:05Z",
            "params":{"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"read",
                "status":"completed"
            }}
        }));
        updates.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:06Z",
            "params":{"update":{
                "sessionUpdate":"turn_completed",
                "prompt_id":"turn-1",
                "usage":{}
            }}
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Idle, false)
        );
    }

    #[test]
    fn event_phases_are_exact_and_scoped_to_the_registry_interval() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let updates = UpdateState::default();
        let mut events = EventState::default();
        events.apply(&serde_json::json!({
            "type":"phase_changed",
            "phase":"streaming_reasoning",
            "ts":"2026-08-01T07:59:59Z"
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Idle, false)
        );

        events.apply(&serde_json::json!({
            "type":"phase_changed",
            "phase":"waiting_for_model",
            "ts":"2026-08-01T08:00:01Z"
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Thinking, false)
        );
        events.apply(&serde_json::json!({
            "type":"phase_changed",
            "phase":"tool_execution",
            "ts":"2026-08-01T08:00:02Z"
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Executing, false)
        );
        events.apply(&serde_json::json!({
            "type":"phase_changed",
            "phase":"permission_prompt",
            "ts":"2026-08-01T08:00:03Z"
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Waiting, true)
        );
        events.apply(&serde_json::json!({
            "type":"turn_ended",
            "ts":"2026-08-01T08:00:04Z"
        }));
        assert_eq!(
            grok_session_status(&updates, &events, opened_at, false),
            (SessionStatus::Idle, false)
        );
    }

    #[test]
    fn background_tasks_and_subagents_keep_quiescent_parent_executing() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({
            "params":{
                "_meta":{"agentTimestampMs":1785571201000_u64},
                "update":{
                    "sessionUpdate":"task_backgrounded",
                    "task_id":42,
                    "description":"run checks"
                }
            }
        }));
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Executing, false)
        );
        assert_eq!(
            state.current_work_labels(opened_at),
            vec!["Background run checks"]
        );

        state.apply(&serde_json::json!({
            "params":{"update":{
                "sessionUpdate":"task_completed",
                "task_snapshot":{"task_id":42,"exit_code":1}
            }}
        }));
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Idle, false)
        );
        assert_eq!(
            state.last_error.as_deref(),
            Some("Background task failed (exit 1)")
        );

        state.apply(&serde_json::json!({
            "params":{
                "_meta":{"agentTimestampMs":1785571202000_u64},
                "update":{
                    "sessionUpdate":"subagent_spawned",
                    "subagent_id":"sub-1",
                    "description":"inspect schema"
                }
            }
        }));
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Executing, false)
        );
        assert_eq!(
            state.current_work_labels(opened_at),
            vec!["Subagent inspect schema"]
        );
        state.apply(&serde_json::json!({
            "params":{
                "_meta":{"agentTimestampMs":1785571203000_u64},
                "update":{
                    "sessionUpdate":"subagent_finished",
                    "subagent_id":"sub-1",
                    "status":"failed",
                    "error":"worker\nfailed"
                }
            }
        }));
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Idle, false)
        );
        assert_eq!(state.last_error.as_deref(), Some("workerfailed"));
    }

    #[test]
    fn late_background_output_does_not_reopen_a_completed_turn() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"background-shell",
                "_meta":{"x.ai/tool":{"kind":"execute","name":"run_terminal_command"}}
            }}
        }));
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:02Z",
            "params":{"update":{
                "sessionUpdate":"task_backgrounded",
                "task_id":"task-1",
                "description":"run checks"
            }}
        }));
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:03Z",
            "params":{"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"background-shell",
                "status":"completed"
            }}
        }));
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:04Z",
            "params":{"update":{
                "sessionUpdate":"turn_completed",
                "prompt_id":"turn-1",
                "stop_reason":"end_turn"
            }}
        }));
        assert!(!state.active_turn);
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Executing, false),
            "the background task itself remains exact execution evidence"
        );

        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:05Z",
            "params":{"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"background-shell",
                "status":"in_progress",
                "locations":[{"path":"/tmp/background.log"}],
                "rawOutput":{"output":"still running"}
            }}
        }));
        assert!(
            !state.active_turn,
            "late background output must not reopen completed foreground lifecycle"
        );
        assert!(state.pending_tools.is_empty());
        assert_eq!(state.lifecycle_failure, None);
        assert_eq!(state.tool_calls[0].arg, "/tmp/background.log");
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Executing, false)
        );

        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:06Z",
            "params":{"update":{
                "sessionUpdate":"task_completed",
                "task_snapshot":{"task_id":"task-1","exit_code":0}
            }}
        }));
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Idle, false),
            "the session is idle after the only background task terminates"
        );
    }

    #[test]
    fn never_seen_nonterminal_tool_update_fails_closed() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"missing-opener",
                "status":"in_progress",
                "rawOutput":{"output":"working"}
            }}
        }));

        assert!(!state.active_turn);
        assert!(state.pending_tools.is_empty());
        assert_eq!(
            state.lifecycle_failure,
            Some(StatusReason::ProtocolMalformed)
        );
        let decision = grok_status_decision(
            &state,
            &EventState::default(),
            EventAvailability::Available,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(decision.status, SessionStatus::Unknown);
        assert_eq!(decision.authority, StatusAuthority::Unavailable);
        assert_eq!(decision.reason, StatusReason::ProtocolMalformed);
    }

    #[test]
    fn never_seen_terminal_tool_updates_fail_closed() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        for status in ["completed", "failed", "cancelled"] {
            let mut state = UpdateState::default();
            state.apply(&serde_json::json!({
                "timestamp":"2026-08-01T08:00:01Z",
                "params":{"update":{
                    "sessionUpdate":"tool_call_update",
                    "toolCallId":"missing-opener",
                    "status":status
                }}
            }));

            assert!(!state.active_turn, "status={status}");
            assert!(state.pending_tools.is_empty(), "status={status}");
            assert_eq!(
                state.lifecycle_failure,
                Some(StatusReason::ProtocolMalformed),
                "status={status}"
            );
            let decision = grok_status_decision(
                &state,
                &EventState::default(),
                EventAvailability::Available,
                EventAvailability::Missing,
                opened_at,
                false,
            );
            assert_eq!(decision.status, SessionStatus::Unknown, "status={status}");
            assert_eq!(
                decision.authority,
                StatusAuthority::Unavailable,
                "status={status}"
            );
            assert_eq!(
                decision.reason,
                StatusReason::ProtocolMalformed,
                "status={status}"
            );
        }
    }

    #[test]
    fn ask_user_tool_metadata_waits_until_terminal_completion() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"question",
                "_meta":{"x.ai/tool":{"kind":"ask_user","name":"ask_user_question"}},
                "rawInput":{"questions":[]}
            }}
        }));
        assert!(state.pending_tools["question"].waits_for_user);
        assert_eq!(state.pending_tools["question"].name, "Ask User");
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Waiting, true)
        );
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at + 2_000, false),
            (SessionStatus::Idle, false)
        );

        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:02Z",
            "params":{"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"question",
                "status":"completed"
            }}
        }));
        assert!(state.pending_tools.is_empty());
        assert_eq!(
            grok_session_status(&state, &EventState::default(), opened_at, false),
            (SessionStatus::Thinking, false),
            "resolving the foreground question returns control to the still-open model turn"
        );

        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:03Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"compat-question",
                "rawInput":{"questions":[]}
            }}
        }));
        assert!(
            !state.pending_tools["compat-question"].waits_for_user,
            "payload shape without the canonical ask-user identity is not exact wait evidence"
        );
    }

    #[test]
    fn nonterminal_tool_update_can_enrich_ask_user_identity() {
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:00Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"question"
            }}
        }));
        assert!(!state.pending_tools["question"].waits_for_user);
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"question",
                "_meta":{"x.ai/tool":{"kind":"ask_user","name":"ask_user_question"}}
            }}
        }));
        assert!(state.pending_tools["question"].waits_for_user);
        assert_eq!(state.tool_calls[0].name, "Ask User");
    }

    #[test]
    fn event_cache_buffers_partial_lines_and_resets_on_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut file = File::create(&path).unwrap();
        file.write_all(br#"{"type":"permission_requested","tool_name":"bash""#)
            .unwrap();
        file.flush().unwrap();
        let mut cache = EventCache::default();
        assert_eq!(
            cache.refresh(&path),
            EventAvailability::Failed(StatusReason::Stale)
        );
        assert!(cache.state.pending_permissions.is_empty());
        file.write_all(br#", "ts":"2026-08-01T08:00:00Z"}"#)
            .unwrap();
        file.write_all(b"\nnot-json\n").unwrap();
        file.flush().unwrap();
        assert_eq!(
            cache.refresh(&path),
            EventAvailability::Failed(StatusReason::ProtocolMalformed)
        );
        assert!(!cache.state.pending_permissions.is_empty());
        drop(file);

        fs::write(
            &path,
            "{\"type\":\"turn_started\",\"ts\":\"2026-08-01T08:00:01Z\"}\n",
        )
        .unwrap();
        assert_eq!(cache.refresh(&path), EventAvailability::Available);
        assert!(cache.state.pending_permissions.is_empty());
    }

    #[test]
    fn plan_mode_file_exposes_pending_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan_mode.json");
        fs::write(&path, r#"{"state":"Active","awaiting_plan_approval":true}"#).unwrap();
        assert_eq!(
            read_awaiting_plan_approval(&path),
            (true, EventAvailability::Available)
        );
        fs::write(
            &path,
            r#"{"state":"Active","awaiting_plan_approval":false}"#,
        )
        .unwrap();
        assert_eq!(
            read_awaiting_plan_approval(&path),
            (false, EventAvailability::Available)
        );
        fs::write(&path, "not json").unwrap();
        assert_eq!(
            read_awaiting_plan_approval(&path),
            (
                false,
                EventAvailability::Failed(StatusReason::ProtocolMalformed)
            )
        );
    }

    #[test]
    fn turn_usage_is_disjoint_and_reasoning_is_not_added() {
        let usage = serde_json::json!({
            "inputTokens": 100,
            "outputTokens": 20,
            "cachedReadTokens": 30,
            "cacheCreationTokens": 10,
            "reasoningTokens": 7
        });
        let parsed = parse_turn_usage(&usage);
        assert_eq!(
            (
                parsed.input,
                parsed.output,
                parsed.cache_read,
                parsed.cache_create
            ),
            (60, 20, 30, 10)
        );
        assert_eq!(parsed.total(), 120);
    }

    #[test]
    fn update_state_tracks_turn_tools_and_deduplicates_event_ids() {
        let mut state = UpdateState::default();
        let call = serde_json::json!({"params":{"_meta":{"eventId":"1","agentTimestampMs":10,"totalTokens":11},"update":{"sessionUpdate":"tool_call","toolCallId":"t","title":"Run `cat /tmp/a`","kind":"read","status":"in_progress","locations":[{"path":"/tmp/a"}]}}});
        state.apply(&call);
        state.apply(&call);
        assert_eq!(state.pending_tools.len(), 1);
        assert_eq!(state.pending_tools["t"].name, "Read");
        assert_eq!(state.meta_context_tokens, Some(11));
        let done = serde_json::json!({"params":{"_meta":{"eventId":"2","agentTimestampMs":20},"update":{"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed"}}});
        state.apply(&done);
        assert!(state.pending_tools.is_empty());
        let turn = serde_json::json!({"params":{"_meta":{"eventId":"3","agentTimestampMs":30},"update":{"sessionUpdate":"turn_completed","prompt_id":"p","stop_reason":"end_turn","usage":{"inputTokens":12,"outputTokens":3,"cachedReadTokens":2}}}});
        state.apply(&turn);
        state.apply(&turn);
        state.apply(&serde_json::json!({"params":{"_meta":{"eventId":"4","agentTimestampMs":40},"update":{"sessionUpdate":"turn_completed","prompt_id":"p","stop_reason":"end_turn","usage":{"inputTokens":99}}}}));
        assert_eq!(
            (
                state.total_input,
                state.total_output,
                state.total_cache_read
            ),
            (10, 3, 2)
        );
        assert_eq!(state.turn_count, 1);
    }

    #[test]
    fn current_xai_shapes_update_model_subagents_retries_and_rewinds() {
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({"params":{"update":{"sessionUpdate":"response_started","model":"grok-code"}}}));
        assert_eq!(state.model, "grok-code");
        state.apply(&serde_json::json!({"params":{"update":{"sessionUpdate":"subagent_spawned","subagent_id":"sub-1","child_session_id":"child-1","subagent_type":"explore","description":"Inspect schema"}}}));
        state.apply(&serde_json::json!({"params":{"update":{"sessionUpdate":"subagent_progress","subagent_id":"sub-1","child_session_id":"child-1","tokens_used":42}}}));
        assert_eq!(state.subagents["sub-1"].name, "Inspect schema");
        assert_eq!(state.subagents["sub-1"].tokens, 42);
        state.apply(&serde_json::json!({"params":{"update":{"sessionUpdate":"retry_state","type":"failed","error_type":"server","message":"request failed"}}}));
        assert_eq!(state.last_error.as_deref(), Some("request failed"));

        state.total_input = 100;
        state.turn_count = 4;
        state.apply(&serde_json::json!({"params":{"update":{"sessionUpdate":"rewind_marker","target_prompt_index":2}}}));
        assert_eq!(state.total_input, 100);
        assert_eq!(state.turn_count, 2);
        assert!(state.subagents.is_empty());
    }

    #[test]
    fn terminal_provider_failures_and_rate_limits_are_not_idle() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut failed = UpdateState::default();
        failed.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{
                "sessionUpdate":"turn_completed",
                "prompt_id":"failed-turn",
                "stop_reason":"error",
                "agent_result":"sensitive provider detail"
            }}
        }));
        let decision = grok_status_decision(
            &failed,
            &EventState::default(),
            EventAvailability::Missing,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(decision.status, SessionStatus::Error);
        assert_eq!(decision.authority, StatusAuthority::Provider);
        assert_eq!(decision.reason, StatusReason::ProviderError);
        assert_eq!(
            grok_current_tasks(&failed, decision.status, opened_at),
            vec!["error".to_string()]
        );
        assert!(!grok_current_tasks(&failed, decision.status, opened_at)
            .iter()
            .any(|task| task.contains("sensitive")));

        let mut limited = UpdateState::default();
        limited.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:02Z",
            "params":{"update":{
                "sessionUpdate":"turn_completed",
                "prompt_id":"limited-turn",
                "stop_reason":"rate_limit"
            }}
        }));
        let decision = grok_status_decision(
            &limited,
            &EventState::default(),
            EventAvailability::Missing,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(decision.status, SessionStatus::RateLimited);
        assert_eq!(decision.reason, StatusReason::ProviderRateLimit);

        let mut exhausted_retry = UpdateState::default();
        exhausted_retry.apply(&serde_json::json!({
            "params":{
                "_meta":{"agentTimestampMs":1785571203000_u64},
                "update":{
                    "sessionUpdate":"retry_state",
                    "type":"exhausted",
                    "isRateLimited":true,
                    "message":"provider detail"
                }
            }
        }));
        let decision = grok_status_decision(
            &exhausted_retry,
            &EventState::default(),
            EventAvailability::Missing,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(decision.status, SessionStatus::RateLimited);
        assert_eq!(decision.authority, StatusAuthority::Provider);
        assert_eq!(decision.reason, StatusReason::ProviderRateLimit);
    }

    #[test]
    fn exact_wait_precedes_a_simultaneous_terminal_error() {
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:01Z",
            "params":{"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"question",
                "_meta":{"x.ai/tool":{"kind":"ask_user","name":"ask_user_question"}}
            }}
        }));
        state.apply(&serde_json::json!({
            "timestamp":"2026-08-01T08:00:02Z",
            "params":{"update":{
                "sessionUpdate":"auto_recovery_exhausted",
                "error":"sensitive failure"
            }}
        }));
        let decision = grok_status_decision(
            &state,
            &EventState::default(),
            EventAvailability::Missing,
            EventAvailability::Missing,
            opened_at,
            false,
        );
        assert_eq!(decision.status, SessionStatus::Waiting);
        assert_eq!(decision.reason, StatusReason::ProviderWaitingUserInput);
    }

    #[test]
    fn malformed_update_records_make_cached_status_unavailable_until_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-08-01T08:00:01Z\",\"params\":{\"update\":{",
                "\"sessionUpdate\":\"tool_call\",\"toolCallId\":\"question\",",
                "\"_meta\":{\"x.ai/tool\":{\"kind\":\"ask_user\",",
                "\"name\":\"ask_user_question\"}}}}}\n",
                "not-json\n",
                "{\"timestamp\":\"2026-08-01T08:00:02Z\",\"params\":{\"update\":{",
                "\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"question\",",
                "\"status\":\"completed\"}}}\n"
            ),
        )
        .unwrap();
        let mut cache = UpdateCache::default();
        assert_eq!(
            cache.refresh(&path),
            EventAvailability::Failed(StatusReason::ProtocolMalformed)
        );
        assert!(cache.state.pending_tools.is_empty());

        fs::write(
            &path,
            "{\"timestamp\":\"2026-08-01T08:00:03Z\",\"params\":{\"update\":{\"sessionUpdate\":\"turn_completed\",\"prompt_id\":\"done\",\"stop_reason\":\"end_turn\"}}}\n",
        )
        .unwrap();
        assert_eq!(cache.refresh(&path), EventAvailability::Available);
    }

    #[test]
    fn invalid_update_timestamps_make_lifecycle_status_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let opened_at = parse_time(&serde_json::json!("2026-08-01T08:00:00Z"));
        for (index, invalid_completion) in [
            "{\"params\":{\"update\":{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"question\",\"status\":\"completed\"}}}\n",
            "{\"timestamp\":\"not-a-time\",\"params\":{\"update\":{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"question\",\"status\":\"completed\"}}}\n",
        ]
        .into_iter()
        .enumerate()
        {
            let path = dir.path().join(format!("updates-{index}.jsonl"));
            fs::write(
                &path,
                concat!(
                    "{\"timestamp\":\"2026-08-01T08:00:01Z\",\"params\":{\"update\":{",
                    "\"sessionUpdate\":\"tool_call\",\"toolCallId\":\"question\",",
                    "\"_meta\":{\"x.ai/tool\":{\"kind\":\"ask_user\",",
                    "\"name\":\"ask_user_question\"}}}}}\n"
                ),
            )
            .unwrap();
            let mut cache = UpdateCache::default();
            assert_eq!(cache.refresh(&path), EventAvailability::Available);

            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(invalid_completion.as_bytes()).unwrap();
            file.flush().unwrap();
            let availability = cache.refresh(&path);
            assert_eq!(
                availability,
                EventAvailability::Failed(StatusReason::ProtocolMalformed)
            );
            assert!(cache.state.waiting_since(opened_at).is_some());

            let mut state = cache.state.clone();
            if let EventAvailability::Failed(reason) = availability {
                state.lifecycle_failure = Some(reason);
            }
            let decision = grok_status_decision(
                &state,
                &EventState::default(),
                EventAvailability::Missing,
                EventAvailability::Missing,
                opened_at,
                false,
            );
            assert_eq!(decision.status, SessionStatus::Unknown);
            assert_eq!(decision.authority, StatusAuthority::Unavailable);
            assert_eq!(decision.reason, StatusReason::ProtocolMalformed);
        }
    }

    #[test]
    fn oversized_update_record_is_protocol_malformed() {
        let mut cache = UpdateCache::default();
        cache.consume(&vec![b'x'; MAX_UPDATE_LINE_BYTES + 1]);
        cache.consume(b"\n");
        assert_eq!(
            cache.availability(cache.offset),
            EventAvailability::Failed(StatusReason::ProtocolMalformed)
        );
    }

    #[test]
    fn successful_turn_clears_old_errors_and_terminal_full_tool_calls() {
        let mut state = UpdateState::default();
        state.apply(&serde_json::json!({"params":{"_meta":{"agentTimestampMs":10},"update":{"sessionUpdate":"tool_call","toolCallId":"tool-1","kind":"read","status":"in_progress","locations":[{"path":"/tmp/a"}]}}}));
        assert!(state.pending_tools.contains_key("tool-1"));
        state.apply(&serde_json::json!({"params":{"_meta":{"agentTimestampMs":20},"update":{"sessionUpdate":"tool_call","toolCallId":"tool-1","kind":"read","status":"failed","locations":[{"path":"/tmp/a"}]}}}));
        assert!(state.pending_tools.is_empty());
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].duration_ms, 10);
        assert!(state.last_error.is_some());

        state.apply(&serde_json::json!({"params":{"update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"try again"}}}}));
        assert!(state.last_error.is_none());
        state.apply(&serde_json::json!({"params":{"update":{"sessionUpdate":"turn_completed","prompt_id":"successful","stop_reason":"end_turn","usage":{"inputTokens":2}}}}));
        assert!(state.last_error.is_none());
        assert!(!state.active_turn);
    }

    #[test]
    fn incremental_cache_buffers_partial_and_detects_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updates.jsonl");
        let mut file = File::create(&path).unwrap();
        file.write_all(
            br#"{"timestamp":"2026-08-01T08:00:01Z","params":{"update":{"sessionUpdate":"turn_completed""#,
        )
        .unwrap();
        file.flush().unwrap();
        let mut cache = UpdateCache::default();
        assert_eq!(
            cache.refresh(&path),
            EventAvailability::Failed(StatusReason::Stale)
        );
        assert_eq!(cache.state.turn_count, 0);
        file.write_all(
            br#", "prompt_id":"turn-1","stop_reason":"end_turn","usage":{"inputTokens":2}}}}"#,
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let complete = fs::read_to_string(&path).unwrap();
        let complete_value = serde_json::from_str::<Value>(complete.trim()).unwrap();
        let mut direct = UpdateState::default();
        direct.apply(&complete_value);
        assert_eq!(direct.turn_count, 1, "{complete}");
        assert_eq!(cache.refresh(&path), EventAvailability::Available);
        assert_eq!(cache.state.turn_count, 1);
        fs::write(&path, "{\"timestamp\":\"2026-08-01T08:00:02Z\",\"params\":{\"update\":{\"sessionUpdate\":\"turn_completed\",\"prompt_id\":\"turn-2\",\"stop_reason\":\"end_turn\",\"usage\":{\"inputTokens\":5}}}}\n").unwrap();
        assert_eq!(cache.refresh(&path), EventAvailability::Available);
        assert_eq!(cache.state.turn_count, 1);
        assert_eq!(cache.state.total_input, 5);
    }

    #[test]
    fn signals_prefer_exact_context_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signals.json");
        fs::write(&path, r#"{"contextTokensUsed":42,"contextWindowTokens":100,"contextWindowUsage":42.0,"turnCount":3,"compactionCount":2}"#).unwrap();
        let signals = read_signals(&path);
        assert_eq!(signals.context_tokens, Some(42));
        assert_eq!(signals.context_window, Some(100));
        assert_eq!(signals.context_percent, Some(42.0));
        assert_eq!(signals.turn_count, Some(3));
        assert_eq!(signals.compaction_count, Some(2));
    }

    #[test]
    fn registry_and_summary_use_authoritative_session_metadata() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("active_sessions.json"),
            r#"[{"session_id":"session-a","pid":42,"cwd":"/stale","opened_at":"2026-08-01T08:00:00Z"}]"#,
        )
        .unwrap();
        let entries = read_active_registry(root.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "session-a");
        assert_eq!(entries[0].pid, 42);
        assert!(entries[0].opened_at > 0);

        let summary_path = root.path().join("summary.json");
        fs::write(
            &summary_path,
            r#"{"info":{"id":"session-a","cwd":"/authoritative"},"created_at":"2026-08-01T08:00:00Z","updated_at":"2026-08-01T08:01:00Z","last_active_at":null,"current_model_id":"grok-code","generated_title":"   ","session_summary":"Fix parser","hidden":false}"#,
        )
        .unwrap();
        let summary = read_summary(&summary_path, &entries[0]).unwrap();
        assert_eq!(summary.cwd, "/authoritative");
        assert_eq!(summary.model, "grok-code");
        assert_eq!(summary.title, "Fix parser");
        assert_eq!(summary.hidden, Some(false));
        assert!(summary.updated_at > summary.created_at);
    }

    #[test]
    fn shared_pid_assigns_process_resources_to_latest_session_only() {
        let make = |id: &str, updated_at: u64| ActiveSession {
            root: PathBuf::from("/tmp/.grok"),
            dir: PathBuf::from(format!("/tmp/.grok/sessions/project/{id}")),
            pid: 42,
            opened_at: 1,
            action_process_incarnation: "process-42".to_string(),
            meta: SessionMeta {
                id: id.to_string(),
                updated_at,
                ..SessionMeta::default()
            },
        };
        let sessions = vec![make("older", 10), make("newer", 20)];
        let owners = resource_owner_indices(&sessions);
        assert_eq!(owners.get(&42), Some(&1));
    }

    #[test]
    fn unavailable_lifecycle_is_never_promoted_to_execution() {
        for logical_sessions in [1, 2] {
            assert!(shared_pid_idle_uncertainty(
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                logical_sessions,
            )
            .is_none());
        }

        let unavailable = GrokStatusDecision::unavailable(StatusReason::ProtocolMalformed);
        let evidence = evidence_for(unavailable);
        assert_eq!(unavailable.status, SessionStatus::Unknown);
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::ProtocolMalformed);
    }

    #[test]
    fn shared_pid_preserves_positive_provider_lifecycle() {
        for status in [
            SessionStatus::Waiting,
            SessionStatus::Executing,
            SessionStatus::Thinking,
            SessionStatus::RateLimited,
            SessionStatus::Error,
        ] {
            assert!(shared_pid_idle_uncertainty(status, StatusAuthority::Provider, 2).is_none());
        }
        assert!(shared_pid_idle_uncertainty(
            SessionStatus::Unknown,
            StatusAuthority::Unavailable,
            2,
        )
        .is_none());

        let waiting = GrokStatusDecision::provider(
            SessionStatus::Waiting,
            StatusReason::ProviderWaitingApproval,
            42,
        );
        let evidence = evidence_for(waiting);
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingApproval);
        assert_eq!(evidence.status_since_ms, 42);
    }

    #[test]
    fn shared_pid_provider_idle_becomes_unknown_when_row_liveness_is_ambiguous() {
        assert!(
            shared_pid_idle_uncertainty(SessionStatus::Idle, StatusAuthority::Provider, 1,)
                .is_none()
        );

        let decision =
            shared_pid_idle_uncertainty(SessionStatus::Idle, StatusAuthority::Provider, 2).unwrap();
        assert_eq!(decision.status, SessionStatus::Unknown);
        assert_eq!(decision.authority, StatusAuthority::Unavailable);
        assert_eq!(decision.reason, StatusReason::OwnershipUnconfirmed);
    }
}
