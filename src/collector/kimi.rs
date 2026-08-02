//! Collector for current Moonshot Kimi Code sessions (`~/.kimi-code`).

use super::{abbrev_path, process, redact_secrets, sanitize_terminal_text, AgentCollector};
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

const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_WIRE_LINE_BYTES: usize = 2 * 1024 * 1024;
const MAX_WIRE_READ_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SESSIONS: usize = 500;
const MAX_WIRE_CACHES: usize = 128;
const MAX_TOOL_CALLS: usize = 500;
const MAX_HISTORY_POINTS: usize = 10_000;
const WIRE_PREFIX_BYTES: u64 = 256;
const MAX_TASK_FILES: usize = 512;
const MAX_TASK_BYTES: u64 = 256 * 1024;
/// v1.4 persists foreground starts but not every fatal completion. Treat a
/// quiet non-wait foreground record as current only for this bounded lease.
const V1_NON_WAIT_FOREGROUND_LEASE_MS: u64 = 10 * 60 * 1_000;

pub struct KimiCollector {
    roots: Vec<PathBuf>,
    sessions: Vec<KimiSession>,
    wires: HashMap<PathBuf, WireCache>,
    assignments: HashMap<u32, KimiAssignment>,
}

impl KimiCollector {
    pub fn new() -> Self {
        Self {
            roots: default_roots(),
            sessions: Vec::new(),
            wires: HashMap::new(),
            assignments: HashMap::new(),
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let mut processes: Vec<KimiProcess> = shared
            .process_info
            .iter()
            .filter(|(_, info)| is_kimi_process(&info.command))
            .filter_map(|(&pid, _)| {
                let incarnation = process::get_process_incarnation(pid)?;
                let tokens = process::get_process_tokens(pid)?;
                if !is_kimi_process_tokens(&tokens) {
                    return None;
                }
                let cwd = process::get_process_cwd(pid)?;
                let root = process_root(pid, &cwd)?;
                let started_at = process::get_process_started_at_ms(pid);
                let current_incarnation = process::get_process_incarnation(pid);
                if !kimi_process_observation_is_exact(
                    &incarnation,
                    current_incarnation.as_deref(),
                    &tokens,
                ) {
                    return None;
                }
                Some(KimiProcess {
                    pid,
                    cwd,
                    root,
                    explicit_session: explicit_session_id_from_tokens(&tokens),
                    bare_title: is_bare_kimi_tokens(&tokens),
                    started_at,
                    incarnation,
                })
            })
            .collect();

        if shared.slow_tick || self.sessions.is_empty() {
            self.refresh_roots(&processes);
            self.sessions = self
                .roots
                .iter()
                .flat_map(|root| read_session_index(root))
                .collect();
            self.sessions
                .sort_by_key(|s| std::cmp::Reverse(s.updated_at));
            self.sessions.truncate(MAX_SESSIONS);
        }

        let live_incarnations: HashMap<u32, &str> = processes
            .iter()
            .map(|process| (process.pid, process.incarnation.as_str()))
            .collect();
        self.assignments.retain(|pid, assignment| {
            live_incarnations.get(pid).copied() == Some(assignment.process_incarnation.as_str())
                && self
                    .sessions
                    .iter()
                    .any(|session| session.dir == assignment.dir)
        });

        let cwd_counts = processes.iter().fold(
            HashMap::<(PathBuf, String), usize>::new(),
            |mut counts, process| {
                *counts
                    .entry((process.root.clone(), normalize_path(&process.cwd)))
                    .or_default() += 1;
                counts
            },
        );
        // Preserve explicit and prior assignments before pairing newcomers.
        // HashMap iteration order must not reshuffle same-cwd rows between
        // polls; ambiguous groups remain Unknown, but their display should be
        // deterministic and sticky.
        processes.sort_by_key(|process| {
            let priority = if process.explicit_session.is_some() {
                0
            } else if self.assignments.contains_key(&process.pid) {
                1
            } else {
                2
            };
            (priority, process.pid)
        });
        let mut claimed = HashSet::new();
        let mut output = Vec::new();

        for proc_ctx in processes {
            let cwd_key = normalize_path(&proc_ctx.cwd);
            let candidates: Vec<&KimiSession> = self
                .sessions
                .iter()
                .filter(|session| session_matches_process(session, &proc_ctx))
                .collect();
            let shared_cwd = cwd_counts
                .get(&(proc_ctx.root.clone(), cwd_key))
                .copied()
                .unwrap_or(0)
                > 1;
            let selected = select_session(
                &proc_ctx,
                &candidates,
                self.assignments.get(&proc_ctx.pid),
                &claimed,
                !shared_cwd,
            );
            let Some(session) = selected.cloned() else {
                continue;
            };
            claimed.insert(session.dir.clone());
            let previous_assignment = self.assignments.get(&proc_ctx.pid).cloned();
            let pairing_authority =
                pairing_authority(&proc_ctx, &session, previous_assignment.as_ref());
            let pairing_confirmed = pairing_authority != StatusAuthority::Unavailable;
            let activity_boundary_ms = proc_ctx
                .started_at
                .or_else(|| {
                    previous_assignment
                        .as_ref()
                        .filter(|assignment| assignment.dir == session.dir)
                        .filter(|assignment| assignment.process_incarnation == proc_ctx.incarnation)
                        .map(|assignment| assignment.activity_boundary_ms)
                })
                .unwrap_or_else(current_time_ms);
            self.assignments.insert(
                proc_ctx.pid,
                KimiAssignment {
                    dir: session.dir.clone(),
                    confirmed: pairing_confirmed,
                    authority: pairing_authority,
                    activity_boundary_ms,
                    process_incarnation: proc_ctx.incarnation.clone(),
                },
            );
            let ownership_unknown = shared_cwd || !pairing_confirmed;
            let action_process_incarnation = (!shared_cwd
                && pairing_authority == StatusAuthority::Provider)
                .then(|| proc_ctx.incarnation.clone());
            if let Some(row) = self.build_session(
                &session,
                proc_ctx.pid,
                activity_boundary_ms,
                KimiRowOwnership {
                    ambiguous: ownership_unknown,
                    authority: pairing_authority,
                    action_process_incarnation,
                },
                shared,
            ) {
                output.push(row);
            }
        }

        let keep: HashSet<PathBuf> = output
            .iter()
            .filter_map(|s| self.assignments.get(&s.pid))
            .flat_map(|assignment| {
                let dir = &assignment.dir;
                let mut paths = vec![dir.join("agents/main/wire.jsonl")];
                paths.extend(
                    self.sessions
                        .iter()
                        .find(|s| &s.dir == dir)
                        .into_iter()
                        .flat_map(|s| s.agents.iter())
                        .filter(|a| a.kind == "sub")
                        .map(|a| dir.join("agents").join(&a.id).join("wire.jsonl")),
                );
                paths
            })
            .collect();
        self.wires.retain(|path, _| keep.contains(path));
        if self.wires.len() > MAX_WIRE_CACHES {
            self.wires.clear();
        }
        output
    }

    fn refresh_roots(&mut self, processes: &[KimiProcess]) {
        let mut roots = default_roots();
        roots.extend(processes.iter().map(|process| process.root.clone()));
        roots.sort();
        roots.dedup();
        self.roots = roots;
    }

    fn parse_wire(&mut self, path: &Path) -> WireState {
        if is_symlink(path) {
            self.wires.remove(path);
            return WireState {
                lifecycle_failure: Some(StatusReason::ProtocolMalformed),
                ..WireState::default()
            };
        }
        let cache = self.wires.entry(path.to_path_buf()).or_default();
        let availability = cache.refresh(path);
        let mut state = cache.state.clone();
        if let WireAvailability::Failed(reason) = availability {
            state.lifecycle_failure = Some(reason);
        }
        state
    }

    fn parse_agent_wire(
        &mut self,
        session_dir: &Path,
        agent_id: &str,
        activity_boundary_ms: u64,
        include_session_fallback: bool,
        observed_at_ms: u64,
    ) -> WireState {
        let agent_dir = session_dir.join("agents").join(agent_id);
        let mut state = self.parse_wire(&agent_dir.join("wire.jsonl"));
        let mut task_dirs = vec![agent_dir.join("tasks")];
        if include_session_fallback {
            task_dirs.push(session_dir.join("tasks"));
        }
        match read_task_snapshots(session_dir, &task_dirs, activity_boundary_ms) {
            Ok(snapshots) => state.reconcile_task_snapshots(snapshots),
            Err(reason) => {
                state.lifecycle_failure.get_or_insert(reason);
            }
        }
        state.expire_foreground_lease_at(observed_at_ms);
        state
    }

    fn build_session(
        &mut self,
        meta: &KimiSession,
        pid: u32,
        activity_boundary_ms: u64,
        ownership: KimiRowOwnership,
        shared: &super::SharedProcessData,
    ) -> Option<AgentSession> {
        let KimiRowOwnership {
            ambiguous,
            authority: pairing_authority,
            action_process_incarnation,
        } = ownership;
        let observed_at_ms = current_time_ms();
        let wire = self.parse_agent_wire(
            &meta.dir,
            "main",
            activity_boundary_ms,
            true,
            observed_at_ms,
        );
        let context_window = model_context_limit(&meta.root, &wire.model_alias, &wire.model);
        let context_percent = if context_window == 0 {
            0.0
        } else {
            wire.last_context_tokens as f64 * 100.0 / context_window as f64
        };

        let mut child_wires = Vec::new();
        let mut subagent_states = wire.subagents.clone();
        for agent in meta.agents.iter().filter(|a| a.kind == "sub") {
            let child = self.parse_agent_wire(
                &meta.dir,
                &agent.id,
                activity_boundary_ms,
                false,
                observed_at_ms,
            );
            merge_child_subagent(
                &mut subagent_states,
                &agent.id,
                &child,
                activity_boundary_ms,
            );
            child_wires.push((agent.id.clone(), child));
        }
        for subagent in subagent_states.values_mut() {
            if task_status_is_active(&subagent.status) && subagent.started_at < activity_boundary_ms
            {
                subagent.status = "idle".to_string();
            }
        }
        let active_child_process =
            process::has_active_descendant(pid, &shared.children_map, &shared.process_info, 5.0);
        let (status, awaiting_input) = wire_session_status(
            &wire,
            child_wires.iter().map(|(_, child)| child),
            activity_boundary_ms,
            ambiguous,
            active_child_process,
        );
        let status_evidence = kimi_status_evidence(
            status,
            &wire,
            child_wires.iter().map(|(_, child)| child),
            activity_boundary_ms,
            active_child_process,
            if ambiguous {
                StatusAuthority::Unavailable
            } else {
                pairing_authority
            },
            observed_at_ms,
        );
        let current_tasks = kimi_current_tasks(
            &wire,
            &child_wires,
            activity_boundary_ms,
            ambiguous,
            status,
            awaiting_input,
            active_child_process,
        );

        let mut subagents = subagent_states
            .values()
            .map(SubagentState::to_model)
            .collect::<Vec<_>>();
        subagents.sort_by(|left, right| left.name.cmp(&right.name));
        let (pending_since_ms, thinking_since_ms) = lifecycle_timestamps(
            status,
            &wire,
            child_wires.iter().map(|(_, child)| child),
            activity_boundary_ms,
        );

        let proc_info = shared.process_info.get(&pid);
        Some(AgentSession {
            agent_cli: "kimi",
            pid,
            action_process_incarnation,
            session_id: meta.id.clone(),
            cwd: meta.cwd.clone(),
            project_name: process::last_path_segment(&meta.cwd)
                .unwrap_or("?")
                .to_string(),
            started_at: meta.created_at,
            status,
            status_evidence,
            model: if wire.model.is_empty() {
                "-".into()
            } else {
                wire.model.clone()
            },
            effort: wire.effort.clone(),
            context_percent,
            total_input_tokens: wire.total_input,
            total_output_tokens: wire.total_output,
            total_cache_read: wire.total_cache_read,
            total_cache_create: wire.total_cache_create,
            turn_count: wire.turn_count,
            current_tasks,
            mem_mb: proc_info.map_or(0, |p| p.rss_kb / 1024),
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: wire.token_history_snapshot(),
            context_history: wire.context_history.iter().copied().collect(),
            compaction_count: wire.compaction_count,
            context_window,
            subagents,
            mem_file_count: 0,
            mem_line_count: 0,
            children: collect_children(pid, shared),
            initial_prompt: if meta.title.is_empty() {
                wire.initial_prompt.clone()
            } else {
                clean_text(&meta.title, 120)
            },
            first_assistant_text: wire.first_assistant_text.clone(),
            chat_messages: wire.chat_messages.clone(),
            tool_calls: wire.tool_calls.clone(),
            pending_since_ms,
            awaiting_input,
            thinking_since_ms,
            file_accesses: wire.file_accesses.clone(),
            config_root: abbrev_path(&meta.root),
        })
    }
}

fn kimi_current_tasks(
    wire: &WireState,
    child_wires: &[(String, WireState)],
    activity_boundary_ms: u64,
    ambiguous: bool,
    status: SessionStatus,
    awaiting_input: bool,
    active_child_process: bool,
) -> Vec<String> {
    if status == SessionStatus::Unknown
        && (wire.lifecycle_failure.is_some()
            || child_wires
                .iter()
                .any(|(_, child)| child.lifecycle_failure.is_some()))
    {
        return vec!["status evidence unavailable".to_string()];
    }
    if ambiguous {
        return vec!["session ownership is ambiguous".to_string()];
    }
    match status {
        SessionStatus::Unknown => vec!["status evidence unavailable".to_string()],
        SessionStatus::Waiting if awaiting_input => vec!["waiting for user input".to_string()],
        SessionStatus::Executing => {
            let mut tasks = wire.execution_labels_since(activity_boundary_ms);
            tasks.extend(
                child_wires
                    .iter()
                    .filter(|(_, child)| child.has_live_activity_since(activity_boundary_ms))
                    .map(|(id, _)| format!("subagent {}", clean_text(id, 80))),
            );
            if active_child_process && tasks.is_empty() {
                tasks.push("child process".to_string());
            }
            tasks.sort();
            tasks.dedup();
            if tasks.is_empty() {
                vec!["executing".to_string()]
            } else {
                tasks
            }
        }
        SessionStatus::Error => vec!["error".to_string()],
        SessionStatus::Thinking => vec!["thinking".to_string()],
        SessionStatus::Idle => vec!["idle".to_string()],
        _ => Vec::new(),
    }
}

impl Default for KimiCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentCollector for KimiCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

#[derive(Clone)]
struct KimiProcess {
    pid: u32,
    cwd: String,
    root: PathBuf,
    explicit_session: Option<String>,
    /// Kimi rewrites argv to this bare title for interactive and host modes.
    /// It can support display correlation, but never provider-owned PID actions.
    bare_title: bool,
    started_at: Option<u64>,
    incarnation: String,
}

#[derive(Clone)]
struct KimiAssignment {
    dir: PathBuf,
    confirmed: bool,
    authority: StatusAuthority,
    activity_boundary_ms: u64,
    process_incarnation: String,
}

struct KimiRowOwnership {
    ambiguous: bool,
    authority: StatusAuthority,
    action_process_incarnation: Option<String>,
}

#[derive(Clone)]
struct KimiSession {
    id: String,
    dir: PathBuf,
    root: PathBuf,
    cwd: String,
    title: String,
    created_at: u64,
    updated_at: u64,
    archived: bool,
    agents: Vec<KimiAgent>,
}

#[derive(Clone)]
struct KimiAgent {
    id: String,
    kind: String,
}

#[derive(Default)]
struct WireCache {
    offset: u64,
    prefix: Vec<u8>,
    partial: Vec<u8>,
    dropping_long_line: bool,
    integrity_failure: Option<StatusReason>,
    state: WireState,
}

impl WireCache {
    fn refresh(&mut self, path: &Path) -> WireAvailability {
        let Ok(meta) = fs::metadata(path) else {
            return WireAvailability::Failed(StatusReason::Unavailable);
        };
        if !meta.is_file() {
            return WireAvailability::Failed(StatusReason::ProtocolMalformed);
        }
        let Ok(mut file) = File::open(path) else {
            return WireAvailability::Failed(StatusReason::Unavailable);
        };
        let mut prefix = Vec::new();
        if file
            .by_ref()
            .take(meta.len().min(WIRE_PREFIX_BYTES))
            .read_to_end(&mut prefix)
            .is_err()
        {
            return WireAvailability::Failed(StatusReason::Unavailable);
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
            return WireAvailability::Failed(StatusReason::Unavailable);
        }
        let mut bytes = Vec::new();
        if file
            .take(MAX_WIRE_READ_BYTES)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return WireAvailability::Failed(StatusReason::Unavailable);
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
                        Ok(value) => self.state.apply(&value),
                        Err(_) => self.integrity_failure = Some(StatusReason::ProtocolMalformed),
                    },
                    Err(_) => self.integrity_failure = Some(StatusReason::ProtocolMalformed),
                }
                self.partial.clear();
            } else if self.partial.len() < MAX_WIRE_LINE_BYTES {
                self.partial.push(byte);
            } else {
                self.partial.clear();
                self.dropping_long_line = true;
                self.integrity_failure = Some(StatusReason::ProtocolMalformed);
            }
        }
    }

    fn availability(&self, file_len: u64) -> WireAvailability {
        if let Some(reason) = self.integrity_failure {
            WireAvailability::Failed(reason)
        } else if self.offset < file_len || !self.partial.is_empty() || self.dropping_long_line {
            WireAvailability::Failed(StatusReason::Stale)
        } else if !self.state.metadata_seen {
            WireAvailability::Failed(StatusReason::ProtocolMalformed)
        } else {
            WireAvailability::Available
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireAvailability {
    Available,
    Failed(StatusReason),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WireProtocol {
    /// Reducer-only test input that did not pass through the persisted-wire parser.
    #[default]
    Synthetic,
    V1_4,
}

impl WireProtocol {
    fn is_persisted(self) -> bool {
        !matches!(self, Self::Synthetic)
    }
}

#[derive(Clone, Default)]
struct WireState {
    protocol: WireProtocol,
    metadata_seen: bool,
    non_metadata_seen: bool,
    model: String,
    model_alias: String,
    effort: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    turn_count: u32,
    active_turn: bool,
    active_step: bool,
    turn_started_at: u64,
    step_started_at: u64,
    llm_started_at: u64,
    foreground_observed_at: u64,
    compaction_active: bool,
    compaction_auto: bool,
    compaction_started_at: u64,
    compaction_observed_at: u64,
    compaction_stale_since: u64,
    current_turn_id: Option<String>,
    current_turn_tokens: u64,
    last_context_tokens: u64,
    token_history: VecDeque<u64>,
    context_history: VecDeque<u64>,
    compaction_count: u32,
    pending_tools: HashMap<String, PendingTool>,
    pending_interactions: HashMap<String, PendingInteraction>,
    active_tasks: HashMap<String, ActiveTask>,
    last_error: Option<String>,
    fatal_error_since: u64,
    foreground_uncertain_since: u64,
    foreground_stale_since: u64,
    lifecycle_failure: Option<StatusReason>,
    initial_prompt: String,
    first_assistant_text: String,
    open_assistant_text: String,
    chat_messages: Vec<ChatMessage>,
    tool_calls: Vec<ToolCall>,
    tool_indices: HashMap<String, usize>,
    file_accesses: Vec<FileAccess>,
    subagents: HashMap<String, SubagentState>,
}

#[derive(Clone)]
struct PendingTool {
    name: String,
    arg: String,
    started_at: u64,
    waits_for_user: bool,
}

#[derive(Clone, Debug)]
struct ActiveTask {
    kind: String,
    name: String,
    started_at: u64,
    detached: bool,
}

#[derive(Debug)]
struct TaskSnapshot {
    id: String,
    task: Option<ActiveTask>,
    modified_at: u64,
}

#[derive(Clone)]
struct PendingInteraction {
    requested_at: u64,
    reason: StatusReason,
}
impl ActiveTask {
    fn label(&self) -> String {
        match self.kind.as_str() {
            "agent" if !self.name.is_empty() => format!("subagent {}", self.name),
            "agent" => "subagent".to_string(),
            "process" => "background process".to_string(),
            "question" if self.detached => "background question".to_string(),
            "question" => "question".to_string(),
            _ if !self.name.is_empty() => self.name.clone(),
            _ => "background task".to_string(),
        }
    }
}
impl PendingTool {
    fn label(&self) -> String {
        if self.arg.is_empty() {
            self.name.clone()
        } else {
            format!("{} {}", self.name, self.arg)
        }
    }
}

fn validate_wire_record(value: &Value, protocol: WireProtocol) -> Result<(), StatusReason> {
    let Some(record) = value.as_object() else {
        return Err(StatusReason::ProtocolMalformed);
    };
    let Some(kind) = record.get("type").and_then(Value::as_str) else {
        return Err(StatusReason::ProtocolMalformed);
    };
    if kind.is_empty() {
        return Err(StatusReason::ProtocolMalformed);
    }
    if protocol == WireProtocol::V1_4 {
        if matches!(
            kind,
            "interaction.request"
                | "interaction.resolved"
                | "turn.started"
                | "turn.ended"
                | "turn.step.started"
                | "turn.step.completed"
                | "turn.step.interrupted"
                | "task.started"
                | "task.terminated"
                | "background.task.started"
                | "background.task.terminated"
        ) {
            return Err(StatusReason::ProtocolUnknown);
        }
        if kind == "llm.request" {
            match record.get("kind").and_then(Value::as_str) {
                Some("loop" | "compaction") => {}
                Some(_) => return Err(StatusReason::ProtocolUnknown),
                None => return Err(StatusReason::ProtocolMalformed),
            }
        }
        if kind == "turn.cancel"
            && record.get("turnId").is_some()
            && record.get("turnId").and_then(Value::as_u64).is_none()
        {
            return Err(StatusReason::ProtocolMalformed);
        }
        if kind == "full_compaction.begin" {
            match record.get("source").and_then(Value::as_str) {
                Some("manual" | "auto") => {}
                Some(_) => return Err(StatusReason::ProtocolUnknown),
                None => return Err(StatusReason::ProtocolMalformed),
            }
        }
        let requires_time = matches!(
            kind,
            "turn.prompt"
                | "turn.steer"
                | "turn.cancel"
                | "full_compaction.begin"
                | "full_compaction.cancel"
                | "full_compaction.complete"
                | "context.apply_compaction"
        ) || kind == "llm.request"
            || (kind == "context.append_loop_event"
                && matches!(
                    record
                        .get("event")
                        .and_then(|event| event.get("type"))
                        .and_then(Value::as_str),
                    Some("step.begin" | "step.end" | "content.part" | "tool.call" | "tool.result")
                ));
        if requires_time
            && record
                .get("time")
                .and_then(Value::as_u64)
                .is_none_or(|time| time == 0)
        {
            return Err(StatusReason::ProtocolMalformed);
        }
        if kind == "turn.cancel" && record.contains_key("target") {
            return Err(StatusReason::ProtocolUnknown);
        }
    }
    match kind {
        "metadata" => Err(StatusReason::ProtocolMalformed),
        "interaction.request" => {
            if record
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                || record
                    .get("time")
                    .and_then(Value::as_u64)
                    .is_none_or(|time| time == 0)
            {
                return Err(StatusReason::ProtocolMalformed);
            }
            match record.get("kind").and_then(Value::as_str) {
                Some("approval" | "question" | "user_tool") => Ok(()),
                Some(_) => Err(StatusReason::ProtocolUnknown),
                None => Err(StatusReason::ProtocolMalformed),
            }
        }
        "interaction.resolved" => {
            if record
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                || record
                    .get("time")
                    .and_then(Value::as_u64)
                    .is_none_or(|time| time == 0)
            {
                Err(StatusReason::ProtocolMalformed)
            } else {
                Ok(())
            }
        }
        "turn.ended" => {
            if record
                .get("time")
                .and_then(Value::as_u64)
                .is_none_or(|time| time == 0)
            {
                return Err(StatusReason::ProtocolMalformed);
            }
            match record.get("reason").and_then(Value::as_str) {
                Some("completed" | "cancelled" | "failed" | "blocked") => Ok(()),
                Some(_) => Err(StatusReason::ProtocolUnknown),
                None => Err(StatusReason::ProtocolMalformed),
            }
        }
        "context.append_loop_event" => {
            let event = record.get("event").unwrap_or(&Value::Null);
            match event.get("type").and_then(Value::as_str) {
                Some("tool.call" | "tool.result")
                    if event
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty) =>
                {
                    Err(StatusReason::ProtocolMalformed)
                }
                Some("step.begin" | "step.end") if protocol.is_persisted() => {
                    if record
                        .get("time")
                        .and_then(Value::as_u64)
                        .is_none_or(|time| time == 0)
                        || event
                            .get("uuid")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty)
                        || json_id(event.get("turnId").unwrap_or(&Value::Null))
                            .is_none_or(|id| id.is_empty())
                    {
                        return Err(StatusReason::ProtocolMalformed);
                    }
                    if event.get("type").and_then(Value::as_str) == Some("step.end") {
                        match event.get("finishReason").and_then(Value::as_str) {
                            Some(
                                "tool_use" | "end_turn" | "max_tokens" | "paused" | "filtered",
                            ) => {}
                            Some(_) => return Err(StatusReason::ProtocolUnknown),
                            None => return Err(StatusReason::ProtocolMalformed),
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        "turn.cancel" => {
            if record
                .get("time")
                .and_then(Value::as_u64)
                .is_none_or(|time| time == 0)
            {
                return Err(StatusReason::ProtocolMalformed);
            }
            if let Some(target) = record.get("target") {
                match target.as_str() {
                    Some("active" | "queued") => {}
                    Some(_) => return Err(StatusReason::ProtocolUnknown),
                    None => return Err(StatusReason::ProtocolMalformed),
                }
            }
            if record.get("turnId").is_some()
                && json_id(record.get("turnId").unwrap_or(&Value::Null))
                    .is_none_or(|id| id.is_empty())
            {
                return Err(StatusReason::ProtocolMalformed);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Default)]
struct SubagentState {
    agent_id: String,
    name: String,
    status: String,
    tokens: u64,
    started_at: u64,
}
impl SubagentState {
    fn to_model(&self) -> SubAgent {
        SubAgent {
            name: self.name.clone(),
            status: self.status.clone(),
            tokens: self.tokens,
        }
    }
}

fn merge_child_subagent(
    subagents: &mut HashMap<String, SubagentState>,
    agent_id: &str,
    child: &WireState,
    activity_boundary_ms: u64,
) {
    let tokens =
        child.total_input + child.total_output + child.total_cache_read + child.total_cache_create;
    if let Some(existing) = subagents.values_mut().find(|state| {
        state.agent_id == agent_id || (state.agent_id.is_empty() && state.name == agent_id)
    }) {
        existing.tokens = tokens;
        if child.has_pending_input_since(activity_boundary_ms) {
            existing.status = "waiting".to_string();
        } else if child.has_live_activity_since(activity_boundary_ms) {
            existing.status = "working".to_string();
        } else if task_status_is_active(&existing.status)
            && existing.started_at < activity_boundary_ms
        {
            existing.status = "idle".to_string();
        }
        return;
    }
    subagents.insert(
        format!("agent:{agent_id}"),
        SubagentState {
            agent_id: agent_id.to_string(),
            name: agent_id.to_string(),
            status: if child.has_pending_input_since(activity_boundary_ms) {
                "waiting".to_string()
            } else if child.has_live_activity_since(activity_boundary_ms) {
                "working".to_string()
            } else {
                "idle".to_string()
            },
            tokens,
            started_at: child.activity_since(activity_boundary_ms),
        },
    );
}

impl WireState {
    fn apply(&mut self, value: &Value) {
        let Some(kind) = value["type"].as_str() else {
            self.lifecycle_failure
                .get_or_insert(StatusReason::ProtocolMalformed);
            return;
        };
        if kind == "metadata" {
            self.apply_metadata(value);
            return;
        }
        self.non_metadata_seen = true;
        if let Err(reason) = validate_wire_record(value, self.protocol) {
            self.lifecycle_failure.get_or_insert(reason);
            return;
        }
        let time = value["time"].as_u64().unwrap_or(0);
        match kind {
            "llm.request" => {
                match value["kind"].as_str() {
                    Some("loop") => {
                        self.fatal_error_since = 0;
                        self.last_error = None;
                        self.foreground_uncertain_since = 0;
                        self.foreground_stale_since = 0;
                        self.llm_started_at = time;
                        self.foreground_observed_at = time;
                    }
                    Some("compaction") => {
                        if self.protocol.is_persisted() && !self.compaction_active {
                            self.lifecycle_failure
                                .get_or_insert(StatusReason::ProtocolMalformed);
                            return;
                        }
                        self.compaction_active = true;
                        if self.compaction_started_at == 0 {
                            self.compaction_started_at = time;
                        }
                        self.compaction_observed_at = time;
                        self.compaction_stale_since = 0;
                    }
                    _ => {}
                }
                if let Some(v) = value["model"].as_str() {
                    self.model = clean_text(v, 120);
                }
                if let Some(v) = value["modelAlias"].as_str() {
                    self.model_alias = clean_text(v, 120);
                }
                if let Some(v) = value["thinkingEffort"].as_str() {
                    self.effort = clean_text(v, 40);
                }
            }
            "full_compaction.begin" => {
                if self.compaction_active {
                    self.lifecycle_failure
                        .get_or_insert(StatusReason::ProtocolMalformed);
                    return;
                }
                if value["source"].as_str() == Some("manual") {
                    self.active_turn = false;
                    self.active_step = false;
                    self.turn_started_at = 0;
                    self.step_started_at = 0;
                    self.llm_started_at = 0;
                    self.current_turn_id = None;
                    self.pending_tools.clear();
                    self.pending_interactions.clear();
                    self.foreground_observed_at = 0;
                    self.foreground_uncertain_since = 0;
                    self.foreground_stale_since = 0;
                }
                self.last_error = None;
                self.fatal_error_since = 0;
                self.compaction_active = true;
                self.compaction_auto = value["source"].as_str() == Some("auto");
                self.compaction_started_at = time;
                self.compaction_observed_at = time;
                self.compaction_stale_since = 0;
            }
            "full_compaction.complete" | "full_compaction.cancel" => {
                if self.protocol.is_persisted() && !self.compaction_active {
                    self.lifecycle_failure
                        .get_or_insert(StatusReason::ProtocolMalformed);
                    return;
                }
                let ambiguous_auto_cancel =
                    kind == "full_compaction.cancel" && self.compaction_auto;
                self.compaction_active = false;
                self.compaction_auto = false;
                self.compaction_started_at = 0;
                self.compaction_observed_at = 0;
                self.compaction_stale_since = 0;
                if ambiguous_auto_cancel {
                    self.active_turn = false;
                    self.active_step = false;
                    self.turn_started_at = 0;
                    self.step_started_at = 0;
                    self.llm_started_at = 0;
                    self.current_turn_id = None;
                    self.pending_tools.clear();
                    self.pending_interactions.clear();
                    self.foreground_observed_at = 0;
                    self.foreground_stale_since = 0;
                    self.foreground_uncertain_since = time;
                }
            }
            "config.update" | "profile.bind" => {
                if let Some(v) = value["modelAlias"].as_str() {
                    self.model_alias = clean_text(v, 120);
                }
                if let Some(v) = value["thinkingEffort"]
                    .as_str()
                    .or_else(|| value["thinkingLevel"].as_str())
                {
                    self.effort = clean_text(v, 40);
                }
            }
            "usage.record" => {
                let u = &value["usage"];
                let input = u64_field(u, &["inputOther", "input_other"]);
                let output = u64_field(u, &["output", "outputTokens"]);
                let cache_read = u64_field(u, &["inputCacheRead", "input_cache_read"]);
                let cache_create = u64_field(u, &["inputCacheCreation", "input_cache_creation"]);
                self.total_input = self.total_input.saturating_add(input);
                self.total_output = self.total_output.saturating_add(output);
                self.total_cache_read = self.total_cache_read.saturating_add(cache_read);
                self.total_cache_create = self.total_cache_create.saturating_add(cache_create);
                let total = input + output + cache_read + cache_create;
                let scope = value["usageScope"].as_str();
                let belongs_to_turn = scope == Some("turn")
                    || (scope.is_none()
                        && (self.active_turn
                            || self.active_step
                            || self.current_turn_id.is_some()));
                if belongs_to_turn {
                    self.current_turn_tokens = self.current_turn_tokens.saturating_add(total);
                }
                if self.model.is_empty() {
                    if let Some(v) = value["model"].as_str() {
                        self.model = clean_text(v, 120);
                    }
                }
            }
            "turn.prompt" => {
                self.finish_assistant_message();
                self.finish_turn_tokens();
                self.pending_tools.clear();
                self.pending_interactions.clear();
                self.turn_count = self.turn_count.saturating_add(1);
                self.active_turn = true;
                self.active_step = false;
                self.turn_started_at = time;
                self.step_started_at = 0;
                self.llm_started_at = 0;
                self.current_turn_id = None;
                self.last_error = None;
                self.fatal_error_since = 0;
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                self.foreground_observed_at = time;
                let text = content_text(&value["input"]);
                if self.initial_prompt.is_empty() {
                    self.initial_prompt = clean_text(&text, 120);
                }
                push_chat(
                    &mut self.chat_messages,
                    ChatRole::User,
                    clean_text(&text, 500),
                );
            }
            "turn.steer" => {
                self.last_error = None;
                self.fatal_error_since = 0;
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                if !self.active_turn {
                    self.active_turn = true;
                    self.active_step = false;
                    self.turn_started_at = time;
                    self.step_started_at = 0;
                    self.llm_started_at = 0;
                }
                self.foreground_observed_at = time;
            }
            "turn.ended" => {
                self.finish_assistant_message();
                self.active_turn = false;
                self.active_step = false;
                self.turn_started_at = 0;
                self.step_started_at = 0;
                self.llm_started_at = 0;
                self.pending_tools.clear();
                self.pending_interactions.clear();
                self.last_error = None;
                self.fatal_error_since = 0;
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                self.foreground_observed_at = 0;
                if matches!(value["reason"].as_str(), Some("failed" | "blocked")) {
                    let msg = value["error"]["message"]
                        .as_str()
                        .or_else(|| value["error"].as_str())
                        .unwrap_or("Kimi turn failed");
                    self.last_error = Some(clean_text(msg, 160));
                    self.fatal_error_since = time;
                }
            }
            "interaction.request" => {
                let id = value["id"].as_str().expect("validated interaction id");
                let reason = match value["kind"].as_str() {
                    Some("approval") => Some(StatusReason::ProviderWaitingApproval),
                    Some("question") => Some(StatusReason::ProviderWaitingUserInput),
                    Some("user_tool") => None,
                    _ => unreachable!("validated interaction kind"),
                };
                if let Some(reason) = reason {
                    self.pending_interactions.insert(
                        id.to_string(),
                        PendingInteraction {
                            requested_at: time,
                            reason,
                        },
                    );
                }
            }
            "interaction.resolved" => {
                if let Some(id) = value["id"].as_str() {
                    self.pending_interactions.remove(id);
                }
            }
            "context.clear" => {
                self.last_context_tokens = 0;
                push_history(&mut self.context_history, 0);
            }
            "context.update_token_count" => {
                if let Some(tokens) = value["tokenCount"].as_u64() {
                    self.last_context_tokens = tokens;
                    push_history(&mut self.context_history, tokens);
                }
            }
            "context.apply_compaction" => {
                if self.protocol.is_persisted() && !self.compaction_active {
                    self.lifecycle_failure
                        .get_or_insert(StatusReason::ProtocolMalformed);
                    return;
                }
                if self.compaction_active {
                    self.compaction_observed_at = time;
                    self.compaction_stale_since = 0;
                }
                self.compaction_count = self.compaction_count.saturating_add(1);
                if let Some(tokens) = value["tokensAfter"].as_u64() {
                    self.last_context_tokens = tokens;
                    push_history(&mut self.context_history, tokens);
                }
            }
            "turn.started" => {
                self.pending_tools.clear();
                self.pending_interactions.clear();
                self.active_turn = true;
                self.active_step = false;
                self.turn_started_at = time;
                self.step_started_at = 0;
                self.llm_started_at = 0;
                self.last_error = None;
                self.fatal_error_since = 0;
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                self.foreground_observed_at = time;
            }
            "turn.step.started" => {
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                self.foreground_observed_at = time;
                if !self.active_turn {
                    self.active_turn = true;
                    self.turn_started_at = time;
                }
                self.active_step = true;
                self.step_started_at = time;
            }
            "turn.step.completed" | "turn.step.interrupted" => {
                self.active_step = false;
                self.step_started_at = 0;
                self.llm_started_at = 0;
            }
            "context.append_message" => self.apply_message(&value["message"]),
            "context.append_loop_event" => self.apply_loop_event(&value["event"], time),
            "turn.cancel" => self.apply_turn_cancel(value, time),
            "task.started" | "background.task.started" => {
                self.apply_task(&value["info"], true, time)
            }
            "task.terminated" | "background.task.terminated" => {
                self.apply_task(&value["info"], false, time)
            }
            _ => {}
        }
    }

    fn apply_metadata(&mut self, value: &Value) {
        if self.metadata_seen || self.non_metadata_seen {
            self.lifecycle_failure
                .get_or_insert(StatusReason::ProtocolMalformed);
            return;
        }
        self.metadata_seen = true;
        if value["created_at"].as_u64().is_none_or(|time| time == 0) {
            self.lifecycle_failure
                .get_or_insert(StatusReason::ProtocolMalformed);
            return;
        }
        self.protocol = match value["protocol_version"].as_str() {
            Some("1.4") => WireProtocol::V1_4,
            Some(_) => {
                self.lifecycle_failure
                    .get_or_insert(StatusReason::ProtocolUnknown);
                return;
            }
            None => {
                self.lifecycle_failure
                    .get_or_insert(StatusReason::ProtocolMalformed);
                return;
            }
        };
    }

    fn apply_turn_cancel(&mut self, value: &Value, time: u64) {
        if value["target"].as_str() == Some("queued") {
            return;
        }
        if let Some(cancelled_id) = json_id(&value["turnId"]) {
            if self.current_turn_id.as_deref() != Some(cancelled_id.as_str()) {
                return;
            }
        }
        if !self.active_turn
            && !self.active_step
            && self.llm_started_at == 0
            && self.pending_tools.is_empty()
            && self.pending_interactions.is_empty()
        {
            return;
        }
        self.finish_assistant_message();
        self.active_turn = false;
        self.active_step = false;
        self.turn_started_at = 0;
        self.step_started_at = 0;
        self.llm_started_at = 0;
        self.current_turn_id = None;
        self.pending_tools.clear();
        self.pending_interactions.clear();
        self.foreground_uncertain_since = time;
        self.foreground_stale_since = 0;
        self.foreground_observed_at = 0;
    }

    fn apply_message(&mut self, message: &Value) {
        if message["role"].as_str() != Some("assistant") {
            return;
        }
        let text = clean_text(&content_text(&message["content"]), 500);
        if self.first_assistant_text.is_empty() {
            self.first_assistant_text = text.clone();
        }
        push_chat(&mut self.chat_messages, ChatRole::Assistant, text);
    }

    fn apply_loop_event(&mut self, event: &Value, time: u64) {
        match event["type"].as_str() {
            Some("step.begin") => {
                self.finish_assistant_message();
                self.pending_tools.clear();
                self.pending_interactions.clear();
                self.begin_loop_turn(event, time);
                self.active_step = true;
                self.step_started_at = time;
                self.llm_started_at = 0;
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                self.foreground_observed_at = time;
            }
            Some("step.end") => {
                self.finish_assistant_message();
                self.active_step = false;
                self.step_started_at = 0;
                self.llm_started_at = 0;
                self.pending_tools.clear();
                self.pending_interactions.clear();
                if let Some(u) = event.get("usage").filter(|usage| usage.is_object()) {
                    let total = u64_field(u, &["inputOther"])
                        + u64_field(u, &["output"])
                        + u64_field(u, &["inputCacheRead"])
                        + u64_field(u, &["inputCacheCreation"]);
                    if total > 0 {
                        self.last_context_tokens = total;
                        push_history(&mut self.context_history, total);
                    }
                }
                let finish_reason = event["finishReason"].as_str();
                self.active_turn = finish_reason == Some("tool_use");
                if self.active_turn {
                    self.foreground_uncertain_since = 0;
                    self.foreground_stale_since = 0;
                    self.foreground_observed_at = time;
                } else {
                    self.turn_started_at = 0;
                    self.foreground_observed_at = 0;
                    if self.protocol.is_persisted() {
                        self.foreground_uncertain_since = time;
                        self.foreground_stale_since = 0;
                    }
                }
                if self.protocol == WireProtocol::V1_4 && finish_reason == Some("filtered") {
                    self.active_turn = false;
                    self.foreground_uncertain_since = 0;
                    self.foreground_stale_since = 0;
                    self.foreground_observed_at = 0;
                    self.last_error = Some("Kimi response filtered".to_string());
                    self.fatal_error_since = time;
                }
            }
            Some("content.part") => {
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                if !self.active_turn {
                    self.active_turn = true;
                    self.turn_started_at = time;
                }
                self.foreground_observed_at = time;
                self.append_assistant_part(&event["part"]);
            }
            Some("tool.call") => {
                self.llm_started_at = 0;
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                if !self.active_turn {
                    self.active_turn = true;
                    self.turn_started_at = time;
                }
                self.foreground_observed_at = time;
                let id = event["toolCallId"].as_str().unwrap_or("").to_string();
                if id.is_empty() {
                    return;
                }
                let name = clean_text(event["name"].as_str().unwrap_or("tool"), 80);
                let arg = safe_tool_arg(&event["args"]);
                // `background` lets Kimi continue other work while the question is
                // parked; it does not make the user's answer optional. Every live,
                // unresolved AskUserQuestion call is therefore an exact input wait.
                let waits_for_user = name == "AskUserQuestion";
                self.pending_tools.insert(
                    id.clone(),
                    PendingTool {
                        name: name.clone(),
                        arg: arg.clone(),
                        started_at: time,
                        waits_for_user,
                    },
                );
                if self.tool_calls.len() < MAX_TOOL_CALLS {
                    self.tool_indices.insert(id.clone(), self.tool_calls.len());
                    self.tool_calls.push(ToolCall {
                        name: name.clone(),
                        arg: arg.clone(),
                        duration_ms: 0,
                    });
                }
                if let Some(op) = file_op(&name) {
                    if !arg.is_empty() && self.file_accesses.len() < MAX_FILE_ACCESSES {
                        self.file_accesses.push(FileAccess {
                            path: arg,
                            operation: op,
                            turn_index: self.turn_count,
                        });
                    }
                }
            }
            Some("tool.result") => {
                self.foreground_uncertain_since = 0;
                self.foreground_stale_since = 0;
                if !self.active_turn {
                    self.active_turn = true;
                    self.turn_started_at = time;
                }
                self.foreground_observed_at = time;
                if let Some(id) = event["toolCallId"].as_str() {
                    if let Some(pending) = self.pending_tools.remove(id) {
                        if let Some(&idx) = self.tool_indices.get(id) {
                            if let Some(call) = self.tool_calls.get_mut(idx) {
                                call.duration_ms = time.saturating_sub(pending.started_at);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_task(&mut self, info: &Value, started: bool, event_time: u64) {
        let Some(id) = json_id(&info["taskId"]) else {
            return;
        };
        let kind = clean_text(info["kind"].as_str().unwrap_or("task"), 40);
        let status = clean_text(
            info["status"]
                .as_str()
                .unwrap_or(if started { "running" } else { "completed" }),
            40,
        );
        let task_started_at = info["startedAt"].as_u64().unwrap_or(event_time);
        let name = clean_text(
            info["subagentType"]
                .as_str()
                .or_else(|| info["agentId"].as_str())
                .or_else(|| info["description"].as_str())
                .unwrap_or(&id),
            80,
        );
        // Kimi's BackgroundTaskInfo contract normalizes an omitted legacy value to
        // detached=true. A present non-boolean value is not trustworthy lifecycle data.
        let detached = match info.get("detached") {
            Some(Value::Bool(detached)) => *detached,
            None => true,
            Some(_) => {
                self.lifecycle_failure
                    .get_or_insert(StatusReason::ProtocolMalformed);
                return;
            }
        };

        if started && task_status_is_active(&status) {
            self.active_tasks.insert(
                id.clone(),
                ActiveTask {
                    kind: kind.clone(),
                    name: name.clone(),
                    started_at: task_started_at,
                    detached,
                },
            );
        } else {
            self.active_tasks.remove(&id);
        }

        if kind == "agent" {
            self.subagents.insert(
                id,
                SubagentState {
                    agent_id: clean_text(info["agentId"].as_str().unwrap_or(""), 120),
                    name,
                    status,
                    tokens: 0,
                    started_at: task_started_at,
                },
            );
        }
    }

    fn reconcile_task_snapshots(&mut self, snapshots: Vec<TaskSnapshot>) {
        let mut newest = HashMap::<String, TaskSnapshot>::new();
        for snapshot in snapshots {
            let replace = newest
                .get(&snapshot.id)
                .is_none_or(|current| snapshot.modified_at >= current.modified_at);
            if replace {
                newest.insert(snapshot.id.clone(), snapshot);
            }
        }
        for (id, snapshot) in newest {
            self.active_tasks.remove(&id);
            if let Some(task) = snapshot.task {
                self.active_tasks.insert(id.clone(), task);
            }
        }
    }

    fn expire_foreground_lease_at(&mut self, observed_at_ms: u64) {
        if self.protocol != WireProtocol::V1_4 {
            return;
        }
        if self.compaction_active
            && (self.compaction_observed_at == 0
                || observed_at_ms.saturating_sub(self.compaction_observed_at)
                    > V1_NON_WAIT_FOREGROUND_LEASE_MS)
        {
            self.compaction_active = false;
            self.compaction_auto = false;
            self.compaction_started_at = 0;
            self.compaction_observed_at = 0;
            self.compaction_stale_since = observed_at_ms.max(1);
        }
        if self.pending_tools.values().any(|tool| tool.waits_for_user)
            || !self.pending_interactions.is_empty()
        {
            return;
        }
        let has_open_foreground = self.active_turn
            || self.active_step
            || self.llm_started_at > 0
            || !self.pending_tools.is_empty();
        if !has_open_foreground
            || (self.foreground_observed_at > 0
                && observed_at_ms.saturating_sub(self.foreground_observed_at)
                    <= V1_NON_WAIT_FOREGROUND_LEASE_MS)
        {
            return;
        }
        self.active_turn = false;
        self.active_step = false;
        self.turn_started_at = 0;
        self.step_started_at = 0;
        self.llm_started_at = 0;
        self.current_turn_id = None;
        self.pending_tools.clear();
        self.pending_interactions.clear();
        self.foreground_observed_at = 0;
        self.foreground_uncertain_since = 0;
        self.foreground_stale_since = observed_at_ms.max(1);
    }

    fn begin_loop_turn(&mut self, event: &Value, time: u64) {
        self.last_error = None;
        self.fatal_error_since = 0;
        let Some(turn_id) = json_id(&event["turnId"]) else {
            if !self.active_turn {
                self.turn_count = self.turn_count.saturating_add(1);
            }
            self.active_turn = true;
            if self.turn_started_at == 0 {
                self.turn_started_at = time;
            }
            return;
        };

        match self.current_turn_id.as_deref() {
            Some(current) if current == turn_id => {}
            None if self.active_turn => self.current_turn_id = Some(turn_id.to_string()),
            _ => {
                self.finish_turn_tokens();
                self.turn_count = self.turn_count.saturating_add(1);
                self.current_turn_id = Some(turn_id.to_string());
                self.turn_started_at = time;
            }
        }
        self.active_turn = true;
        if self.turn_started_at == 0 {
            self.turn_started_at = time;
        }
    }

    fn append_assistant_part(&mut self, part: &Value) {
        if part["type"].as_str() != Some("text") {
            return;
        }
        let Some(text) = part["text"].as_str() else {
            return;
        };
        let text = clean_text(text, 500);
        if text.is_empty() {
            return;
        }
        let current_len = self.open_assistant_text.chars().count();
        if current_len >= 500 {
            return;
        }
        if !self.open_assistant_text.is_empty() {
            self.open_assistant_text.push(' ');
        }
        let remaining = 500usize.saturating_sub(self.open_assistant_text.chars().count());
        self.open_assistant_text
            .extend(text.chars().take(remaining));
    }

    fn finish_assistant_message(&mut self) {
        let text = clean_text(&std::mem::take(&mut self.open_assistant_text), 500);
        if text.is_empty() {
            return;
        }
        if self.first_assistant_text.is_empty() {
            self.first_assistant_text = text.clone();
        }
        push_chat(&mut self.chat_messages, ChatRole::Assistant, text);
    }

    fn finish_turn_tokens(&mut self) {
        if self.current_turn_tokens == 0 {
            return;
        }
        push_history(&mut self.token_history, self.current_turn_tokens);
        self.current_turn_tokens = 0;
    }

    fn token_history_snapshot(&self) -> Vec<u64> {
        let mut history = self.token_history.clone();
        if self.current_turn_tokens > 0 {
            push_history(&mut history, self.current_turn_tokens);
        }
        history.into_iter().collect()
    }

    fn has_pending_input_since(&self, activity_boundary_ms: u64) -> bool {
        self.pending_input_reason_since(activity_boundary_ms)
            .is_some()
    }

    fn pending_input_reason_since(&self, activity_boundary_ms: u64) -> Option<StatusReason> {
        let interaction_reason = self
            .pending_interactions
            .values()
            .filter(|interaction| interaction.requested_at >= activity_boundary_ms)
            .map(|interaction| interaction.reason)
            .max_by_key(|reason| u8::from(*reason == StatusReason::ProviderWaitingApproval));
        if interaction_reason.is_some() {
            return interaction_reason;
        }
        let tool_wait = self
            .pending_tools
            .values()
            .any(|tool| tool.waits_for_user && tool.started_at >= activity_boundary_ms)
            || self
                .active_tasks
                .values()
                .any(|task| task.kind == "question" && task.started_at >= activity_boundary_ms);
        tool_wait.then_some(StatusReason::ProviderWaitingUserInput)
    }

    fn pending_input_since(&self, activity_boundary_ms: u64) -> u64 {
        self.pending_interactions
            .values()
            .filter_map(|interaction| {
                (interaction.requested_at >= activity_boundary_ms)
                    .then_some(interaction.requested_at)
            })
            .chain(self.pending_tools.values().filter_map(|tool| {
                (tool.waits_for_user && tool.started_at >= activity_boundary_ms)
                    .then_some(tool.started_at)
            }))
            .chain(self.active_tasks.values().filter_map(|task| {
                (task.kind == "question" && task.started_at >= activity_boundary_ms)
                    .then_some(task.started_at)
            }))
            .min()
            .unwrap_or(0)
    }

    fn fatal_error_since(&self, activity_boundary_ms: u64) -> u64 {
        if self.fatal_error_since > 0 && self.fatal_error_since >= activity_boundary_ms {
            self.fatal_error_since
        } else {
            0
        }
    }

    fn has_executing_work_since(&self, activity_boundary_ms: u64) -> bool {
        self.pending_tools
            .values()
            .any(|tool| tool.started_at >= activity_boundary_ms)
            || self
                .active_tasks
                .values()
                .any(|task| task.kind != "question" && task.started_at >= activity_boundary_ms)
    }

    fn has_foreground_uncertainty_since(&self, activity_boundary_ms: u64) -> bool {
        (self.foreground_uncertain_since >= activity_boundary_ms
            && self.foreground_uncertain_since > 0)
            || (self.foreground_stale_since >= activity_boundary_ms
                && self.foreground_stale_since > 0)
            || (self.compaction_stale_since >= activity_boundary_ms
                && self.compaction_stale_since > 0)
    }

    fn foreground_uncertainty_reason_since(
        &self,
        activity_boundary_ms: u64,
    ) -> Option<StatusReason> {
        if (self.foreground_stale_since >= activity_boundary_ms && self.foreground_stale_since > 0)
            || (self.compaction_stale_since >= activity_boundary_ms
                && self.compaction_stale_since > 0)
        {
            Some(StatusReason::Stale)
        } else if self.foreground_uncertain_since >= activity_boundary_ms
            && self.foreground_uncertain_since > 0
        {
            Some(StatusReason::ProtocolUnknown)
        } else {
            None
        }
    }

    fn has_thinking_work_since(&self, activity_boundary_ms: u64) -> bool {
        (self.active_turn && self.turn_started_at >= activity_boundary_ms)
            || (self.active_step && self.step_started_at >= activity_boundary_ms)
            || (self.llm_started_at > 0 && self.llm_started_at >= activity_boundary_ms)
            || (self.compaction_active && self.compaction_started_at >= activity_boundary_ms)
    }

    fn has_live_activity_since(&self, activity_boundary_ms: u64) -> bool {
        self.has_pending_input_since(activity_boundary_ms)
            || self.has_executing_work_since(activity_boundary_ms)
            || self.has_thinking_work_since(activity_boundary_ms)
    }

    fn execution_labels_since(&self, activity_boundary_ms: u64) -> Vec<String> {
        let mut labels = self
            .pending_tools
            .values()
            .filter(|tool| tool.started_at >= activity_boundary_ms)
            .map(PendingTool::label)
            .chain(
                self.active_tasks
                    .values()
                    .filter(|task| {
                        task.kind != "question" && task.started_at >= activity_boundary_ms
                    })
                    .map(ActiveTask::label),
            )
            .collect::<Vec<_>>();
        labels.sort();
        labels.dedup();
        labels
    }

    fn pending_since(&self, activity_boundary_ms: u64) -> u64 {
        self.pending_tools
            .values()
            .filter_map(|tool| (tool.started_at >= activity_boundary_ms).then_some(tool.started_at))
            .chain(self.active_tasks.values().filter_map(|task| {
                (task.kind != "question" && task.started_at >= activity_boundary_ms)
                    .then_some(task.started_at)
            }))
            .chain(
                self.pending_interactions
                    .values()
                    .filter_map(|interaction| {
                        (interaction.requested_at >= activity_boundary_ms)
                            .then_some(interaction.requested_at)
                    }),
            )
            .min()
            .unwrap_or(0)
    }

    fn thinking_since(&self, activity_boundary_ms: u64) -> u64 {
        [
            self.active_turn.then_some(self.turn_started_at),
            self.active_step.then_some(self.step_started_at),
            (self.llm_started_at > 0).then_some(self.llm_started_at),
            self.compaction_active.then_some(self.compaction_started_at),
        ]
        .into_iter()
        .flatten()
        .filter(|started_at| *started_at >= activity_boundary_ms)
        .min()
        .unwrap_or(0)
    }

    fn activity_since(&self, activity_boundary_ms: u64) -> u64 {
        [
            self.pending_since(activity_boundary_ms),
            self.thinking_since(activity_boundary_ms),
        ]
        .into_iter()
        .filter(|started_at| *started_at > 0)
        .min()
        .unwrap_or(0)
    }
}

fn wire_session_status<'a>(
    wire: &WireState,
    child_wires: impl IntoIterator<Item = &'a WireState>,
    activity_boundary_ms: u64,
    ambiguous: bool,
    active_child_process: bool,
) -> (SessionStatus, bool) {
    let children = child_wires.into_iter().collect::<Vec<_>>();
    if wire.lifecycle_failure.is_some()
        || children
            .iter()
            .any(|child| child.lifecycle_failure.is_some())
    {
        return (SessionStatus::Unknown, false);
    }
    if ambiguous {
        return (SessionStatus::Unknown, false);
    }

    let awaiting_input = wire.has_pending_input_since(activity_boundary_ms)
        || children
            .iter()
            .any(|child| child.has_pending_input_since(activity_boundary_ms));
    let foreground_uncertain = wire.has_foreground_uncertainty_since(activity_boundary_ms)
        || children
            .iter()
            .any(|child| child.has_foreground_uncertainty_since(activity_boundary_ms));
    let status = if awaiting_input {
        SessionStatus::Waiting
    } else if wire.fatal_error_since(activity_boundary_ms) > 0
        || children
            .iter()
            .any(|child| child.fatal_error_since(activity_boundary_ms) > 0)
    {
        SessionStatus::Error
    } else if wire.has_executing_work_since(activity_boundary_ms)
        || children
            .iter()
            .any(|child| child.has_live_activity_since(activity_boundary_ms))
        || active_child_process
    {
        SessionStatus::Executing
    } else if wire.has_thinking_work_since(activity_boundary_ms) {
        SessionStatus::Thinking
    } else if foreground_uncertain {
        SessionStatus::Unknown
    } else {
        SessionStatus::Idle
    };
    (status, awaiting_input)
}

fn lifecycle_timestamps<'a>(
    status: SessionStatus,
    wire: &WireState,
    child_wires: impl IntoIterator<Item = &'a WireState>,
    activity_boundary_ms: u64,
) -> (u64, u64) {
    match status {
        SessionStatus::Executing => {
            let pending_since_ms = std::iter::once(wire.pending_since(activity_boundary_ms))
                .chain(
                    child_wires
                        .into_iter()
                        .map(|child| child.activity_since(activity_boundary_ms)),
                )
                .filter(|started_at| *started_at > 0)
                .min()
                .unwrap_or(0);
            (pending_since_ms, 0)
        }
        SessionStatus::Thinking => (0, wire.thinking_since(activity_boundary_ms)),
        _ => (0, 0),
    }
}

fn kimi_status_evidence<'a>(
    status: SessionStatus,
    wire: &WireState,
    child_wires: impl IntoIterator<Item = &'a WireState>,
    activity_boundary_ms: u64,
    active_child_process: bool,
    pairing_authority: StatusAuthority,
    observed_at_ms: u64,
) -> StatusEvidence {
    let children = child_wires.into_iter().collect::<Vec<_>>();
    let lifecycle_failure = wire
        .lifecycle_failure
        .or_else(|| children.iter().find_map(|child| child.lifecycle_failure));
    let (mut authority, mut reason) = if let Some(reason) = lifecycle_failure {
        (StatusAuthority::Unavailable, reason)
    } else if status == SessionStatus::Unknown {
        let reason = std::iter::once(wire)
            .chain(children.iter().copied())
            .filter_map(|state| state.foreground_uncertainty_reason_since(activity_boundary_ms))
            .max_by_key(|reason| u8::from(*reason == StatusReason::Stale))
            .unwrap_or(StatusReason::OwnershipUnconfirmed);
        (StatusAuthority::Unavailable, reason)
    } else if pairing_authority == StatusAuthority::Unavailable {
        (
            StatusAuthority::Unavailable,
            StatusReason::OwnershipUnconfirmed,
        )
    } else {
        match status {
            SessionStatus::Waiting => {
                let reason = wire
                    .pending_input_reason_since(activity_boundary_ms)
                    .or_else(|| {
                        children.iter().find_map(|child| {
                            child.pending_input_reason_since(activity_boundary_ms)
                        })
                    })
                    .unwrap_or(StatusReason::ProviderWaitingUserInput);
                (StatusAuthority::Provider, reason)
            }
            SessionStatus::Executing
                if wire.has_executing_work_since(activity_boundary_ms)
                    || children
                        .iter()
                        .any(|child| child.has_live_activity_since(activity_boundary_ms)) =>
            {
                (StatusAuthority::Provider, StatusReason::ProviderExecuting)
            }
            SessionStatus::Executing if active_child_process => (
                StatusAuthority::Heuristic,
                StatusReason::BackgroundTerminalActive,
            ),
            SessionStatus::Thinking => (StatusAuthority::Provider, StatusReason::ProviderThinking),
            SessionStatus::Error => (StatusAuthority::Provider, StatusReason::ProviderError),
            SessionStatus::Idle => (StatusAuthority::Provider, StatusReason::ProviderIdle),
            _ => (StatusAuthority::Unavailable, StatusReason::Unavailable),
        }
    };

    if authority == StatusAuthority::Provider && pairing_authority == StatusAuthority::Heuristic {
        authority = StatusAuthority::Heuristic;
        reason = StatusReason::CollectorInference;
    }
    if authority == StatusAuthority::Provider
        && matches!(
            status,
            SessionStatus::Thinking | SessionStatus::Executing | SessionStatus::Idle
        )
        && std::iter::once(wire)
            .chain(children.iter().copied())
            .any(|state| state.protocol == WireProtocol::V1_4)
    {
        authority = StatusAuthority::Heuristic;
        reason = StatusReason::CollectorInference;
    }

    let mut evidence = StatusEvidence::default();
    evidence.observe(StatusObservation::new(
        status,
        authority,
        reason,
        observed_at_ms,
        0,
    ));
    let exact_since = match status {
        SessionStatus::Waiting => std::iter::once(wire.pending_input_since(activity_boundary_ms))
            .chain(
                children
                    .iter()
                    .map(|child| child.pending_input_since(activity_boundary_ms)),
            )
            .filter(|since| *since > 0)
            .min()
            .unwrap_or(0),
        SessionStatus::Error => std::iter::once(wire.fatal_error_since(activity_boundary_ms))
            .chain(
                children
                    .iter()
                    .map(|child| child.fatal_error_since(activity_boundary_ms)),
            )
            .filter(|since| *since > 0)
            .min()
            .unwrap_or(0),
        SessionStatus::Executing => std::iter::once(wire.pending_since(activity_boundary_ms))
            .chain(
                children
                    .iter()
                    .map(|child| child.activity_since(activity_boundary_ms)),
            )
            .filter(|since| *since > 0)
            .min()
            .unwrap_or(0),
        SessionStatus::Thinking => wire.thinking_since(activity_boundary_ms),
        _ => 0,
    };
    if exact_since > 0 {
        evidence.status_since_ms = exact_since;
    }
    evidence
}

fn task_status_is_active(status: &str) -> bool {
    matches!(status.to_ascii_lowercase().as_str(), "running" | "queued")
}

fn read_task_snapshots(
    session_dir: &Path,
    task_dirs: &[PathBuf],
    activity_boundary_ms: u64,
) -> Result<Vec<TaskSnapshot>, StatusReason> {
    let mut snapshots = Vec::new();
    for task_dir in task_dirs {
        let relative = task_dir
            .strip_prefix(session_dir)
            .map_err(|_| StatusReason::ProtocolMalformed)?;
        if relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) || has_symlink_component(session_dir, task_dir)
        {
            return Err(StatusReason::ProtocolMalformed);
        }
        let entries = match fs::read_dir(task_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(StatusReason::Unavailable),
        };
        let mut json_count = 0usize;
        for entry in entries {
            let entry = entry.map_err(|_| StatusReason::Unavailable)?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            json_count = json_count.saturating_add(1);
            if json_count > MAX_TASK_FILES {
                return Err(StatusReason::Stale);
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| StatusReason::Unavailable)?;
            let modified_at = metadata_modified_ms(&metadata).unwrap_or(0);
            let current = modified_at >= activity_boundary_ms;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > MAX_TASK_BYTES
            {
                if current {
                    return Err(StatusReason::ProtocolMalformed);
                }
                continue;
            }
            let bytes = fs::read(&path).map_err(|_| StatusReason::Unavailable)?;
            let value = match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) => value,
                Err(_) if !current => continue,
                Err(_) => return Err(StatusReason::ProtocolMalformed),
            };
            match parse_task_snapshot(&path, &value, modified_at, activity_boundary_ms) {
                Ok(Some(snapshot)) => snapshots.push(snapshot),
                Ok(None) => {}
                Err(_) if !current => {}
                Err(reason) => return Err(reason),
            }
        }
    }
    Ok(snapshots)
}

fn parse_task_snapshot(
    path: &Path,
    value: &Value,
    modified_at: u64,
    activity_boundary_ms: u64,
) -> Result<Option<TaskSnapshot>, StatusReason> {
    let file_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|id| valid_task_id(id))
        .ok_or(StatusReason::ProtocolMalformed)?;
    let id = value["taskId"]
        .as_str()
        .or_else(|| value["task_id"].as_str())
        .filter(|id| *id == file_id)
        .ok_or(StatusReason::ProtocolMalformed)?;
    let kind = value["kind"]
        .as_str()
        .filter(|kind| matches!(*kind, "process" | "agent" | "question"))
        .ok_or(StatusReason::ProtocolMalformed)?;
    let status = value["status"]
        .as_str()
        .filter(|status| {
            matches!(
                *status,
                "running" | "completed" | "failed" | "timed_out" | "killed" | "lost"
            )
        })
        .ok_or(StatusReason::ProtocolMalformed)?;
    let started_at = u64_field(value, &["startedAt", "started_at"]);
    if started_at == 0 {
        return Err(StatusReason::ProtocolMalformed);
    }
    let ended_at = u64_field(value, &["endedAt", "ended_at"]);
    if (status == "running" && ended_at != 0) || (status != "running" && ended_at == 0) {
        return Err(StatusReason::ProtocolMalformed);
    }
    if started_at < activity_boundary_ms {
        return Ok(None);
    }
    // Current snapshots carry `detached`; Kimi explicitly defines omitted legacy
    // values as detached, while any present non-boolean value is malformed.
    let detached = match value.get("detached") {
        Some(Value::Bool(detached)) => *detached,
        None => true,
        Some(_) => return Err(StatusReason::ProtocolMalformed),
    };
    let task = (status == "running").then(|| ActiveTask {
        kind: kind.to_string(),
        name: clean_text(
            value["subagentType"]
                .as_str()
                .or_else(|| value["subagent_type"].as_str())
                .or_else(|| value["agentId"].as_str())
                .or_else(|| value["agent_id"].as_str())
                .or_else(|| value["description"].as_str())
                .unwrap_or(id),
            80,
        ),
        started_at,
        detached,
    });
    Ok(Some(TaskSnapshot {
        id: id.to_string(),
        task,
        modified_at,
    }))
}

fn valid_task_id(id: &str) -> bool {
    let mut parts = id.split('-').peekable();
    let mut prefix_parts = 0usize;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return prefix_parts > 0
                && part.len() == 8
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        }
        if part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return false;
        }
        prefix_parts += 1;
    }
    false
}

fn select_session<'a>(
    process: &KimiProcess,
    candidates: &[&'a KimiSession],
    assignment: Option<&KimiAssignment>,
    claimed: &HashSet<PathBuf>,
    follow_fresher_activity: bool,
) -> Option<&'a KimiSession> {
    if let Some(explicit) = process.explicit_session.as_deref() {
        if let Some(session) = candidates
            .iter()
            .copied()
            .filter(|session| !claimed.contains(&session.dir))
            .find(|session| session.id == explicit)
        {
            return Some(session);
        }
    }

    let freshest = candidates
        .iter()
        .copied()
        .filter(|session| !claimed.contains(&session.dir))
        .max_by_key(|session| session.updated_at);
    let assigned = assignment.and_then(|assignment| {
        candidates
            .iter()
            .copied()
            .filter(|session| !claimed.contains(&session.dir))
            .find(|session| session.dir == assignment.dir)
    });

    match (assigned, freshest) {
        (Some(current), Some(newest))
            if follow_fresher_activity
                && newest.updated_at > current.updated_at
                && assignment.is_some_and(|assignment| {
                    newest.updated_at > assignment.activity_boundary_ms
                }) =>
        {
            Some(newest)
        }
        (Some(current), _) => Some(current),
        (None, newest) => newest,
    }
}

#[cfg(test)]
fn pairing_is_confirmed(
    process: &KimiProcess,
    session: &KimiSession,
    previous: Option<&KimiAssignment>,
) -> bool {
    pairing_authority(process, session, previous) != StatusAuthority::Unavailable
}

fn pairing_authority(
    process: &KimiProcess,
    session: &KimiSession,
    previous: Option<&KimiAssignment>,
) -> StatusAuthority {
    if process.explicit_session.as_deref() == Some(session.id.as_str()) {
        return if process.bare_title {
            StatusAuthority::Heuristic
        } else {
            StatusAuthority::Provider
        };
    }
    if let Some(assignment) = previous.filter(|assignment| {
        assignment.confirmed
            && assignment.dir == session.dir
            && assignment.process_incarnation == process.incarnation
    }) {
        return if process.bare_title && assignment.authority == StatusAuthority::Provider {
            StatusAuthority::Heuristic
        } else {
            assignment.authority
        };
    }
    if process
        .started_at
        .is_some_and(|started_at| session.updated_at >= started_at)
        || previous.is_some_and(|assignment| {
            assignment.dir == session.dir
                && assignment.process_incarnation == process.incarnation
                && session.updated_at > assignment.activity_boundary_ms
        })
    {
        StatusAuthority::Heuristic
    } else {
        StatusAuthority::Unavailable
    }
}

fn session_matches_process(session: &KimiSession, process: &KimiProcess) -> bool {
    !session.archived
        && session.root == process.root
        && normalize_path(&session.cwd) == normalize_path(&process.cwd)
}

fn process_root(pid: u32, cwd: &str) -> Option<PathBuf> {
    let configured = process::read_process_env_var(pid, "KIMI_CODE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("KIMI_CODE_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        });
    configured
        .or_else(|| dirs::home_dir().map(|home| home.join(".kimi-code")))
        .map(|root| absolute_root(root, Path::new(cwd)))
}

fn absolute_root(root: PathBuf, base: &Path) -> PathBuf {
    if root.is_absolute() {
        root
    } else {
        base.join(root)
    }
}

fn default_roots() -> Vec<PathBuf> {
    let configured = std::env::var_os("KIMI_CODE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let root = configured.or_else(|| dirs::home_dir().map(|home| home.join(".kimi-code")));
    let Some(root) = root else { return Vec::new() };
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    vec![absolute_root(root, &base)]
}

pub(crate) fn is_kimi_process(command: &str) -> bool {
    let tokens = process::command_tokens(command);
    is_kimi_process_tokens(&tokens)
}

pub(crate) fn is_kimi_process_tokens(tokens: &[String]) -> bool {
    let direct = tokens.first().is_some_and(|token| {
        process::token_has_binary(token, "kimi") || process::token_has_binary(token, "kimi-code")
    });
    let node_entrypoint = tokens.len() >= 2
        && tokens.first().is_some_and(|token| {
            process::token_has_binary(token, "node") || process::token_has_binary(token, "nodejs")
        })
        && is_kimi_node_entrypoint(&tokens[1]);
    let recognized = direct || node_entrypoint;
    recognized
        && !kimi_subcommand(tokens, if node_entrypoint { 2 } else { 1 })
            .is_some_and(is_kimi_non_session_command)
}

fn kimi_process_observation_is_exact(
    expected_incarnation: &str,
    current_incarnation: Option<&str>,
    tokens: &[String],
) -> bool {
    current_incarnation == Some(expected_incarnation) && is_kimi_process_tokens(tokens)
}

fn is_bare_kimi_tokens(tokens: &[String]) -> bool {
    tokens.len() == 1
        && tokens.first().is_some_and(|token| {
            process::token_has_binary(token, "kimi")
                || process::token_has_binary(token, "kimi-code")
        })
}

fn is_kimi_node_entrypoint(value: &str) -> bool {
    let normalized = value.replace('\\', "/").to_ascii_lowercase();
    normalized == "@moonshot-ai/kimi-code/dist/main.mjs"
        || normalized == "@moonshot-ai/kimi-code/dist/main.js"
        || normalized.ends_with("/@moonshot-ai/kimi-code/dist/main.mjs")
        || normalized.ends_with("/@moonshot-ai/kimi-code/dist/main.js")
}

fn kimi_subcommand(tokens: &[String], mut index: usize) -> Option<&str> {
    while let Some(token) = tokens.get(index) {
        if token == "--" {
            return None;
        }
        if is_kimi_prompt_option(token) {
            // `ps` may flatten a multi-word prompt into later whitespace
            // tokens. Prompt mode owns the rest of the invocation, so those
            // words must never be reinterpreted as `web`, `acp`, or a helper.
            return None;
        }
        if option_has_inline_value(token) || is_kimi_boolean_option(token) {
            index += 1;
            continue;
        }
        if is_kimi_value_option(token) {
            index += 2;
            continue;
        }
        if token.starts_with('-') {
            // Unknown options are rejected by Kimi. Treat them as flags here
            // so arbitrary following prompt text cannot become a host mode.
            index += 1;
            continue;
        }
        return Some(token);
    }
    None
}

fn is_kimi_prompt_option(value: &str) -> bool {
    matches!(value, "-p" | "--prompt") || value.starts_with("-p=") || value.starts_with("--prompt=")
}

fn is_kimi_non_session_command(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "export"
            | "provider"
            | "acp"
            | "web"
            | "server"
            | "login"
            | "doctor"
            | "vis"
            | "migrate"
            | "upgrade"
            | "update"
            | "__plugin_run_node"
    )
}

fn option_has_inline_value(value: &str) -> bool {
    value.starts_with("--") && value.contains('=')
}

fn is_kimi_value_option(value: &str) -> bool {
    matches!(
        value,
        "-S" | "--session"
            | "-r"
            | "--resume"
            | "-m"
            | "--model"
            | "-p"
            | "--prompt"
            | "--output-format"
            | "--skills-dir"
            | "--agent"
            | "--agent-file"
            | "--add-dir"
    )
}

fn is_kimi_boolean_option(value: &str) -> bool {
    matches!(
        value,
        "-c" | "--continue"
            | "-C"
            | "-y"
            | "--yolo"
            | "--auto"
            | "--yes"
            | "--auto-approve"
            | "--plan"
            | "-h"
            | "--help"
            | "-V"
            | "--version"
    )
}

#[cfg(test)]
fn explicit_session_id(command: &str) -> Option<String> {
    let parts = process::command_tokens(command);
    explicit_session_id_from_tokens(&parts)
}

fn explicit_session_id_from_tokens(parts: &[String]) -> Option<String> {
    for (i, part) in parts.iter().enumerate() {
        if let Some(id) = part
            .strip_prefix("--session=")
            .or_else(|| part.strip_prefix("--resume="))
            .or_else(|| part.strip_prefix("-r="))
            .or_else(|| part.strip_prefix("-S="))
        {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        if matches!(part.as_str(), "--session" | "--resume" | "-r" | "-S") {
            if let Some(id) = parts.get(i + 1) {
                if !id.starts_with('-') {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

fn read_session_index(root: &Path) -> Vec<KimiSession> {
    let index_path = root.join("session_index.jsonl");
    let sessions_root = root.join("sessions");
    let mut entries = HashMap::<String, (PathBuf, String)>::new();
    let mut tombstoned = HashSet::new();
    if !is_symlink(&index_path) {
        if let Ok(meta) = fs::metadata(&index_path) {
            if meta.len() <= MAX_INDEX_BYTES {
                if let Ok(text) = fs::read_to_string(&index_path) {
                    for line in text.lines() {
                        if line.len() > MAX_WIRE_LINE_BYTES {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        let Some(id) = value["sessionId"]
                            .as_str()
                            .filter(|s| !s.is_empty() && s.len() <= 256)
                        else {
                            continue;
                        };
                        if value["deleted"].as_bool() == Some(true) {
                            entries.remove(id);
                            tombstoned.insert(id.to_string());
                            continue;
                        }
                        let Some(dir) = value["sessionDir"].as_str().map(PathBuf::from) else {
                            continue;
                        };
                        let cwd = value["workDir"].as_str().unwrap_or("").to_string();
                        if valid_session_dir(&sessions_root, &dir, id) {
                            tombstoned.remove(id);
                            entries.insert(id.to_string(), (dir, cwd));
                        }
                    }
                }
            }
        }
    }

    // The index is an optimization rather than the only source of truth. Kimi
    // itself falls back to directory enumeration when it is absent or stale.
    // Preserve explicit deletion tombstones while adding valid unindexed dirs.
    if !is_symlink(&sessions_root) {
        if let Ok(workdirs) = fs::read_dir(&sessions_root) {
            for workdir in workdirs.flatten().take(MAX_SESSIONS * 2) {
                if is_symlink(&workdir.path()) {
                    continue;
                }
                let Ok(session_dirs) = fs::read_dir(workdir.path()) else {
                    continue;
                };
                for entry in session_dirs.flatten().take(MAX_SESSIONS * 2) {
                    let dir = entry.path();
                    if is_symlink(&dir) || !dir.is_dir() {
                        continue;
                    }
                    let Some(id) = dir.file_name().and_then(|v| v.to_str()) else {
                        continue;
                    };
                    if id.is_empty()
                        || id.len() > 256
                        || tombstoned.contains(id)
                        || entries.contains_key(id)
                        || !valid_session_dir(&sessions_root, &dir, id)
                    {
                        continue;
                    }
                    entries.insert(id.to_string(), (dir, String::new()));
                }
            }
        }
    }

    entries
        .into_iter()
        .filter_map(|(id, (dir, cwd))| read_session_state(root, id, dir, cwd))
        .collect()
}

fn valid_session_dir(sessions_root: &Path, dir: &Path, id: &str) -> bool {
    let safe_relative = dir.strip_prefix(sessions_root).is_ok_and(|relative| {
        !relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
    });
    dir.is_absolute()
        && dir.starts_with(sessions_root)
        && safe_relative
        && dir.file_name().is_some_and(|v| v == id)
        && !has_symlink_component(sessions_root, dir)
}

fn read_session_state(
    root: &Path,
    id: String,
    dir: PathBuf,
    indexed_cwd: String,
) -> Option<KimiSession> {
    let dir_meta = fs::metadata(&dir).ok()?;
    let (_state_path, state_meta, value) = read_state_document(&dir)?;
    let state_id = value["id"].as_str().unwrap_or(&id);
    if state_id != id {
        return None;
    }
    let cwd = value["cwd"]
        .as_str()
        .or_else(|| value["workDir"].as_str())
        .or_else(|| value["custom"]["cwd"].as_str())
        .unwrap_or(&indexed_cwd);
    if cwd.is_empty() {
        return None;
    }
    let parsed_created_at = parse_time(&value["createdAt"]);
    let created_at = if parsed_created_at > 0 {
        parsed_created_at
    } else {
        metadata_created_ms(&dir_meta)
            .or_else(|| metadata_modified_ms(&dir_meta))
            .unwrap_or(0)
    };
    let mut updated_at = parse_time(&value["updatedAt"])
        .max(created_at)
        .max(metadata_modified_ms(&dir_meta).unwrap_or(0))
        .max(metadata_modified_ms(&state_meta).unwrap_or(0));
    let agents: Vec<KimiAgent> = value["agents"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.iter())
        .map(|(id, v)| KimiAgent {
            id: id.clone(),
            kind: v["type"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    for wire_path in [dir.join("wire.jsonl")].into_iter().chain(
        std::iter::once("main")
            .chain(agents.iter().map(|agent| agent.id.as_str()))
            .map(|agent_id| dir.join("agents").join(agent_id).join("wire.jsonl")),
    ) {
        if !is_symlink(&wire_path) {
            if let Some(modified) = fs::metadata(wire_path)
                .ok()
                .as_ref()
                .and_then(metadata_modified_ms)
            {
                updated_at = updated_at.max(modified);
            }
        }
    }
    Some(KimiSession {
        id,
        dir,
        root: root.to_path_buf(),
        cwd: cwd.to_string(),
        title: value["isCustomTitle"]
            .as_bool()
            .and_then(|_| value["title"].as_str())
            .or_else(|| value["customTitle"].as_str())
            .or_else(|| value["title"].as_str())
            .or_else(|| value["lastPrompt"].as_str())
            .unwrap_or("")
            .to_string(),
        created_at,
        updated_at,
        archived: value["archived"].as_bool().unwrap_or(false),
        agents,
    })
}

fn read_state_document(dir: &Path) -> Option<(PathBuf, fs::Metadata, Value)> {
    [dir.join("state.json"), dir.join("session-meta/state.json")]
        .into_iter()
        .find_map(|path| {
            if has_symlink_component(dir, &path) {
                return None;
            }
            let meta = fs::metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > MAX_STATE_BYTES {
                return None;
            }
            let value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            Some((path, meta, value))
        })
}

fn metadata_created_ms(meta: &fs::Metadata) -> Option<u64> {
    system_time_ms(meta.created().ok()?)
}

fn metadata_modified_ms(meta: &fs::Metadata) -> Option<u64> {
    system_time_ms(meta.modified().ok()?)
}

fn system_time_ms(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn current_time_ms() -> u64 {
    system_time_ms(std::time::SystemTime::now()).unwrap_or(0)
}

fn parse_time(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .and_then(|d| u64::try_from(d.timestamp_millis()).ok())
        })
        .unwrap_or(0)
}

fn model_context_limit(root: &Path, alias: &str, model: &str) -> u64 {
    let path = root.join("config.toml");
    if is_symlink(&path) {
        return 0;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return 0;
    };
    if text.len() > MAX_STATE_BYTES as usize {
        return 0;
    }
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return 0;
    };
    let models = value.get("models").and_then(toml::Value::as_table);
    let entry = models
        .and_then(|m| m.get(alias))
        .or_else(|| models.and_then(|m| m.get(model)))
        .or_else(|| unique_model_entry(models, model));
    let Some(entry) = entry else { return 0 };
    let overrides = entry.get("overrides");
    let max_input = overrides
        .and_then(|v| v.get("max_input_size"))
        .and_then(toml::Value::as_integer)
        .or_else(|| {
            entry
                .get("max_input_size")
                .and_then(toml::Value::as_integer)
        });
    let max_context = overrides
        .and_then(|v| v.get("max_context_size"))
        .and_then(toml::Value::as_integer)
        .or_else(|| {
            entry
                .get("max_context_size")
                .and_then(toml::Value::as_integer)
        });
    let max_input = max_input.and_then(|v| u64::try_from(v).ok());
    let max_context = max_context.and_then(|v| u64::try_from(v).ok());
    match (max_input, max_context) {
        (Some(input), Some(context)) => input.min(context),
        (Some(input), None) => input,
        (None, Some(context)) => context,
        (None, None) => 0,
    }
}

fn unique_model_entry<'a>(
    models: Option<&'a toml::map::Map<String, toml::Value>>,
    model: &str,
) -> Option<&'a toml::Value> {
    if model.is_empty() {
        return None;
    }
    let mut matches = models?.values().filter(|entry| {
        entry
            .get("model")
            .and_then(toml::Value::as_str)
            .is_some_and(|configured| configured == model)
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn collect_children(pid: u32, shared: &super::SharedProcessData) -> Vec<ChildProcess> {
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
                    .and_then(|p| p.first().copied()),
            });
        }
        if let Some(children) = shared.children_map.get(&child_pid) {
            stack.extend(children);
        }
    }
    out
}

fn safe_tool_arg(args: &Value) -> String {
    for key in ["file_path", "path", "cwd", "directory"] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            return clean_text(value, 120);
        }
    }
    String::new()
}

fn file_op(name: &str) -> Option<FileOp> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("read") {
        Some(FileOp::Read)
    } else if lower.contains("edit") {
        Some(FileOp::Edit)
    } else if lower.contains("write") || lower.contains("create") {
        Some(FileOp::Write)
    } else {
        None
    }
}

fn content_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value
        .as_array()
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|part| {
            if part["type"].as_str().is_none_or(|t| t == "text") {
                part["text"].as_str()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_text(text: &str, max: usize) -> String {
    let safe = sanitize_terminal_text(text);
    let redacted = redact_secrets(&safe);
    redacted
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
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

fn u64_field(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| value[*key].as_u64())
        .unwrap_or(0)
}

fn json_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(ToString::to_string)
        .or_else(|| value.as_u64().map(|id| id.to_string()))
}

fn push_history(history: &mut VecDeque<u64>, value: u64) {
    if history.len() == MAX_HISTORY_POINTS {
        history.pop_front();
    }
    history.push_back(value);
}

fn normalize_path(path: &str) -> String {
    Path::new(path)
        .components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .into_owned()
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
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

    fn status_at(wire: &WireState, activity_boundary_ms: u64) -> (SessionStatus, bool) {
        wire_session_status(wire, std::iter::empty(), activity_boundary_ms, false, false)
    }

    fn evidence_at(
        wire: &WireState,
        activity_boundary_ms: u64,
        pairing_authority: StatusAuthority,
        active_child_process: bool,
    ) -> (SessionStatus, bool, StatusEvidence) {
        let ambiguous = pairing_authority == StatusAuthority::Unavailable;
        let (status, awaiting_input) = wire_session_status(
            wire,
            std::iter::empty(),
            activity_boundary_ms,
            ambiguous,
            active_child_process,
        );
        let evidence = kimi_status_evidence(
            status,
            wire,
            std::iter::empty(),
            activity_boundary_ms,
            active_child_process,
            pairing_authority,
            50,
        );
        (status, awaiting_input, evidence)
    }

    #[test]
    fn recognizes_current_processes_and_excludes_hosts() {
        assert!(is_kimi_process("kimi-code"));
        assert!(is_kimi_process("kimi -r abc"));
        assert!(is_kimi_process(
            "node /x/@moonshot-ai/kimi-code/dist/main.mjs"
        ));
        assert!(!is_kimi_process("kimi __plugin_run_node"));
        assert!(!is_kimi_process("kimi acp"));
        assert!(!is_kimi_process("kimi web"));
        assert!(!is_kimi_process("bash -lc kimi"));
        assert!(!is_kimi_process(
            "cat /x/@moonshot-ai/kimi-code/dist/main.mjs"
        ));
        assert!(!is_kimi_process("node /tmp/kimi-code/dist/main.mjs"));
        assert!(!is_kimi_process(
            "node /x/@moonshot-ai/kimi-code/dist/main.mjs web"
        ));
        assert!(!is_kimi_process(
            r#"node.exe "C:\Program Files\node_modules\@moonshot-ai\kimi-code\dist\main.mjs" acp"#
        ));
        assert!(!is_kimi_process(
            "node /x/@moonshot-ai/kimi-code/dist/main.mjs __plugin_run_node"
        ));
        assert!(!is_kimi_process(
            "node server.js --banner=/x/@moonshot-ai/kimi-code/dist/main.mjs"
        ));
        assert!(is_kimi_process("kimi -p web"));
        assert!(is_kimi_process("kimi -p build web UI"));
        assert!(is_kimi_process("kimi --prompt \"build web UI\""));
        assert!(is_kimi_process("kimi --prompt=build web UI"));
        assert!(is_kimi_process("kimi -S web"));
        assert!(is_kimi_process("kimi --model acp"));
        assert!(is_kimi_process("kimi -p __plugin_run_node"));
        assert!(is_kimi_process(
            "node /x/@moonshot-ai/kimi-code/dist/main.mjs -p build acp client"
        ));
        assert!(!is_kimi_process("kimi vis session-123"));
        assert!(!is_kimi_process("kimi server"));
        assert!(!is_kimi_process("kimi login"));
        assert!(!is_kimi_process("not-kimi"));
    }

    #[test]
    fn parses_explicit_session_flags() {
        assert_eq!(explicit_session_id("kimi -r abc"), Some("abc".into()));
        assert_eq!(
            explicit_session_id("kimi --session=xyz"),
            Some("xyz".into())
        );
        assert_eq!(explicit_session_id("kimi -S old"), Some("old".into()));
        assert_eq!(
            explicit_session_id("kimi --resume=restored"),
            Some("restored".into())
        );
        assert_eq!(
            explicit_session_id("kimi --session \"session with spaces\""),
            Some("session with spaces".into())
        );
        assert_eq!(explicit_session_id("kimi -r --model k2"), None);
    }

    #[test]
    fn directory_fallback_respects_index_tombstones() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root
            .path()
            .join("sessions")
            .join("wd_project_abc")
            .join("session_test");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("state.json"),
            r#"{"id":"session_test","cwd":"/tmp/project","createdAt":1,"updatedAt":2,"agents":{}}"#,
        )
        .unwrap();

        assert_eq!(read_session_index(root.path()).len(), 1);
        fs::write(
            root.path().join("session_index.jsonl"),
            r#"{"sessionId":"session_test","deleted":true}
"#,
        )
        .unwrap();
        assert!(read_session_index(root.path()).is_empty());
    }

    #[test]
    fn selection_is_root_scoped_keeps_old_resumes_and_follows_fresher_activity() {
        let root_a = PathBuf::from("/tmp/kimi-a");
        let root_b = PathBuf::from("/tmp/kimi-b");
        let make_session = |id: &str, root: &Path, updated_at| KimiSession {
            id: id.to_string(),
            dir: root.join("sessions/bucket").join(id),
            root: root.to_path_buf(),
            cwd: "/tmp/project".to_string(),
            title: String::new(),
            created_at: 1,
            updated_at,
            archived: false,
            agents: Vec::new(),
        };
        let old = make_session("old", &root_a, 10);
        let fresh = make_session("fresh", &root_a, 1_001);
        let other_profile = make_session("other", &root_b, 1_002);
        let process = KimiProcess {
            pid: 1,
            cwd: "/tmp/project".to_string(),
            root: root_a,
            explicit_session: None,
            bare_title: true,
            started_at: Some(1_000),
            incarnation: "process-1".to_string(),
        };
        assert!(session_matches_process(&old, &process));
        assert!(!session_matches_process(&other_profile, &process));

        let candidates = vec![&old, &fresh];
        let assignment = KimiAssignment {
            dir: old.dir.clone(),
            confirmed: false,
            authority: StatusAuthority::Unavailable,
            activity_boundary_ms: 1_000,
            process_incarnation: "process-1".to_string(),
        };
        let selected = select_session(
            &process,
            &candidates,
            Some(&assignment),
            &HashSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(selected.id, "fresh");
        assert!(pairing_is_confirmed(&process, selected, Some(&assignment)));
        assert_eq!(
            pairing_authority(&process, selected, Some(&assignment)),
            StatusAuthority::Heuristic
        );
        let shared = select_session(
            &process,
            &candidates,
            Some(&assignment),
            &HashSet::new(),
            false,
        )
        .unwrap();
        assert_eq!(shared.id, "old");
        let resumed = select_session(&process, &[&old], None, &HashSet::new(), true).unwrap();
        assert_eq!(resumed.id, "old");
        assert!(!pairing_is_confirmed(&process, &old, None));
        assert_eq!(
            pairing_authority(&process, &old, None),
            StatusAuthority::Unavailable
        );

        let process_without_start = KimiProcess {
            started_at: None,
            ..process
        };
        let observed_assignment = KimiAssignment {
            dir: old.dir.clone(),
            confirmed: false,
            authority: StatusAuthority::Unavailable,
            activity_boundary_ms: 10,
            process_incarnation: "process-1".to_string(),
        };
        assert!(!pairing_is_confirmed(
            &process_without_start,
            &old,
            Some(&observed_assignment)
        ));
        let post_observation = make_session("old", &PathBuf::from("/tmp/kimi-a"), 11);
        assert!(pairing_is_confirmed(
            &process_without_start,
            &post_observation,
            Some(&observed_assignment)
        ));
        assert_eq!(
            pairing_authority(
                &process_without_start,
                &post_observation,
                Some(&observed_assignment)
            ),
            StatusAuthority::Heuristic
        );
    }

    #[test]
    fn explicit_session_mapping_has_provider_authority() {
        let root = PathBuf::from("/tmp/kimi-explicit");
        let session = KimiSession {
            id: "session-exact".to_string(),
            dir: root.join("sessions/bucket/session-exact"),
            root: root.clone(),
            cwd: "/tmp/project".to_string(),
            title: String::new(),
            created_at: 1,
            updated_at: 2,
            archived: false,
            agents: Vec::new(),
        };
        let process = KimiProcess {
            pid: 1,
            cwd: session.cwd.clone(),
            root,
            explicit_session: Some(session.id.clone()),
            bare_title: false,
            started_at: None,
            incarnation: "process-exact".to_string(),
        };

        assert_eq!(
            pairing_authority(&process, &session, None),
            StatusAuthority::Provider
        );
    }

    #[test]
    fn exact_explicit_session_mapping_rejects_pid_reuse() {
        let tokens = vec![
            "/usr/local/bin/kimi-code".to_string(),
            "--session".to_string(),
            "session-a".to_string(),
        ];

        assert_eq!(
            explicit_session_id_from_tokens(&tokens).as_deref(),
            Some("session-a")
        );
        assert!(kimi_process_observation_is_exact(
            "process-a",
            Some("process-a"),
            &tokens
        ));
        assert!(!kimi_process_observation_is_exact(
            "process-a",
            Some("process-b"),
            &tokens
        ));
    }

    #[test]
    fn reused_pid_cannot_inherit_a_confirmed_assignment_from_an_old_incarnation() {
        let root = PathBuf::from("/tmp/kimi-reused-pid");
        let session = KimiSession {
            id: "session-old".to_string(),
            dir: root.join("sessions/bucket/session-old"),
            root: root.clone(),
            cwd: "/tmp/project".to_string(),
            title: String::new(),
            created_at: 1,
            updated_at: 100,
            archived: false,
            agents: Vec::new(),
        };
        let process = KimiProcess {
            pid: 42,
            cwd: session.cwd.clone(),
            root,
            explicit_session: None,
            bare_title: true,
            started_at: Some(1_000),
            incarnation: "new-process".to_string(),
        };
        let old_assignment = KimiAssignment {
            dir: session.dir.clone(),
            confirmed: true,
            authority: StatusAuthority::Provider,
            activity_boundary_ms: 10,
            process_incarnation: "old-process".to_string(),
        };

        assert_eq!(
            pairing_authority(&process, &session, Some(&old_assignment)),
            StatusAuthority::Unavailable
        );
    }

    #[test]
    fn unique_process_follows_in_process_session_switch_activity() {
        let root = PathBuf::from("/tmp/kimi-switch");
        let make_session = |id: &str, updated_at| KimiSession {
            id: id.to_string(),
            dir: root.join("sessions/bucket").join(id),
            root: root.clone(),
            cwd: "/tmp/project".to_string(),
            title: String::new(),
            created_at: 1,
            updated_at,
            archived: false,
            agents: Vec::new(),
        };
        let session_a = make_session("session-a", 1_100);
        let session_b_before_switch = make_session("session-b", 1_050);
        let process = KimiProcess {
            pid: 42,
            cwd: "/tmp/project".to_string(),
            root: root.clone(),
            explicit_session: None,
            bare_title: true,
            started_at: Some(1_000),
            incarnation: "process-42".to_string(),
        };
        let assignment = KimiAssignment {
            dir: session_a.dir.clone(),
            confirmed: true,
            authority: StatusAuthority::Heuristic,
            activity_boundary_ms: 1_000,
            process_incarnation: "process-42".to_string(),
        };

        let selected = select_session(
            &process,
            &[&session_a, &session_b_before_switch],
            Some(&assignment),
            &HashSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(selected.id, "session-a");

        let session_b_after_switch = make_session("session-b", 1_200);
        let selected = select_session(
            &process,
            &[&session_a, &session_b_after_switch],
            Some(&assignment),
            &HashSet::new(),
            true,
        )
        .unwrap();
        assert_eq!(selected.id, "session-b");

        let ambiguous = select_session(
            &process,
            &[&session_a, &session_b_after_switch],
            Some(&assignment),
            &HashSet::new(),
            false,
        )
        .unwrap();
        assert_eq!(ambiguous.id, "session-a");
    }

    #[test]
    fn reads_legacy_v2_state_and_v1_metadata_fallbacks() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("sessions/bucket/session_legacy");
        fs::create_dir_all(session_dir.join("session-meta")).unwrap();
        fs::write(
            session_dir.join("session-meta/state.json"),
            r#"{
                "id":"session_legacy",
                "custom":{"cwd":"/tmp/legacy-project"},
                "customTitle":"Legacy title",
                "agents":{}
            }"#,
        )
        .unwrap();

        let session = read_session_state(
            root.path(),
            "session_legacy".to_string(),
            session_dir,
            String::new(),
        )
        .unwrap();
        assert_eq!(session.cwd, "/tmp/legacy-project");
        assert_eq!(session.title, "Legacy title");
        assert!(session.created_at > 0);
        assert!(session.updated_at >= session.created_at);
    }

    #[test]
    fn usage_is_not_double_counted_from_step_end() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":10,"output":5,"inputCacheRead":3,"inputCacheCreation":2}}}));
        state.apply(&serde_json::json!({"type":"usage.record","model":"k2","usage":{"inputOther":10,"output":5,"inputCacheRead":3,"inputCacheCreation":2}}));
        assert_eq!(
            (
                state.total_input,
                state.total_output,
                state.total_cache_read,
                state.total_cache_create
            ),
            (10, 5, 3, 2)
        );
        assert_eq!(state.last_context_tokens, 20);
        state.apply(&serde_json::json!({"type":"context.update_token_count","tokenCount":42}));
        assert_eq!(state.last_context_tokens, 42);
    }

    #[test]
    fn v1_step_end_finishes_status_and_reconstructs_assistant_text() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"hello"}],"time":10}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s1","turnId":"t1"},"time":11}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","stepUuid":"s1","part":{"type":"text","text":"Hello"}},"time":12}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","stepUuid":"s1","part":{"type":"text","text":"world"}},"time":13}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"stale","name":"ReadFile","args":{"path":"/tmp/a"}},"time":14}));
        state.apply(&serde_json::json!({"type":"interaction.request","id":"approval","kind":"approval","time":15}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s1","turnId":"t1","finishReason":"end_turn","usage":{"inputOther":10,"output":5,"inputCacheRead":0,"inputCacheCreation":0}},"time":16}));
        state.apply(&serde_json::json!({"type":"usage.record","usageScope":"turn","usage":{"inputOther":10,"output":5,"inputCacheRead":0,"inputCacheCreation":0},"time":17}));

        assert!(!state.active_turn && !state.active_step);
        assert!(state.pending_tools.is_empty());
        assert!(state.pending_interactions.is_empty());
        assert_eq!(state.first_assistant_text, "Hello world");
        assert_eq!(state.chat_messages.last().unwrap().text, "Hello world");
        assert_eq!(state.turn_count, 1);
        assert_eq!(state.token_history_snapshot(), vec![15]);
    }

    #[test]
    fn token_history_aggregates_multi_step_turns() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":1}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.begin","turnId":"t1"},"time":2}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","turnId":"t1","finishReason":"tool_use"},"time":3}));
        state.apply(&serde_json::json!({"type":"usage.record","usageScope":"turn","usage":{"inputOther":3},"time":4}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.begin","turnId":"t1"},"time":5}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","turnId":"t1","finishReason":"end_turn"},"time":6}));
        state.apply(&serde_json::json!({"type":"usage.record","usageScope":"turn","usage":{"output":4},"time":7}));
        assert_eq!(state.token_history_snapshot(), vec![7]);
        assert_eq!(state.turn_count, 1);

        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":8}));
        assert_eq!(
            state.token_history.iter().copied().collect::<Vec<_>>(),
            vec![7]
        );
    }

    #[test]
    fn context_zero_usage_preserves_last_measurement_and_histories_keep_newest() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"context.update_token_count","tokenCount":42}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","usage":{"inputOther":0,"output":0,"inputCacheRead":0,"inputCacheCreation":0}}}));
        state.apply(&serde_json::json!({"type":"context.apply_compaction"}));
        assert_eq!(state.last_context_tokens, 42);

        let mut history = VecDeque::new();
        for value in 0..=MAX_HISTORY_POINTS as u64 {
            push_history(&mut history, value);
        }
        assert_eq!(history.len(), MAX_HISTORY_POINTS);
        assert_eq!(history.front(), Some(&1));
        assert_eq!(history.back(), Some(&(MAX_HISTORY_POINTS as u64)));
    }

    #[test]
    fn interactions_before_process_start_are_not_live() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"interaction.request","id":"old","kind":"question","time":100}));
        assert!(state.has_pending_input_since(100));
        assert!(!state.has_pending_input_since(101));
    }

    #[test]
    fn first_observation_boundary_rejects_historical_waits_and_tools() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"interaction.request","id":"old","kind":"question","time":100}));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":101,
            "event":{
                "type":"tool.call",
                "toolCallId":"old-question",
                "name":"AskUserQuestion",
                "args":{}
            }
        }));

        let (status, awaiting_input) = status_at(&state, 102);
        assert_eq!(status, SessionStatus::Idle);
        assert!(!awaiting_input);

        state.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"new",
            "kind":"question",
            "time":103
        }));
        let (status, awaiting_input) = status_at(&state, 102);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);
    }

    #[test]
    fn foreground_ask_user_question_waits_for_input() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":10}));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"question",
                "name":"AskUserQuestion",
                "args":{"questions":[{"question":"Continue?"}]}
            }
        }));

        let (status, awaiting_input) = status_at(&state, 10);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);
        assert!(state.pending_tools["question"].waits_for_user);
    }

    #[test]
    fn background_ask_user_question_still_waits_for_user_input() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":10}));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"question",
                "name":"AskUserQuestion",
                "args":{"background":true,"questions":[]}
            }
        }));

        let (status, awaiting_input, evidence) =
            evidence_at(&state, 10, StatusAuthority::Provider, false);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);
        assert!(state.pending_tools["question"].waits_for_user);
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingUserInput);

        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":30,
            "event":{"type":"tool.result","toolCallId":"question"}
        }));
        assert_eq!(status_at(&state, 10), (SessionStatus::Thinking, false));
    }

    #[test]
    fn ask_user_question_result_clears_input_wait() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":10}));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"question",
                "name":"AskUserQuestion",
                "args":{}
            }
        }));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":30,
            "event":{"type":"tool.result","toolCallId":"question"}
        }));

        let (status, awaiting_input) = status_at(&state, 10);
        assert_eq!(status, SessionStatus::Thinking);
        assert!(!awaiting_input);
        assert!(state.pending_tools.is_empty());
    }

    #[test]
    fn ask_user_question_before_process_start_is_not_a_live_wait() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":100,
            "event":{
                "type":"tool.call",
                "toolCallId":"old-question",
                "name":"AskUserQuestion",
                "args":{}
            }
        }));

        let (status, awaiting_input) = status_at(&state, 101);
        assert_eq!(status, SessionStatus::Idle);
        assert!(!awaiting_input);
    }

    #[test]
    fn ordinary_pending_tool_does_not_wait_for_input() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"read",
                "name":"ReadFile",
                "args":{"path":"/tmp/a"}
            }
        }));

        let (status, awaiting_input) = status_at(&state, 10);
        assert_eq!(status, SessionStatus::Executing);
        assert!(!awaiting_input);
    }

    #[test]
    fn v2_interaction_wait_behavior_is_preserved() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":10}));
        state.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"approval",
            "kind":"approval",
            "time":20
        }));
        let (status, awaiting_input) = status_at(&state, 10);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);

        state.apply(&serde_json::json!({
            "type":"interaction.resolved",
            "id":"approval",
            "time":30
        }));
        let (status, awaiting_input) = status_at(&state, 10);
        assert_eq!(status, SessionStatus::Thinking);
        assert!(!awaiting_input);
    }

    #[test]
    fn quiet_live_session_is_idle_and_unknown_ownership_wins() {
        let state = WireState::default();
        let (status, awaiting_input, evidence) =
            evidence_at(&state, 10, StatusAuthority::Provider, false);
        assert_eq!(status, SessionStatus::Idle);
        assert!(!awaiting_input);
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderIdle);
        assert_eq!(evidence.observations[0].status, SessionStatus::Idle);

        let mut waiting = WireState::default();
        waiting.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"approval",
            "kind":"approval",
            "time":20
        }));
        let (status, awaiting_input) =
            wire_session_status(&waiting, std::iter::empty(), 10, true, true);
        assert_eq!(status, SessionStatus::Unknown);
        assert!(!awaiting_input);
        let evidence = kimi_status_evidence(
            status,
            &waiting,
            std::iter::empty(),
            10,
            true,
            StatusAuthority::Unavailable,
            50,
        );
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::OwnershipUnconfirmed);
    }

    #[test]
    fn exact_wait_evidence_distinguishes_approval_and_question() {
        let mut approval = WireState::default();
        approval.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"approval",
            "kind":"approval",
            "time":20
        }));
        let (status, awaiting_input, evidence) =
            evidence_at(&approval, 10, StatusAuthority::Provider, false);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingApproval);

        let mut question = WireState::default();
        question.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"question",
                "name":"AskUserQuestion",
                "args":{}
            }
        }));
        let (status, awaiting_input, evidence) =
            evidence_at(&question, 10, StatusAuthority::Provider, false);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingUserInput);
    }

    #[test]
    fn only_actionable_native_interaction_kinds_wait() {
        let mut user_tool = WireState::default();
        user_tool.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"user-tool",
            "kind":"user_tool",
            "time":20
        }));
        assert_eq!(status_at(&user_tool, 10), (SessionStatus::Idle, false));
        assert!(user_tool.lifecycle_failure.is_none());

        let mut unknown = WireState::default();
        unknown.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"future",
            "kind":"future_kind",
            "time":20
        }));
        assert_eq!(
            unknown.lifecycle_failure,
            Some(StatusReason::ProtocolUnknown)
        );
        assert_eq!(status_at(&unknown, 10), (SessionStatus::Unknown, false));

        let mut malformed = WireState::default();
        malformed.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"missing-kind",
            "time":20
        }));
        assert_eq!(
            malformed.lifecycle_failure,
            Some(StatusReason::ProtocolMalformed)
        );
        assert_eq!(status_at(&malformed, 10), (SessionStatus::Unknown, false));
    }

    #[test]
    fn background_question_task_and_interaction_wait_for_user() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"question-call",
                "name":"AskUserQuestion",
                "args":{"background":true}
            }
        }));
        state.apply(&serde_json::json!({
            "type":"task.started",
            "time":21,
            "info":{
                "kind":"question",
                "taskId":"question-task",
                "toolCallId":"question-call",
                "status":"running",
                "detached":true,
                "startedAt":21
            }
        }));
        state.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"background-question",
            "kind":"question",
            "toolCallId":"question-call",
            "time":22
        }));

        assert!(state
            .pending_interactions
            .contains_key("background-question"));
        let (status, awaiting_input, evidence) =
            evidence_at(&state, 10, StatusAuthority::Provider, false);
        assert_eq!((status, awaiting_input), (SessionStatus::Waiting, true));
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingUserInput);
    }

    #[test]
    fn exact_wait_precedes_provider_error() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({
            "type":"turn.ended",
            "reason":"failed",
            "error":{"message":"sensitive failure"},
            "time":20
        }));
        state.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"approval",
            "kind":"approval",
            "time":21
        }));

        assert_eq!(status_at(&state, 10), (SessionStatus::Waiting, true));
    }

    #[test]
    fn best_effort_pairing_downgrades_wire_evidence_to_heuristic() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":20}));
        let (status, awaiting_input, evidence) =
            evidence_at(&state, 10, StatusAuthority::Heuristic, false);
        assert_eq!(status, SessionStatus::Thinking);
        assert!(!awaiting_input);
        assert_eq!(evidence.authority, StatusAuthority::Heuristic);
        assert_eq!(evidence.reason, StatusReason::CollectorInference);
    }

    #[test]
    fn lifecycle_timestamps_are_owned_only_by_active_statuses() {
        let mut waiting = WireState::default();
        waiting.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"approval",
            "kind":"approval",
            "time":20
        }));
        assert_eq!(
            lifecycle_timestamps(SessionStatus::Waiting, &waiting, std::iter::empty(), 10),
            (0, 0)
        );
        assert_eq!(
            lifecycle_timestamps(SessionStatus::Unknown, &waiting, std::iter::empty(), 10),
            (0, 0)
        );
        assert_eq!(
            lifecycle_timestamps(
                SessionStatus::Idle,
                &WireState::default(),
                std::iter::empty(),
                10
            ),
            (0, 0)
        );

        let mut executing = WireState::default();
        executing.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"read",
                "name":"ReadFile",
                "args":{}
            }
        }));
        assert_eq!(
            lifecycle_timestamps(SessionStatus::Executing, &executing, std::iter::empty(), 10),
            (20, 0)
        );

        let mut thinking = WireState::default();
        thinking.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":20}));
        assert_eq!(
            lifecycle_timestamps(SessionStatus::Thinking, &thinking, std::iter::empty(), 10),
            (0, 20)
        );
    }

    #[test]
    fn background_question_wait_takes_precedence_over_tools_tasks_and_child_processes() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":20,
            "event":{
                "type":"tool.call",
                "toolCallId":"read",
                "name":"ReadFile",
                "args":{}
            }
        }));
        state.apply(&serde_json::json!({
            "type":"task.started",
            "time":21,
            "info":{
                "kind":"process",
                "taskId":"background-process",
                "status":"running",
                "detached":true,
                "startedAt":21
            }
        }));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":22,
            "event":{
                "type":"tool.call",
                "toolCallId":"question",
                "name":"AskUserQuestion",
                "args":{"background":true}
            }
        }));

        let (status, awaiting_input) =
            wire_session_status(&state, std::iter::empty(), 10, false, true);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);
        let evidence = kimi_status_evidence(
            status,
            &state,
            std::iter::empty(),
            10,
            true,
            StatusAuthority::Provider,
            30,
        );
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingUserInput);
    }

    #[test]
    fn active_detached_tasks_classify_question_as_wait_and_terminal_updates_to_idle() {
        for kind in ["process", "agent", "question"] {
            let mut state = WireState::default();
            state.apply(&serde_json::json!({
                "type":"task.started",
                "time":20,
                "info":{
                    "kind":kind,
                    "taskId":"task-1",
                    "status":"running",
                    "detached":true,
                    "startedAt":20,
                    "description":"background work",
                    "agentId":"agent-1"
                }
            }));
            let (status, awaiting_input) = status_at(&state, 10);
            let expected = if kind == "question" {
                (SessionStatus::Waiting, true)
            } else {
                (SessionStatus::Executing, false)
            };
            assert_eq!((status, awaiting_input), expected, "kind={kind}");

            state.apply(&serde_json::json!({
                "type":"task.terminated",
                "time":30,
                "info":{
                    "kind":kind,
                    "taskId":"task-1",
                    "status":"completed",
                    "detached":true,
                    "startedAt":20,
                    "endedAt":30,
                    "agentId":"agent-1"
                }
            }));
            let (status, awaiting_input) = status_at(&state, 10);
            assert_eq!(status, SessionStatus::Idle, "kind={kind}");
            assert!(!awaiting_input, "kind={kind}");
        }
    }

    #[test]
    fn stale_turn_tools_and_tasks_do_not_create_live_activity() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":10}));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":11,
            "event":{
                "type":"tool.call",
                "toolCallId":"old",
                "name":"ReadFile",
                "args":{}
            }
        }));
        state.apply(&serde_json::json!({
            "type":"task.started",
            "time":12,
            "info":{
                "kind":"process",
                "taskId":"old-task",
                "status":"running",
                "startedAt":12
            }
        }));

        let (status, awaiting_input) = status_at(&state, 20);
        assert_eq!(status, SessionStatus::Idle);
        assert!(!awaiting_input);
        assert!(state.execution_labels_since(20).is_empty());
        assert_eq!(state.pending_since(20), 0);
        let evidence = kimi_status_evidence(
            status,
            &state,
            std::iter::empty(),
            20,
            false,
            StatusAuthority::Provider,
            50,
        );
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderIdle);
    }

    #[test]
    fn child_wait_and_work_are_aggregated_before_parent_classification() {
        let parent = WireState::default();
        let mut child = WireState::default();
        child.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":20}));

        let (status, awaiting_input) =
            wire_session_status(&parent, std::iter::once(&child), 10, false, false);
        assert_eq!(status, SessionStatus::Executing);
        assert!(!awaiting_input);

        child.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"child-question",
            "kind":"question",
            "time":21
        }));
        let (status, awaiting_input) =
            wire_session_status(&parent, std::iter::once(&child), 10, false, false);
        assert_eq!(status, SessionStatus::Waiting);
        assert!(awaiting_input);
        let evidence = kimi_status_evidence(
            status,
            &parent,
            std::iter::once(&child),
            10,
            false,
            StatusAuthority::Provider,
            50,
        );
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingUserInput);
    }

    #[test]
    fn unavailable_child_wire_fails_parent_status_closed() {
        let parent = WireState::default();
        let child = WireState {
            lifecycle_failure: Some(StatusReason::ProtocolMalformed),
            ..WireState::default()
        };
        let (status, awaiting_input) =
            wire_session_status(&parent, std::iter::once(&child), 10, false, false);
        assert_eq!(status, SessionStatus::Unknown);
        assert!(!awaiting_input);
        let evidence = kimi_status_evidence(
            status,
            &parent,
            std::iter::once(&child),
            10,
            false,
            StatusAuthority::Provider,
            50,
        );
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::ProtocolMalformed);
    }

    #[test]
    fn stale_child_wait_tool_and_task_do_not_leak_across_boundary() {
        let parent = WireState::default();
        let mut child = WireState::default();
        child.apply(&serde_json::json!({
            "type":"interaction.request",
            "id":"old-wait",
            "kind":"question",
            "time":5
        }));
        child.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":6,
            "event":{
                "type":"tool.call",
                "toolCallId":"old-tool",
                "name":"ReadFile",
                "args":{}
            }
        }));
        child.apply(&serde_json::json!({
            "type":"task.started",
            "time":7,
            "info":{
                "kind":"agent",
                "taskId":"old-task",
                "status":"running",
                "startedAt":7
            }
        }));

        let (status, awaiting_input) =
            wire_session_status(&parent, std::iter::once(&child), 10, false, false);
        assert_eq!(status, SessionStatus::Idle);
        assert!(!awaiting_input);
        let evidence = kimi_status_evidence(
            status,
            &parent,
            std::iter::once(&child),
            10,
            false,
            StatusAuthority::Provider,
            50,
        );
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderIdle);
    }

    #[test]
    fn open_llm_request_thinks_and_failed_turn_becomes_provider_error() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({
            "type":"llm.request",
            "kind":"loop",
            "model":"k2",
            "time":20
        }));
        let (status, awaiting_input) = status_at(&state, 10);
        assert_eq!(status, SessionStatus::Thinking);
        assert!(!awaiting_input);

        state.apply(&serde_json::json!({
            "type":"turn.ended",
            "reason":"failed",
            "error":{"message":"model failed"},
            "time":30
        }));
        let (status, awaiting_input) = status_at(&state, 10);
        assert_eq!(status, SessionStatus::Error);
        assert!(!awaiting_input);
        assert_eq!(state.last_error.as_deref(), Some("model failed"));
        let evidence = kimi_status_evidence(
            status,
            &state,
            std::iter::empty(),
            10,
            false,
            StatusAuthority::Provider,
            40,
        );
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderError);
        assert_eq!(evidence.status_since_ms, 30);
        assert_eq!(
            kimi_current_tasks(&state, &[], 10, false, status, false, false),
            vec!["error".to_string()]
        );
    }

    #[test]
    fn active_os_child_process_is_executing() {
        let state = WireState::default();
        let (status, awaiting_input, evidence) =
            evidence_at(&state, 10, StatusAuthority::Provider, true);
        assert_eq!(status, SessionStatus::Executing);
        assert!(!awaiting_input);
        assert_eq!(evidence.authority, StatusAuthority::Heuristic);
        assert_eq!(evidence.reason, StatusReason::BackgroundTerminalActive);
    }

    #[test]
    fn child_wire_tokens_join_task_lifecycle_by_agent_id() {
        let mut parent = WireState::default();
        parent.apply(&serde_json::json!({
            "type":"task.started",
            "info":{
                "kind":"agent",
                "taskId":"task-1",
                "agentId":"agent-1",
                "subagentType":"coder",
                "status":"running"
            }
        }));
        let child = WireState {
            total_input: 10,
            total_output: 2,
            ..WireState::default()
        };
        merge_child_subagent(&mut parent.subagents, "agent-1", &child, 0);
        assert_eq!(parent.subagents.len(), 1);
        let subagent = parent.subagents.values().next().unwrap();
        assert_eq!(subagent.name, "coder");
        assert_eq!(subagent.tokens, 12);
    }

    #[test]
    fn status_state_tracks_tools_interactions_and_turns() {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"hello"}],"time":10}));
        assert!(state.active_turn);
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"t","name":"ReadFile","args":{"path":"/tmp/a"}},"time":20}));
        assert!(state.pending_tools.contains_key("t"));
        state.apply(
            &serde_json::json!({"type":"interaction.request","id":"i","kind":"approval","time":21}),
        );
        assert!(state.pending_interactions.contains_key("i"));
        state.apply(&serde_json::json!({"type":"interaction.resolved","id":"i","time":22}));
        state.apply(&serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"t"},"time":30}));
        state.apply(
            &serde_json::json!({"type":"turn.ended","turnId":0,"reason":"completed","time":31}),
        );
        assert!(
            !state.active_turn
                && state.pending_tools.is_empty()
                && state.pending_interactions.is_empty()
        );
        assert_eq!(state.tool_calls[0].duration_ms, 10);
    }

    #[test]
    fn incremental_parser_buffers_partial_lines_and_resets_on_shrink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        let mut file = File::create(&path).unwrap();
        write!(
            file,
            "{{\"type\":\"metadata\",\"protocol_version\":\"1.4\",\"created_at\":1"
        )
        .unwrap();
        file.flush().unwrap();
        let mut cache = WireCache::default();
        assert_eq!(
            cache.refresh(&path),
            WireAvailability::Failed(StatusReason::Stale)
        );
        assert_eq!(cache.state.turn_count, 0);
        writeln!(file, "}}").unwrap();
        file.flush().unwrap();
        assert_eq!(cache.refresh(&path), WireAvailability::Available);
        assert_eq!(cache.state.turn_count, 0);
        writeln!(file, "{{\"type\":\"turn.prompt\",\"input\":[],\"time\":2}}").unwrap();
        file.flush().unwrap();
        assert_eq!(cache.refresh(&path), WireAvailability::Available);
        assert_eq!(cache.state.turn_count, 1);
        fs::write(
            &path,
            concat!(
                "{\"type\":\"metadata\",\"protocol_version\":\"1.4\",",
                "\"created_at\":1}\n",
                "{\"type\":\"usage.record\",\"usage\":{\"inputOther\":2},",
                "\"time\":3}\n"
            ),
        )
        .unwrap();
        assert_eq!(cache.refresh(&path), WireAvailability::Available);
        assert_eq!(cache.state.turn_count, 0);
        assert_eq!(cache.state.total_input, 2);
    }

    #[test]
    fn malformed_wire_record_fails_closed_until_file_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"metadata\",\"protocol_version\":\"1.4\",",
                "\"created_at\":1}\n",
                "{\"type\":\"interaction.request\",\"id\":\"approval\",",
                "\"kind\":\"approval\",\"time\":20}\n",
                "not-json\n",
                "{\"type\":\"interaction.resolved\",\"id\":\"approval\",",
                "\"time\":21}\n"
            ),
        )
        .unwrap();
        let mut cache = WireCache::default();
        assert_eq!(
            cache.refresh(&path),
            WireAvailability::Failed(StatusReason::ProtocolMalformed)
        );
        assert!(cache.state.pending_interactions.is_empty());

        fs::write(
            &path,
            concat!(
                "{\"type\":\"metadata\",\"protocol_version\":\"1.4\",",
                "\"created_at\":1}\n",
                "{\"type\":\"usage.record\",\"usage\":{\"inputOther\":2},",
                "\"time\":30}\n"
            ),
        )
        .unwrap();
        assert_eq!(cache.refresh(&path), WireAvailability::Available);
        assert_eq!(cache.state.total_input, 2);
    }

    #[test]
    fn oversized_wire_record_is_protocol_malformed() {
        let mut cache = WireCache::default();
        cache.consume(&vec![b'x'; MAX_WIRE_LINE_BYTES + 1]);
        cache.consume(b"\n");
        assert_eq!(
            cache.availability(cache.offset),
            WireAvailability::Failed(StatusReason::ProtocolMalformed)
        );
    }

    #[test]
    fn missing_wire_source_is_unknown_instead_of_dropping_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut collector = KimiCollector::new();
        let state = collector.parse_wire(&dir.path().join("missing-wire.jsonl"));
        assert_eq!(state.lifecycle_failure, Some(StatusReason::Unavailable));
        let (status, awaiting_input, evidence) =
            evidence_at(&state, 10, StatusAuthority::Provider, false);
        assert_eq!(status, SessionStatus::Unknown);
        assert!(!awaiting_input);
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::Unavailable);
    }

    fn persisted_state(version: &str) -> WireState {
        let mut state = WireState::default();
        state.apply(&serde_json::json!({
            "type":"metadata",
            "protocol_version":version,
            "created_at":1
        }));
        state
    }

    #[test]
    fn persisted_wire_requires_exact_first_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        fs::write(
            &path,
            "{\"type\":\"turn.prompt\",\"input\":[],\"time\":2}\n",
        )
        .unwrap();
        let mut cache = WireCache::default();
        assert_eq!(
            cache.refresh(&path),
            WireAvailability::Failed(StatusReason::ProtocolMalformed)
        );

        let mut unsupported = WireState::default();
        unsupported.apply(&serde_json::json!({
            "type":"metadata",
            "protocol_version":"1.5",
            "created_at":1
        }));
        assert_eq!(
            unsupported.lifecycle_failure,
            Some(StatusReason::ProtocolUnknown)
        );

        let mut duplicate = persisted_state("1.4");
        duplicate.apply(&serde_json::json!({
            "type":"metadata",
            "protocol_version":"1.4",
            "created_at":1
        }));
        assert_eq!(
            duplicate.lifecycle_failure,
            Some(StatusReason::ProtocolMalformed)
        );
    }

    #[test]
    fn v1_terminal_step_and_cancel_fail_closed_instead_of_staying_busy() {
        let mut ended = persisted_state("1.4");
        ended.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":10}));
        ended.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":11,
            "event":{"type":"step.begin","uuid":"step-1","turnId":"1"}
        }));
        ended.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":12,
            "event":{"type":"step.end","uuid":"step-1","turnId":"1","finishReason":"end_turn"}
        }));
        assert_eq!(status_at(&ended, 1), (SessionStatus::Unknown, false));
        assert!(!ended.active_turn && !ended.active_step && ended.pending_tools.is_empty());

        let mut cancelled = persisted_state("1.4");
        cancelled.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":20}));
        cancelled.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":21,
            "event":{"type":"step.begin","uuid":"step-2","turnId":"2"}
        }));
        cancelled.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":22,
            "event":{"type":"tool.call","toolCallId":"question","name":"AskUserQuestion","args":{}}
        }));
        assert_eq!(status_at(&cancelled, 1), (SessionStatus::Waiting, true));
        cancelled.apply(&serde_json::json!({"type":"turn.cancel","turnId":2,"time":23}));
        assert_eq!(status_at(&cancelled, 1), (SessionStatus::Unknown, false));
        assert!(cancelled.pending_tools.is_empty());

        cancelled.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":24}));
        assert_eq!(status_at(&cancelled, 1), (SessionStatus::Thinking, false));
    }

    #[test]
    fn v1_mismatched_cancel_is_ignored_and_filtered_is_error() {
        let mut state = persisted_state("1.4");
        state.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":10}));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":11,
            "event":{"type":"step.begin","uuid":"step-1","turnId":"1"}
        }));
        state.apply(&serde_json::json!({"type":"turn.cancel","turnId":999,"time":12}));
        assert_eq!(status_at(&state, 1), (SessionStatus::Thinking, false));
        state.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":13,
            "event":{"type":"step.end","uuid":"step-1","turnId":"1","finishReason":"filtered"}
        }));
        let (status, awaiting_input, evidence) =
            evidence_at(&state, 1, StatusAuthority::Provider, false);
        assert_eq!((status, awaiting_input), (SessionStatus::Error, false));
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(state.last_error.as_deref(), Some("Kimi response filtered"));
    }

    #[test]
    fn v1_rejects_live_only_lifecycle_records() {
        let records = [
            serde_json::json!({"type":"interaction.request","id":"i","kind":"question","time":10}),
            serde_json::json!({"type":"interaction.resolved","id":"i","time":10}),
            serde_json::json!({"type":"turn.started","time":10}),
            serde_json::json!({"type":"turn.ended","reason":"completed","time":10}),
            serde_json::json!({"type":"turn.step.started","time":10}),
            serde_json::json!({"type":"turn.step.completed","time":10}),
            serde_json::json!({"type":"turn.step.interrupted","time":10}),
            serde_json::json!({"type":"task.started","info":{},"time":10}),
            serde_json::json!({"type":"task.terminated","info":{},"time":10}),
        ];
        for record in records {
            let mut state = persisted_state("1.4");
            state.apply(&record);
            assert_eq!(
                state.lifecycle_failure,
                Some(StatusReason::ProtocolUnknown),
                "record={record}"
            );
            assert_eq!(status_at(&state, 1), (SessionStatus::Unknown, false));
        }
    }

    #[test]
    fn v1_requires_timestamps_on_every_foreground_mutation() {
        let records = [
            serde_json::json!({"type":"turn.prompt","input":[]}),
            serde_json::json!({"type":"turn.steer","input":[],"time":0}),
            serde_json::json!({"type":"turn.cancel"}),
            serde_json::json!({"type":"llm.request","kind":"loop"}),
            serde_json::json!({"type":"full_compaction.begin","source":"manual"}),
            serde_json::json!({"type":"full_compaction.complete"}),
            serde_json::json!({"type":"full_compaction.cancel"}),
            serde_json::json!({"type":"context.apply_compaction"}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"s","turnId":"1"}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"step.end","uuid":"s","turnId":"1","finishReason":"end_turn"}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"x"}}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"t","name":"ReadFile","args":{}}}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"t"}}),
        ];
        for record in records {
            let mut state = persisted_state("1.4");
            state.apply(&record);
            assert_eq!(
                state.lifecycle_failure,
                Some(StatusReason::ProtocolMalformed),
                "record={record}"
            );
        }
    }

    #[test]
    fn v1_validates_llm_request_kind_and_numeric_cancel_id() {
        let mut missing_kind = persisted_state("1.4");
        missing_kind.apply(&serde_json::json!({"type":"llm.request","time":10}));
        assert_eq!(
            missing_kind.lifecycle_failure,
            Some(StatusReason::ProtocolMalformed)
        );

        let mut unknown_kind = persisted_state("1.4");
        unknown_kind.apply(&serde_json::json!({"type":"llm.request","kind":"future","time":10}));
        assert_eq!(
            unknown_kind.lifecycle_failure,
            Some(StatusReason::ProtocolUnknown)
        );

        let mut orphan_compaction_request = persisted_state("1.4");
        orphan_compaction_request
            .apply(&serde_json::json!({"type":"llm.request","kind":"compaction","time":10}));
        assert_eq!(
            orphan_compaction_request.lifecycle_failure,
            Some(StatusReason::ProtocolMalformed)
        );

        let mut compaction = persisted_state("1.4");
        compaction.apply(&serde_json::json!({
            "type":"full_compaction.begin",
            "source":"manual",
            "time":9
        }));
        compaction.apply(&serde_json::json!({"type":"llm.request","kind":"compaction","time":10}));
        assert_eq!(status_at(&compaction, 1), (SessionStatus::Thinking, false));

        let mut string_cancel = persisted_state("1.4");
        string_cancel.apply(&serde_json::json!({"type":"turn.cancel","turnId":"1","time":10}));
        assert_eq!(
            string_cancel.lifecycle_failure,
            Some(StatusReason::ProtocolMalformed)
        );
    }

    #[test]
    fn v1_compaction_lifecycle_is_fail_closed_and_leased() {
        let mut manual = persisted_state("1.4");
        manual.apply(&serde_json::json!({
            "type":"full_compaction.begin",
            "source":"manual",
            "time":10
        }));
        assert_eq!(status_at(&manual, 1), (SessionStatus::Thinking, false));
        manual.apply(&serde_json::json!({
            "type":"llm.request",
            "kind":"compaction",
            "time":20
        }));
        manual.apply(&serde_json::json!({
            "type":"context.apply_compaction",
            "tokensAfter":100,
            "time":30
        }));
        assert_eq!(status_at(&manual, 1), (SessionStatus::Thinking, false));
        manual.apply(&serde_json::json!({"type":"full_compaction.complete","time":40}));
        assert_eq!(status_at(&manual, 1), (SessionStatus::Idle, false));
        assert_eq!(manual.compaction_count, 1);

        let mut manual_cancel = persisted_state("1.4");
        manual_cancel.apply(&serde_json::json!({
            "type":"full_compaction.begin",
            "source":"manual",
            "time":45
        }));
        manual_cancel.apply(&serde_json::json!({"type":"full_compaction.cancel","time":46}));
        assert_eq!(status_at(&manual_cancel, 1), (SessionStatus::Idle, false));

        let mut auto = persisted_state("1.4");
        auto.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":50}));
        auto.apply(&serde_json::json!({
            "type":"full_compaction.begin",
            "source":"auto",
            "time":60
        }));
        auto.apply(&serde_json::json!({"type":"full_compaction.cancel","time":70}));
        let (status, awaiting_input, evidence) =
            evidence_at(&auto, 1, StatusAuthority::Provider, false);
        assert_eq!((status, awaiting_input), (SessionStatus::Unknown, false));
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::ProtocolUnknown);
        assert!(!auto.active_turn && !auto.active_step && auto.pending_tools.is_empty());

        auto.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":80,
            "event":{"type":"step.begin","uuid":"step-2","turnId":1}
        }));
        assert_eq!(status_at(&auto, 1), (SessionStatus::Thinking, false));
        assert_eq!(auto.foreground_uncertain_since, 0);

        let mut stale = persisted_state("1.4");
        stale.apply(&serde_json::json!({
            "type":"full_compaction.begin",
            "source":"manual",
            "time":100
        }));
        stale.expire_foreground_lease_at(101 + V1_NON_WAIT_FOREGROUND_LEASE_MS);
        let (status, awaiting_input, evidence) =
            evidence_at(&stale, 1, StatusAuthority::Provider, false);
        assert_eq!((status, awaiting_input), (SessionStatus::Unknown, false));
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::Stale);
    }

    #[test]
    fn v1_steer_starts_work_and_foreground_lease_expires_fail_closed() {
        let mut state = persisted_state("1.4");
        state.apply(&serde_json::json!({"type":"turn.steer","input":[],"time":100}));
        assert_eq!(status_at(&state, 1), (SessionStatus::Thinking, false));
        state.expire_foreground_lease_at(100 + V1_NON_WAIT_FOREGROUND_LEASE_MS);
        assert_eq!(status_at(&state, 1), (SessionStatus::Thinking, false));
        state.expire_foreground_lease_at(101 + V1_NON_WAIT_FOREGROUND_LEASE_MS);
        assert_eq!(status_at(&state, 1), (SessionStatus::Unknown, false));
        assert!(!state.active_turn && state.pending_tools.is_empty());
        let (_, _, evidence) = evidence_at(&state, 1, StatusAuthority::Provider, false);
        assert_eq!(evidence.authority, StatusAuthority::Unavailable);
        assert_eq!(evidence.reason, StatusReason::Stale);
        state.apply(&serde_json::json!({
            "type":"turn.steer",
            "input":[],
            "time":102 + V1_NON_WAIT_FOREGROUND_LEASE_MS
        }));
        assert_eq!(status_at(&state, 1), (SessionStatus::Thinking, false));

        let mut executing = persisted_state("1.4");
        executing.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":200}));
        executing.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":201,
            "event":{"type":"tool.call","toolCallId":"tool","name":"ReadFile","args":{}}
        }));
        assert_eq!(status_at(&executing, 1), (SessionStatus::Executing, false));
        executing.expire_foreground_lease_at(202 + V1_NON_WAIT_FOREGROUND_LEASE_MS);
        assert_eq!(status_at(&executing, 1), (SessionStatus::Unknown, false));

        let mut waiting = persisted_state("1.4");
        waiting.apply(&serde_json::json!({"type":"turn.prompt","input":[],"time":300}));
        waiting.apply(&serde_json::json!({
            "type":"context.append_loop_event",
            "time":301,
            "event":{"type":"tool.call","toolCallId":"question","name":"AskUserQuestion","args":{}}
        }));
        waiting.expire_foreground_lease_at(301 + V1_NON_WAIT_FOREGROUND_LEASE_MS * 10);
        assert_eq!(status_at(&waiting, 1), (SessionStatus::Waiting, true));
        let (_, _, evidence) = evidence_at(&waiting, 1, StatusAuthority::Provider, false);
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingUserInput);
    }

    #[test]
    fn durable_task_snapshots_keep_background_work_executing() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("agents/main/tasks");
        fs::create_dir_all(&task_dir).unwrap();
        let path = task_dir.join("bash-abcdefgh.json");
        fs::write(
            &path,
            r#"{"taskId":"bash-abcdefgh","kind":"process","status":"running","startedAt":20,"endedAt":null,"description":"tests"}"#,
        )
        .unwrap();
        let snapshots =
            read_task_snapshots(dir.path(), std::slice::from_ref(&task_dir), 10).unwrap();
        let mut state = WireState::default();
        state.reconcile_task_snapshots(snapshots);
        assert_eq!(status_at(&state, 10), (SessionStatus::Executing, false));
        assert_eq!(state.execution_labels_since(10), vec!["background process"]);

        fs::write(
            &path,
            r#"{"taskId":"bash-abcdefgh","kind":"process","status":"completed","startedAt":20,"endedAt":30}"#,
        )
        .unwrap();
        let snapshots = read_task_snapshots(dir.path(), &[task_dir], 10).unwrap();
        state.reconcile_task_snapshots(snapshots);
        assert_eq!(status_at(&state, 10), (SessionStatus::Idle, false));
    }

    #[test]
    fn durable_detached_question_waits_over_parallel_background_work_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("agents/main/tasks");
        fs::create_dir_all(&task_dir).unwrap();
        let process_path = task_dir.join("bash-abcdefgh.json");
        let question_path = task_dir.join("question-abcdefgh.json");
        fs::write(
            &process_path,
            r#"{"taskId":"bash-abcdefgh","kind":"process","status":"running","detached":true,"startedAt":20,"endedAt":null}"#,
        )
        .unwrap();
        fs::write(
            &question_path,
            r#"{"taskId":"question-abcdefgh","kind":"question","status":"running","detached":true,"startedAt":21,"endedAt":null,"toolCallId":"question-call"}"#,
        )
        .unwrap();

        let snapshots =
            read_task_snapshots(dir.path(), std::slice::from_ref(&task_dir), 10).unwrap();
        let mut state = persisted_state("1.4");
        state.reconcile_task_snapshots(snapshots);
        state.expire_foreground_lease_at(21 + V1_NON_WAIT_FOREGROUND_LEASE_MS * 10);
        let (status, awaiting_input, evidence) =
            evidence_at(&state, 10, StatusAuthority::Provider, false);
        assert_eq!((status, awaiting_input), (SessionStatus::Waiting, true));
        assert_eq!(evidence.authority, StatusAuthority::Provider);
        assert_eq!(evidence.reason, StatusReason::ProviderWaitingUserInput);
        assert_eq!(state.pending_input_since(10), 21);
        assert_eq!(state.execution_labels_since(10), vec!["background process"]);

        fs::write(
            &question_path,
            r#"{"taskId":"question-abcdefgh","kind":"question","status":"completed","detached":true,"startedAt":21,"endedAt":30,"toolCallId":"question-call"}"#,
        )
        .unwrap();
        let snapshots =
            read_task_snapshots(dir.path(), std::slice::from_ref(&task_dir), 10).unwrap();
        state.reconcile_task_snapshots(snapshots);
        assert_eq!(status_at(&state, 10), (SessionStatus::Executing, false));

        fs::write(
            &process_path,
            r#"{"taskId":"bash-abcdefgh","kind":"process","status":"completed","detached":true,"startedAt":20,"endedAt":31}"#,
        )
        .unwrap();
        let snapshots = read_task_snapshots(dir.path(), &[task_dir], 10).unwrap();
        state.reconcile_task_snapshots(snapshots);
        assert_eq!(status_at(&state, 10), (SessionStatus::Idle, false));
    }

    #[test]
    fn durable_foreground_question_waits_and_terminal_snapshot_clears() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("agents/main/tasks");
        fs::create_dir_all(&task_dir).unwrap();
        let path = task_dir.join("question-abcdefgh.json");
        fs::write(
            &path,
            r#"{"taskId":"question-abcdefgh","kind":"question","status":"running","detached":false,"startedAt":20,"endedAt":null,"toolCallId":"question-call"}"#,
        )
        .unwrap();
        let snapshots =
            read_task_snapshots(dir.path(), std::slice::from_ref(&task_dir), 10).unwrap();
        let mut state = WireState::default();
        state.reconcile_task_snapshots(snapshots);
        assert_eq!(status_at(&state, 10), (SessionStatus::Waiting, true));
        assert_eq!(state.pending_input_since(10), 20);

        fs::write(
            &path,
            r#"{"taskId":"question-abcdefgh","kind":"question","status":"completed","detached":false,"startedAt":20,"endedAt":30,"toolCallId":"question-call"}"#,
        )
        .unwrap();
        let snapshots = read_task_snapshots(dir.path(), &[task_dir], 10).unwrap();
        state.reconcile_task_snapshots(snapshots);
        assert_eq!(status_at(&state, 10), (SessionStatus::Idle, false));
    }

    #[test]
    fn legacy_question_snapshot_without_detached_still_waits_for_user() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("agents/main/tasks");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(
            task_dir.join("question-abcdefgh.json"),
            r#"{"taskId":"question-abcdefgh","kind":"question","status":"running","startedAt":20,"endedAt":null}"#,
        )
        .unwrap();
        let snapshots = read_task_snapshots(dir.path(), &[task_dir], 10).unwrap();
        let mut state = WireState::default();
        state.reconcile_task_snapshots(snapshots);
        assert!(state.active_tasks["question-abcdefgh"].detached);
        assert_eq!(status_at(&state, 10), (SessionStatus::Waiting, true));
    }

    #[test]
    fn current_malformed_task_snapshot_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let task_dir = dir.path().join("agents/main/tasks");
        fs::create_dir_all(&task_dir).unwrap();
        let path = task_dir.join("bash-abcdefgh.json");
        fs::write(&path, "not-json").unwrap();
        assert_eq!(
            read_task_snapshots(dir.path(), std::slice::from_ref(&task_dir), 0).unwrap_err(),
            StatusReason::ProtocolMalformed
        );

        fs::write(
            &path,
            r#"{"taskId":"bash-abcdefgh","kind":"process","status":"running","detached":"maybe","startedAt":20,"endedAt":null}"#,
        )
        .unwrap();
        assert_eq!(
            read_task_snapshots(dir.path(), &[task_dir], 0).unwrap_err(),
            StatusReason::ProtocolMalformed
        );
    }

    #[test]
    fn bare_rewritten_title_never_retains_provider_ownership() {
        let root = PathBuf::from("/tmp/kimi-bare");
        let session = KimiSession {
            id: "session-a".to_string(),
            dir: root.join("sessions/bucket/session-a"),
            root: root.clone(),
            cwd: "/tmp/project".to_string(),
            title: String::new(),
            created_at: 1,
            updated_at: 20,
            archived: false,
            agents: Vec::new(),
        };
        let process = KimiProcess {
            pid: 1,
            cwd: session.cwd.clone(),
            root,
            explicit_session: None,
            bare_title: true,
            started_at: Some(10),
            incarnation: "same-process".to_string(),
        };
        let prior = KimiAssignment {
            dir: session.dir.clone(),
            confirmed: true,
            authority: StatusAuthority::Provider,
            activity_boundary_ms: 10,
            process_incarnation: "same-process".to_string(),
        };
        assert_eq!(
            pairing_authority(&process, &session, Some(&prior)),
            StatusAuthority::Heuristic
        );
    }

    #[test]
    fn config_limit_uses_override_input_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.toml"),
            r#"[models.k2]
max_context_size = 200000
max_input_size = 150000
[models.k2.overrides]
max_input_size = 120000

[models.alias]
model = "moonshot/model-id"
max_context_size = 64000
"#,
        )
        .unwrap();
        assert_eq!(model_context_limit(dir.path(), "k2", ""), 120_000);
        assert_eq!(
            model_context_limit(dir.path(), "", "moonshot/model-id"),
            64_000
        );
        assert_eq!(model_context_limit(dir.path(), "missing", ""), 0);
    }

    #[test]
    fn sanitizes_tool_arguments() {
        let arg = safe_tool_arg(&serde_json::json!({"path":"\u{202e}/tmp/sk-ant-secret"}));
        assert!(!arg.contains('\u{202e}'));
        assert!(!arg.contains("secret"));
        assert!(safe_tool_arg(&serde_json::json!({"command":"echo secret"})).is_empty());
        assert!(safe_tool_arg(&serde_json::json!({"pattern":"private search text"})).is_empty());
    }
}
