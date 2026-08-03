use super::herdr::{HerdrObservation, HerdrStatus, HerdrStatusResolver, HerdrTarget};
use super::process::{self, ProcInfo};
use crate::codex_hooks::{
    plugin::{self, PluginPaths},
    state::{
        HookProjection, HookRootProjection, HookSessionState, HookStateStore, HookToolClass,
        IntegrationIdentity,
    },
};
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, RateLimitInfo, RateLimitProvenance,
    RateLimitWindow, SessionStatus, StatusAuthority, StatusEvidence, StatusObservation,
    StatusReason, SubAgent, ToolCall, MAX_CHAT_MESSAGES,
};
use serde::Deserialize;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

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
/// - Open `request_user_input` records: metrics only; never authoritative Waiting
/// - `turn_context`: model, cwd, effort, context window size
pub struct CodexCollector {
    sessions_dir: PathBuf,
    /// Latest rate limit info parsed from Codex JSONL token_count events.
    pub last_rate_limit: Option<RateLimitInfo>,
    desktop_recent_scanner: DesktopRecentRolloutScanner,
    parse_cache: RefCell<CodexParseCache>,
    rollout_lifecycle: RefCell<HashMap<String, RolloutLifecycle>>,
    hook_process_states: RefCell<HashMap<HookDoneKey, HookProcessState>>,
    hook_exit_observations: RefCell<HashMap<HookDoneKey, u64>>,
    hook_process_rollout_bindings: RefCell<HashMap<HookDoneKey, HookProcessRolloutBinding>>,
    hook_live_session_snapshots: RefCell<HashMap<HookDoneKey, HookSessionSnapshot>>,
    hook_done_tombstones: RefCell<HashMap<HookDoneKey, HookDoneTombstone>>,
    herdr_status_resolver: RefCell<HerdrStatusResolver>,
    herdr_working_continuity: RefCell<HashMap<HerdrWorkingContinuityKey, u32>>,
}

const MAX_CODEX_PARSE_CACHE_ENTRIES: usize = 256;
const MAX_CODEX_DONE_TOMBSTONES: usize = 128;
const MAX_OPEN_ROLLOUT_CALLS: usize = 256;
const MAX_TRACKED_ROLLOUT_CALL_IDS: usize = 4096;
const MAX_COPIED_ROLLOUT_SESSION_META: usize = 64;
const MAX_ROLLOUT_LIFECYCLE_ID_BYTES: usize = 512;
const MAX_ROLLOUT_TOOL_NAME_BYTES: usize = 256;
const MAX_TRACKED_CODE_MODE_CELLS: usize = 128;
const MAX_CODE_MODE_STATUS_FRAME_BYTES: usize = 256;
/// Native account quota windows longer than one year are malformed rather
/// than useful display data. Bounding the duration also keeps derived labels
/// predictably small and content-free.
const MAX_NATIVE_RATE_LIMIT_WINDOW_MINUTES: u64 = 365 * 24 * 60;
#[derive(Clone, PartialEq, Eq)]
struct RolloutFingerprint {
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mtime_sec: i64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime_sec: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
    #[cfg(target_os = "windows")]
    volume_serial: u64,
    #[cfg(target_os = "windows")]
    file_id: [u8; 16],
    #[cfg(target_os = "windows")]
    creation_time: i64,
    #[cfg(target_os = "windows")]
    last_write_time: i64,
    #[cfg(target_os = "windows")]
    change_time: i64,
    #[cfg(not(any(unix, target_os = "windows")))]
    modified: std::time::SystemTime,
}

impl RolloutFingerprint {
    fn read(file: &fs::File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Codex rollout is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                len: metadata.len(),
                dev: metadata.dev(),
                ino: metadata.ino(),
                mtime_sec: metadata.mtime(),
                mtime_nsec: metadata.mtime_nsec(),
                ctime_sec: metadata.ctime(),
                ctime_nsec: metadata.ctime_nsec(),
            })
        }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO,
                FILE_ID_INFO,
            };

            let mut basic = FILE_BASIC_INFO::default();
            let mut identity = FILE_ID_INFO::default();
            // SAFETY: both buffers have the exact Win32 structures and remain
            // valid for the duration of the synchronous calls. The File owns
            // the queried handle.
            let basic_ok = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileBasicInfo,
                    (&raw mut basic).cast(),
                    std::mem::size_of::<FILE_BASIC_INFO>() as u32,
                )
            };
            // SAFETY: same argument as above for FILE_ID_INFO.
            let identity_ok = unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    FileIdInfo,
                    (&raw mut identity).cast(),
                    std::mem::size_of::<FILE_ID_INFO>() as u32,
                )
            };
            if basic_ok == 0 || identity_ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                len: metadata.len(),
                volume_serial: identity.VolumeSerialNumber,
                file_id: identity.FileId.Identifier,
                creation_time: basic.CreationTime,
                last_write_time: basic.LastWriteTime,
                change_time: basic.ChangeTime,
            })
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified()?,
        })
    }
}

fn open_rollout_file(path: &Path) -> io::Result<fs::File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new().read(true).open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex rollout is not a regular file",
        ));
    }
    Ok(file)
}

fn rollout_path_matches_fingerprint(
    path: &Path,
    canonical_path: &Path,
    expected: &RolloutFingerprint,
) -> bool {
    let Ok(before_path) = fs::canonicalize(path) else {
        return false;
    };
    if before_path != canonical_path {
        return false;
    }
    let Ok(file) = open_rollout_file(path) else {
        return false;
    };
    let Ok(observed) = RolloutFingerprint::read(&file) else {
        return false;
    };
    let Ok(after_path) = fs::canonicalize(path) else {
        return false;
    };
    observed == *expected && after_path == canonical_path
}

struct CachedCodexParse {
    fingerprint: RolloutFingerprint,
    result: CodexJSONLResult,
    last_used: u64,
}

#[derive(Default)]
struct CodexParseCache {
    entries: HashMap<PathBuf, CachedCodexParse>,
    clock: u64,
}

#[derive(Clone)]
struct RolloutLifecycle {
    root_cli_version: String,
    turn_active: bool,
    task_complete: bool,
    lifecycle_valid: bool,
    active_turn_id: Option<String>,
    completed_turn_id: Option<String>,
    turn_started_at_ms: u64,
    latest_lifecycle_at_ms: u64,
    task_completed_at_ms: u64,
    open_tool_ids: HashSet<String>,
    open_tool_started_at_ms: HashMap<String, u64>,
    open_tool_classes: HashMap<String, RolloutToolClass>,
    completed_tool_calls: HashMap<String, CompletedRolloutCall>,
    nested_code_mode_end_at_ms: HashMap<String, u64>,
    live_code_mode_cells: usize,
    code_mode_correlation_ambiguous: bool,
    descendants: Vec<DescendantRolloutLifecycle>,
    relevant_process_descendant: bool,
}

#[derive(Clone)]
struct DescendantRolloutLifecycle {
    session_id: String,
    cli_version: String,
    direct_child: bool,
    lifecycle_valid: bool,
    turn_active: bool,
    task_complete: bool,
    active_turn_id: Option<String>,
    completed_turn_id: Option<String>,
    turn_started_at_ms: u64,
    latest_lifecycle_at_ms: u64,
    task_completed_at_ms: u64,
    open_tool_ids: HashSet<String>,
    open_tool_started_at_ms: HashMap<String, u64>,
    open_tool_classes: HashMap<String, RolloutToolClass>,
    live_code_mode_cells: usize,
    code_mode_correlation_ambiguous: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RolloutToolClass {
    Ordinary,
    RequestUserInput,
    CodeModeExec {
        exec_started_at_ms: u64,
    },
    CodeModeWait {
        exec_started_at_ms: u64,
        exec_yielded_at_ms: u64,
    },
    CodeModeUncorrelatable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompletedRolloutCall {
    started_at_ms: u64,
    completed_at_ms: u64,
    class: RolloutToolClass,
    code_mode_terminal: bool,
}

impl Default for RolloutLifecycle {
    fn default() -> Self {
        Self {
            root_cli_version: String::new(),
            turn_active: false,
            task_complete: false,
            lifecycle_valid: true,
            active_turn_id: None,
            completed_turn_id: None,
            turn_started_at_ms: 0,
            latest_lifecycle_at_ms: 0,
            task_completed_at_ms: 0,
            open_tool_ids: HashSet::new(),
            open_tool_started_at_ms: HashMap::new(),
            open_tool_classes: HashMap::new(),
            completed_tool_calls: HashMap::new(),
            nested_code_mode_end_at_ms: HashMap::new(),
            live_code_mode_cells: 0,
            code_mode_correlation_ambiguous: false,
            descendants: Vec::new(),
            relevant_process_descendant: false,
        }
    }
}

impl DescendantRolloutLifecycle {
    fn has_exact_active_shape(&self, now_ms: u64) -> bool {
        self.lifecycle_valid
            && self.turn_active
            && !self.task_complete
            && self.active_turn_id.is_some()
            && self.completed_turn_id.is_none()
            && self.turn_started_at_ms > 0
            && self.turn_started_at_ms <= self.latest_lifecycle_at_ms
            && self.latest_lifecycle_at_ms <= now_ms
            && self.task_completed_at_ms == 0
            && self.open_tool_started_at_ms.len() == self.open_tool_ids.len()
            && self.open_tool_started_at_ms.iter().all(|(id, timestamp)| {
                self.open_tool_ids.contains(id)
                    && *timestamp >= self.turn_started_at_ms
                    && *timestamp <= self.latest_lifecycle_at_ms
            })
            && self.open_tool_classes.len() == self.open_tool_ids.len()
            && self
                .open_tool_classes
                .keys()
                .all(|id| self.open_tool_ids.contains(id))
    }

    fn is_exact_active(&self, now_ms: u64) -> bool {
        self.has_exact_active_shape(now_ms)
            && self.open_tool_ids.is_empty()
            && self.live_code_mode_cells == 0
            && !self.code_mode_correlation_ambiguous
    }

    fn is_exact_active_with_open_tool(&self, now_ms: u64) -> bool {
        self.has_exact_active_shape(now_ms)
            && !self.open_tool_ids.is_empty()
            && self.live_code_mode_cells == 0
            && !self.code_mode_correlation_ambiguous
    }

    fn is_exact_terminal(&self, now_ms: u64) -> bool {
        self.lifecycle_valid
            && self.task_complete
            && !self.turn_active
            && self.active_turn_id.is_none()
            && self.completed_turn_id.is_some()
            && self.turn_started_at_ms > 0
            && self.turn_started_at_ms <= self.latest_lifecycle_at_ms
            && self.latest_lifecycle_at_ms <= self.task_completed_at_ms
            && self.task_completed_at_ms <= now_ms
            && self.open_tool_ids.is_empty()
            && self.open_tool_started_at_ms.is_empty()
            && self.open_tool_classes.is_empty()
            && self.live_code_mode_cells == 0
            && !self.code_mode_correlation_ambiguous
    }
}

impl RolloutLifecycle {
    fn has_compatible_release_tree(&self) -> bool {
        // Only the selected root attests the process release. Descendant
        // metadata cannot supply a missing root version, and disagreement
        // makes the selected lifecycle tree internally inconsistent.
        plugin::codex_version_is_supported(&self.root_cli_version)
            && self
                .descendants
                .iter()
                .all(|child| child.cli_version == self.root_cli_version)
    }

    fn root_is_exact_active(&self, now_ms: u64) -> bool {
        self.lifecycle_valid
            && self.turn_active
            && !self.task_complete
            && self.active_turn_id.is_some()
            && self.completed_turn_id.is_none()
            && self.turn_started_at_ms > 0
            && self.turn_started_at_ms <= self.latest_lifecycle_at_ms
            && self.latest_lifecycle_at_ms <= now_ms
            && self.task_completed_at_ms == 0
            && self.open_tool_started_at_ms.len() == self.open_tool_ids.len()
            && self.open_tool_started_at_ms.iter().all(|(id, timestamp)| {
                self.open_tool_ids.contains(id)
                    && *timestamp >= self.turn_started_at_ms
                    && *timestamp <= self.latest_lifecycle_at_ms
            })
            && self.open_tool_classes.len() == self.open_tool_ids.len()
            && self
                .open_tool_classes
                .keys()
                .all(|id| self.open_tool_ids.contains(id))
            && self.completed_tool_calls.len() <= MAX_TRACKED_ROLLOUT_CALL_IDS
            && self.completed_tool_calls.iter().all(|(id, completion)| {
                !self.open_tool_ids.contains(id)
                    && completed_rollout_call_is_exact(
                        id,
                        completion,
                        self.turn_started_at_ms,
                        self.latest_lifecycle_at_ms,
                    )
            })
            && self.nested_code_mode_end_at_ms.len() <= MAX_TRACKED_ROLLOUT_CALL_IDS
            && self
                .nested_code_mode_end_at_ms
                .iter()
                .all(|(id, timestamp)| {
                    code_mode_nested_call_id_is_exact(id)
                        && *timestamp >= self.turn_started_at_ms
                        && *timestamp <= self.latest_lifecycle_at_ms
                })
    }

    fn descendants_are_exact_terminal(&self, now_ms: u64) -> bool {
        self.descendants
            .iter()
            .all(|child| child.is_exact_terminal(now_ms))
    }

    fn direct_child(&self, session_id: &str) -> Option<&DescendantRolloutLifecycle> {
        self.descendants
            .iter()
            .find(|child| child.direct_child && child.session_id == session_id)
    }

    /// Return the complete active direct-child rollout set only when every
    /// descriptor in the selected rollout tree is exact. Nested children are
    /// deliberately unsupported for hook promotion because a flat root hook
    /// set cannot prove their parentage.
    fn exact_direct_child_sets(&self, now_ms: u64) -> Option<(HashSet<String>, HashSet<String>)> {
        if !self.lifecycle_valid || self.descendants.iter().any(|child| !child.direct_child) {
            return None;
        }
        let mut active = HashSet::new();
        let mut terminal = HashSet::new();
        for child in &self.descendants {
            if child.is_exact_active(now_ms) {
                if !active.insert(child.session_id.clone()) {
                    return None;
                }
            } else if child.is_exact_terminal(now_ms) {
                if !terminal.insert(child.session_id.clone()) {
                    return None;
                }
            } else {
                return None;
            }
        }
        Some((active, terminal))
    }
}

#[derive(Clone, Debug)]
enum HookCandidate {
    Unknown(StatusReason),
    TurnOpen,
    ToolOpen(HashSet<String>),
    SubagentOpen {
        active: HashSet<String>,
        provisional: HashSet<String>,
        root: HookRootCandidate,
    },
    TurnStopped,
    Ended,
}

#[derive(Clone, Debug)]
enum HookRootCandidate {
    Unknown(StatusReason),
    TurnOpen,
    ToolOpen(HashSet<String>),
    TurnStopped,
    Ended,
}

impl From<HookRootProjection> for HookRootCandidate {
    fn from(projection: HookRootProjection) -> Self {
        match projection {
            HookRootProjection::Unknown(reason) => Self::Unknown(reason),
            HookRootProjection::TurnOpen => Self::TurnOpen,
            HookRootProjection::ToolOpen(ids) => {
                Self::ToolOpen(ids.into_iter().collect::<HashSet<_>>())
            }
            HookRootProjection::TurnStopped => Self::TurnStopped,
            HookRootProjection::Ended => Self::Ended,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookProcessState {
    Live,
    Gone,
    Unverified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HookProcessRolloutBinding {
    session_id: String,
    supported_release: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct HookDoneKey {
    session_id: String,
    generation_id: String,
    pid: u32,
    process_incarnation: String,
}

#[derive(Clone)]
struct HookDoneTombstone {
    exit_observed_at_ms: u64,
    snapshot: HookSessionSnapshot,
}

#[derive(Clone)]
struct HookSessionSnapshot {
    cwd: String,
    project_name: String,
    started_at: u64,
    model: String,
    effort: String,
    context_percent: f64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read: u64,
    total_cache_create: u64,
    turn_count: u32,
    version: String,
    git_added: u32,
    git_modified: u32,
    token_history: Vec<u64>,
    context_history: Vec<u64>,
    compaction_count: u32,
    context_window: u64,
    mem_file_count: u32,
    mem_line_count: u32,
    config_root: String,
}

impl HookSessionSnapshot {
    fn capture(session: &AgentSession) -> Self {
        Self {
            cwd: session.cwd.clone(),
            project_name: session.project_name.clone(),
            started_at: session.started_at,
            model: session.model.clone(),
            effort: session.effort.clone(),
            context_percent: session.context_percent,
            total_input_tokens: session.total_input_tokens,
            total_output_tokens: session.total_output_tokens,
            total_cache_read: session.total_cache_read,
            total_cache_create: session.total_cache_create,
            turn_count: session.turn_count,
            version: session.version.clone(),
            git_added: session.git_added,
            git_modified: session.git_modified,
            token_history: session.token_history.clone(),
            context_history: session.context_history.clone(),
            compaction_count: session.compaction_count,
            context_window: session.context_window,
            mem_file_count: session.mem_file_count,
            mem_line_count: session.mem_line_count,
            config_root: session.config_root.clone(),
        }
    }

    fn done_session(&self, key: &HookDoneKey, exit_observed_at_ms: u64) -> AgentSession {
        AgentSession {
            agent_cli: "codex",
            pid: key.pid,
            action_process_incarnation: None,
            session_id: key.session_id.clone(),
            cwd: self.cwd.clone(),
            project_name: self.project_name.clone(),
            started_at: self.started_at,
            status: SessionStatus::Done,
            status_evidence: status_evidence(
                SessionStatus::Done,
                StatusAuthority::Heuristic,
                StatusReason::ProcessExited,
                exit_observed_at_ms,
                0,
            ),
            model: self.model.clone(),
            effort: self.effort.clone(),
            context_percent: self.context_percent,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            total_cache_read: self.total_cache_read,
            total_cache_create: self.total_cache_create,
            turn_count: self.turn_count,
            current_tasks: vec!["finished".to_string()],
            mem_mb: 0,
            version: self.version.clone(),
            git_branch: String::new(),
            git_added: self.git_added,
            git_modified: self.git_modified,
            token_history: self.token_history.clone(),
            context_history: self.context_history.clone(),
            compaction_count: self.compaction_count,
            context_window: self.context_window,
            subagents: Vec::new(),
            mem_file_count: self.mem_file_count,
            mem_line_count: self.mem_line_count,
            children: Vec::new(),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            awaiting_input: false,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
            config_root: self.config_root.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct HookCollectorRecord {
    generation_id: String,
    session_id: String,
    cwd: String,
    started_at_ms: u64,
    observed_at_ms: u64,
    status_since_ms: u64,
    ended_at_ms: u64,
    exit_observed_at_ms: u64,
    /// The last exact live root binding used a compatible Codex release.
    exit_supported_rollout_correlated: bool,
    pid: u32,
    process_incarnation: Option<String>,
    process_state: HookProcessState,
    native_process_verified: bool,
    /// Current process ownership and the matched root rollout both attest the audited release.
    supported_release_attested: bool,
    /// Exact, thread-bound proof that this live Codex process actually loaded
    /// and enabled abtop's complete hook engine. Supported Codex releases expose no such
    /// proof, so production state conversion always leaves this false. It is
    /// an actionability boundary; exact lifecycle shapes may still support a
    /// conservative, non-actionable heuristic display.
    effective_hook_engine_attested: bool,
    actionable: bool,
    owns_resources: bool,
    local_config_ambiguous: bool,
    interaction_ambiguous: bool,
    subagent_set_complete: bool,
    turn_id: Option<String>,
    prompt_observed_at_ms: u64,
    stop_observed_at_ms: u64,
    tool_opened_at_ms: HashMap<String, u64>,
    tool_classes: HashMap<String, HookToolClass>,
    subagent_opened_at_ms: HashMap<String, u64>,
    subagent_stopped_at_ms: HashMap<String, u64>,
    candidate: HookCandidate,
    observations: Vec<StatusObservation>,
}

fn hook_record_is_active_generation(record: &HookCollectorRecord) -> bool {
    !matches!(record.candidate, HookCandidate::Ended)
        && record.ended_at_ms == 0
        && record.process_state == HookProcessState::Live
}

fn hook_candidate_allows_exit_transition(candidate: &HookCandidate) -> bool {
    match candidate {
        HookCandidate::Unknown(
            StatusReason::HookInteractionResolutionUnavailable | StatusReason::HookToolOpen,
        ) => true,
        HookCandidate::Unknown(_) => false,
        HookCandidate::TurnOpen
        | HookCandidate::ToolOpen(_)
        | HookCandidate::SubagentOpen { .. }
        | HookCandidate::TurnStopped
        | HookCandidate::Ended => true,
    }
}

fn hook_record_process_key(record: &HookCollectorRecord) -> Option<(u32, String)> {
    (record.pid != 0)
        .then(|| {
            record
                .process_incarnation
                .as_ref()
                .filter(|incarnation| !incarnation.is_empty())
                .map(|incarnation| (record.pid, incarnation.clone()))
        })
        .flatten()
}

fn hook_done_key(record: &HookCollectorRecord) -> Option<HookDoneKey> {
    let (_, process_incarnation) = hook_record_process_key(record)?;
    (!record.session_id.is_empty() && !record.generation_id.is_empty()).then(|| HookDoneKey {
        session_id: record.session_id.clone(),
        generation_id: record.generation_id.clone(),
        pid: record.pid,
        process_incarnation,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurrentGenerationSelection {
    Absent,
    Unique(usize),
    Ambiguous,
}

fn classify_current_generation(
    candidates: impl IntoIterator<Item = usize>,
) -> CurrentGenerationSelection {
    let mut candidates = candidates.into_iter();
    let Some(first) = candidates.next() else {
        return CurrentGenerationSelection::Absent;
    };
    if candidates.next().is_some() {
        CurrentGenerationSelection::Ambiguous
    } else {
        CurrentGenerationSelection::Unique(first)
    }
}

fn agreed_current_generation(
    rollout: CurrentGenerationSelection,
    herdr: CurrentGenerationSelection,
) -> Option<usize> {
    use CurrentGenerationSelection::{Absent, Ambiguous, Unique};

    match (rollout, herdr) {
        (Unique(left), Unique(right)) if left == right => Some(left),
        (Unique(index), Absent) | (Absent, Unique(index)) => Some(index),
        (Absent, Absent) | (Ambiguous, _) | (_, Ambiguous) | (Unique(_), Unique(_)) => None,
    }
}

/// Select the one current thread hosted by an exact native Codex process.
///
/// Codex can switch threads in place without ending the prior hook generation,
/// leaving several live-looking records on one PID/incarnation. A unique
/// process-owned rollout or exact Herdr identity may choose the display record;
/// disagreement remains fail-closed. This selection is status-only: it never
/// grants process actions or resource ownership.
fn reconcile_current_hook_generations(
    records: &mut Vec<HookCollectorRecord>,
    sessions: &mut Vec<AgentSession>,
    rollout_lifecycle: &HashMap<String, RolloutLifecycle>,
    herdr_observations: &HashMap<(String, u32), HerdrObservation>,
    process_info: &HashMap<u32, ProcInfo>,
    eligible_pids: &HashSet<u32>,
) {
    let mut groups = HashMap::<(u32, String), Vec<usize>>::new();
    for (index, record) in records.iter().enumerate() {
        if !hook_record_is_active_generation(record) {
            continue;
        }
        if let Some(key) = hook_record_process_key(record) {
            groups.entry(key).or_default().push(index);
        }
    }

    let mut suppressed = HashSet::new();
    for indices in groups.values().filter(|indices| indices.len() > 1) {
        let rollout_selection =
            classify_current_generation(indices.iter().copied().filter(|index| {
                let record = &records[*index];
                if !record.native_process_verified || !eligible_pids.contains(&record.pid) {
                    return false;
                }
                let mut matching = sessions.iter().filter(|session| {
                    session.session_id == record.session_id
                        && session.pid == record.pid
                        && session.cwd == record.cwd
                        && process_info.contains_key(&session.pid)
                        && rollout_lifecycle
                            .get(&record.session_id)
                            .is_some_and(|lifecycle| {
                                lifecycle.lifecycle_valid
                                    && session.version == lifecycle.root_cli_version
                            })
                });
                matching.next().is_some() && matching.next().is_none()
            }));
        let herdr_selection =
            classify_current_generation(indices.iter().copied().filter(|index| {
                let record = &records[*index];
                record.native_process_verified
                    && eligible_pids.contains(&record.pid)
                    && herdr_observations.contains_key(&(record.session_id.clone(), record.pid))
            }));
        let Some(selected) = agreed_current_generation(rollout_selection, herdr_selection) else {
            continue;
        };

        // Multiple persisted records still claim this process. The selected
        // record can describe status, but Herdr/current-thread selection never
        // resolves destructive-action or resource ownership.
        records[selected].actionable = false;
        records[selected].owns_resources = false;
        suppressed.extend(indices.iter().copied().filter(|index| *index != selected));
    }

    if suppressed.is_empty() {
        return;
    }
    let suppressed_claims = suppressed
        .iter()
        .map(|index| {
            let record = &records[*index];
            (record.session_id.clone(), record.pid, record.cwd.clone())
        })
        .collect::<Vec<_>>();
    sessions.retain(|session| {
        !suppressed_claims.iter().any(|(session_id, pid, cwd)| {
            session.session_id == *session_id
                && session.cwd == *cwd
                && (session.pid == 0 || session.pid == *pid)
        })
    });
    let mut index = 0_usize;
    records.retain(|_| {
        let keep = !suppressed.contains(&index);
        index += 1;
        keep
    });
}

fn cwd_has_unattested_codex_config(cwd: &str, codex_home: &Path) -> bool {
    let cwd = Path::new(cwd);
    if !cwd.is_absolute() {
        return true;
    }
    let Ok(canonical_cwd) = fs::canonicalize(cwd) else {
        return true;
    };
    let Ok(canonical_codex_home) = fs::canonicalize(codex_home) else {
        return true;
    };
    for ancestor in canonical_cwd.ancestors() {
        let project_config = ancestor.join(".codex");
        let is_attested_base =
            fs::canonicalize(&project_config).is_ok_and(|path| path == canonical_codex_home);
        for name in ["config.toml", ".config.lock.toml", "config.lock.toml"] {
            if name == "config.toml" && is_attested_base {
                continue;
            }
            match fs::symlink_metadata(project_config.join(name)) {
                Ok(_) => return true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return true,
            }
        }
    }
    false
}

#[derive(Clone, Copy)]
struct CodexProcessContext {
    pid: Option<u32>,
    is_exec: bool,
    owns_process_tree: bool,
    unknown_process_owner: bool,
}

struct CodexCliSessionGroupLoad {
    session: Option<AgentSession>,
    rate_limit: Option<RateLimitInfo>,
    owned_paths: Vec<PathBuf>,
}

struct DesktopRecentRolloutScanResult {
    rollouts: Vec<PathBuf>,
}

struct DesktopRecentRolloutScanner {
    cached: Vec<PathBuf>,
    in_flight: bool,
    last_started: Option<Instant>,
    tx: Sender<DesktopRecentRolloutScanResult>,
    rx: Receiver<DesktopRecentRolloutScanResult>,
}

const DESKTOP_RECENT_ROLLOUT_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

impl DesktopRecentRolloutScanner {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            cached: Vec::new(),
            in_flight: false,
            last_started: None,
            tx,
            rx,
        }
    }

    fn update(&mut self, sessions_dir: &Path, active_mtime_secs: u64) -> Vec<PathBuf> {
        self.poll_completed();
        if self.should_start(sessions_dir) {
            self.start(sessions_dir.to_path_buf(), active_mtime_secs);
        }
        self.cached.clone()
    }

    fn poll_completed(&mut self) {
        while let Ok(result) = self.rx.try_recv() {
            self.cached = result.rollouts;
            self.in_flight = false;
        }
    }

    fn should_start(&self, sessions_dir: &Path) -> bool {
        if self.in_flight || !sessions_dir.exists() {
            return false;
        }
        self.last_started
            .is_none_or(|started| started.elapsed() >= DESKTOP_RECENT_ROLLOUT_RESCAN_INTERVAL)
    }

    fn start(&mut self, sessions_dir: PathBuf, active_mtime_secs: u64) {
        self.in_flight = true;
        self.last_started = Some(Instant::now());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let rollouts = CodexCollector::recent_desktop_rollouts(
                &sessions_dir,
                &HashSet::new(),
                &HashSet::new(),
                active_mtime_secs,
            );
            let _ = tx.send(DesktopRecentRolloutScanResult { rollouts });
        });
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn status_evidence(
    status: SessionStatus,
    authority: StatusAuthority,
    reason: StatusReason,
    observed_at_ms: u64,
    connection_generation: u64,
) -> StatusEvidence {
    let mut evidence = StatusEvidence::default();
    evidence.observe(StatusObservation::new(
        status,
        authority,
        reason,
        observed_at_ms,
        connection_generation,
    ));
    evidence
}

fn timestamp_is_recent_past(now_ms: u64, timestamp_ms: u64, maximum_age_ms: u64) -> bool {
    timestamp_ms != 0 && timestamp_ms <= now_ms && now_ms - timestamp_ms <= maximum_age_ms
}

fn is_native_codex_executable(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    (name == "codex" || name == "codex.exe" || name.starts_with("codex-"))
        && name != "codex-app-server"
        && name != "codex-app-server.exe"
        && name != "codex-code-mode-host"
        && name != "codex-code-mode-host.exe"
}

/// Bind hook state to one exact native Codex process observation. Incarnation
/// reads bracket argv/executable classification so PID reuse fails closed.
pub(super) fn native_codex_process_is_exact(pid: u32, expected_incarnation: &str) -> bool {
    let Some(before) = process::get_process_incarnation(pid) else {
        return false;
    };
    if before != expected_incarnation {
        return false;
    }
    let Some(executable) = process::get_process_executable(pid) else {
        return false;
    };
    let Some(argv) = process::get_process_argv(pid) else {
        return false;
    };
    let Some(after) = process::get_process_incarnation(pid) else {
        return false;
    };
    if after != expected_incarnation || !is_native_codex_executable(&executable) {
        return false;
    }
    !argv.iter().skip(1).any(|argument| {
        matches!(
            argument.to_str(),
            Some(
                "app-server"
                    | "daemon"
                    | "mcp-server"
                    | "remote-control"
                    | "exec-server"
                    | "codex-code-mode-host"
            )
        )
    })
}

fn hook_record_from_state(state: HookSessionState) -> HookCollectorRecord {
    let projection = state.projection();
    let prompt_observed_at_ms = state.prompt_observed_at_ms;
    let stop_observed_at_ms = state.stop_observed_at_ms;
    let tool_opened_at_ms = state
        .tool_opened_at_ms
        .iter()
        .map(|(id, timestamp)| (id.clone(), *timestamp))
        .collect::<HashMap<_, _>>();
    let tool_classes = state
        .open_tools
        .iter()
        .map(|(id, class)| (id.clone(), *class))
        .collect::<HashMap<_, _>>();
    let subagent_opened_at_ms = state
        .subagent_opened_at_ms
        .iter()
        .map(|(id, timestamp)| (id.clone(), *timestamp))
        .collect::<HashMap<_, _>>();
    let subagent_stopped_at_ms = state
        .subagent_stopped_at_ms
        .iter()
        .map(|(id, timestamp)| (id.clone(), *timestamp))
        .collect::<HashMap<_, _>>();
    let turn_id = match &projection {
        HookProjection::TurnOpen | HookProjection::ToolOpen(_) => state.active_turn_id.clone(),
        HookProjection::SubagentOpen { root, .. } => match root {
            HookRootProjection::TurnStopped => state.stop_turn_id.clone(),
            HookRootProjection::TurnOpen | HookRootProjection::ToolOpen(_) => {
                state.active_turn_id.clone()
            }
            HookRootProjection::Unknown(_) | HookRootProjection::Ended => None,
        },
        HookProjection::TurnStopped => state.stop_turn_id.clone(),
        HookProjection::Unknown(_) | HookProjection::Ended => None,
    };
    let candidate = match projection {
        HookProjection::Unknown(reason) => HookCandidate::Unknown(reason),
        HookProjection::TurnOpen => HookCandidate::TurnOpen,
        HookProjection::ToolOpen(ids) => {
            HookCandidate::ToolOpen(ids.into_iter().collect::<HashSet<_>>())
        }
        HookProjection::SubagentOpen {
            active,
            provisional,
            root,
        } => HookCandidate::SubagentOpen {
            active: active.into_iter().collect::<HashSet<_>>(),
            provisional: provisional.into_iter().collect::<HashSet<_>>(),
            root: root.into(),
        },
        HookProjection::TurnStopped => HookCandidate::TurnStopped,
        HookProjection::Ended => HookCandidate::Ended,
    };
    let status_since_ms = match &candidate {
        HookCandidate::TurnOpen => prompt_observed_at_ms,
        HookCandidate::ToolOpen(ids) => ids
            .iter()
            .filter_map(|id| tool_opened_at_ms.get(id).copied())
            .filter(|timestamp| *timestamp > 0)
            .min()
            .unwrap_or(0),
        HookCandidate::SubagentOpen {
            active,
            provisional,
            ..
        } => active
            .iter()
            .filter_map(|id| subagent_opened_at_ms.get(id).copied())
            .chain(
                provisional
                    .iter()
                    .filter_map(|id| subagent_stopped_at_ms.get(id).copied()),
            )
            .filter(|timestamp| *timestamp > 0)
            .min()
            .unwrap_or(0),
        HookCandidate::TurnStopped => stop_observed_at_ms,
        HookCandidate::Unknown(_) | HookCandidate::Ended => state.updated_at_ms,
    };
    let process_state = if state.process.matches_live_process() {
        HookProcessState::Live
    } else if state.process.confirmed_gone() {
        HookProcessState::Gone
    } else {
        HookProcessState::Unverified
    };
    let native_process_verified = process_state == HookProcessState::Live
        && native_codex_process_is_exact(state.process.pid, &state.process.incarnation);
    let process_shape_valid = state.process.started_at_ms > 0
        && state.process.started_at_ms <= state.created_at_ms
        && state.created_at_ms <= state.updated_at_ms;
    let candidate = if process_shape_valid {
        candidate
    } else {
        HookCandidate::Unknown(StatusReason::HookStateMalformed)
    };
    let actionable = native_process_verified && state.process.actionable();
    let interaction_ambiguous = state.interaction_ambiguous();
    let subagent_set_complete = state.integration.complete_hook_set;
    let observations = state
        .samples
        .iter()
        .map(|sample| {
            // Persisted hook samples are lifecycle candidates. Only the
            // current projection can be correlated with rollout/process
            // evidence, so historical samples remain conservative.
            StatusObservation::new(
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                sample.reason,
                sample.observed_at_ms,
                0,
            )
        })
        .collect();

    HookCollectorRecord {
        generation_id: state.generation_id,
        session_id: state.session_id,
        cwd: state.cwd,
        started_at_ms: state.created_at_ms,
        observed_at_ms: state.updated_at_ms,
        status_since_ms,
        ended_at_ms: state.ended_at_ms,
        exit_observed_at_ms: 0,
        exit_supported_rollout_correlated: false,
        pid: state.process.pid,
        process_incarnation: Some(state.process.incarnation),
        process_state,
        native_process_verified,
        supported_release_attested: false,
        effective_hook_engine_attested: false,
        actionable,
        owns_resources: actionable,
        local_config_ambiguous: false,
        interaction_ambiguous,
        subagent_set_complete,
        turn_id,
        prompt_observed_at_ms,
        stop_observed_at_ms,
        tool_opened_at_ms,
        tool_classes,
        subagent_opened_at_ms,
        subagent_stopped_at_ms,
        candidate,
        observations,
    }
}

fn safe_rollout_task_preview(session: &AgentSession) -> Option<String> {
    let task = session
        .current_tasks
        .last()
        .map(String::as_str)
        .map(str::trim)
        .filter(|task| {
            !task.is_empty()
                && !matches!(
                    *task,
                    "unknown"
                        | "idle"
                        | "thinking"
                        | "executing"
                        | "finished"
                        | "waiting for user input"
                        | "rate limited"
                        | "error"
                        | "authoritative status unavailable"
                )
        })?;
    // Tool names originate in provider data and are not bounded by the
    // rollout schema. Bound work before allocating sanitized copies, but keep
    // enough lookahead to redact a credential that begins near the 160-char
    // display boundary.
    let scan_bounded = task.chars().take(512).collect::<String>();
    let terminal_safe = super::sanitize_terminal_text(&scan_bounded);
    let known_redacted = super::redact_secrets(&terminal_safe);
    let redacted = redact_generic_sk_token(&known_redacted);
    let bounded = redacted.chars().take(160).collect::<String>();
    (!bounded.trim().is_empty()).then_some(bounded)
}

/// Redact legacy/unknown `sk-...` credentials not covered by the provider-
/// specific prefixes in the shared redactor. Require a lexical boundary and
/// a substantial token body so ordinary substrings such as `task-sketch`,
/// `sketch`, and short identifiers remain visible.
fn redact_generic_sk_token(input: &str) -> String {
    const MIN_SECRET_BODY_CHARS: usize = 8;

    let mut output = String::with_capacity(input.len());
    let mut emitted = 0;
    let mut scan = 0;
    while let Some(relative) = input[scan..].find("sk-") {
        let start = scan + relative;
        let boundary = input[..start].chars().next_back().is_none_or(|character| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-')
        });
        let body_start = start + "sk-".len();
        let body_end = input[body_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                (!character.is_ascii_alphanumeric() && !matches!(character, '_' | '-'))
                    .then_some(body_start + offset)
            })
            .unwrap_or(input.len());
        let body = &input[body_start..body_end];
        let looks_secret = boundary
            && body.chars().count() >= MIN_SECRET_BODY_CHARS
            && body.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            });

        if looks_secret {
            output.push_str(&input[emitted..start]);
            output.push_str("[REDACTED]");
            emitted = body_end;
            scan = body_end;
        } else {
            scan = body_start;
        }
    }
    output.push_str(&input[emitted..]);
    output
}

fn collect_resource_children(
    root_pid: u32,
    process_info: &HashMap<u32, ProcInfo>,
    children_map: &HashMap<u32, Vec<u32>>,
    ports: &HashMap<u32, Vec<u16>>,
) -> Vec<ChildProcess> {
    let mut children = Vec::new();
    let mut stack = children_map.get(&root_pid).cloned().unwrap_or_default();
    let mut visited = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if let Some(process) = process_info.get(&pid) {
            children.push(ChildProcess {
                pid,
                command: process.command.clone(),
                mem_kb: process.rss_kb,
                port: ports.get(&pid).and_then(|values| values.first().copied()),
            });
        }
        if let Some(descendants) = children_map.get(&pid) {
            stack.extend(descendants);
        }
    }
    children.sort_by_key(|child| child.pid);
    children
}

fn has_relevant_codex_process_descendant(
    root_pid: u32,
    process_info: &HashMap<u32, ProcInfo>,
    children_map: &HashMap<u32, Vec<u32>>,
    mcp_server_pids: &HashSet<u32>,
) -> bool {
    let mut stack = children_map.get(&root_pid).cloned().unwrap_or_default();
    let mut visited = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if mcp_server_pids.contains(&pid) {
            // Shared discovery has already proven this exact root as an MCP
            // host. Its complete subtree is provider infrastructure.
            continue;
        }
        let Some(child) = process_info.get(&pid) else {
            // A process edge without a contemporaneous process snapshot cannot
            // corroborate inactivity.
            return true;
        };
        if process::cmd_has_binary(&child.command, "codex-code-mode-host") {
            if let Some(descendants) = children_map.get(&pid) {
                stack.extend(descendants);
            }
            continue;
        }
        return true;
    }
    false
}

fn mark_codex_status_unavailable(session: &mut AgentSession, now_ms: u64, reason: StatusReason) {
    session.status = SessionStatus::Unknown;
    session.action_process_incarnation = None;
    session.status_evidence = status_evidence(
        SessionStatus::Unknown,
        StatusAuthority::Unavailable,
        reason,
        now_ms,
        0,
    );
    session.current_tasks = vec!["status evidence unavailable".to_string()];
    session.pending_since_ms = 0;
    session.thinking_since_ms = 0;
    session.awaiting_input = false;
    session.enforce_status_contract();
}

fn hook_edge_timestamp_is_valid(record: &HookCollectorRecord, timestamp_ms: u64) -> bool {
    timestamp_ms >= record.started_at_ms
        && timestamp_ms <= record.observed_at_ms
        && timestamp_ms > 0
}

fn hook_id_timestamps_are_exact(
    record: &HookCollectorRecord,
    ids: &HashSet<String>,
    timestamps: &HashMap<String, u64>,
) -> bool {
    timestamps.len() == ids.len()
        && ids.iter().all(|id| {
            timestamps
                .get(id)
                .is_some_and(|timestamp| hook_edge_timestamp_is_valid(record, *timestamp))
        })
}

fn hook_open_tools_are_exact_ordinary(
    ids: &HashSet<String>,
    classes: &HashMap<String, HookToolClass>,
) -> bool {
    classes.len() == ids.len()
        && ids.iter().all(|id| {
            classes
                .get(id)
                .is_some_and(|class| *class == HookToolClass::Ordinary)
        })
}

fn rollout_open_tools_are_exact_ordinary(
    ids: &HashSet<String>,
    classes: &HashMap<String, RolloutToolClass>,
) -> bool {
    classes.len() == ids.len()
        && ids.iter().all(|id| {
            classes
                .get(id)
                .is_some_and(|class| *class == RolloutToolClass::Ordinary)
        })
}

fn code_mode_nested_call_id_is_exact(id: &str) -> bool {
    let Some(uuid) = id.strip_prefix("exec-") else {
        return false;
    };
    let bytes = uuid.as_bytes();
    if bytes.len() != 36
        || ![8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        || bytes[14] != b'4'
        || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
    {
        return false;
    }
    bytes.iter().enumerate().all(|(index, byte)| {
        matches!(index, 8 | 13 | 18 | 23) || byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
    })
}

fn uuid_v7_timestamp_ms(id: &str) -> Option<u64> {
    let bytes = id.as_bytes();
    if bytes.len() != 36
        || ![8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        || bytes[14] != b'7'
        || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
        || !bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 8 | 13 | 18 | 23)
                || byte.is_ascii_digit()
                || matches!(byte, b'a'..=b'f')
        })
    {
        return None;
    }

    let timestamp_hex = [&id[..8], &id[9..13]].concat();
    u64::from_str_radix(&timestamp_hex, 16).ok()
}

fn rollout_call_id_is_exact(id: &str) -> bool {
    let Some(suffix) = id.strip_prefix("call_") else {
        return false;
    };
    !suffix.is_empty()
        && suffix.len() <= 128
        && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn completed_rollout_call_is_exact(
    call_id: &str,
    completion: &CompletedRolloutCall,
    turn_started_at_ms: u64,
    latest_lifecycle_at_ms: u64,
) -> bool {
    rollout_call_id_is_exact(call_id)
        && turn_started_at_ms <= completion.started_at_ms
        && completion.started_at_ms <= completion.completed_at_ms
        && completion.completed_at_ms <= latest_lifecycle_at_ms
        && match completion.class {
            RolloutToolClass::CodeModeExec { exec_started_at_ms } => {
                exec_started_at_ms == completion.started_at_ms
            }
            RolloutToolClass::Ordinary
            | RolloutToolClass::RequestUserInput
            | RolloutToolClass::CodeModeWait { .. }
            | RolloutToolClass::CodeModeUncorrelatable => !completion.code_mode_terminal,
        }
}

fn code_mode_open_tool_shape_is_exact(
    hook_ids: &HashSet<String>,
    hook_classes: &HashMap<String, HookToolClass>,
    hook_opened_at_ms: &HashMap<String, u64>,
    state: &RolloutLifecycle,
) -> bool {
    if !plugin::codex_version_has_verified_code_mode_shape(&state.root_cli_version)
        || hook_ids.len() != 1
        || state.open_tool_ids.len() != 1
        || !hook_open_tools_are_exact_ordinary(hook_ids, hook_classes)
        || state.open_tool_classes.len() != 1
        || state.code_mode_correlation_ambiguous
    {
        return false;
    }
    let Some(hook_id) = hook_ids.iter().next() else {
        return false;
    };
    let Some(rollout_id) = state.open_tool_ids.iter().next() else {
        return false;
    };
    let Some(rollout_started_at_ms) = state.open_tool_started_at_ms.get(rollout_id).copied() else {
        return false;
    };
    let Some(class) = state.open_tool_classes.get(rollout_id) else {
        return false;
    };
    let Some(hook_opened_at_ms) = hook_opened_at_ms.get(hook_id).copied() else {
        return false;
    };
    if !code_mode_nested_call_id_is_exact(hook_id) || !rollout_call_id_is_exact(rollout_id) {
        return false;
    }
    match class {
        RolloutToolClass::CodeModeExec { exec_started_at_ms } => {
            state.live_code_mode_cells == 0
                && *exec_started_at_ms == rollout_started_at_ms
                && state.turn_started_at_ms <= *exec_started_at_ms
                && *exec_started_at_ms <= hook_opened_at_ms
        }
        RolloutToolClass::CodeModeWait {
            exec_started_at_ms,
            exec_yielded_at_ms,
        } => {
            state.live_code_mode_cells == 1
                && *exec_started_at_ms <= *exec_yielded_at_ms
                && *exec_yielded_at_ms <= rollout_started_at_ms
                && *exec_started_at_ms <= hook_opened_at_ms
                && hook_opened_at_ms <= *exec_yielded_at_ms
        }
        RolloutToolClass::Ordinary
        | RolloutToolClass::RequestUserInput
        | RolloutToolClass::CodeModeUncorrelatable => false,
    }
}

fn exactly_completed_root_hook_ids(
    record: &HookCollectorRecord,
    hook_ids: &HashSet<String>,
    state: &RolloutLifecycle,
    now_ms: u64,
) -> Option<HashSet<String>> {
    if !hook_id_timestamps_are_exact(record, hook_ids, &record.tool_opened_at_ms)
        || !hook_open_tools_are_exact_ordinary(hook_ids, &record.tool_classes)
        || !state.root_is_exact_active(now_ms)
        || state.active_turn_id != record.turn_id
        || record.turn_id.is_none()
        || state.code_mode_correlation_ambiguous
    {
        return None;
    }

    let mut nested_end_timestamps = state
        .nested_code_mode_end_at_ms
        .values()
        .copied()
        .collect::<Vec<_>>();
    nested_end_timestamps.sort_unstable();
    let completions_with_nested_identity = state
        .completed_tool_calls
        .iter()
        .filter_map(|(outer_id, call)| {
            let first =
                nested_end_timestamps.partition_point(|timestamp| *timestamp < call.started_at_ms);
            nested_end_timestamps
                .get(first)
                .is_some_and(|timestamp| *timestamp <= call.completed_at_ms)
                .then_some(outer_id.as_str())
        })
        .collect::<HashSet<_>>();

    let mut completed = HashSet::new();
    let mut nested_candidates = Vec::<(String, String)>::new();
    for hook_id in hook_ids {
        let hook_opened_at_ms = record.tool_opened_at_ms.get(hook_id).copied()?;
        if rollout_call_id_is_exact(hook_id) {
            if state.completed_tool_calls.get(hook_id).is_some_and(|call| {
                call.class == RolloutToolClass::Ordinary
                    && !state.open_tool_ids.contains(hook_id)
                    && call.started_at_ms <= hook_opened_at_ms
                    && hook_opened_at_ms <= call.completed_at_ms
                    && call.completed_at_ms <= record.observed_at_ms
            }) {
                completed.insert(hook_id.clone());
            }
            continue;
        }
        if !plugin::codex_version_has_verified_code_mode_shape(&state.root_cli_version)
            || !code_mode_nested_call_id_is_exact(hook_id)
        {
            continue;
        }
        let mut matching_outer_calls =
            state
                .completed_tool_calls
                .iter()
                .filter_map(|(outer_id, call)| {
                    matches!(
                        call.class,
                        RolloutToolClass::CodeModeExec { exec_started_at_ms }
                            if exec_started_at_ms == call.started_at_ms
                    )
                    .then_some((outer_id, call))
                    .filter(|(outer_id, call)| {
                        let nested_identity_observed =
                            completions_with_nested_identity.contains(outer_id.as_str());
                        let nested_identity_matches = !nested_identity_observed
                            || state.nested_code_mode_end_at_ms.get(hook_id).is_some_and(
                                |ended_at_ms| {
                                    call.started_at_ms <= *ended_at_ms
                                        && hook_opened_at_ms <= *ended_at_ms
                                        && *ended_at_ms <= call.completed_at_ms
                                },
                            );
                        call.code_mode_terminal
                            && !state.open_tool_ids.contains(*outer_id)
                            && call.started_at_ms <= hook_opened_at_ms
                            && hook_opened_at_ms <= call.completed_at_ms
                            && call.completed_at_ms <= record.observed_at_ms
                            && nested_identity_matches
                    })
                });
        let Some((outer_id, _)) = matching_outer_calls.next() else {
            continue;
        };
        if matching_outer_calls.next().is_none() {
            nested_candidates.push((hook_id.clone(), outer_id.clone()));
        }
    }

    let mut outer_counts = HashMap::<String, usize>::new();
    for (_, outer_id) in &nested_candidates {
        *outer_counts.entry(outer_id.clone()).or_default() += 1;
    }
    completed.extend(
        nested_candidates
            .into_iter()
            .filter_map(|(hook_id, outer_id)| {
                (outer_counts.get(&outer_id) == Some(&1)).then_some(hook_id)
            }),
    );
    Some(completed)
}

fn reconciled_herdr_hook_record(
    record: &HookCollectorRecord,
    state: &RolloutLifecycle,
    now_ms: u64,
) -> Option<HookCollectorRecord> {
    let root_hook_ids = match &record.candidate {
        HookCandidate::ToolOpen(ids) => ids,
        HookCandidate::SubagentOpen {
            root: HookRootCandidate::ToolOpen(ids),
            ..
        } => ids,
        _ => return Some(record.clone()),
    };
    let completed = exactly_completed_root_hook_ids(record, root_hook_ids, state, now_ms)?;
    if completed.is_empty() {
        return Some(record.clone());
    }
    let remaining = root_hook_ids
        .difference(&completed)
        .cloned()
        .collect::<HashSet<_>>();
    let mut reconciled = record.clone();
    reconciled
        .tool_opened_at_ms
        .retain(|id, _| remaining.contains(id));
    reconciled
        .tool_classes
        .retain(|id, _| remaining.contains(id));
    reconciled.candidate = match &record.candidate {
        HookCandidate::ToolOpen(_) if remaining.is_empty() => HookCandidate::TurnOpen,
        HookCandidate::ToolOpen(_) => HookCandidate::ToolOpen(remaining.clone()),
        HookCandidate::SubagentOpen {
            active,
            provisional,
            root: HookRootCandidate::ToolOpen(_),
        } => HookCandidate::SubagentOpen {
            active: active.clone(),
            provisional: provisional.clone(),
            root: if remaining.is_empty() {
                HookRootCandidate::TurnOpen
            } else {
                HookRootCandidate::ToolOpen(remaining.clone())
            },
        },
        _ => return Some(record.clone()),
    };
    if matches!(reconciled.candidate, HookCandidate::TurnOpen) {
        reconciled.status_since_ms = reconciled.prompt_observed_at_ms;
    } else if matches!(reconciled.candidate, HookCandidate::ToolOpen(_)) {
        reconciled.status_since_ms = remaining
            .iter()
            .filter_map(|id| reconciled.tool_opened_at_ms.get(id).copied())
            .min()
            .unwrap_or(0);
    }
    Some(reconciled)
}

fn subagent_root_tool_refinement_is_exact(
    record: &HookCollectorRecord,
    state: &RolloutLifecycle,
    now_ms: u64,
) -> bool {
    let HookCandidate::SubagentOpen {
        active,
        provisional,
        root: HookRootCandidate::ToolOpen(_),
    } = &record.candidate
    else {
        return matches!(record.candidate, HookCandidate::ToolOpen(_));
    };
    if !record.subagent_set_complete
        || !active.is_empty()
        || provisional.is_empty()
        || !active.is_disjoint(provisional)
        || !hook_id_timestamps_are_exact(record, provisional, &record.subagent_opened_at_ms)
        || !hook_id_timestamps_are_exact(record, provisional, &record.subagent_stopped_at_ms)
        || provisional.iter().any(|id| {
            record
                .subagent_opened_at_ms
                .get(id)
                .zip(record.subagent_stopped_at_ms.get(id))
                .is_none_or(|(opened, stopped)| stopped < opened)
        })
        || !state.descendants_are_exact_terminal(now_ms)
    {
        return false;
    }
    let terminal_ids = state
        .descendants
        .iter()
        .map(|child| child.session_id.clone())
        .collect::<HashSet<_>>();
    terminal_ids == *provisional
        && state.descendants.iter().all(|child| {
            record
                .subagent_stopped_at_ms
                .get(&child.session_id)
                .is_some_and(|stopped| child.task_completed_at_ms >= *stopped)
        })
}

fn project_hook_root_status(
    candidate: &HookRootCandidate,
    record: &HookCollectorRecord,
    rollout: Option<&RolloutLifecycle>,
    now_ms: u64,
) -> (SessionStatus, StatusAuthority, StatusReason) {
    let unavailable = |reason| (SessionStatus::Unknown, StatusAuthority::Unavailable, reason);
    match candidate {
        HookRootCandidate::Unknown(reason) => unavailable(*reason),
        HookRootCandidate::Ended => unavailable(StatusReason::HookEventGap),
        HookRootCandidate::TurnOpen => {
            if rollout.is_some_and(|state| {
                hook_edge_timestamp_is_valid(record, record.prompt_observed_at_ms)
                    && state.root_is_exact_active(now_ms)
                    && state.active_turn_id == record.turn_id
                    && record.turn_id.is_some()
                    && state.open_tool_ids.is_empty()
                    && state.live_code_mode_cells == 0
                    && !state.code_mode_correlation_ambiguous
                    && state.descendants_are_exact_terminal(now_ms)
                    && !state.relevant_process_descendant
            }) {
                (
                    SessionStatus::Thinking,
                    StatusAuthority::Heuristic,
                    StatusReason::HookTurnOpen,
                )
            } else {
                unavailable(StatusReason::HookEventGap)
            }
        }
        HookRootCandidate::ToolOpen(hook_ids) => {
            let _edge_shape_valid =
                hook_id_timestamps_are_exact(record, hook_ids, &record.tool_opened_at_ms);
            let _ = (rollout, now_ms);
            // Codex does not attest the process-effective PermissionRequest
            // coverage. The same open PreToolUse/rollout call can therefore
            // mean either execution or a selectively unobserved approval.
            unavailable(StatusReason::HookInteractionResolutionUnavailable)
        }
        HookRootCandidate::TurnStopped => {
            if rollout.is_some_and(|state| {
                hook_edge_timestamp_is_valid(record, record.stop_observed_at_ms)
                    && state.lifecycle_valid
                    && state.task_complete
                    && state.completed_turn_id == record.turn_id
                    && record.turn_id.is_some()
                    && state.turn_started_at_ms > 0
                    && state.turn_started_at_ms <= state.latest_lifecycle_at_ms
                    && state.latest_lifecycle_at_ms <= state.task_completed_at_ms
                    && state.task_completed_at_ms <= now_ms
                    && state.task_completed_at_ms >= record.stop_observed_at_ms
                    && !state.turn_active
                    && state.active_turn_id.is_none()
                    && state.open_tool_ids.is_empty()
                    && state.open_tool_started_at_ms.is_empty()
                    && state.open_tool_classes.is_empty()
                    && state.live_code_mode_cells == 0
                    && !state.code_mode_correlation_ambiguous
                    && state.descendants_are_exact_terminal(now_ms)
                    && !state.relevant_process_descendant
            }) {
                (
                    SessionStatus::Idle,
                    StatusAuthority::Heuristic,
                    StatusReason::HookTurnComplete,
                )
            } else {
                unavailable(StatusReason::HookEventGap)
            }
        }
    }
}

fn project_hook_status(
    record: &HookCollectorRecord,
    rollout: Option<&RolloutLifecycle>,
    now_ms: u64,
) -> (SessionStatus, StatusAuthority, StatusReason) {
    let unavailable = |reason| (SessionStatus::Unknown, StatusAuthority::Unavailable, reason);
    if record.session_id.is_empty()
        || record.observed_at_ms == 0
        || record.observed_at_ms > now_ms
        || record.started_at_ms == 0
        || record.started_at_ms > record.observed_at_ms
        || record.status_since_ms < record.started_at_ms
        || record.status_since_ms > record.observed_at_ms
        || record.observations.iter().any(|sample| {
            sample.observed_at_ms == 0 || sample.observed_at_ms > record.observed_at_ms
        })
        || record
            .observations
            .windows(2)
            .any(|samples| samples[0].observed_at_ms > samples[1].observed_at_ms)
    {
        return unavailable(StatusReason::HookStateMalformed);
    }
    if record.process_state == HookProcessState::Live
        && (record.pid == 0
            || record
                .process_incarnation
                .as_deref()
                .is_none_or(str::is_empty)
            || !record.native_process_verified)
    {
        return unavailable(StatusReason::OwnershipUnconfirmed);
    }
    if record.local_config_ambiguous {
        return unavailable(StatusReason::HookConfigChanged);
    }
    if matches!(record.candidate, HookCandidate::Ended) {
        return if record.pid != 0
            && record
                .process_incarnation
                .as_deref()
                .is_some_and(|incarnation| !incarnation.is_empty())
            && record.process_state == HookProcessState::Gone
            && record.exit_supported_rollout_correlated
            && record.exit_observed_at_ms >= record.started_at_ms
            && timestamp_is_recent_past(now_ms, record.exit_observed_at_ms, 30_000)
        {
            (
                SessionStatus::Done,
                StatusAuthority::Heuristic,
                StatusReason::ProcessExited,
            )
        } else {
            unavailable(StatusReason::OwnershipUnconfirmed)
        };
    }
    if !matches!(record.candidate, HookCandidate::Unknown(_))
        && (!record.supported_release_attested
            || rollout.is_none_or(|state| !state.has_compatible_release_tree()))
    {
        return unavailable(StatusReason::HookIntegrationUnverified);
    }
    if record.interaction_ambiguous {
        return unavailable(StatusReason::HookInteractionResolutionUnavailable);
    }

    match &record.candidate {
        HookCandidate::Unknown(reason) => unavailable(*reason),
        HookCandidate::Ended => unreachable!("Ended is handled before live precedence"),
        _ if record.process_state != HookProcessState::Live => {
            unavailable(StatusReason::OwnershipUnconfirmed)
        }
        HookCandidate::TurnOpen => {
            project_hook_root_status(&HookRootCandidate::TurnOpen, record, rollout, now_ms)
        }
        HookCandidate::ToolOpen(hook_ids) => project_hook_root_status(
            &HookRootCandidate::ToolOpen(hook_ids.clone()),
            record,
            rollout,
            now_ms,
        ),
        HookCandidate::SubagentOpen {
            active,
            provisional,
            root,
        } => {
            match root {
                HookRootCandidate::Unknown(reason) => return unavailable(*reason),
                HookRootCandidate::Ended => return unavailable(StatusReason::HookEventGap),
                HookRootCandidate::TurnOpen
                | HookRootCandidate::ToolOpen(_)
                | HookRootCandidate::TurnStopped => {}
            }
            if matches!(root, HookRootCandidate::ToolOpen(_))
                || rollout.is_some_and(|state| {
                    !state.open_tool_ids.is_empty()
                        || !state.open_tool_started_at_ms.is_empty()
                        || !state.open_tool_classes.is_empty()
                        || state.live_code_mode_cells > 0
                        || state.code_mode_correlation_ambiguous
                })
            {
                // Root interaction ambiguity has higher precedence than
                // exact background child work. A rollout call can also be
                // present when the corresponding PreToolUse edge was missed.
                return unavailable(StatusReason::HookInteractionResolutionUnavailable);
            }
            if !record.subagent_set_complete
                || (active.is_empty() && provisional.is_empty())
                || !active.is_disjoint(provisional)
            {
                return unavailable(StatusReason::HookEventGap);
            }
            let tracked_ids = active
                .iter()
                .chain(provisional.iter())
                .cloned()
                .collect::<HashSet<_>>();
            if !hook_id_timestamps_are_exact(record, &tracked_ids, &record.subagent_opened_at_ms)
                || !hook_id_timestamps_are_exact(
                    record,
                    provisional,
                    &record.subagent_stopped_at_ms,
                )
                || provisional.iter().any(|id| {
                    record
                        .subagent_opened_at_ms
                        .get(id)
                        .zip(record.subagent_stopped_at_ms.get(id))
                        .is_none_or(|(opened, stopped)| stopped < opened)
                })
            {
                return unavailable(StatusReason::HookEventGap);
            }
            if rollout.is_some_and(|state| {
                state.descendants.iter().any(|child| {
                    child.direct_child
                        && tracked_ids.contains(&child.session_id)
                        && child.is_exact_active_with_open_tool(now_ms)
                })
            }) {
                return unavailable(StatusReason::HookInteractionResolutionUnavailable);
            }
            let Some((rollout_active, rollout_terminal)) =
                rollout.and_then(|state| state.exact_direct_child_sets(now_ms))
            else {
                return unavailable(StatusReason::HookEventGap);
            };
            if active.iter().any(|id| !rollout_active.contains(id))
                || provisional
                    .iter()
                    .any(|id| !rollout_active.contains(id) && !rollout_terminal.contains(id))
            {
                return unavailable(StatusReason::HookEventGap);
            }
            let state = rollout.expect("exact child sets require rollout state");
            if active.iter().any(|id| {
                state
                    .direct_child(id)
                    .zip(record.subagent_opened_at_ms.get(id))
                    .is_none_or(|(child, opened)| child.latest_lifecycle_at_ms < *opened)
            }) || provisional.iter().any(|id| {
                state
                    .direct_child(id)
                    .zip(record.subagent_stopped_at_ms.get(id))
                    .is_none_or(|(child, stopped)| {
                        if rollout_active.contains(id) {
                            child.latest_lifecycle_at_ms < *stopped
                        } else {
                            child.task_completed_at_ms < *stopped
                        }
                    })
            }) {
                return unavailable(StatusReason::HookEventGap);
            }
            let expected_active = active
                .iter()
                .chain(provisional.iter().filter(|id| rollout_active.contains(*id)))
                .cloned()
                .collect::<HashSet<_>>();
            let expected_terminal = provisional
                .iter()
                .filter(|id| rollout_terminal.contains(*id))
                .cloned()
                .collect::<HashSet<_>>();
            if rollout_active != expected_active || rollout_terminal != expected_terminal {
                return unavailable(StatusReason::HookEventGap);
            }
            if !rollout_active.is_empty() {
                (
                    SessionStatus::Executing,
                    StatusAuthority::Heuristic,
                    StatusReason::HookSubagentActive,
                )
            } else {
                project_hook_root_status(root, record, rollout, now_ms)
            }
        }
        HookCandidate::TurnStopped => {
            project_hook_root_status(&HookRootCandidate::TurnStopped, record, rollout, now_ms)
        }
    }
}

fn hook_record_has_exact_public_done(record: &HookCollectorRecord, now_ms: u64) -> bool {
    matches!(
        project_hook_status(record, None, now_ms),
        (
            SessionStatus::Done,
            StatusAuthority::Heuristic,
            StatusReason::ProcessExited
        )
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HerdrWorkingProjection {
    status: SessionStatus,
    status_since_ms: u64,
    consecutive_matching: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HerdrWorkingContinuityKey {
    session_id: String,
    pid: u32,
    executing: bool,
    status_since_ms: u64,
}

fn project_herdr_working_status(
    record: &HookCollectorRecord,
    rollout: Option<&RolloutLifecycle>,
    now_ms: u64,
) -> Option<HerdrWorkingProjection> {
    let (status, authority, reason) = project_hook_status(record, rollout, now_ms);
    if authority == StatusAuthority::Heuristic
        && matches!(status, SessionStatus::Thinking | SessionStatus::Executing)
    {
        return Some(HerdrWorkingProjection {
            status,
            status_since_ms: record.status_since_ms,
            consecutive_matching: 0,
        });
    }
    if status != SessionStatus::Unknown
        || authority != StatusAuthority::Unavailable
        || reason != StatusReason::HookInteractionResolutionUnavailable
    {
        return None;
    }
    let state = rollout?;
    let reconciled = reconciled_herdr_hook_record(record, state, now_ms)?;
    let (status, authority, reason) = project_hook_status(&reconciled, Some(state), now_ms);
    if authority == StatusAuthority::Heuristic
        && matches!(status, SessionStatus::Thinking | SessionStatus::Executing)
    {
        return Some(HerdrWorkingProjection {
            status,
            status_since_ms: reconciled.status_since_ms,
            consecutive_matching: 0,
        });
    }
    if status != SessionStatus::Unknown
        || authority != StatusAuthority::Unavailable
        || reason != StatusReason::HookInteractionResolutionUnavailable
    {
        return None;
    }
    let hook_ids = match &reconciled.candidate {
        HookCandidate::ToolOpen(ids) => ids,
        HookCandidate::SubagentOpen {
            root: HookRootCandidate::ToolOpen(ids),
            ..
        } => ids,
        _ => return None,
    };
    let direct_tool_shape = !hook_ids.is_empty()
        && state.open_tool_ids == *hook_ids
        && state.live_code_mode_cells == 0
        && !state.code_mode_correlation_ambiguous
        && hook_open_tools_are_exact_ordinary(hook_ids, &reconciled.tool_classes)
        && rollout_open_tools_are_exact_ordinary(&state.open_tool_ids, &state.open_tool_classes);
    let code_mode_tool_shape = code_mode_open_tool_shape_is_exact(
        hook_ids,
        &reconciled.tool_classes,
        &reconciled.tool_opened_at_ms,
        state,
    );
    if reconciled.process_state != HookProcessState::Live
        || !reconciled.native_process_verified
        || reconciled.local_config_ambiguous
        || reconciled.interaction_ambiguous
        || !reconciled.supported_release_attested
        || !state.has_compatible_release_tree()
        || !state.root_is_exact_active(now_ms)
        || state.active_turn_id != reconciled.turn_id
        || reconciled.turn_id.is_none()
        || !subagent_root_tool_refinement_is_exact(&reconciled, state, now_ms)
        // Direct tools retain exact shared IDs. In audited Code Mode
        // releases, Codex intentionally wraps one nested `exec-<UUIDv4>` hook
        // inside one outer rollout `exec` / linked `wait` call with a separate
        // `call_*` ID. No generic cardinality or timestamp guess is accepted.
        || (!direct_tool_shape && !code_mode_tool_shape)
        || !hook_id_timestamps_are_exact(
            &reconciled,
            hook_ids,
            &reconciled.tool_opened_at_ms,
        )
        || !state.descendants_are_exact_terminal(now_ms)
    {
        return None;
    }
    let rollout_since_ms = state
        .open_tool_started_at_ms
        .values()
        .copied()
        .min()
        .unwrap_or(0);
    Some(HerdrWorkingProjection {
        status: SessionStatus::Executing,
        // The later source-local edge starts the currently corroborated
        // execution interval. This also resets continuity when a long-running
        // `exec` is followed by a distinct provider `wait` call.
        status_since_ms: reconciled.status_since_ms.max(rollout_since_ms),
        consecutive_matching: 0,
    })
}

fn apply_herdr_observation(
    session: &mut AgentSession,
    observation: HerdrObservation,
    working_projection: Option<HerdrWorkingProjection>,
    rollout_preview: Option<&String>,
) {
    if session.status == SessionStatus::Done
        || session.status_evidence.authority == StatusAuthority::Provider
    {
        return;
    }
    let (status, authority, reason, status_since_ms, consecutive_matching) =
        match observation.status {
            HerdrStatus::Blocked => (
                SessionStatus::Waiting,
                StatusAuthority::Heuristic,
                StatusReason::HerdrScreenBlocked,
                observation.status_since_ms,
                observation.consecutive_matching,
            ),
            HerdrStatus::Idle => (
                SessionStatus::Idle,
                StatusAuthority::Heuristic,
                StatusReason::HerdrScreenIdle,
                observation.status_since_ms,
                observation.consecutive_matching,
            ),
            HerdrStatus::Working => match working_projection {
                Some(projection) => (
                    projection.status,
                    StatusAuthority::Heuristic,
                    StatusReason::HerdrScreenWorking,
                    projection.status_since_ms,
                    projection.consecutive_matching.max(1),
                ),
                None => (
                    SessionStatus::Working,
                    StatusAuthority::Heuristic,
                    StatusReason::HerdrWorkingUnrefined,
                    observation.status_since_ms,
                    observation.consecutive_matching,
                ),
            },
        };
    session.status_evidence.observe(StatusObservation::new(
        status,
        authority,
        reason,
        observation.observed_at_ms,
        0,
    ));
    session.status_evidence.status_since_ms = status_since_ms;
    session.status_evidence.consecutive_matching = consecutive_matching;
    session.status = status;
    session.action_process_incarnation = None;
    session.current_tasks = vec![match status {
        SessionStatus::Executing => rollout_preview
            .cloned()
            .unwrap_or_else(|| "executing".to_string()),
        SessionStatus::Working => "working".to_string(),
        SessionStatus::Thinking => "thinking".to_string(),
        SessionStatus::Waiting => "waiting for user input".to_string(),
        SessionStatus::Idle => "idle".to_string(),
        SessionStatus::Unknown => "status evidence unavailable".to_string(),
        _ => unreachable!("Herdr projects only live Codex states"),
    }];
    session.pending_since_ms = if status == SessionStatus::Executing {
        status_since_ms
    } else {
        0
    };
    session.thinking_since_ms = if status == SessionStatus::Thinking {
        status_since_ms
    } else {
        0
    };
    session.enforce_status_contract();
}

fn hook_task_label(status: SessionStatus, rollout_preview: Option<String>) -> String {
    match status {
        SessionStatus::Thinking => "thinking".to_string(),
        SessionStatus::Executing => rollout_preview.unwrap_or_else(|| "executing".to_string()),
        SessionStatus::Working => "working".to_string(),
        SessionStatus::Idle => "idle".to_string(),
        SessionStatus::Done => "finished".to_string(),
        SessionStatus::Unknown => "status evidence unavailable".to_string(),
        // Supported Codex hook evidence never emits these live states.
        SessionStatus::Waiting => "status evidence unavailable".to_string(),
        SessionStatus::RateLimited => "status evidence unavailable".to_string(),
        SessionStatus::Error => "status evidence unavailable".to_string(),
    }
}

impl CodexCollector {
    pub fn new() -> Self {
        let default_home = dirs::home_dir().unwrap_or_default().join(".codex");
        let codex_home = std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .unwrap_or(default_home);
        Self {
            sessions_dir: codex_home.join("sessions"),
            last_rate_limit: None,
            desktop_recent_scanner: DesktopRecentRolloutScanner::new(),
            parse_cache: RefCell::new(CodexParseCache::default()),
            rollout_lifecycle: RefCell::new(HashMap::new()),
            hook_process_states: RefCell::new(HashMap::new()),
            hook_exit_observations: RefCell::new(HashMap::new()),
            hook_process_rollout_bindings: RefCell::new(HashMap::new()),
            hook_live_session_snapshots: RefCell::new(HashMap::new()),
            hook_done_tombstones: RefCell::new(HashMap::new()),
            herdr_status_resolver: RefCell::new(HerdrStatusResolver::default()),
            herdr_working_continuity: RefCell::new(HashMap::new()),
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let now_ms = unix_now_ms();
        self.rollout_lifecycle.borrow_mut().clear();

        // Reset live rate limit each pass — only keep it if a current session provides one.
        self.last_rate_limit = None;
        if !self.sessions_dir.exists() {
            return self.finalize_hook_sessions(Vec::new(), shared, now_ms);
        }

        // Step 1: Find running codex processes from shared ps data (no extra ps call).
        // When MCP suppression is on, exclude `codex mcp-server` PIDs — those
        // are surfaced through the MCP servers panel instead. See issue #95.
        let codex_pids =
            Self::find_codex_pids_from_shared(&shared.process_info, &shared.mcp_server_pids);
        let just_pids: Vec<u32> = codex_pids.iter().map(|(p, _)| *p).collect();
        let pid_to_jsonl = Self::map_pid_to_jsonl(&just_pids, &self.sessions_dir);
        let pid_is_exec: HashMap<u32, bool> = codex_pids.into_iter().collect();

        let mut sessions = Vec::new();
        let mut seen_jsonl = std::collections::HashSet::new();

        // Active sessions: running codex processes with open JSONL files
        for (pid, jsonl_paths) in &pid_to_jsonl {
            let is_exec = pid_is_exec.get(pid).copied().unwrap_or(false);
            let loaded = self.load_cli_session_group(
                CodexProcessContext {
                    pid: Some(*pid),
                    is_exec,
                    owns_process_tree: true,
                    unknown_process_owner: false,
                },
                jsonl_paths,
                &shared.process_info,
                &shared.children_map,
                &shared.ports,
                &shared.mcp_server_pids,
            );
            seen_jsonl.extend(loaded.owned_paths);
            if let Some(session) = loaded.session {
                if let Some(new_rl) = loaded.rate_limit {
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

        let desktop_pids = Self::find_codex_desktop_pids_from_shared(
            &shared.process_info,
            &shared.mcp_server_pids,
        );
        if !desktop_pids.is_empty() {
            let desktop_pid_to_rollouts: HashMap<u32, Vec<PathBuf>> = desktop_pids
                .iter()
                .filter_map(|pid| {
                    shared
                        .desktop_rollout_fd_map
                        .get(pid)
                        .map(|paths| (*pid, paths.clone()))
                })
                .collect();

            // Prefer the filesystem view so Desktop sessions appear immediately,
            // then use the async fd cache only to improve PID ownership.
            let desktop_pid_for_path = Self::desktop_pid_by_rollout_path(
                &desktop_pid_to_rollouts,
                super::mcp::ACTIVE_MTIME_SECS,
            );
            let mut desktop_rollout_paths = Self::foreground_desktop_rollouts(
                &self.sessions_dir,
                &seen_jsonl,
                &shared.mcp_owned_rollouts,
                super::mcp::ACTIVE_MTIME_SECS,
            );
            for path in self
                .desktop_recent_scanner
                .update(&self.sessions_dir, super::mcp::ACTIVE_MTIME_SECS)
            {
                if seen_jsonl.contains(&path) || shared.mcp_owned_rollouts.contains(&path) {
                    continue;
                }
                if !desktop_rollout_paths.contains(&path) {
                    desktop_rollout_paths.push(path);
                }
            }
            Self::sort_rollouts_by_mtime_desc(&mut desktop_rollout_paths);

            for path in desktop_rollout_paths {
                let pid = desktop_pid_for_path.get(&path).copied();
                let process_ctx = CodexProcessContext {
                    pid,
                    is_exec: false,
                    owns_process_tree: false,
                    unknown_process_owner: pid.is_none(),
                };
                if let Some((session, rl)) = self.load_session_with_rate_limit(
                    process_ctx,
                    &path,
                    &shared.process_info,
                    &shared.children_map,
                    &shared.ports,
                    &shared.mcp_server_pids,
                ) {
                    seen_jsonl.insert(path);
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

            // Retain fd-only discovery for files not visible in today's active
            // scan; this is a fallback, not the first-paint path.
            for (pid, path) in Self::active_desktop_rollouts(
                desktop_pid_to_rollouts,
                &seen_jsonl,
                &shared.mcp_owned_rollouts,
                super::mcp::ACTIVE_MTIME_SECS,
            ) {
                if let Some((session, rl)) = self.load_session_with_rate_limit(
                    CodexProcessContext {
                        pid: Some(pid),
                        is_exec: false,
                        owns_process_tree: false,
                        unknown_process_owner: false,
                    },
                    &path,
                    &shared.process_info,
                    &shared.children_map,
                    &shared.ports,
                    &shared.mcp_server_pids,
                ) {
                    seen_jsonl.insert(path);
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

        // Recently modified unowned rollouts can enrich an exact hook exit
        // tombstone with metrics. By themselves they never create Done rows.
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
                    // Skip rollouts still held open by an mcp-server PID:
                    // the thread isn't actually finished, the mcp-server is
                    // just holding the fd for resume. Without this skip, the
                    // sessions panel grows a PID=0 "Done" row for every
                    // historical thread on every active mcp-server.
                    if shared.mcp_owned_rollouts.contains(&path) {
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
                        CodexProcessContext {
                            pid: None,
                            is_exec: false,
                            owns_process_tree: false,
                            unknown_process_owner: false,
                        },
                        &path,
                        &shared.process_info,
                        &shared.children_map,
                        &shared.ports,
                        &shared.mcp_server_pids,
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

        self.finalize_hook_sessions(sessions, shared, now_ms)
    }

    /// Overlay content-free Codex hook state onto rollout-derived metrics.
    /// Direct CLI and Desktop rows remain Unknown when no validated hook state matches.
    fn finalize_hook_sessions(
        &self,
        sessions: Vec<AgentSession>,
        shared: &super::SharedProcessData,
        now_ms: u64,
    ) -> Vec<AgentSession> {
        match self.read_hook_records(now_ms) {
            Some(records) => {
                self.finalize_hook_records_with_scan(sessions, records, shared, now_ms, true)
            }
            None => {
                self.finalize_hook_records_with_scan(sessions, Vec::new(), shared, now_ms, false)
            }
        }
    }

    /// Open only an already-existing private hook-state tree through the
    /// store's non-mutating collector path.
    fn read_hook_records(&self, now_ms: u64) -> Option<Vec<HookCollectorRecord>> {
        let codex_home = self.sessions_dir.parent()?;
        let paths = PluginPaths::new(codex_home).ok()?;
        let attestation = plugin::read_installation_attestation(codex_home).ok()??;
        let current_exe = std::env::current_exe().ok()?;
        let runtime = plugin::runtime_hook_config(codex_home, &current_exe).ok()?;
        let expected = IntegrationIdentity {
            hook_schema_revision: attestation.hook_schema_revision,
            helper_digest: attestation.helper_digest,
            installation_id: attestation.installation_id,
            config_digest: runtime.config_digest,
            complete_hook_set: runtime.complete_hook_set,
        };
        let store = HookStateStore::open_existing(&paths.plugin_data_root, expected).ok()?;
        let scan = store.read_all(now_ms).ok()?;
        let mut records = scan
            .states
            .into_iter()
            .map(hook_record_from_state)
            .collect::<Vec<_>>();
        for record in &mut records {
            record.local_config_ambiguous =
                cwd_has_unattested_codex_config(&record.cwd, codex_home);
        }
        if scan.rejected > 0 {
            for record in &mut records {
                record.candidate = HookCandidate::Unknown(StatusReason::HookStateMalformed);
                record.actionable = false;
                record.owns_resources = false;
            }
        }
        Some(records)
    }

    fn observe_hook_process_transitions(
        &self,
        records: &mut [HookCollectorRecord],
        now_ms: u64,
        scan_available: bool,
    ) {
        let seen = records
            .iter()
            .filter_map(hook_done_key)
            .collect::<HashSet<_>>();
        if scan_available {
            self.hook_process_states
                .borrow_mut()
                .retain(|key, _| seen.contains(key));
            self.hook_exit_observations
                .borrow_mut()
                .retain(|key, _| seen.contains(key));
            self.hook_process_rollout_bindings
                .borrow_mut()
                .retain(|key, _| seen.contains(key));
        }

        for record in records {
            let Some(key) = hook_done_key(record) else {
                continue;
            };
            let exit_transition_eligible = hook_candidate_allows_exit_transition(&record.candidate)
                && !record.local_config_ambiguous;
            let observed_process_state = if record.process_state == HookProcessState::Live
                && !record.native_process_verified
            {
                HookProcessState::Unverified
            } else {
                record.process_state
            };
            let previous = self
                .hook_process_states
                .borrow_mut()
                .insert(key.clone(), observed_process_state);
            match observed_process_state {
                HookProcessState::Live => {
                    self.hook_exit_observations.borrow_mut().remove(&key);
                }
                HookProcessState::Gone
                    if previous == Some(HookProcessState::Live) && exit_transition_eligible =>
                {
                    self.hook_exit_observations
                        .borrow_mut()
                        .entry(key.clone())
                        .or_insert(now_ms);
                }
                HookProcessState::Gone | HookProcessState::Unverified => {}
            }
            if !exit_transition_eligible {
                self.hook_exit_observations.borrow_mut().remove(&key);
                self.hook_process_rollout_bindings.borrow_mut().remove(&key);
                self.hook_done_tombstones.borrow_mut().remove(&key);
                continue;
            }
            if let Some(exited_at_ms) = self.hook_exit_observations.borrow().get(&key).copied() {
                record.exit_observed_at_ms = exited_at_ms;
                record.observed_at_ms = exited_at_ms;
                record.status_since_ms = exited_at_ms;
                let binding = self
                    .hook_process_rollout_bindings
                    .borrow()
                    .get(&key)
                    .cloned();
                let binding_matches = binding
                    .as_ref()
                    .is_some_and(|binding| binding.session_id == record.session_id);
                record.exit_supported_rollout_correlated = binding_matches
                    && binding
                        .as_ref()
                        .is_some_and(|binding| binding.supported_release);
                record.candidate = match binding {
                    Some(binding)
                        if binding.session_id == record.session_id && binding.supported_release =>
                    {
                        HookCandidate::Ended
                    }
                    Some(binding) if binding.session_id == record.session_id => {
                        HookCandidate::Unknown(StatusReason::HookIntegrationUnverified)
                    }
                    _ => HookCandidate::Unknown(StatusReason::OwnershipUnconfirmed),
                };
            }
        }
    }

    fn trim_live_hook_snapshots(&self) {
        let mut snapshots = self.hook_live_session_snapshots.borrow_mut();
        if snapshots.len() <= MAX_CODEX_DONE_TOMBSTONES {
            return;
        }
        let mut oldest = snapshots
            .iter()
            .map(|(key, snapshot)| (snapshot.started_at, key.clone()))
            .collect::<Vec<_>>();
        oldest.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
        for (_, key) in oldest
            .into_iter()
            .take(snapshots.len() - MAX_CODEX_DONE_TOMBSTONES)
        {
            snapshots.remove(&key);
        }
    }

    fn prune_hook_done_tombstones(&self, now_ms: u64) {
        let mut tombstones = self.hook_done_tombstones.borrow_mut();
        tombstones.retain(|key, tombstone| {
            key.pid != 0
                && !key.process_incarnation.is_empty()
                && timestamp_is_recent_past(now_ms, tombstone.exit_observed_at_ms, 30_000)
        });
        if tombstones.len() <= MAX_CODEX_DONE_TOMBSTONES {
            return;
        }
        let mut oldest = tombstones
            .iter()
            .map(|(key, tombstone)| (tombstone.exit_observed_at_ms, key.clone()))
            .collect::<Vec<_>>();
        oldest.sort();
        for (_, key) in oldest
            .into_iter()
            .take(tombstones.len() - MAX_CODEX_DONE_TOMBSTONES)
        {
            tombstones.remove(&key);
        }
    }

    fn remember_hook_done_tombstone(
        &self,
        record: &HookCollectorRecord,
        current_session: &AgentSession,
        now_ms: u64,
    ) {
        if !matches!(record.candidate, HookCandidate::Ended)
            || record.process_state != HookProcessState::Gone
            || !record.exit_supported_rollout_correlated
            || !timestamp_is_recent_past(now_ms, record.exit_observed_at_ms, 30_000)
        {
            return;
        }
        let Some(key) = hook_done_key(record) else {
            return;
        };
        let mut snapshot = self
            .hook_live_session_snapshots
            .borrow()
            .get(&key)
            .cloned()
            .unwrap_or_else(|| HookSessionSnapshot::capture(current_session));
        snapshot.cwd = record.cwd.clone();
        snapshot.project_name = process::last_path_segment(&record.cwd)
            .unwrap_or("?")
            .to_string();
        snapshot.started_at = record.started_at_ms;
        self.hook_done_tombstones.borrow_mut().insert(
            key,
            HookDoneTombstone {
                exit_observed_at_ms: record.exit_observed_at_ms,
                snapshot,
            },
        );
        self.prune_hook_done_tombstones(now_ms);
    }

    #[cfg(test)]
    fn finalize_hook_records(
        &self,
        sessions: Vec<AgentSession>,
        records: Vec<HookCollectorRecord>,
        shared: &super::SharedProcessData,
        now_ms: u64,
    ) -> Vec<AgentSession> {
        self.finalize_hook_records_with_scan(sessions, records, shared, now_ms, true)
    }

    fn finalize_hook_records_with_scan(
        &self,
        sessions: Vec<AgentSession>,
        records: Vec<HookCollectorRecord>,
        shared: &super::SharedProcessData,
        now_ms: u64,
        hook_scan_available: bool,
    ) -> Vec<AgentSession> {
        self.finalize_hook_records_with_scan_and_herdr(
            sessions,
            records,
            shared,
            now_ms,
            hook_scan_available,
            None,
        )
    }

    fn reconcile_herdr_working_continuity(
        &self,
        projections: &mut HashMap<(String, u32), HerdrWorkingProjection>,
        observations: &HashMap<(String, u32), HerdrObservation>,
    ) {
        let mut continuity = self.herdr_working_continuity.borrow_mut();
        let mut current = HashSet::new();
        for (target, projection) in projections {
            let Some(observation) = observations
                .get(target)
                .filter(|observation| observation.status == HerdrStatus::Working)
            else {
                continue;
            };
            let phase_since_ms = projection.status_since_ms;
            let status_since_ms = observation.status_since_ms.max(phase_since_ms);
            let key = HerdrWorkingContinuityKey {
                session_id: target.0.clone(),
                pid: target.1,
                executing: projection.status == SessionStatus::Executing,
                status_since_ms,
            };
            let initial_count = if phase_since_ms <= observation.status_since_ms {
                observation.consecutive_matching.max(1)
            } else {
                1
            };
            let consecutive_matching = continuity
                .entry(key.clone())
                .and_modify(|count| *count = count.saturating_add(1).max(initial_count))
                .or_insert(initial_count);
            projection.status_since_ms = status_since_ms;
            projection.consecutive_matching = *consecutive_matching;
            current.insert(key);
        }
        continuity.retain(|key, _| current.contains(key));
    }

    #[cfg(test)]
    fn finalize_hook_records_with_herdr(
        &self,
        sessions: Vec<AgentSession>,
        records: Vec<HookCollectorRecord>,
        shared: &super::SharedProcessData,
        now_ms: u64,
        herdr_observations: HashMap<(String, u32), HerdrObservation>,
    ) -> Vec<AgentSession> {
        self.finalize_hook_records_with_scan_and_herdr(
            sessions,
            records,
            shared,
            now_ms,
            true,
            Some(herdr_observations),
        )
    }

    fn finalize_hook_records_with_scan_and_herdr(
        &self,
        mut sessions: Vec<AgentSession>,
        mut records: Vec<HookCollectorRecord>,
        shared: &super::SharedProcessData,
        now_ms: u64,
        hook_scan_available: bool,
        herdr_override: Option<HashMap<(String, u32), HerdrObservation>>,
    ) -> Vec<AgentSession> {
        let eligible_pids =
            Self::find_codex_pids_from_shared(&shared.process_info, &shared.mcp_server_pids)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect::<HashSet<_>>();
        let herdr_observations = herdr_override.unwrap_or_else(|| {
            let mut targets = Vec::new();
            let mut seen = HashSet::<(String, u32, String)>::new();
            let mut push_target = |session_id: String, pid: u32, incarnation: String| {
                if seen.insert((session_id.clone(), pid, incarnation.clone())) {
                    targets.push(HerdrTarget {
                        session_id,
                        pid,
                        expected_incarnation: Some(incarnation),
                    });
                }
            };

            for record in &records {
                if !hook_record_is_active_generation(record)
                    || !record.native_process_verified
                    || !eligible_pids.contains(&record.pid)
                {
                    continue;
                }
                if let Some(incarnation) = record
                    .process_incarnation
                    .as_ref()
                    .filter(|incarnation| !incarnation.is_empty())
                {
                    push_target(record.session_id.clone(), record.pid, incarnation.clone());
                }
            }
            for session in &sessions {
                if session.pid == 0
                    || session.status == SessionStatus::Done
                    || !eligible_pids.contains(&session.pid)
                    || !shared.process_info.contains_key(&session.pid)
                {
                    continue;
                }
                let Some(incarnation) = process::get_process_incarnation(session.pid) else {
                    continue;
                };
                if native_codex_process_is_exact(session.pid, &incarnation) {
                    push_target(session.session_id.clone(), session.pid, incarnation);
                }
            }
            self.herdr_status_resolver
                .borrow_mut()
                .resolve(&targets, now_ms)
        });
        {
            let rollout_lifecycle = self.rollout_lifecycle.borrow();
            reconcile_current_hook_generations(
                &mut records,
                &mut sessions,
                &rollout_lifecycle,
                &herdr_observations,
                &shared.process_info,
                &eligible_pids,
            );
        }
        self.observe_hook_process_transitions(&mut records, now_ms, hook_scan_available);
        self.prune_hook_done_tombstones(now_ms);

        // Persisted hook state can outlive the exact Codex process for hours.
        // A collector that first observes that incarnation as already gone
        // must neither fabricate a Done transition nor keep a PID=0 Unknown
        // placeholder in the live Sessions list. Observe transitions first so
        // an exact Live->Gone edge can still become the bounded Done row, then
        // suppress only generations that have no public live or exit state.
        let publicly_retained_record_ids = records
            .iter()
            .filter(|record| {
                record.process_state != HookProcessState::Gone
                    || hook_record_has_exact_public_done(record, now_ms)
            })
            .map(|record| record.session_id.clone())
            .collect::<HashSet<_>>();
        let suppressed_gone_session_ids = records
            .iter()
            .filter(|record| {
                record.process_state == HookProcessState::Gone
                    && !hook_record_has_exact_public_done(record, now_ms)
                    && !publicly_retained_record_ids.contains(&record.session_id)
            })
            .map(|record| record.session_id.clone())
            .collect::<HashSet<_>>();

        let rollout_only_done_ids = sessions
            .iter()
            .filter(|session| session.status == SessionStatus::Done && session.pid == 0)
            .map(|session| session.session_id.clone())
            .collect::<HashSet<_>>();
        let mut remaining = sessions.into_iter().map(Some).collect::<Vec<_>>();
        let rollout_previews = remaining
            .iter()
            .flatten()
            .filter_map(|session| {
                safe_rollout_task_preview(session)
                    .map(|preview| (session.session_id.clone(), preview))
            })
            .collect::<HashMap<_, _>>();
        let mut herdr_working_status = HashMap::<(String, u32), HerdrWorkingProjection>::new();
        for session in remaining.iter_mut().flatten() {
            mark_codex_status_unavailable(session, now_ms, StatusReason::HookIntegrationUnverified);
        }

        records.retain(|record| {
            !record.session_id.is_empty()
                && (record.process_state != HookProcessState::Gone
                    || hook_record_has_exact_public_done(record, now_ms))
                && if matches!(record.candidate, HookCandidate::Ended) {
                    match record.process_state {
                        HookProcessState::Gone => {
                            timestamp_is_recent_past(now_ms, record.exit_observed_at_ms, 30_000)
                        }
                        HookProcessState::Live | HookProcessState::Unverified => {
                            timestamp_is_recent_past(now_ms, record.ended_at_ms, 30_000)
                        }
                    }
                } else {
                    record.ended_at_ms == 0
                }
        });
        let active_session_ids = records
            .iter()
            .filter(|record| hook_record_is_active_generation(record))
            .map(|record| record.session_id.clone())
            .collect::<HashSet<_>>();
        let active_process_keys = records
            .iter()
            .filter(|record| hook_record_is_active_generation(record))
            .filter_map(hook_record_process_key)
            .collect::<HashSet<_>>();
        records.retain(|record| {
            !matches!(record.candidate, HookCandidate::Ended)
                || (!active_session_ids.contains(&record.session_id)
                    && hook_record_process_key(record)
                        .is_none_or(|key| !active_process_keys.contains(&key)))
        });

        // One exact process incarnation can accumulate several terminal hook
        // generations (resume/clear/session replacement). Keep only the
        // newest Gone generation, and likewise one newest Gone generation per
        // session ID. Generation recency is creation time; exit observation
        // time is shared and can be overwritten by transition detection.
        let mut gone_session_max = HashMap::<String, (u64, usize)>::new();
        let mut gone_process_max = HashMap::<(u32, String), (u64, usize)>::new();
        for record in records
            .iter()
            .filter(|record| record.process_state == HookProcessState::Gone)
        {
            let entry = gone_session_max
                .entry(record.session_id.clone())
                .or_insert((record.started_at_ms, 0));
            if record.started_at_ms > entry.0 {
                *entry = (record.started_at_ms, 1);
            } else if record.started_at_ms == entry.0 {
                entry.1 += 1;
            }
            if let Some(key) = hook_record_process_key(record) {
                let entry = gone_process_max
                    .entry(key)
                    .or_insert((record.started_at_ms, 0));
                if record.started_at_ms > entry.0 {
                    *entry = (record.started_at_ms, 1);
                } else if record.started_at_ms == entry.0 {
                    entry.1 += 1;
                }
            }
        }
        records.sort_by(|left, right| {
            right
                .started_at_ms
                .cmp(&left.started_at_ms)
                .then_with(|| left.generation_id.cmp(&right.generation_id))
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        let mut seen_gone_sessions = HashSet::new();
        let mut seen_gone_processes = HashSet::new();
        records.retain_mut(|record| {
            if record.process_state != HookProcessState::Gone {
                return true;
            }
            let process_key = hook_record_process_key(record);
            if seen_gone_sessions.contains(&record.session_id)
                || process_key
                    .as_ref()
                    .is_some_and(|key| seen_gone_processes.contains(key))
            {
                return false;
            }
            let tied_session =
                gone_session_max
                    .get(&record.session_id)
                    .is_some_and(|(started_at_ms, count)| {
                        *started_at_ms == record.started_at_ms && *count > 1
                    });
            let tied_process = process_key.as_ref().is_some_and(|key| {
                gone_process_max
                    .get(key)
                    .is_some_and(|(started_at_ms, count)| {
                        *started_at_ms == record.started_at_ms && *count > 1
                    })
            });
            if tied_session || tied_process {
                record.candidate = HookCandidate::Unknown(StatusReason::OwnershipUnconfirmed);
                record.actionable = false;
                record.owns_resources = false;
            }
            seen_gone_sessions.insert(record.session_id.clone());
            if let Some(key) = process_key {
                seen_gone_processes.insert(key);
            }
            true
        });
        records.sort_by(|left, right| {
            let rank = |record: &HookCollectorRecord| {
                if hook_record_is_active_generation(record) {
                    0
                } else if !matches!(record.candidate, HookCandidate::Ended) {
                    1
                } else {
                    2
                }
            };
            rank(left)
                .cmp(&rank(right))
                .then_with(|| right.observed_at_ms.cmp(&left.observed_at_ms))
                .then_with(|| right.started_at_ms.cmp(&left.started_at_ms))
                .then_with(|| left.generation_id.cmp(&right.generation_id))
                .then_with(|| left.session_id.cmp(&right.session_id))
        });

        let mut session_counts = HashMap::<String, usize>::new();
        let mut pid_counts = HashMap::<u32, usize>::new();
        for record in &records {
            if !hook_record_is_active_generation(record) {
                continue;
            }
            *session_counts.entry(record.session_id.clone()).or_default() += 1;
            if record.pid != 0 {
                *pid_counts.entry(record.pid).or_default() += 1;
            }
        }

        let mut emitted_sessions = HashSet::new();
        let mut retained_live_snapshot_keys = HashSet::new();
        let mut result = Vec::new();
        for mut record in records {
            if !emitted_sessions.insert(record.session_id.clone()) {
                continue;
            }

            let matching_rollouts = remaining
                .iter()
                .enumerate()
                .filter_map(|(index, slot)| {
                    slot.as_ref()
                        .is_some_and(|session| session.session_id == record.session_id)
                        .then_some(index)
                })
                .collect::<Vec<_>>();
            let selected = matching_rollouts.first().copied();
            let selected_is_live = matching_rollouts.iter().any(|index| {
                remaining[*index].as_ref().is_some_and(|session| {
                    session.pid != 0 && shared.process_info.contains_key(&session.pid)
                })
            });
            let rollout_binding_conflict = record.process_state == HookProcessState::Live
                && matching_rollouts.iter().any(|index| {
                    remaining[*index].as_ref().is_some_and(|session| {
                        (session.pid != 0 && session.pid != record.pid) || session.cwd != record.cwd
                    })
                });
            let rollout_pid_session_conflict = record.process_state == HookProcessState::Live
                && remaining.iter().flatten().any(|session| {
                    session.pid == record.pid && session.session_id != record.session_id
                });

            // A recently ended generation must not relabel a resumed live
            // rollout with the same thread ID as Done.
            if matches!(record.candidate, HookCandidate::Ended) && selected_is_live {
                continue;
            }

            let active_generation = hook_record_is_active_generation(&record);
            let ownership_conflict = rollout_binding_conflict
                || rollout_pid_session_conflict
                || (active_generation
                    && (session_counts
                        .get(&record.session_id)
                        .is_some_and(|count| *count > 1)
                        || (record.pid != 0
                            && pid_counts.get(&record.pid).is_some_and(|count| *count > 1))));
            let process_visible = record.pid != 0
                && record.process_state == HookProcessState::Live
                && record.native_process_verified
                && eligible_pids.contains(&record.pid);
            if ownership_conflict
                || (record.process_state == HookProcessState::Live && !process_visible)
            {
                record.candidate = HookCandidate::Unknown(StatusReason::OwnershipUnconfirmed);
                record.actionable = false;
                record.owns_resources = false;
            }

            let rollout = self
                .rollout_lifecycle
                .borrow()
                .get(&record.session_id)
                .cloned();
            let pidless_rollout_recovered = matching_rollouts.len() == 1
                && selected.is_some_and(|index| {
                    remaining[index].as_ref().is_some_and(|session| {
                        session.pid == 0
                            && session.cwd == record.cwd
                            && process_visible
                            && !rollout_pid_session_conflict
                    })
                });
            let rollout_binding_exact = matching_rollouts.len() == 1
                && selected.is_some_and(|index| {
                    remaining[index].as_ref().is_some_and(|session| {
                        (session.pid == record.pid || pidless_rollout_recovered)
                            && session.cwd == record.cwd
                            && shared.process_info.contains_key(&record.pid)
                            && rollout.as_ref().is_some_and(|lifecycle| {
                                session.version == lifecycle.root_cli_version
                            })
                    })
                })
                && rollout
                    .as_ref()
                    .is_some_and(|lifecycle| lifecycle.lifecycle_valid);
            let supported_release = rollout_binding_exact
                && rollout
                    .as_ref()
                    .is_some_and(RolloutLifecycle::has_compatible_release_tree);
            record.supported_release_attested = active_generation
                && process_visible
                && !ownership_conflict
                && !record.local_config_ambiguous
                && supported_release;
            if pidless_rollout_recovered {
                // The hook/process binding can recover display status for an
                // otherwise unowned rollout, but that recovery is not action
                // or resource-ownership proof.
                record.actionable = false;
                record.owns_resources = false;
            }
            if active_generation
                && (!record.supported_release_attested || !record.effective_hook_engine_attested)
            {
                record.actionable = false;
            }
            if active_generation {
                if let Some(done_key) = hook_done_key(&record) {
                    let mut bindings = self.hook_process_rollout_bindings.borrow_mut();
                    bindings.remove(&done_key);
                    if process_visible
                        && !ownership_conflict
                        && !pidless_rollout_recovered
                        && !record.local_config_ambiguous
                        && rollout_binding_exact
                    {
                        bindings.insert(
                            done_key,
                            HookProcessRolloutBinding {
                                session_id: record.session_id.clone(),
                                supported_release,
                            },
                        );
                    }
                }
            }
            if active_generation && process_visible && !ownership_conflict {
                let key = (record.session_id.clone(), record.pid);
                if let Some(status) =
                    project_herdr_working_status(&record, rollout.as_ref(), now_ms)
                {
                    herdr_working_status.insert(key, status);
                }
            }
            let rollout_preview = rollout_previews.get(&record.session_id).cloned();
            let mut session = selected
                .and_then(|index| remaining[index].take())
                .unwrap_or_else(|| self.hook_placeholder(&record));
            self.apply_hook_record(
                &mut session,
                &record,
                rollout.as_ref(),
                rollout_preview,
                shared,
                now_ms,
                process_visible && !ownership_conflict,
            );
            if active_generation {
                if let Some(key) = hook_done_key(&record) {
                    if record.supported_release_attested {
                        self.hook_live_session_snapshots
                            .borrow_mut()
                            .insert(key.clone(), HookSessionSnapshot::capture(&session));
                        retained_live_snapshot_keys.insert(key);
                        self.trim_live_hook_snapshots();
                    } else {
                        self.hook_live_session_snapshots.borrow_mut().remove(&key);
                    }
                }
            } else if session.status == SessionStatus::Done {
                self.remember_hook_done_tombstone(&record, &session, now_ms);
                if let Some(key) = hook_done_key(&record) {
                    if let Some(tombstone) = self.hook_done_tombstones.borrow().get(&key) {
                        session = tombstone
                            .snapshot
                            .done_session(&key, tombstone.exit_observed_at_ms);
                    }
                }
            }

            // Duplicate rollout rows for one exact hook session are metadata
            // aliases, not independent sessions.
            for slot in &mut remaining {
                if slot
                    .as_ref()
                    .is_some_and(|candidate| candidate.session_id == record.session_id)
                {
                    *slot = None;
                }
            }
            result.push(session);
        }

        if hook_scan_available {
            self.hook_live_session_snapshots
                .borrow_mut()
                .retain(|key, _| retained_live_snapshot_keys.contains(key));
        }

        let live_rollout_session_ids = remaining
            .iter()
            .flatten()
            .filter(|session| session.pid != 0 && shared.process_info.contains_key(&session.pid))
            .map(|session| session.session_id.clone())
            .collect::<HashSet<_>>();
        self.hook_done_tombstones.borrow_mut().retain(|key, _| {
            !active_session_ids.contains(&key.session_id)
                && !active_process_keys.contains(&(key.pid, key.process_incarnation.clone()))
                && !live_rollout_session_ids.contains(&key.session_id)
        });
        let mut retained_done = self
            .hook_done_tombstones
            .borrow()
            .iter()
            .map(|(key, tombstone)| {
                (
                    tombstone.exit_observed_at_ms,
                    key.clone(),
                    tombstone
                        .snapshot
                        .done_session(key, tombstone.exit_observed_at_ms),
                )
            })
            .collect::<Vec<_>>();
        retained_done
            .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        for (_, key, session) in retained_done {
            if emitted_sessions.contains(&key.session_id) {
                continue;
            }
            for slot in &mut remaining {
                if slot
                    .as_ref()
                    .is_some_and(|candidate| candidate.session_id == key.session_id)
                {
                    *slot = None;
                }
            }
            emitted_sessions.insert(key.session_id);
            result.push(session);
        }

        result.extend(
            remaining
                .into_iter()
                .flatten()
                // Rollout completion is never process-exit proof. A matching
                // exact hook tombstone above may still retain its metrics.
                .filter(|session| !rollout_only_done_ids.contains(&session.session_id))
                // A matching unowned rollout is only metadata for a hook
                // generation whose exact process is already gone. Preserve
                // an independently live rollout with the same session ID.
                .filter(|session| {
                    session.pid != 0 || !suppressed_gone_session_ids.contains(&session.session_id)
                }),
        );
        self.reconcile_herdr_working_continuity(&mut herdr_working_status, &herdr_observations);
        for session in &mut result {
            let key = (session.session_id.clone(), session.pid);
            let Some(observation) = herdr_observations.get(&key).copied() else {
                continue;
            };
            apply_herdr_observation(
                session,
                observation,
                herdr_working_status.get(&key).copied(),
                rollout_previews.get(&session.session_id),
            );
        }
        result.sort_by_key(|session| std::cmp::Reverse(session.started_at));
        result
    }

    fn hook_placeholder(&self, record: &HookCollectorRecord) -> AgentSession {
        AgentSession {
            agent_cli: "codex",
            pid: 0,
            action_process_incarnation: None,
            session_id: record.session_id.clone(),
            cwd: record.cwd.clone(),
            project_name: process::last_path_segment(&record.cwd)
                .unwrap_or("?")
                .to_string(),
            started_at: record.started_at_ms,
            status: SessionStatus::Unknown,
            status_evidence: StatusEvidence::default(),
            model: String::new(),
            effort: String::new(),
            context_percent: 0.0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 0,
            current_tasks: Vec::new(),
            mem_mb: 0,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: Vec::new(),
            context_history: Vec::new(),
            compaction_count: 0,
            context_window: 0,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            awaiting_input: false,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
            config_root: super::abbrev_path(self.sessions_dir.parent().unwrap_or(Path::new("."))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_hook_record(
        &self,
        session: &mut AgentSession,
        record: &HookCollectorRecord,
        rollout: Option<&RolloutLifecycle>,
        rollout_preview: Option<String>,
        shared: &super::SharedProcessData,
        now_ms: u64,
        process_visible: bool,
    ) {
        let (status, authority, reason) = project_hook_status(record, rollout, now_ms);
        let observed_at_ms = record.observed_at_ms.min(now_ms).max(1);
        let mut evidence = StatusEvidence::default();
        for sample in record.observations.iter().rev().take(127).rev() {
            evidence.observe(StatusObservation::new(
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                sample.reason,
                sample.observed_at_ms.min(now_ms),
                0,
            ));
        }
        evidence.observe(StatusObservation::new(
            status,
            authority,
            reason,
            observed_at_ms,
            0,
        ));
        if record.status_since_ms > 0 && record.status_since_ms <= observed_at_ms {
            evidence.status_since_ms = record.status_since_ms;
        }

        session.status = status;
        session.status_evidence = evidence;
        session.awaiting_input = false;
        session.current_tasks = vec![hook_task_label(status, rollout_preview)];
        session.pending_since_ms = if status == SessionStatus::Executing {
            session.status_evidence.status_since_ms
        } else {
            0
        };
        session.thinking_since_ms = if status == SessionStatus::Thinking {
            session.status_evidence.status_since_ms
        } else {
            0
        };

        if process_visible && record.process_state == HookProcessState::Live {
            session.pid = record.pid;
            session.action_process_incarnation = record
                .actionable
                .then_some(status)
                .filter(|status| {
                    matches!(
                        status,
                        SessionStatus::Idle | SessionStatus::Thinking | SessionStatus::Executing
                    )
                })
                .and(record.process_incarnation.clone());
            if record.owns_resources {
                session.mem_mb = shared
                    .process_info
                    .get(&record.pid)
                    .map(|process| process.rss_kb / 1024)
                    .unwrap_or(0);
                session.children = collect_resource_children(
                    record.pid,
                    &shared.process_info,
                    &shared.children_map,
                    &shared.ports,
                );
            } else {
                session.mem_mb = 0;
                session.children.clear();
            }
        } else {
            session.pid = 0;
            session.action_process_incarnation = None;
            session.mem_mb = 0;
            session.children.clear();
        }
        session.enforce_status_contract();
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

    fn is_active_desktop_rollout(path: &Path, active_mtime_secs: u64) -> bool {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        let age = std::time::SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age.as_secs() >= active_mtime_secs {
            return false;
        }

        parse_codex_jsonl(path).is_some_and(|result| result.is_codex_desktop())
    }

    fn active_desktop_rollouts(
        pid_to_rollouts: HashMap<u32, Vec<PathBuf>>,
        seen_jsonl: &HashSet<PathBuf>,
        mcp_owned_rollouts: &HashSet<PathBuf>,
        active_mtime_secs: u64,
    ) -> Vec<(u32, PathBuf)> {
        let mut candidates: Vec<(u32, PathBuf)> = pid_to_rollouts
            .into_iter()
            .flat_map(|(pid, paths)| paths.into_iter().map(move |path| (pid, path)))
            .collect();
        candidates.sort_by_key(|(_, path)| {
            std::cmp::Reverse(
                fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .unwrap_or(std::time::UNIX_EPOCH),
            )
        });

        let mut emitted = HashSet::new();
        candidates
            .into_iter()
            .filter(|(_, path)| {
                !seen_jsonl.contains(path)
                    && !mcp_owned_rollouts.contains(path)
                    && emitted.insert(path.clone())
                    && Self::is_active_desktop_rollout(path, active_mtime_secs)
            })
            .collect()
    }

    fn desktop_pid_by_rollout_path(
        pid_to_rollouts: &HashMap<u32, Vec<PathBuf>>,
        active_mtime_secs: u64,
    ) -> HashMap<PathBuf, u32> {
        Self::active_desktop_rollouts(
            pid_to_rollouts.clone(),
            &HashSet::new(),
            &HashSet::new(),
            active_mtime_secs,
        )
        .into_iter()
        .map(|(pid, path)| (path, pid))
        .collect()
    }

    fn foreground_desktop_rollouts(
        sessions_dir: &Path,
        seen_jsonl: &HashSet<PathBuf>,
        mcp_owned_rollouts: &HashSet<PathBuf>,
        active_mtime_secs: u64,
    ) -> Vec<PathBuf> {
        let Some(today_dir) = Self::today_session_dir(sessions_dir) else {
            return Vec::new();
        };
        let roots = [today_dir];
        Self::recent_desktop_rollouts_from_roots(
            &roots,
            seen_jsonl,
            mcp_owned_rollouts,
            active_mtime_secs,
        )
    }

    fn recent_desktop_rollouts_from_roots(
        roots: &[PathBuf],
        seen_jsonl: &HashSet<PathBuf>,
        mcp_owned_rollouts: &HashSet<PathBuf>,
        active_mtime_secs: u64,
    ) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        for root in roots {
            Self::collect_recent_desktop_rollouts(
                root,
                seen_jsonl,
                mcp_owned_rollouts,
                active_mtime_secs,
                &mut candidates,
            );
        }
        Self::sort_rollouts_by_mtime_desc(&mut candidates);
        candidates
    }

    fn recent_desktop_rollouts(
        sessions_dir: &Path,
        seen_jsonl: &HashSet<PathBuf>,
        mcp_owned_rollouts: &HashSet<PathBuf>,
        active_mtime_secs: u64,
    ) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        Self::collect_recent_desktop_rollouts(
            sessions_dir,
            seen_jsonl,
            mcp_owned_rollouts,
            active_mtime_secs,
            &mut candidates,
        );
        Self::sort_rollouts_by_mtime_desc(&mut candidates);
        candidates
    }

    fn sort_rollouts_by_mtime_desc(paths: &mut [PathBuf]) {
        paths.sort_by_key(|path| {
            std::cmp::Reverse(
                fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .unwrap_or(std::time::UNIX_EPOCH),
            )
        });
    }

    fn collect_recent_desktop_rollouts(
        dir: &Path,
        seen_jsonl: &HashSet<PathBuf>,
        mcp_owned_rollouts: &HashSet<PathBuf>,
        active_mtime_secs: u64,
        candidates: &mut Vec<PathBuf>,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                Self::collect_recent_desktop_rollouts(
                    &path,
                    seen_jsonl,
                    mcp_owned_rollouts,
                    active_mtime_secs,
                    candidates,
                );
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            if seen_jsonl.contains(&path) || mcp_owned_rollouts.contains(&path) {
                continue;
            }
            if Self::is_active_desktop_rollout(&path, active_mtime_secs) {
                candidates.push(path);
            }
        }
    }

    fn load_session_with_rate_limit(
        &self,
        process_ctx: CodexProcessContext,
        jsonl_path: &Path,
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
        mcp_server_pids: &HashSet<u32>,
    ) -> Option<(AgentSession, Option<RateLimitInfo>)> {
        let result = self.parse_rollout_cached(jsonl_path)?;
        self.build_session_with_rate_limit(
            process_ctx,
            result,
            &[],
            process_info,
            children_map,
            ports,
            mcp_server_pids,
        )
    }

    /// Load the current root rollout for one CLI PID and aggregate any child
    /// rollouts held open by that process. Codex keeps subagent rollout file
    /// descriptors open alongside the root, so selecting the last `lsof` path
    /// would otherwise surface an arbitrary child as the interactive session.
    fn load_cli_session_group(
        &self,
        process_ctx: CodexProcessContext,
        jsonl_paths: &[PathBuf],
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
        mcp_server_pids: &HashSet<u32>,
    ) -> CodexCliSessionGroupLoad {
        let mut owned_paths = jsonl_paths.to_vec();
        owned_paths.sort();
        owned_paths.dedup();
        let mut every_descriptor_parsed = true;
        let parsed = jsonl_paths
            .iter()
            .filter_map(|path| match self.parse_rollout_cached(path) {
                Some(result) => Some((path.clone(), result)),
                None => {
                    every_descriptor_parsed = false;
                    None
                }
            })
            .collect();
        let Some(mut group) = select_codex_rollout_group(parsed) else {
            return CodexCliSessionGroupLoad {
                session: None,
                rate_limit: None,
                owned_paths,
            };
        };
        // Every path came from the live process's open descriptor set. Dropping
        // one unparseable descriptor could hide another active root or child,
        // so the selected tree remains useful only for metrics.
        group.lifecycle_valid &= every_descriptor_parsed;
        let mut root = group.root;
        root.lifecycle_valid &= group.lifecycle_valid;
        let Some((session, rate_limit)) = self.build_session_with_rate_limit(
            process_ctx,
            root,
            &group.children,
            process_info,
            children_map,
            ports,
            mcp_server_pids,
        ) else {
            return CodexCliSessionGroupLoad {
                session: None,
                rate_limit: None,
                owned_paths,
            };
        };
        CodexCliSessionGroupLoad {
            session: Some(session),
            rate_limit,
            owned_paths,
        }
    }

    fn parse_rollout_cached(&self, path: &Path) -> Option<CodexJSONLResult> {
        let cache_clock = {
            let mut cache = self.parse_cache.borrow_mut();
            cache.clock = cache.clock.saturating_add(1);
            cache.clock
        };
        let mut unstable_result = None;
        for _ in 0..2 {
            let canonical_path = fs::canonicalize(path).ok()?;
            let file = open_rollout_file(path).ok()?;
            let before = RolloutFingerprint::read(&file).ok()?;

            let cached = self
                .parse_cache
                .borrow()
                .entries
                .get(&canonical_path)
                .filter(|entry| entry.fingerprint == before)
                .map(|entry| entry.result.clone());
            if let Some(result) = cached {
                let descriptor_after = RolloutFingerprint::read(&file).ok()?;
                if descriptor_after == before
                    && rollout_path_matches_fingerprint(path, &canonical_path, &before)
                {
                    if let Some(entry) = self
                        .parse_cache
                        .borrow_mut()
                        .entries
                        .get_mut(&canonical_path)
                    {
                        entry.last_used = cache_clock;
                    }
                    return Some(result);
                }
                continue;
            }

            let mut result = parse_codex_open_file(&file)?;
            let descriptor_after = RolloutFingerprint::read(&file).ok()?;
            if descriptor_after == before
                && rollout_path_matches_fingerprint(path, &canonical_path, &before)
            {
                let mut cache = self.parse_cache.borrow_mut();
                cache.entries.insert(
                    canonical_path,
                    CachedCodexParse {
                        fingerprint: before,
                        result: result.clone(),
                        last_used: cache_clock,
                    },
                );
                while cache.entries.len() > MAX_CODEX_PARSE_CACHE_ENTRIES {
                    let Some(oldest) = cache
                        .entries
                        .iter()
                        .min_by_key(|(_, entry)| entry.last_used)
                        .map(|(path, _)| path.clone())
                    else {
                        break;
                    };
                    cache.entries.remove(&oldest);
                }
                return Some(result);
            }

            // Preserve metadata visibility for this tick, but ensure an
            // unstable read cannot corroborate any positive hook status.
            result.lifecycle_valid = false;
            unstable_result = Some(result);
        }
        unstable_result
    }

    #[allow(clippy::too_many_arguments)]
    fn build_session_with_rate_limit(
        &self,
        process_ctx: CodexProcessContext,
        result: CodexJSONLResult,
        related_rollouts: &[CodexJSONLResult],
        process_info: &HashMap<u32, ProcInfo>,
        children_map: &HashMap<u32, Vec<u32>>,
        ports: &HashMap<u32, Vec<u16>>,
        mcp_server_pids: &HashSet<u32>,
    ) -> Option<(AgentSession, Option<RateLimitInfo>)> {
        let proc = process_ctx.pid.and_then(|p| process_info.get(&p));
        let mem_mb = if process_ctx.owns_process_tree {
            proc.map(|p| p.rss_kb / 1024).unwrap_or(0)
        } else {
            0
        };
        let display_pid = process_ctx.pid.unwrap_or(0);

        let project_name = process::last_path_segment(&result.cwd)
            .unwrap_or("?")
            .to_string();

        // Build a provisional rollout lifecycle for correlation and display
        // metadata. No status computed in this block leaves the collector
        // directly: finalize_hook_sessions replaces it with validated hook
        // evidence or Unknown.
        //
        // Codex interactive sessions emit task_complete after every turn, so
        // task_complete alone is not process-exit evidence.
        let pid_alive = proc.is_some();
        // Codex emits exact task boundaries for each interactive turn. An
        // assistant commentary message does not end the turn because tool calls
        // or more model output may follow it.
        let related_activity = related_rollouts.iter().any(|child| {
            child.turn_active || child.pending_since_ms > 0 || child.awaiting_input_since_ms > 0
        });
        let session_done = !pid_alive
            || (process_ctx.is_exec
                && result.task_complete
                && result.pending_since_ms == 0
                && !related_activity);
        let related_awaiting_input = related_rollouts
            .iter()
            .any(|child| child.awaiting_input_since_ms > 0);
        let awaiting_input = !process_ctx.unknown_process_owner
            && !session_done
            && (result.awaiting_input_since_ms > 0 || related_awaiting_input);
        let status = if process_ctx.unknown_process_owner {
            SessionStatus::Unknown
        } else if session_done {
            SessionStatus::Done
        } else if awaiting_input {
            SessionStatus::Waiting
        } else {
            let has_active_child = process_ctx.owns_process_tree
                && process_ctx.pid.is_some_and(|p| {
                    process::has_active_descendant(p, children_map, process_info, 5.0)
                });
            let has_active_subagent = related_rollouts
                .iter()
                .any(|child| child.turn_active || child.pending_since_ms > 0);
            if has_active_child || result.pending_since_ms > 0 || has_active_subagent {
                SessionStatus::Executing
            } else if result.turn_active {
                SessionStatus::Thinking
            } else {
                SessionStatus::Idle
            }
        };

        let active_subagent = related_rollouts
            .iter()
            .filter(|child| child.turn_active || child.pending_since_ms > 0)
            .max_by_key(|child| child.last_activity);

        // Current task from last tool use
        // For exec (one-shot) sessions, task_complete means truly finished.
        // For interactive sessions, task_complete fires after every turn — ignore it.
        let current_tasks = if awaiting_input {
            vec!["waiting for user input".to_string()]
        } else if !result.current_task.is_empty() {
            vec![result.current_task]
        } else if let Some(child) = active_subagent {
            vec![format!("subagent {}", child.subagent_display_name())]
        } else if matches!(status, SessionStatus::Unknown) {
            vec!["unknown".to_string()]
        } else if !pid_alive || (process_ctx.is_exec && result.task_complete) {
            vec!["finished".to_string()]
        } else if matches!(status, SessionStatus::Idle) {
            vec!["idle".to_string()]
        } else {
            vec!["thinking".to_string()]
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
        if let (true, Some(p)) = (process_ctx.owns_process_tree, process_ctx.pid) {
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
        let rate_limit = std::iter::once(result.rate_limit.as_ref())
            .chain(
                related_rollouts
                    .iter()
                    .map(|child| child.rate_limit.as_ref()),
            )
            .flatten()
            .max_by_key(|info| info.updated_at)
            .cloned();

        let subagents = related_rollouts
            .iter()
            .map(|child| SubAgent {
                name: child.subagent_display_name(),
                status: if child.turn_active || child.pending_since_ms > 0 {
                    "working".to_string()
                } else {
                    "done".to_string()
                },
                tokens: child
                    .total_input
                    .saturating_add(child.total_output)
                    .saturating_add(child.total_cache_read),
            })
            .collect();

        let pending_since_ms = std::iter::once(result.pending_since_ms)
            .chain(related_rollouts.iter().map(|child| child.pending_since_ms))
            .filter(|started| *started > 0)
            .min()
            .unwrap_or(0);

        self.rollout_lifecycle.borrow_mut().insert(
            result.session_id.clone(),
            RolloutLifecycle {
                root_cli_version: result.version.clone(),
                turn_active: result.turn_active,
                task_complete: result.task_complete,
                lifecycle_valid: result.lifecycle_valid,
                active_turn_id: result.active_turn_id.clone(),
                completed_turn_id: result.completed_turn_id.clone(),
                turn_started_at_ms: result.turn_started_at_ms,
                latest_lifecycle_at_ms: result.latest_lifecycle_at_ms,
                task_completed_at_ms: result.task_completed_at_ms,
                // Ordinary root tools correlate only with root hook IDs.
                // Child work is promoted through a complete SubagentStart /
                // SubagentStop hook set after exact child-to-root mapping.
                open_tool_ids: result.open_tool_ids.clone(),
                open_tool_started_at_ms: result.open_tool_started_at_ms.clone(),
                open_tool_classes: result.open_tool_classes.clone(),
                completed_tool_calls: result.completed_tool_calls.clone(),
                nested_code_mode_end_at_ms: result.nested_code_mode_end_at_ms.clone(),
                live_code_mode_cells: result.live_code_mode_cells,
                code_mode_correlation_ambiguous: result.code_mode_correlation_ambiguous,
                descendants: related_rollouts
                    .iter()
                    .map(|child| DescendantRolloutLifecycle {
                        session_id: child.session_id.clone(),
                        cli_version: child.version.clone(),
                        direct_child: child.parent_thread_id.as_deref()
                            == Some(result.session_id.as_str()),
                        lifecycle_valid: child.lifecycle_valid,
                        turn_active: child.turn_active,
                        task_complete: child.task_complete,
                        active_turn_id: child.active_turn_id.clone(),
                        completed_turn_id: child.completed_turn_id.clone(),
                        turn_started_at_ms: child.turn_started_at_ms,
                        latest_lifecycle_at_ms: child.latest_lifecycle_at_ms,
                        task_completed_at_ms: child.task_completed_at_ms,
                        open_tool_ids: child.open_tool_ids.clone(),
                        open_tool_started_at_ms: child.open_tool_started_at_ms.clone(),
                        open_tool_classes: child.open_tool_classes.clone(),
                        live_code_mode_cells: child.live_code_mode_cells,
                        code_mode_correlation_ambiguous: child.code_mode_correlation_ambiguous,
                    })
                    .collect(),
                relevant_process_descendant: process_ctx.owns_process_tree
                    && process_ctx.pid.is_some_and(|pid| {
                        has_relevant_codex_process_descendant(
                            pid,
                            process_info,
                            children_map,
                            mcp_server_pids,
                        )
                    }),
            },
        );
        Some((
            AgentSession {
                agent_cli: "codex",
                pid: display_pid,
                // Rollout/process association is useful for metadata, but only
                // validated hook ownership can supply an action target.
                action_process_incarnation: None,
                session_id: result.session_id,
                cwd: result.cwd,
                project_name,
                started_at: result.started_at,
                status,
                status_evidence: Default::default(),
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
                subagents,
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt: result.initial_prompt,
                first_assistant_text: String::new(),
                chat_messages: result.chat_messages,
                tool_calls: result.tool_calls,
                pending_since_ms,
                awaiting_input,
                thinking_since_ms: result.thinking_since_ms,
                file_accesses: vec![],
                config_root: super::abbrev_path(
                    self.sessions_dir
                        .parent()
                        .unwrap_or(std::path::Path::new(".")),
                ),
            },
            rate_limit,
        ))
    }

    /// Find PIDs of running codex processes from shared process data (no extra ps call).
    /// Returns (pid, is_exec) tuples — `is_exec` is true for one-shot `codex exec` runs.
    /// PIDs in `mcp_server_pids` are skipped so `codex mcp-server` processes
    /// are reported via the MCP servers panel instead.
    fn find_codex_pids_from_shared(
        process_info: &HashMap<u32, ProcInfo>,
        mcp_server_pids: &HashSet<u32>,
    ) -> Vec<(u32, bool)> {
        let mut pids = Vec::new();
        for (pid, info) in process_info {
            if mcp_server_pids.contains(pid) {
                continue;
            }
            let cmd = &info.command;
            let tokens = process::command_tokens(cmd);
            let is_exec = tokens.iter().any(|token| token == "exec");
            let is_host = tokens.iter().any(|token| {
                matches!(
                    token.as_str(),
                    "app-server" | "daemon" | "mcp-server" | "remote-control"
                )
            });
            let is_codex = process::cmd_has_binary(cmd, "codex")
                || process::cmd_has_binary(&cmd.replace('\\', "/"), "codex");
            if is_codex && !is_host && !cmd.contains("grep") {
                pids.push((*pid, is_exec));
            }
        }

        // Windows npm/Git shims can create a chain like:
        // sh.exe -> node.exe ...\codex.js -> codex.exe.
        // Once the real codex child exists, keep that child and drop wrapper
        // ancestors; otherwise Windows rollout fallback maps each candidate PID
        // to a different recent JSONL file and historical sessions look live.
        let candidates = pids.clone();
        pids.retain(|(pid, _)| {
            process::cmd_first_token_has_binary(
                process_info
                    .get(pid)
                    .map(|info| info.command.as_str())
                    .unwrap_or_default(),
                "codex",
            ) || !candidates.iter().any(|(other_pid, _)| {
                *other_pid != *pid && process::is_descendant_of(*other_pid, *pid, process_info)
            })
        });

        pids
    }

    /// Find Codex Desktop app-server host PIDs. Desktop is kept separate from
    /// CLI discovery because a single app-server PID can hold many rollout fds.
    pub(crate) fn find_codex_desktop_pids_from_shared(
        process_info: &HashMap<u32, ProcInfo>,
        mcp_server_pids: &HashSet<u32>,
    ) -> Vec<u32> {
        let mut pids = Vec::new();
        for (pid, info) in process_info {
            if mcp_server_pids.contains(pid) {
                continue;
            }
            let cmd = &info.command;
            if process::cmd_has_binary(cmd, "codex")
                && cmd.contains(" app-server")
                && !cmd.contains("grep")
            {
                pids.push(*pid);
            }
        }
        pids.sort_unstable();
        pids
    }

    /// Map Codex PIDs to every open rollout-*.jsonl file. A single CLI process
    /// holds both its root rollout and spawned subagent rollouts open.
    ///
    /// On Linux, scans /proc/{pid}/fd symlinks directly (no process spawn).
    /// On Windows, scans ~/.codex/sessions/YYYY/MM/DD/ for recently modified
    /// JSONL files and assigns them to discovered PIDs, since Windows has no
    /// equivalent of lsof for enumerating open file descriptors.
    /// Falls back to lsof on macOS/other platforms.
    fn map_pid_to_jsonl(pids: &[u32], sessions_dir: &Path) -> HashMap<u32, Vec<PathBuf>> {
        // sessions_dir is consumed only by the windows arm below.
        #[cfg(not(target_os = "windows"))]
        let _ = sessions_dir;

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
                        map.entry(pid).or_insert_with(Vec::new).push(target);
                    }
                }
                if let Some(paths) = map.get_mut(&pid) {
                    paths.sort();
                    paths.dedup();
                }
            }
            map
        }

        #[cfg(target_os = "windows")]
        {
            // Windows has no lsof or /proc/{pid}/fd to map PIDs to open files.
            // Instead, scan today's ~/.codex/sessions/YYYY/MM/DD/ directory for
            // rollout-*.jsonl files, then assign them to discovered codex PIDs.
            // Prefer recently modified files, but fall back to any today's file
            // since Codex may be idle (waiting for input) and not actively writing.
            let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

            if let Some(today_dir) = Self::today_session_dir(sessions_dir) {
                if let Ok(entries) = fs::read_dir(&today_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                            continue;
                        }
                        if let Ok(meta) = fs::metadata(&path) {
                            if let Ok(modified) = meta.modified() {
                                candidates.push((path, modified));
                            }
                        }
                    }
                }
            }

            // Sort by modification time descending (most recent first)
            candidates.sort_by_key(|b| std::cmp::Reverse(b.1));

            // Assign candidates to PIDs (most recent file → first PID)
            for (i, &pid_u32) in pids.iter().enumerate() {
                if i < candidates.len() {
                    map.insert(pid_u32, vec![candidates[i].0.clone()]);
                }
            }

            map
        }

        #[cfg(all(not(target_os = "linux"), not(target_os = "windows")))]
        {
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
                                map.entry(pid)
                                    .or_insert_with(Vec::new)
                                    .push(PathBuf::from(name));
                            }
                        }
                    }
                }
            }
            for paths in map.values_mut() {
                paths.sort();
                paths.dedup();
            }
            map
        }
    }
}

impl Default for CodexCollector {
    fn default() -> Self {
        Self::new()
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
#[derive(Clone)]
struct CodexJSONLResult {
    session_id: String,
    /// Parent thread when this rollout belongs to a spawned Codex subagent.
    parent_thread_id: Option<String>,
    /// Provider-assigned subagent nickname or path, kept bounded for display.
    subagent_name: String,
    cwd: String,
    originator: String,
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
    lifecycle_valid: bool,
    active_turn_id: Option<String>,
    completed_turn_id: Option<String>,
    turn_started_at_ms: u64,
    latest_lifecycle_at_ms: u64,
    task_completed_at_ms: u64,
    /// Exact interactive-turn lifecycle from task_started through
    /// task_complete/turn_aborted. Unlike assistant messages, commentary does
    /// not close the turn because tools may follow it.
    turn_active: bool,
    last_activity: std::time::SystemTime,
    initial_prompt: String,
    chat_messages: Vec<ChatMessage>,
    /// Input tokens excluding cached input, matching AgentSession's additive
    /// token accounting where cache reads are stored separately.
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
    /// Exact provider call IDs that are still open at the rollout tail.
    open_tool_ids: HashSet<String>,
    /// Provider observation time for every exact currently open call ID.
    open_tool_started_at_ms: HashMap<String, u64>,
    /// Content-free interaction class for every exact currently open call ID.
    open_tool_classes: HashMap<String, RolloutToolClass>,
    /// Exact canonical completions from the currently selected root turn.
    completed_tool_calls: HashMap<String, CompletedRolloutCall>,
    /// Exact nested Code Mode end identities exposed by the selected root turn.
    nested_code_mode_end_at_ms: HashMap<String, u64>,
    /// Bounded yielded Code Mode cells that remain live beyond an outer call.
    live_code_mode_cells: usize,
    /// Multiple/cross-turn Code Mode shapes with no exact hook-call bijection.
    code_mode_correlation_ambiguous: bool,
    /// Earliest start timestamp among open `request_user_input` calls.
    awaiting_input_since_ms: u64,
    /// Timestamp of the latest user prompt not yet followed by assistant output.
    thinking_since_ms: u64,
}

impl CodexJSONLResult {
    fn is_codex_desktop(&self) -> bool {
        self.originator == "Codex Desktop"
    }

    fn subagent_display_name(&self) -> String {
        if !self.subagent_name.is_empty() {
            return self.subagent_name.clone();
        }
        self.session_id.chars().take(12).collect()
    }

    fn is_exact_terminal_lifecycle(&self) -> bool {
        self.lifecycle_valid
            && self.task_complete
            && !self.turn_active
            && self.active_turn_id.is_none()
            && self.completed_turn_id.is_some()
            && self.turn_started_at_ms > 0
            && self.turn_started_at_ms <= self.latest_lifecycle_at_ms
            && self.latest_lifecycle_at_ms <= self.task_completed_at_ms
            && self.task_completed_at_ms > 0
            && self.open_tool_ids.is_empty()
            && self.open_tool_started_at_ms.is_empty()
            && self.open_tool_classes.is_empty()
            && self.live_code_mode_cells == 0
            && !self.code_mode_correlation_ambiguous
    }
}

struct CodexRolloutGroup {
    root: CodexJSONLResult,
    children: Vec<CodexJSONLResult>,
    lifecycle_valid: bool,
}

/// Select the most recently active root tree from all rollout descriptors held
/// by one Codex CLI process. Subagent metadata provides an exact parent thread
/// ID. If a parent file is no longer open, that subtree is treated as its own
/// candidate rather than being attached to an unrelated root.
fn select_codex_rollout_group(
    candidates: Vec<(PathBuf, CodexJSONLResult)>,
) -> Option<CodexRolloutGroup> {
    let mut parsed = Vec::new();
    let mut seen_paths = HashSet::new();
    for (path, result) in candidates {
        if !seen_paths.insert(path.clone()) {
            continue;
        }
        parsed.push((path, result));
    }
    if parsed.is_empty() {
        return None;
    }

    let mut lifecycle_valid = true;
    let mut id_to_index = HashMap::new();
    let mut parent_by_id = HashMap::<String, Option<String>>::new();
    for (idx, (_, result)) in parsed.iter().enumerate() {
        if result.session_id.is_empty() {
            lifecycle_valid = false;
            continue;
        }
        if id_to_index.insert(result.session_id.clone(), idx).is_some() {
            lifecycle_valid = false;
        }
        if let Some(previous_parent) =
            parent_by_id.insert(result.session_id.clone(), result.parent_thread_id.clone())
        {
            if previous_parent != result.parent_thread_id {
                lifecycle_valid = false;
            }
        }
    }

    // Validate every resolvable parent chain independently. A parent whose
    // descriptor is not currently open remains a legitimate detached root,
    // but cycles and self-parenting make the whole process association
    // ambiguous and must never be split into apparently valid trees.
    for start in 0..parsed.len() {
        let mut current = start;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let Some(parent_id) = parsed[current].1.parent_thread_id.as_ref() else {
                break;
            };
            let Some(parent) = id_to_index.get(parent_id).copied() else {
                break;
            };
            current = parent;
        }
        if visited.contains(&current)
            && parsed[current]
                .1
                .parent_thread_id
                .as_ref()
                .and_then(|parent| id_to_index.get(parent))
                .is_some()
        {
            lifecycle_valid = false;
        }
    }

    fn root_for(
        start: usize,
        parsed: &[(PathBuf, CodexJSONLResult)],
        id_to_index: &HashMap<String, usize>,
    ) -> usize {
        let mut current = start;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let Some(parent_id) = parsed[current].1.parent_thread_id.as_ref() else {
                return current;
            };
            let Some(parent) = id_to_index.get(parent_id).copied() else {
                return current;
            };
            current = parent;
        }
        // Malformed cyclic metadata: fail closed to the originally selected
        // rollout instead of merging unrelated records.
        start
    }

    let mut trees: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..parsed.len() {
        trees
            .entry(root_for(idx, &parsed, &id_to_index))
            .or_default()
            .push(idx);
    }

    let (&selected_root, selected_indices) =
        trees
            .iter()
            .max_by(|(root_a, members_a), (root_b, members_b)| {
                let activity_a = members_a
                    .iter()
                    .map(|idx| parsed[*idx].1.last_activity)
                    .max()
                    .unwrap_or(std::time::UNIX_EPOCH);
                let activity_b = members_b
                    .iter()
                    .map(|idx| parsed[*idx].1.last_activity)
                    .max()
                    .unwrap_or(std::time::UNIX_EPOCH);
                activity_a
                    .cmp(&activity_b)
                    .then_with(|| parsed[**root_a].0.cmp(&parsed[**root_b].0))
            })?;

    // One native CLI can retain descriptors for several root threads. A
    // non-selected tree is harmless only when every one of its descriptors is
    // exact-terminal; otherwise choosing the newest root could hide concurrent
    // or malformed work owned by the same process.
    lifecycle_valid &= trees.iter().all(|(root, members)| {
        *root == selected_root
            || members
                .iter()
                .all(|idx| parsed[*idx].1.is_exact_terminal_lifecycle())
    });

    let root = parsed[selected_root].1.clone();
    let mut child_indices: Vec<usize> = selected_indices
        .iter()
        .copied()
        .filter(|idx| *idx != selected_root)
        .collect();
    child_indices.sort_by_key(|idx| parsed[*idx].1.started_at);
    let children = child_indices
        .iter()
        .map(|idx| parsed[*idx].1.clone())
        .collect();
    Some(CodexRolloutGroup {
        root,
        children,
        lifecycle_valid,
    })
}

fn parse_rollout_timestamp_ms(raw: &str, now_ms: u64) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
        .filter(|timestamp_ms| *timestamp_ms > 0 && *timestamp_ms <= now_ms)
}

fn event_timestamp_ms(val: &Value, now_ms: u64) -> Option<u64> {
    val["timestamp"]
        .as_str()
        .and_then(|timestamp| parse_rollout_timestamp_ms(timestamp, now_ms))
}

fn advance_rollout_lifecycle(result: &mut CodexJSONLResult, timestamp_ms: u64) {
    if timestamp_ms == 0 || timestamp_ms < result.latest_lifecycle_at_ms {
        result.lifecycle_valid = false;
        return;
    }
    result.latest_lifecycle_at_ms = timestamp_ms;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CopiedRolloutMeta {
    parent_thread_id: Option<String>,
    cli_version: String,
}

fn copied_rollout_epoch_thresholds(
    session_id: &str,
    parent_thread_id: &str,
    cli_version: &str,
    replayed_meta: &HashMap<String, CopiedRolloutMeta>,
) -> Option<Vec<u64>> {
    if cli_version != "0.146.0" {
        return None;
    }
    let child_timestamp_ms = uuid_v7_timestamp_ms(session_id)?;
    let mut current = parent_thread_id.to_string();
    let mut visited = HashSet::new();
    let mut thresholds = Vec::new();

    loop {
        if !visited.insert(current.clone()) {
            return None;
        }
        let meta = replayed_meta.get(&current)?;
        if meta.cli_version != cli_version {
            return None;
        }
        let current_timestamp_ms = uuid_v7_timestamp_ms(&current)?;
        if current_timestamp_ms >= child_timestamp_ms {
            return None;
        }
        let Some(parent_thread_id) = meta.parent_thread_id.as_ref() else {
            break;
        };
        if uuid_v7_timestamp_ms(parent_thread_id)? >= current_timestamp_ms {
            return None;
        }
        // Only a replayed subagent (never the root metadata) authorizes an
        // inherited copied-epoch boundary.
        thresholds.push(current_timestamp_ms);
        current = parent_thread_id.clone();
    }

    if visited.len() != replayed_meta.len() {
        return None;
    }
    thresholds.reverse();
    if !thresholds.windows(2).all(|pair| pair[0] < pair[1]) {
        return None;
    }
    thresholds.push(child_timestamp_ms);
    Some(thresholds)
}

fn copied_prefix_record_has_irreversible_failure(val: &Value) -> bool {
    match val["type"].as_str() {
        Some("error") => true,
        Some("event_msg") => match val["payload"]["type"].as_str() {
            Some("stream_error" | "error") => true,
            Some("task_complete") => !val["payload"]["error"].is_null(),
            _ => false,
        },
        _ => false,
    }
}

fn reset_copied_subagent_rollout_epoch(result: &mut CodexJSONLResult, boundary_ms: u64) {
    // Full-history subagents persist a copied parent prefix before their own
    // first turn. Keep the child file identity, but discard inherited metrics
    // and lifecycle state once the exact child-local delimiter is observed.
    result.model = String::from("-");
    result.effort.clear();
    result.context_window = 0;
    result.turn_count = 0;
    result.current_task.clear();
    result.task_complete = false;
    result.active_turn_id = None;
    result.completed_turn_id = None;
    result.turn_started_at_ms = 0;
    result.latest_lifecycle_at_ms = 0;
    result.task_completed_at_ms = 0;
    result.turn_active = false;
    result.last_activity = std::time::UNIX_EPOCH + std::time::Duration::from_millis(boundary_ms);
    result.initial_prompt.clear();
    result.chat_messages.clear();
    result.total_input = 0;
    result.total_output = 0;
    result.total_cache_read = 0;
    result.last_context_tokens = 0;
    result.token_history.clear();
    result.rate_limit = None;
    result.tool_calls.clear();
    result.pending_since_ms = 0;
    result.open_tool_ids.clear();
    result.open_tool_started_at_ms.clear();
    result.open_tool_classes.clear();
    result.completed_tool_calls.clear();
    result.nested_code_mode_end_at_ms.clear();
    result.live_code_mode_cells = 0;
    result.code_mode_correlation_ambiguous = false;
    result.awaiting_input_since_ms = 0;
    result.thinking_since_ms = 0;
}

fn sanitize_tool_arg(arg: &str) -> String {
    let terminal_safe = super::sanitize_terminal_text(arg);
    let redacted = super::redact_secrets(&terminal_safe);
    redacted.chars().take(120).collect()
}

fn push_chat_message(messages: &mut Vec<ChatMessage>, role: ChatRole, text: String) {
    if text.is_empty() {
        return;
    }
    messages.push(ChatMessage { role, text });
    let len = messages.len();
    if len > MAX_CHAT_MESSAGES {
        messages.drain(..len - MAX_CHAT_MESSAGES);
    }
}

fn clean_chat_text(raw: &str, max: usize) -> String {
    let cleaned = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with("```"))
        .collect::<Vec<_>>()
        .join(" ");
    let terminal_safe = super::sanitize_terminal_text(&cleaned);
    let redacted = super::redact_secrets(&terminal_safe);
    redacted.chars().take(max).collect()
}

fn parse_codex_tool_arg(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return String::new();
    };

    for key in ["file_path", "path"] {
        if let Some(raw) = value[key].as_str() {
            let short = process::last_path_segment(raw).unwrap_or(raw);
            return sanitize_tool_arg(short);
        }
    }

    // Commands, stdin, prompts, request bodies, and arbitrary tool arguments
    // can contain source or secrets. Only provider process/session identifiers
    // are safe enough to preview beyond an allowlisted path.
    if let Some(raw) = value["session_id"].as_str() {
        return sanitize_tool_arg(raw);
    }
    if let Some(raw) = value["session_id"].as_u64() {
        return raw.to_string();
    }

    String::new()
}

fn parse_codex_tool_session_id(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    let raw = &value["session_id"];
    if let Some(s) = raw.as_str() {
        return bounded_rollout_session_id(s);
    }
    raw.as_u64()
        .map(|n| n.to_string())
        .and_then(|id| bounded_rollout_session_id(&id))
}

fn bounded_rollout_session_id(raw: &str) -> Option<String> {
    (!raw.is_empty()
        && raw.len() <= 128
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    .then(|| raw.to_string())
}

fn bounded_rollout_lifecycle_id(raw: &str) -> Option<String> {
    (!raw.is_empty()
        && raw.len() <= MAX_ROLLOUT_LIFECYCLE_ID_BYTES
        && raw.bytes().all(|byte| byte.is_ascii_graphic()))
    .then(|| raw.to_string())
}

fn bounded_rollout_tool_name(raw: &str) -> Option<&str> {
    (!raw.is_empty()
        && raw.len() <= MAX_ROLLOUT_TOOL_NAME_BYTES
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')))
    .then_some(raw)
}

fn known_rollout_end_matches_tool(
    event_type: &str,
    namespace: Option<&str>,
    tool_name: &str,
) -> bool {
    match event_type {
        "exec_command_end" => namespace.is_none() && tool_name == "exec_command",
        "patch_apply_end" => namespace.is_none() && tool_name == "apply_patch",
        "web_search_end" => namespace == Some("web") && tool_name == "run",
        "image_generation_end" => namespace == Some("image_gen") && tool_name == "imagegen",
        "mcp_tool_call_end" => namespace.is_some_and(|namespace| namespace.starts_with("mcp__")),
        _ => false,
    }
}

fn bounded_code_mode_cell_id(raw: &str) -> Option<String> {
    fn canonical_positive_decimal(raw: &str) -> Option<u64> {
        let value = raw.parse::<u64>().ok().filter(|value| *value > 0)?;
        (value.to_string() == raw).then_some(value)
    }

    if raw.is_empty() || raw.len() > 128 {
        return None;
    }
    if canonical_positive_decimal(raw).is_some() {
        return Some(raw.to_string());
    }
    let (generation, cell) = raw.strip_prefix('g')?.split_once(':')?;
    (canonical_positive_decimal(generation).is_some_and(|generation| generation >= 2)
        && canonical_positive_decimal(cell).is_some())
    .then(|| raw.to_string())
}

fn default_code_mode_wait_yield_time_ms() -> u64 {
    10_000
}

#[derive(Debug, Deserialize)]
struct CodeModeWaitArgs {
    cell_id: String,
    #[serde(
        rename = "yield_time_ms",
        default = "default_code_mode_wait_yield_time_ms"
    )]
    _yield_time_ms: u64,
    #[serde(rename = "max_tokens", default)]
    _max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedCodeModeWaitArgs {
    cell_id: String,
    terminate: bool,
}

fn parse_code_mode_wait_args(arguments: &str) -> Option<ParsedCodeModeWaitArgs> {
    let CodeModeWaitArgs {
        cell_id, terminate, ..
    } = serde_json::from_str(arguments).ok()?;
    Some(ParsedCodeModeWaitArgs {
        cell_id: bounded_code_mode_cell_id(&cell_id)?,
        terminate,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum CodeModeOutputState {
    Running(String),
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct YieldedCodeModeCell {
    turn_id: String,
    origin_call_id: String,
    exec_started_at_ms: u64,
    exec_yielded_at_ms: u64,
}

fn code_mode_output_text(output: &Value) -> Option<&str> {
    // Rollouts preserve a single text-only status item as a string, while a
    // status plus runtime output is represented as a content-item array.
    if let Some(text) = output.as_str() {
        return Some(text);
    }
    let first = output.as_array()?.first()?.as_object()?;
    if first.len() != 2 {
        return None;
    }
    (first.get("type").and_then(Value::as_str) == Some("input_text"))
        .then(|| first.get("text")?.as_str())
        .flatten()
}

fn code_mode_output_state(output: &Value) -> Option<CodeModeOutputState> {
    const RUNNING_PREFIX: &str = "Script running with cell ID ";
    let frame = code_mode_output_text(output)?;
    if frame.len() > MAX_CODE_MODE_STATUS_FRAME_BYTES {
        return None;
    }
    let mut lines = frame.split('\n');
    let status = lines.next()?;
    let wall_time = lines
        .next()?
        .strip_prefix("Wall time ")?
        .strip_suffix(" seconds")?;
    let (whole_seconds, fractional_seconds) = wall_time.split_once('.')?;
    if whole_seconds.is_empty()
        || !whole_seconds.bytes().all(|byte| byte.is_ascii_digit())
        || (whole_seconds.len() > 1 && whole_seconds.starts_with('0'))
        || fractional_seconds.len() != 1
        || !fractional_seconds.bytes().all(|byte| byte.is_ascii_digit())
        || lines.next()? != "Output:"
        || !lines.next()?.is_empty()
        || lines.next().is_some()
    {
        return None;
    }
    if let Some(raw_cell_id) = status.strip_prefix(RUNNING_PREFIX) {
        return bounded_code_mode_cell_id(raw_cell_id).map(CodeModeOutputState::Running);
    }
    matches!(
        status,
        "Script completed" | "Script failed" | "Script terminated"
    )
    .then_some(CodeModeOutputState::Terminal)
}

fn running_process_session_id(output: &str) -> Option<String> {
    let marker = "Process running with session ID ";
    let after = output
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(marker))?;
    let id = after.split_whitespace().next()?;
    bounded_rollout_session_id(id.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
}

fn output_reports_process_exit(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.trim_start().starts_with("Process exited"))
}

#[derive(Clone, Debug)]
struct OpenRolloutCall {
    started_at_ms: u64,
    name: String,
    custom: bool,
    class: RolloutToolClass,
    tool_index: Option<usize>,
    write_stdin_target: Option<String>,
    code_mode_wait_target: Option<String>,
    code_mode_wait_terminate: bool,
    known_end_at_ms: Option<u64>,
}

#[derive(Clone, Debug)]
struct SeenRolloutCall {
    namespace: Option<String>,
    name: String,
}

fn close_codex_tool_call(
    call_id: &str,
    end_ms: u64,
    tool_calls: &mut [ToolCall],
    open_calls: &mut HashMap<String, OpenRolloutCall>,
    pending_tasks: &mut Vec<(String, String)>,
) -> Option<OpenRolloutCall> {
    let call = open_calls.remove(call_id)?;
    if let Some(idx) = call.tool_index {
        if let Some(tool_call) = tool_calls.get_mut(idx) {
            tool_call.duration_ms = call
                .known_end_at_ms
                .unwrap_or(end_ms)
                .saturating_sub(call.started_at_ms);
        }
    }
    pending_tasks.retain(|(id, _)| id != call_id);
    Some(call)
}

fn remember_completed_rollout_call(
    completed: &mut HashMap<String, CompletedRolloutCall>,
    call_id: &str,
    call: &OpenRolloutCall,
    completed_at_ms: u64,
    code_mode_terminal: bool,
) {
    if !rollout_call_id_is_exact(call_id)
        || call.started_at_ms == 0
        || completed_at_ms < call.started_at_ms
        || completed.len() >= MAX_TRACKED_ROLLOUT_CALL_IDS
        || completed.contains_key(call_id)
    {
        return;
    }
    completed.insert(
        call_id.to_string(),
        CompletedRolloutCall {
            started_at_ms: call.started_at_ms,
            completed_at_ms,
            class: call.class,
            code_mode_terminal,
        },
    );
}

fn complete_yielded_code_mode_exec(
    completed: &mut HashMap<String, CompletedRolloutCall>,
    open_calls: &HashMap<String, OpenRolloutCall>,
    provenance: &YieldedCodeModeCell,
    wait_call: &OpenRolloutCall,
    active_turn_id: Option<&str>,
    completed_at_ms: u64,
) -> bool {
    if active_turn_id != Some(provenance.turn_id.as_str())
        || !rollout_call_id_is_exact(&provenance.origin_call_id)
        || open_calls.contains_key(&provenance.origin_call_id)
        || provenance.exec_started_at_ms == 0
        || provenance.exec_yielded_at_ms < provenance.exec_started_at_ms
        || wait_call.started_at_ms < provenance.exec_yielded_at_ms
        || completed_at_ms < wait_call.started_at_ms
        || wait_call.code_mode_wait_terminate
    {
        return false;
    }
    let RolloutToolClass::CodeModeWait {
        exec_started_at_ms,
        exec_yielded_at_ms,
    } = wait_call.class
    else {
        return false;
    };
    if exec_started_at_ms != provenance.exec_started_at_ms
        || exec_yielded_at_ms != provenance.exec_yielded_at_ms
    {
        return false;
    }

    let Some(origin) = completed.get_mut(&provenance.origin_call_id) else {
        return false;
    };
    if origin.started_at_ms != provenance.exec_started_at_ms
        || origin.completed_at_ms < origin.started_at_ms
        || origin.completed_at_ms > provenance.exec_yielded_at_ms
        || origin.code_mode_terminal
        || origin.class
            != (RolloutToolClass::CodeModeExec {
                exec_started_at_ms: provenance.exec_started_at_ms,
            })
    {
        return false;
    }

    origin.completed_at_ms = completed_at_ms;
    origin.code_mode_terminal = true;
    true
}

fn close_codex_turn_calls(
    end_ms: u64,
    tool_calls: &mut [ToolCall],
    open_calls: &mut HashMap<String, OpenRolloutCall>,
    pending_tasks: &mut Vec<(String, String)>,
    running_exec_by_session: &HashMap<String, String>,
) {
    let background_execs: HashSet<&str> = running_exec_by_session
        .values()
        .map(String::as_str)
        .collect();
    let call_ids: Vec<String> = open_calls
        .keys()
        .filter(|call_id| !background_execs.contains(call_id.as_str()))
        .cloned()
        .collect();
    for call_id in call_ids {
        close_codex_tool_call(&call_id, end_ms, tool_calls, open_calls, pending_tasks);
    }
}

fn codex_tool_waits_for_user(name: &str) -> bool {
    name == "request_user_input"
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
/// - response_item (function_call): current tool use and user-input waits
/// - turn_context: model, effort
fn parse_codex_jsonl(path: &Path) -> Option<CodexJSONLResult> {
    let file = open_rollout_file(path).ok()?;
    parse_codex_open_file(&file)
}

fn parse_codex_open_file(file: &fs::File) -> Option<CodexJSONLResult> {
    let mut reader = BufReader::new(file);
    let parse_now_ms = unix_now_ms();

    let mut result = CodexJSONLResult {
        session_id: String::new(),
        parent_thread_id: None,
        subagent_name: String::new(),
        cwd: String::new(),
        originator: String::new(),
        started_at: 0,
        model: String::from("-"),
        effort: String::new(),
        version: String::new(),
        git_branch: String::new(),
        context_window: 0,
        turn_count: 0,
        current_task: String::new(),
        task_complete: false,
        lifecycle_valid: true,
        active_turn_id: None,
        completed_turn_id: None,
        turn_started_at_ms: 0,
        latest_lifecycle_at_ms: 0,
        task_completed_at_ms: 0,
        turn_active: false,
        last_activity: std::time::UNIX_EPOCH,
        initial_prompt: String::new(),
        chat_messages: Vec::new(),
        total_input: 0,
        total_output: 0,
        total_cache_read: 0,
        last_context_tokens: 0,
        token_history: Vec::new(),
        rate_limit: None,
        tool_calls: Vec::new(),
        pending_since_ms: 0,
        open_tool_ids: HashSet::new(),
        open_tool_started_at_ms: HashMap::new(),
        open_tool_classes: HashMap::new(),
        completed_tool_calls: HashMap::new(),
        nested_code_mode_end_at_ms: HashMap::new(),
        live_code_mode_cells: 0,
        code_mode_correlation_ambiguous: false,
        awaiting_input_since_ms: 0,
        thinking_since_ms: 0,
    };
    let mut open_calls: HashMap<String, OpenRolloutCall> = HashMap::new();
    let mut seen_calls: HashMap<String, SeenRolloutCall> = HashMap::new();
    let mut seen_known_end_ids: HashSet<String> = HashSet::new();
    let mut running_exec_by_session: HashMap<String, String> = HashMap::new();
    let mut yielded_code_mode_cells: HashMap<String, YieldedCodeModeCell> = HashMap::new();
    let mut code_mode_correlation_ambiguous = false;
    let mut pending_tasks: Vec<(String, String)> = Vec::new();
    let mut fork_metadata_attested = false;
    let mut saw_exact_inherited_parent_meta = false;
    let mut replayed_meta: HashMap<String, CopiedRolloutMeta> = HashMap::new();
    let mut copied_epoch_thresholds: Option<Vec<u64>> = None;
    let mut copied_epoch_index = 0_usize;
    let mut copied_epoch_chain_initialized = false;
    let mut previous_record_was_thread_settings_applied = false;

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
            Err(_) => {
                result.lifecycle_valid = false;
                break;
            }
        }
        // Cap hit without a newline — skip this file's remainder.
        if line_buf.len() > MAX_LINE_BYTES && !line_buf.ends_with('\n') {
            result.lifecycle_valid = false;
            break;
        }
        let line = line_buf.trim();
        if line.is_empty() {
            continue;
        }

        let val: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // A final incomplete append is normal while Codex is writing, but
            // its hidden event could be a tool, error, child, or turn edge.
            // Fail this poll closed; an appended completion changes the file
            // fingerprint and reparses the full record on the next poll.
            Err(_) if !line_buf.ends_with('\n') => {
                result.lifecycle_valid = false;
                break;
            }
            Err(_) => {
                result.lifecycle_valid = false;
                continue;
            }
        };

        // Provider timestamps participate in lifecycle proof. A pre-epoch or
        // future timestamp cannot be used to order a current observation.
        let record_timestamp_ms = val["timestamp"]
            .as_str()
            .and_then(|timestamp| parse_rollout_timestamp_ms(timestamp, parse_now_ms));
        if !val["timestamp"].is_null() && record_timestamp_ms.is_none() {
            result.lifecycle_valid = false;
        }
        if let Some(timestamp_ms) = record_timestamp_ms {
            let sys_time = std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms);
            if sys_time > result.last_activity {
                result.last_activity = sys_time;
            }
        }

        let follows_thread_settings_applied = previous_record_was_thread_settings_applied;
        previous_record_was_thread_settings_applied = val["type"].as_str() == Some("event_msg")
            && val["payload"]["type"].as_str() == Some("thread_settings_applied");

        let direct_fork_prefix_pending = fork_metadata_attested
            && replayed_meta.is_empty()
            && copied_epoch_index == 0
            && val["type"].as_str() != Some("session_meta");
        if direct_fork_prefix_pending {
            let local_epoch_candidate = if val["type"].as_str() == Some("event_msg")
                && val["payload"]["type"].as_str() == Some("task_started")
            {
                let child_timestamp_ms = uuid_v7_timestamp_ms(&result.session_id);
                let turn_timestamp_ms = val["payload"]["turn_id"]
                    .as_str()
                    .and_then(bounded_rollout_lifecycle_id)
                    .as_deref()
                    .and_then(uuid_v7_timestamp_ms);
                match (child_timestamp_ms, turn_timestamp_ms) {
                    (Some(child_timestamp_ms), Some(turn_timestamp_ms)) => {
                        turn_timestamp_ms >= child_timestamp_ms
                    }
                    _ => {
                        result.lifecycle_valid = false;
                        false
                    }
                }
            } else {
                false
            };
            if !local_epoch_candidate {
                if copied_prefix_record_has_irreversible_failure(&val) {
                    result.lifecycle_valid = false;
                }
                continue;
            }
        }
        match val["type"].as_str() {
            Some("session_meta") => {
                // A forked subagent rollout starts with its own metadata, then
                // may replay parent/root session_meta records as inherited
                // history. The first valid identity belongs to this file;
                // replacing it with a later parent collapses the rollout graph
                // and hides active child work from the root session.
                if !result.session_id.is_empty() {
                    let payload = &val["payload"];
                    let replayed_id = payload["id"]
                        .as_str()
                        .and_then(bounded_rollout_lifecycle_id);
                    let raw_parent_thread_id = payload["parent_thread_id"].as_str().or_else(|| {
                        payload["source"]["subagent"]["thread_spawn"]["parent_thread_id"].as_str()
                    });
                    let replayed_parent_thread_id =
                        raw_parent_thread_id.and_then(bounded_rollout_lifecycle_id);
                    let replayed_version = payload["cli_version"].as_str().filter(|version| {
                        !version.is_empty()
                            && version.len() <= 64
                            && version.bytes().all(|byte| byte.is_ascii_graphic())
                    });
                    if copied_epoch_chain_initialized
                        || replayed_meta.len() >= MAX_COPIED_ROLLOUT_SESSION_META
                        || replayed_id.is_none()
                        || replayed_version.is_none()
                        || (raw_parent_thread_id.is_some() && replayed_parent_thread_id.is_none())
                    {
                        result.lifecycle_valid = false;
                        continue;
                    }
                    let replayed_id = replayed_id.unwrap_or_default();
                    if result.parent_thread_id.as_deref() == Some(replayed_id.as_str()) {
                        saw_exact_inherited_parent_meta = true;
                    }
                    if replayed_meta
                        .insert(
                            replayed_id,
                            CopiedRolloutMeta {
                                parent_thread_id: replayed_parent_thread_id,
                                cli_version: replayed_version.unwrap_or_default().to_string(),
                            },
                        )
                        .is_some()
                    {
                        result.lifecycle_valid = false;
                    }
                    continue;
                }
                let payload = &val["payload"];
                if let Some(id) = payload["id"].as_str() {
                    result.session_id = id.to_string();
                }
                let direct_parent_value = &payload["parent_thread_id"];
                let raw_direct_parent_id = direct_parent_value.as_str();
                let direct_parent_id = raw_direct_parent_id.and_then(bounded_rollout_lifecycle_id);
                let forked_from_value = &payload["forked_from_id"];
                let raw_forked_from_id = forked_from_value.as_str();
                let forked_from_id = raw_forked_from_id.and_then(bounded_rollout_lifecycle_id);
                let source_parent_value =
                    &payload["source"]["subagent"]["thread_spawn"]["parent_thread_id"];
                let raw_source_parent_id = source_parent_value.as_str();
                let source_parent_id = raw_source_parent_id.and_then(bounded_rollout_lifecycle_id);
                let parent_thread_id = direct_parent_id
                    .clone()
                    .or_else(|| source_parent_id.clone());
                if (!direct_parent_value.is_null() && raw_direct_parent_id.is_none())
                    || (!forked_from_value.is_null() && raw_forked_from_id.is_none())
                    || (!source_parent_value.is_null() && raw_source_parent_id.is_none())
                    || (raw_direct_parent_id.is_some() && direct_parent_id.is_none())
                    || (raw_forked_from_id.is_some() && forked_from_id.is_none())
                    || (raw_source_parent_id.is_some() && source_parent_id.is_none())
                    || (direct_parent_id.is_some()
                        && source_parent_id.is_some()
                        && direct_parent_id != source_parent_id)
                {
                    result.lifecycle_valid = false;
                }
                fork_metadata_attested = direct_parent_id.is_some()
                    && forked_from_id == direct_parent_id
                    && source_parent_id == direct_parent_id;
                if !forked_from_value.is_null() && !fork_metadata_attested {
                    result.lifecycle_valid = false;
                }
                result.parent_thread_id = parent_thread_id;
                if let Some(name) = payload["agent_nickname"]
                    .as_str()
                    .or_else(|| {
                        payload["source"]["subagent"]["thread_spawn"]["agent_nickname"].as_str()
                    })
                    .or_else(|| payload["agent_path"].as_str())
                    .or_else(|| {
                        payload["source"]["subagent"]["thread_spawn"]["agent_path"].as_str()
                    })
                {
                    let short = process::last_path_segment(name).unwrap_or(name);
                    result.subagent_name = sanitize_tool_arg(short).chars().take(60).collect();
                }
                if let Some(cwd) = payload["cwd"].as_str() {
                    result.cwd = cwd.to_string();
                }
                if let Some(originator) = payload["originator"].as_str() {
                    result.originator = originator.to_string();
                }
                if let Some(ver) = payload["cli_version"].as_str() {
                    result.version = ver.to_string();
                }
                // started_at from timestamp
                if let Some(timestamp) = payload["timestamp"].as_str() {
                    if let Some(timestamp_ms) = parse_rollout_timestamp_ms(timestamp, parse_now_ms)
                    {
                        result.started_at = timestamp_ms;
                    } else {
                        result.lifecycle_valid = false;
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
                        let boundary_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                        let turn_id = payload["turn_id"]
                            .as_str()
                            .and_then(bounded_rollout_lifecycle_id);
                        let copied_history_attested =
                            saw_exact_inherited_parent_meta || fork_metadata_attested;
                        if copied_history_attested && !copied_epoch_chain_initialized {
                            copied_epoch_chain_initialized = true;
                            copied_epoch_thresholds =
                                result
                                    .parent_thread_id
                                    .as_deref()
                                    .and_then(|parent_thread_id| {
                                        if replayed_meta.is_empty() && fork_metadata_attested {
                                            if result.version != "0.146.0" {
                                                return None;
                                            }
                                            let child_timestamp_ms =
                                                uuid_v7_timestamp_ms(&result.session_id)?;
                                            let parent_timestamp_ms =
                                                uuid_v7_timestamp_ms(parent_thread_id)?;
                                            return (parent_timestamp_ms < child_timestamp_ms)
                                                .then_some(vec![child_timestamp_ms]);
                                        }
                                        if saw_exact_inherited_parent_meta {
                                            copied_rollout_epoch_thresholds(
                                                &result.session_id,
                                                parent_thread_id,
                                                &result.version,
                                                &replayed_meta,
                                            )
                                        } else {
                                            None
                                        }
                                    });
                            if copied_epoch_thresholds.is_none() {
                                result.lifecycle_valid = false;
                            }
                        }
                        let turn_timestamp_ms = turn_id.as_deref().and_then(uuid_v7_timestamp_ms);
                        let copied_epoch_candidate = copied_epoch_thresholds
                            .as_ref()
                            .and_then(|thresholds| {
                                thresholds
                                    .get(copied_epoch_index)
                                    .copied()
                                    .map(|threshold_ms| (thresholds, threshold_ms))
                            })
                            .zip(turn_timestamp_ms)
                            .filter(|((_, threshold_ms), turn_timestamp_ms)| {
                                *turn_timestamp_ms >= *threshold_ms
                            });
                        if let Some(((thresholds, threshold_ms), turn_timestamp_ms)) =
                            copied_epoch_candidate
                        {
                            let upper_threshold_ms =
                                thresholds.get(copied_epoch_index + 1).copied();
                            // Consume the first candidate for this exact fork
                            // threshold even when it is malformed. A later
                            // delimiter can therefore never repair invalidity.
                            copied_epoch_index += 1;
                            let exact_copied_epoch_boundary = result.lifecycle_valid
                                && follows_thread_settings_applied
                                && boundary_ms > 0
                                && turn_timestamp_ms > threshold_ms
                                && turn_timestamp_ms <= boundary_ms
                                && upper_threshold_ms
                                    .is_none_or(|upper_ms| turn_timestamp_ms < upper_ms);
                            if exact_copied_epoch_boundary {
                                reset_copied_subagent_rollout_epoch(&mut result, boundary_ms);
                                open_calls.clear();
                                seen_calls.clear();
                                seen_known_end_ids.clear();
                                running_exec_by_session.clear();
                                yielded_code_mode_cells.clear();
                                code_mode_correlation_ambiguous = false;
                                pending_tasks.clear();
                            } else {
                                result.lifecycle_valid = false;
                            }
                        }
                        close_codex_turn_calls(
                            boundary_ms,
                            &mut result.tool_calls,
                            &mut open_calls,
                            &mut pending_tasks,
                            &running_exec_by_session,
                        );
                        seen_calls.retain(|call_id, _| open_calls.contains_key(call_id));
                        result.completed_tool_calls.clear();
                        result.nested_code_mode_end_at_ms.clear();
                        if result.turn_active {
                            result.lifecycle_valid = false;
                        }
                        advance_rollout_lifecycle(&mut result, boundary_ms);
                        result.task_complete = false;
                        result.lifecycle_valid &= boundary_ms > 0 && turn_id.is_some();
                        result.active_turn_id = turn_id;
                        result.completed_turn_id = None;
                        result.turn_started_at_ms = boundary_ms;
                        result.task_completed_at_ms = 0;
                        result.turn_active = true;
                        result.thinking_since_ms = boundary_ms;
                        if yielded_code_mode_cells.values().any(|cell| {
                            result.active_turn_id.as_deref() != Some(cell.turn_id.as_str())
                        }) {
                            code_mode_correlation_ambiguous = true;
                        }
                        if let Some(cw) = payload["model_context_window"].as_u64() {
                            result.context_window = cw;
                        }
                    }
                    Some("user_message") => {
                        let boundary_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                        advance_rollout_lifecycle(&mut result, boundary_ms);
                        result.task_complete = false;
                        result.turn_active = true;
                        if result.active_turn_id.is_none() {
                            result.lifecycle_valid = false;
                        }
                        result.thinking_since_ms = boundary_ms;
                        if let Some(msg) = payload["message"].as_str() {
                            if result.initial_prompt.is_empty() {
                                let truncated: String = msg.chars().take(120).collect();
                                result.initial_prompt = super::redact_secrets(&truncated);
                            }
                            push_chat_message(
                                &mut result.chat_messages,
                                ChatRole::User,
                                clean_chat_text(msg, 500),
                            );
                        }
                    }
                    Some("token_count") => {
                        let info = &payload["info"];
                        // Codex input_tokens already includes cached_input_tokens.
                        // Store only the non-cached input portion so
                        // AgentSession::total_tokens() does not double-count cache.
                        let total = &info["total_token_usage"];
                        if total.is_object() {
                            let inp = total["input_tokens"].as_u64().unwrap_or(0);
                            let out = total["output_tokens"].as_u64().unwrap_or(0);
                            let cache = total["cached_input_tokens"]
                                .as_u64()
                                .or_else(|| total["cache_read_input_tokens"].as_u64())
                                .unwrap_or(0);
                            result.total_input = inp.saturating_sub(cache);
                            result.total_output = out;
                            result.total_cache_read = cache;
                        }
                        // Use last_token_usage input as the current context window.
                        // cached_input_tokens is a subset of input_tokens, not extra
                        // context after compaction.
                        let last = &info["last_token_usage"];
                        if last.is_object() {
                            let inp = last["input_tokens"].as_u64().unwrap_or(0);
                            let out = last["output_tokens"].as_u64().unwrap_or(0);
                            result.last_context_tokens = inp;
                            if result.token_history.len() < 10_000 {
                                result.token_history.push(inp + out);
                            }
                        }
                        // Context window may also appear inside info
                        if let Some(cw) = info["model_context_window"].as_u64() {
                            result.context_window = cw;
                        }
                        // Preserve Codex's exact primary/secondary identities,
                        // then populate the legacy short/long compatibility
                        // fields based on duration. A free-plan primary can be
                        // a longer window such as 30d (43200 minutes).
                        let rl = &payload["rate_limits"];
                        if rl.is_object() && is_account_level_codex_rate_limit(rl) {
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
                                let Some(window) = native_codex_rate_limit_window(slot, w) else {
                                    continue;
                                };
                                let mins = window.window_minutes.unwrap_or(0);
                                if mins <= 300 {
                                    info.five_hour_pct = Some(window.used_pct);
                                    info.five_hour_resets_at = window.resets_at;
                                    info.five_hour_window_minutes = window.window_minutes;
                                } else {
                                    info.seven_day_pct = Some(window.used_pct);
                                    info.seven_day_resets_at = window.resets_at;
                                    info.seven_day_window_minutes = window.window_minutes;
                                }
                                info.windows.push(window);
                            }
                            if !info.windows.is_empty() {
                                result.rate_limit = Some(info);
                            }
                        }
                    }
                    Some("agent_message") => {
                        result.turn_count += 1;
                        // Agent messages can be progress commentary followed by
                        // tools. Only the exact turn boundary closes thinking.
                        if let Some(msg) = payload["message"].as_str() {
                            push_chat_message(
                                &mut result.chat_messages,
                                ChatRole::Assistant,
                                clean_chat_text(msg, 500),
                            );
                        }
                    }
                    Some("task_complete") => {
                        let boundary_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                        let completed_turn_id = payload["turn_id"]
                            .as_str()
                            .and_then(bounded_rollout_lifecycle_id);
                        let exact_boundary = payload["error"].is_null()
                            && boundary_ms > 0
                            && completed_turn_id.is_some()
                            && completed_turn_id == result.active_turn_id
                            && result.turn_started_at_ms > 0
                            && result.turn_started_at_ms <= result.latest_lifecycle_at_ms
                            && result.latest_lifecycle_at_ms <= boundary_ms
                            && open_calls.is_empty();
                        close_codex_turn_calls(
                            boundary_ms,
                            &mut result.tool_calls,
                            &mut open_calls,
                            &mut pending_tasks,
                            &running_exec_by_session,
                        );
                        result.lifecycle_valid &= exact_boundary;
                        advance_rollout_lifecycle(&mut result, boundary_ms);
                        result.task_complete = result.lifecycle_valid && exact_boundary;
                        result.completed_turn_id = completed_turn_id;
                        result.task_completed_at_ms = boundary_ms;
                        result.active_turn_id = None;
                        result.turn_active = false;
                        result.thinking_since_ms = 0;
                    }
                    Some("turn_aborted") => {
                        let boundary_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                        let aborted_turn_id = payload["turn_id"]
                            .as_str()
                            .and_then(bounded_rollout_lifecycle_id);
                        let exact_boundary = boundary_ms > 0
                            && aborted_turn_id.is_some()
                            && aborted_turn_id == result.active_turn_id
                            && result.turn_active
                            && !result.task_complete
                            && result.completed_turn_id.is_none()
                            && result.turn_started_at_ms > 0
                            && result.turn_started_at_ms <= result.latest_lifecycle_at_ms
                            && result.latest_lifecycle_at_ms <= boundary_ms;
                        close_codex_turn_calls(
                            boundary_ms,
                            &mut result.tool_calls,
                            &mut open_calls,
                            &mut pending_tasks,
                            &running_exec_by_session,
                        );
                        // A yielded Code Mode cell is session-scoped and can
                        // continue after the turn task is aborted. Preserve
                        // its exact provenance and require a later matching
                        // wait/termination edge before lifecycle promotion.
                        code_mode_correlation_ambiguous = !yielded_code_mode_cells.is_empty();
                        // An exact abort makes this turn unavailable, but it is
                        // also a complete boundary: a later exact task_started
                        // begins a new lifecycle epoch in the same rollout.
                        // Structural ambiguity remains sticky because `&=` can
                        // never restore a lifecycle invalidated earlier.
                        result.lifecycle_valid &= exact_boundary;
                        advance_rollout_lifecycle(&mut result, boundary_ms);
                        result.task_complete = false;
                        result.active_turn_id = None;
                        result.completed_turn_id = None;
                        result.task_completed_at_ms = 0;
                        result.turn_active = false;
                        result.thinking_since_ms = 0;
                    }
                    Some("stream_error" | "error") => {
                        // A transport/model stream failure can interrupt any
                        // point in the turn and has no exact recovery boundary
                        // in the rollout. Keep metrics, but permanently revoke
                        // lifecycle promotion for this parsed generation.
                        result.lifecycle_valid = false;
                        result.task_complete = false;
                        yielded_code_mode_cells.clear();
                        code_mode_correlation_ambiguous = false;
                    }
                    Some(
                        event_type @ ("exec_command_end"
                        | "image_generation_end"
                        | "mcp_tool_call_end"
                        | "patch_apply_end"
                        | "web_search_end"),
                    ) => {
                        let end_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                        advance_rollout_lifecycle(&mut result, end_ms);
                        let call_id = payload["call_id"]
                            .as_str()
                            .and_then(bounded_rollout_lifecycle_id);
                        let unique_end = call_id.as_ref().is_some_and(|call_id| {
                            if seen_known_end_ids.contains(call_id)
                                || seen_known_end_ids.len() >= MAX_TRACKED_ROLLOUT_CALL_IDS
                            {
                                false
                            } else {
                                seen_known_end_ids.insert(call_id.clone());
                                true
                            }
                        });
                        let seen_call =
                            call_id.as_ref().and_then(|call_id| seen_calls.get(call_id));
                        let direct_call = seen_call.is_some_and(|call| {
                            known_rollout_end_matches_tool(
                                event_type,
                                call.namespace.as_deref(),
                                &call.name,
                            )
                        });
                        let direct_open_timestamp_valid = call_id.as_ref().is_some_and(|call_id| {
                            open_calls
                                .get(call_id)
                                .is_some_and(|call| end_ms >= call.started_at_ms)
                        });
                        let nested_code_mode_started_at_ms = if seen_call.is_none() {
                            let mut candidates = open_calls.values().filter_map(|call| {
                                matches!(
                                    call.class,
                                    RolloutToolClass::CodeModeExec { .. }
                                        | RolloutToolClass::CodeModeWait { .. }
                                        | RolloutToolClass::CodeModeUncorrelatable
                                )
                                .then_some(call.started_at_ms)
                            });
                            let first = candidates.next();
                            if candidates.next().is_none() {
                                first
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        let inherited_copied_prefix = copied_epoch_thresholds
                            .as_ref()
                            .is_some_and(|thresholds| copied_epoch_index < thresholds.len());
                        let nested_code_mode_call = call_id.as_ref().is_some_and(|call_id| {
                            plugin::codex_version_has_verified_code_mode_shape(&result.version)
                                && code_mode_nested_call_id_is_exact(call_id)
                                && (nested_code_mode_started_at_ms
                                    .is_some_and(|started_at_ms| end_ms >= started_at_ms)
                                    || (inherited_copied_prefix
                                        && result.turn_started_at_ms > 0
                                        && end_ms >= result.turn_started_at_ms))
                        });
                        // These provider events precede the canonical response
                        // output for direct tools, so they validate correlation
                        // but do not normally close the top-level descriptor.
                        // Code Mode additionally emits them for one exact
                        // nested exec-UUIDv4 call with no rollout open record.
                        let event_shape_valid = end_ms > 0
                            && result.turn_active
                            && result.active_turn_id.is_some()
                            && unique_end
                            && ((direct_call && direct_open_timestamp_valid)
                                || nested_code_mode_call);
                        result.lifecycle_valid &= event_shape_valid;

                        if event_shape_valid && nested_code_mode_call {
                            if let Some(call_id) = call_id.as_ref() {
                                if result.nested_code_mode_end_at_ms.len()
                                    >= MAX_TRACKED_ROLLOUT_CALL_IDS
                                {
                                    result.lifecycle_valid = false;
                                } else {
                                    result
                                        .nested_code_mode_end_at_ms
                                        .insert(call_id.clone(), end_ms);
                                }
                            }
                        }

                        if event_shape_valid && direct_call {
                            if let Some(call_id) = call_id.as_deref() {
                                let mut direct_tool_timing = None;
                                if let Some(call) = open_calls.get_mut(call_id) {
                                    if call.known_end_at_ms.is_some() {
                                        result.lifecycle_valid = false;
                                    } else {
                                        call.known_end_at_ms = Some(end_ms);
                                        direct_tool_timing =
                                            Some((call.tool_index, call.started_at_ms));
                                    }
                                }
                                if let Some((Some(tool_index), started_at_ms)) = direct_tool_timing
                                {
                                    if let Some(tool_call) = result.tool_calls.get_mut(tool_index) {
                                        tool_call.duration_ms =
                                            end_ms.saturating_sub(started_at_ms);
                                    }
                                }
                                let background_sessions = running_exec_by_session
                                    .iter()
                                    .filter_map(|(session_id, exec_call_id)| {
                                        (exec_call_id == call_id).then_some(session_id.clone())
                                    })
                                    .collect::<Vec<_>>();
                                if !background_sessions.is_empty() {
                                    for session_id in background_sessions {
                                        running_exec_by_session.remove(&session_id);
                                    }
                                    if let Some(closed) = close_codex_tool_call(
                                        call_id,
                                        end_ms,
                                        &mut result.tool_calls,
                                        &mut open_calls,
                                        &mut pending_tasks,
                                    ) {
                                        remember_completed_rollout_call(
                                            &mut result.completed_tool_calls,
                                            call_id,
                                            &closed,
                                            end_ms,
                                            false,
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Some(event_type) if event_type.ends_with("_end") => {
                        result.lifecycle_valid = false;
                    }
                    _ => {}
                }
            }

            Some("response_item") => {
                let payload = &val["payload"];
                let item_type = payload["type"].as_str();
                // Codex uses function_call for built-in tools and
                // custom_tool_call for freeform tools such as the current
                // `exec` implementation. Both remain open until their matching
                // output record arrives.
                if matches!(item_type, Some("function_call" | "custom_tool_call")) {
                    if result.active_turn_id.is_none() {
                        result.lifecycle_valid = false;
                    }
                    if let Some(raw_name) = payload["name"].as_str() {
                        let Some(name) = bounded_rollout_tool_name(raw_name) else {
                            result.lifecycle_valid = false;
                            continue;
                        };
                        let namespace = match &payload["namespace"] {
                            Value::Null => None,
                            Value::String(raw_namespace) => {
                                let Some(namespace) = bounded_rollout_tool_name(raw_namespace)
                                else {
                                    result.lifecycle_valid = false;
                                    continue;
                                };
                                Some(namespace.to_string())
                            }
                            _ => {
                                result.lifecycle_valid = false;
                                continue;
                            }
                        };
                        // Extract first arg (typically file path or command)
                        let raw_input = if item_type == Some("function_call") {
                            &payload["arguments"]
                        } else {
                            &payload["input"]
                        };
                        // custom_tool_call input is freeform and may contain
                        // source, prompts, or secrets. Never surface it; only
                        // structured built-in function calls get a safe preview.
                        let arg = if item_type == Some("function_call") {
                            raw_input
                                .as_str()
                                .map(parse_codex_tool_arg)
                                .unwrap_or_default()
                        } else {
                            String::new()
                        };

                        let task = if arg.is_empty() {
                            name.to_string()
                        } else {
                            format!("{} {}", name, arg)
                        };

                        result.task_complete = false;
                        result.turn_active = true;
                        result.thinking_since_ms = 0;

                        if let Some(call_id) = payload["call_id"].as_str() {
                            let start_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                            advance_rollout_lifecycle(&mut result, start_ms);
                            if bounded_rollout_lifecycle_id(call_id).is_none()
                                || start_ms == 0
                                || open_calls.contains_key(call_id)
                                || seen_calls.contains_key(call_id)
                                || open_calls.len() >= MAX_OPEN_ROLLOUT_CALLS
                                || seen_calls.len() >= MAX_TRACKED_ROLLOUT_CALL_IDS
                            {
                                result.lifecycle_valid = false;
                            } else {
                                seen_calls.insert(
                                    call_id.to_string(),
                                    SeenRolloutCall {
                                        namespace: namespace.clone(),
                                        name: name.to_string(),
                                    },
                                );
                                let is_code_mode_wait =
                                    item_type == Some("function_call") && name == "wait";
                                let wait_args = is_code_mode_wait
                                    .then(|| raw_input.as_str().and_then(parse_code_mode_wait_args))
                                    .flatten();
                                if is_code_mode_wait && wait_args.is_none() {
                                    result.lifecycle_valid = false;
                                }
                                let wait_cell_id =
                                    wait_args.as_ref().map(|args| args.cell_id.clone());
                                let wait_terminate =
                                    wait_args.as_ref().is_some_and(|args| args.terminate);
                                let code_mode_call_open = open_calls.values().any(|call| {
                                    matches!(
                                        call.class,
                                        RolloutToolClass::CodeModeExec { .. }
                                            | RolloutToolClass::CodeModeWait { .. }
                                            | RolloutToolClass::CodeModeUncorrelatable
                                    )
                                });
                                let known_wait_cell = wait_cell_id
                                    .as_ref()
                                    .and_then(|cell_id| yielded_code_mode_cells.get(cell_id))
                                    .cloned();
                                let exact_wait_cell = known_wait_cell.as_ref().filter(|cell| {
                                    !code_mode_correlation_ambiguous
                                        && yielded_code_mode_cells.len() == 1
                                        && !code_mode_call_open
                                        && result.active_turn_id.as_deref()
                                            == Some(cell.turn_id.as_str())
                                });
                                let opens_code_mode_exec =
                                    item_type == Some("custom_tool_call") && name == "exec";
                                let introduces_ambiguity = (opens_code_mode_exec
                                    && (code_mode_call_open
                                        || !yielded_code_mode_cells.is_empty()
                                        || code_mode_correlation_ambiguous))
                                    || (is_code_mode_wait
                                        && (known_wait_cell.is_none()
                                            || exact_wait_cell.is_none()));
                                if introduces_ambiguity {
                                    code_mode_correlation_ambiguous = true;
                                    for call in open_calls.values_mut() {
                                        if matches!(
                                            call.class,
                                            RolloutToolClass::CodeModeExec { .. }
                                                | RolloutToolClass::CodeModeWait { .. }
                                        ) {
                                            call.class = RolloutToolClass::CodeModeUncorrelatable;
                                        }
                                    }
                                }
                                let exact_wait_cell = wait_cell_id.as_ref().and_then(|cell_id| {
                                    (!code_mode_correlation_ambiguous
                                        && yielded_code_mode_cells.len() == 1
                                        && !code_mode_call_open)
                                        .then(|| yielded_code_mode_cells.get(cell_id))
                                        .flatten()
                                        .filter(|cell| {
                                            result.active_turn_id.as_deref()
                                                == Some(cell.turn_id.as_str())
                                        })
                                });
                                let class = match (item_type, name, exact_wait_cell) {
                                    (Some("custom_tool_call"), "exec", _) => {
                                        if code_mode_correlation_ambiguous {
                                            RolloutToolClass::CodeModeUncorrelatable
                                        } else {
                                            RolloutToolClass::CodeModeExec {
                                                exec_started_at_ms: start_ms,
                                            }
                                        }
                                    }
                                    (Some("function_call"), "wait", _) if wait_terminate => {
                                        RolloutToolClass::CodeModeUncorrelatable
                                    }
                                    (Some("function_call"), "wait", Some(cell)) => {
                                        RolloutToolClass::CodeModeWait {
                                            exec_started_at_ms: cell.exec_started_at_ms,
                                            exec_yielded_at_ms: cell.exec_yielded_at_ms,
                                        }
                                    }
                                    (Some("function_call"), "wait", _) => {
                                        RolloutToolClass::CodeModeUncorrelatable
                                    }
                                    (_, "request_user_input", _) => {
                                        RolloutToolClass::RequestUserInput
                                    }
                                    _ => RolloutToolClass::Ordinary,
                                };
                                let code_mode_wait_target = wait_cell_id;
                                let write_stdin_target = (name == "write_stdin")
                                    .then(|| {
                                        raw_input.as_str().and_then(parse_codex_tool_session_id)
                                    })
                                    .flatten();
                                let tool_index = if result.tool_calls.len() < 500 {
                                    let idx = result.tool_calls.len();
                                    result.tool_calls.push(ToolCall {
                                        name: name.to_string(),
                                        arg,
                                        duration_ms: 0,
                                    });
                                    Some(idx)
                                } else {
                                    None
                                };
                                open_calls.insert(
                                    call_id.to_string(),
                                    OpenRolloutCall {
                                        started_at_ms: start_ms,
                                        name: name.to_string(),
                                        custom: item_type == Some("custom_tool_call"),
                                        class,
                                        tool_index,
                                        write_stdin_target,
                                        code_mode_wait_target,
                                        code_mode_wait_terminate: wait_terminate,
                                        known_end_at_ms: None,
                                    },
                                );
                                pending_tasks.retain(|(id, _)| id != call_id);
                                pending_tasks.push((call_id.to_string(), task));
                                if pending_tasks.len() > MAX_OPEN_ROLLOUT_CALLS {
                                    result.lifecycle_valid = false;
                                    pending_tasks.truncate(MAX_OPEN_ROLLOUT_CALLS);
                                }
                            }
                        } else {
                            result.lifecycle_valid = false;
                        }
                    } else {
                        result.lifecycle_valid = false;
                    }
                } else if matches!(
                    item_type,
                    Some("function_call_output" | "custom_tool_call_output")
                ) {
                    if let Some(call_id) = payload["call_id"].as_str() {
                        let Some(call) = open_calls.get(call_id).cloned() else {
                            result.lifecycle_valid = false;
                            continue;
                        };
                        let end_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                        advance_rollout_lifecycle(&mut result, end_ms);
                        if end_ms == 0
                            || call
                                .known_end_at_ms
                                .is_some_and(|known_end_at_ms| end_ms < known_end_at_ms)
                        {
                            result.lifecycle_valid = false;
                        }
                        let custom_output = item_type == Some("custom_tool_call_output");
                        if call.custom != custom_output {
                            result.lifecycle_valid = false;
                            close_codex_tool_call(
                                call_id,
                                end_ms,
                                &mut result.tool_calls,
                                &mut open_calls,
                                &mut pending_tasks,
                            );
                            continue;
                        }
                        let output_value = &payload["output"];
                        let output = output_value.as_str().unwrap_or_default();
                        let mut code_mode_terminal = false;
                        match call.name.as_str() {
                            "exec" if item_type == Some("custom_tool_call_output") => {
                                match code_mode_output_state(output_value) {
                                    Some(CodeModeOutputState::Running(cell_id)) => {
                                        if yielded_code_mode_cells.contains_key(&cell_id)
                                            || result.active_turn_id.is_none()
                                            || yielded_code_mode_cells.len()
                                                >= MAX_TRACKED_CODE_MODE_CELLS
                                        {
                                            result.lifecycle_valid = false;
                                        } else {
                                            if !yielded_code_mode_cells.is_empty()
                                                || matches!(
                                                    call.class,
                                                    RolloutToolClass::CodeModeUncorrelatable
                                                )
                                            {
                                                code_mode_correlation_ambiguous = true;
                                            }
                                            yielded_code_mode_cells.insert(
                                                cell_id,
                                                YieldedCodeModeCell {
                                                    turn_id: result
                                                        .active_turn_id
                                                        .clone()
                                                        .unwrap_or_default(),
                                                    origin_call_id: call_id.to_string(),
                                                    exec_started_at_ms: call.started_at_ms,
                                                    exec_yielded_at_ms: end_ms,
                                                },
                                            );
                                        }
                                    }
                                    Some(CodeModeOutputState::Terminal) => {
                                        code_mode_terminal = true;
                                    }
                                    None => result.lifecycle_valid = false,
                                }
                                close_codex_tool_call(
                                    call_id,
                                    end_ms,
                                    &mut result.tool_calls,
                                    &mut open_calls,
                                    &mut pending_tasks,
                                );
                            }
                            "wait" => {
                                close_codex_tool_call(
                                    call_id,
                                    end_ms,
                                    &mut result.tool_calls,
                                    &mut open_calls,
                                    &mut pending_tasks,
                                );
                                if let Some(target) = call.code_mode_wait_target.as_ref() {
                                    let unique_yielded_cell = yielded_code_mode_cells.len() == 1;
                                    let provenance = yielded_code_mode_cells.remove(target);
                                    match (
                                        provenance,
                                        code_mode_output_state(output_value),
                                        call.code_mode_wait_terminate,
                                    ) {
                                        (
                                            Some(mut provenance),
                                            Some(CodeModeOutputState::Running(cell_id)),
                                            false,
                                        ) if cell_id == *target => {
                                            provenance.exec_yielded_at_ms = end_ms;
                                            yielded_code_mode_cells
                                                .insert(target.clone(), provenance);
                                        }
                                        (
                                            Some(provenance),
                                            Some(CodeModeOutputState::Terminal),
                                            _,
                                        ) => {
                                            if matches!(
                                                call.class,
                                                RolloutToolClass::CodeModeWait { .. }
                                            ) && (!unique_yielded_cell
                                                || code_mode_correlation_ambiguous
                                                || !complete_yielded_code_mode_exec(
                                                    &mut result.completed_tool_calls,
                                                    &open_calls,
                                                    &provenance,
                                                    &call,
                                                    result.active_turn_id.as_deref(),
                                                    end_ms,
                                                ))
                                            {
                                                result.lifecycle_valid = false;
                                            }
                                        }
                                        _ => result.lifecycle_valid = false,
                                    }
                                }
                            }
                            "exec_command" => {
                                if let Some(session_id) = running_process_session_id(output) {
                                    if session_id.len() > MAX_ROLLOUT_LIFECYCLE_ID_BYTES
                                        || (!running_exec_by_session.contains_key(&session_id)
                                            && running_exec_by_session.len()
                                                >= MAX_OPEN_ROLLOUT_CALLS)
                                    {
                                        result.lifecycle_valid = false;
                                        close_codex_tool_call(
                                            call_id,
                                            end_ms,
                                            &mut result.tool_calls,
                                            &mut open_calls,
                                            &mut pending_tasks,
                                        );
                                    } else {
                                        running_exec_by_session
                                            .insert(session_id, call_id.to_string());
                                    }
                                } else {
                                    close_codex_tool_call(
                                        call_id,
                                        end_ms,
                                        &mut result.tool_calls,
                                        &mut open_calls,
                                        &mut pending_tasks,
                                    );
                                }
                            }
                            "write_stdin" => {
                                close_codex_tool_call(
                                    call_id,
                                    end_ms,
                                    &mut result.tool_calls,
                                    &mut open_calls,
                                    &mut pending_tasks,
                                );
                                if output_reports_process_exit(output) {
                                    if let Some(exec_call_id) =
                                        call.write_stdin_target.as_ref().and_then(|session_id| {
                                            running_exec_by_session.remove(session_id)
                                        })
                                    {
                                        if let Some(closed) = close_codex_tool_call(
                                            &exec_call_id,
                                            end_ms,
                                            &mut result.tool_calls,
                                            &mut open_calls,
                                            &mut pending_tasks,
                                        ) {
                                            remember_completed_rollout_call(
                                                &mut result.completed_tool_calls,
                                                &exec_call_id,
                                                &closed,
                                                end_ms,
                                                false,
                                            );
                                        }
                                    }
                                }
                            }
                            _ => {
                                close_codex_tool_call(
                                    call_id,
                                    end_ms,
                                    &mut result.tool_calls,
                                    &mut open_calls,
                                    &mut pending_tasks,
                                );
                            }
                        }
                        if result.lifecycle_valid && !open_calls.contains_key(call_id) {
                            remember_completed_rollout_call(
                                &mut result.completed_tool_calls,
                                call_id,
                                &call,
                                end_ms,
                                code_mode_terminal,
                            );
                        }
                        if yielded_code_mode_cells.is_empty()
                            && !open_calls.values().any(|open| {
                                matches!(
                                    open.class,
                                    RolloutToolClass::CodeModeExec { .. }
                                        | RolloutToolClass::CodeModeWait { .. }
                                        | RolloutToolClass::CodeModeUncorrelatable
                                )
                            })
                        {
                            code_mode_correlation_ambiguous = false;
                        }
                    } else {
                        result.lifecycle_valid = false;
                    }
                } else if item_type.is_some_and(|kind| kind.contains("call")) {
                    // A future/hosted tool lifecycle is not safely correlated
                    // until this parser understands its exact open/close IDs.
                    result.lifecycle_valid = false;
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

            Some("error") => {
                // Top-level provider errors likewise have no exact lifecycle
                // resolution edge. Never retain a stale active or completed
                // positive projection after observing one.
                result.lifecycle_valid = false;
                result.task_complete = false;
                yielded_code_mode_cells.clear();
                code_mode_correlation_ambiguous = false;
            }

            _ => {}
        }
    }

    if result.session_id.is_empty() {
        return None;
    }

    if !replayed_meta.is_empty() && !saw_exact_inherited_parent_meta {
        result.lifecycle_valid = false;
    }
    if (saw_exact_inherited_parent_meta || fork_metadata_attested)
        && (!copied_epoch_chain_initialized
            || copied_epoch_thresholds.is_none()
            || copied_epoch_thresholds
                .as_ref()
                .is_some_and(|thresholds| copied_epoch_index != thresholds.len()))
    {
        // A copied prefix without every exact ancestor/current fork boundary
        // can only describe inherited state, never this child rollout's live
        // lifecycle.
        result.lifecycle_valid = false;
    }

    result.current_task = pending_tasks
        .last()
        .map(|(_, task)| task.clone())
        .unwrap_or_default();
    result.pending_since_ms = open_calls
        .values()
        .map(|call| call.started_at_ms)
        .min()
        .unwrap_or(0);
    result.open_tool_ids = open_calls.keys().cloned().collect();
    result.open_tool_started_at_ms = open_calls
        .iter()
        .map(|(call_id, call)| (call_id.clone(), call.started_at_ms))
        .collect();
    result.open_tool_classes = open_calls
        .iter()
        .map(|(call_id, call)| (call_id.clone(), call.class))
        .collect();
    result.live_code_mode_cells = yielded_code_mode_cells.len();
    result.code_mode_correlation_ambiguous = code_mode_correlation_ambiguous;
    result.awaiting_input_since_ms = open_calls
        .values()
        .filter_map(|call| codex_tool_waits_for_user(&call.name).then_some(call.started_at_ms))
        .min()
        .unwrap_or(0);
    if !result.turn_active || result.pending_since_ms > 0 {
        result.thinking_since_ms = 0;
    } else if result.thinking_since_ms == 0 {
        result.thinking_since_ms = result
            .last_activity
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
    }

    Some(result)
}

fn native_codex_rate_limit_window(slot: &str, value: &Value) -> Option<RateLimitWindow> {
    let fallback_label = match slot {
        "primary" => "Primary",
        "secondary" => "Secondary",
        _ => return None,
    };
    let used_pct = value["used_percent"].as_f64()?;
    if !used_pct.is_finite() || !(0.0..=100.0).contains(&used_pct) {
        return None;
    }
    let window_minutes = value["window_minutes"].as_u64();
    if window_minutes
        .is_some_and(|minutes| minutes == 0 || minutes > MAX_NATIVE_RATE_LIMIT_WINDOW_MINUTES)
    {
        return None;
    }
    let label = native_rate_limit_window_label(window_minutes, fallback_label);
    RateLimitWindow::try_new(
        slot,
        label,
        used_pct,
        value["resets_at"].as_u64(),
        window_minutes,
        RateLimitProvenance::Native,
    )
}

fn native_rate_limit_window_label(window_minutes: Option<u64>, fallback: &str) -> String {
    match window_minutes {
        Some(minutes) if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
        None => fallback.to_string(),
    }
}

fn is_account_level_codex_rate_limit(rate_limits: &Value) -> bool {
    matches!(rate_limits["limit_id"].as_str(), Some("codex") | None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_hooks::state::{HookEventKind, HookProcessIdentity};
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs::File;
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    const SESSION_META: &str = r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"sess-123","cwd":"/home/user/project","cli_version":"0.1.5","timestamp":"2026-03-28T15:00:00Z","git":{"branch":"feature/x"}}}"#;
    const DESKTOP_SESSION_META: &str = r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"desktop-123","cwd":"/home/user/project","originator":"Codex Desktop","cli_version":"0.131.0-alpha.9","timestamp":"2026-03-28T15:00:00Z","git":{"branch":"feature/x"}}}"#;

    fn write_lines(file: &mut tempfile::NamedTempFile, lines: &[&str]) {
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    fn proc_info(pid: u32, ppid: u32, command: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            rss_kb: 0,
            cpu_pct: 0.0,
            command: command.to_string(),
        }
    }

    fn owned_process(pid: u32) -> CodexProcessContext {
        CodexProcessContext {
            pid: Some(pid),
            is_exec: false,
            owns_process_tree: true,
            unknown_process_owner: false,
        }
    }

    fn host_process(pid: u32) -> CodexProcessContext {
        CodexProcessContext {
            pid: Some(pid),
            is_exec: false,
            owns_process_tree: false,
            unknown_process_owner: false,
        }
    }

    fn finalize_unintegrated(
        collector: &CodexCollector,
        session: AgentSession,
        process_info: HashMap<u32, ProcInfo>,
    ) -> AgentSession {
        let shared = super::super::SharedProcessData {
            children_map: process::get_children_map(&process_info),
            process_info,
            ports: HashMap::new(),
            slow_tick: false,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        };
        collector
            .finalize_hook_records(vec![session], Vec::new(), &shared, unix_now_ms())
            .into_iter()
            .next()
            .unwrap()
    }

    fn hook_record(candidate: HookCandidate, now_ms: u64) -> HookCollectorRecord {
        let edge_ms = now_ms.saturating_sub(1_000);
        let mut record = HookCollectorRecord {
            generation_id: "test-generation".to_string(),
            session_id: "hook-session".to_string(),
            cwd: "/home/user/project".to_string(),
            started_at_ms: now_ms.saturating_sub(10_000),
            observed_at_ms: now_ms,
            status_since_ms: edge_ms,
            ended_at_ms: 0,
            exit_observed_at_ms: 0,
            exit_supported_rollout_correlated: false,
            pid: 42,
            process_incarnation: Some("test:codex:42".to_string()),
            process_state: HookProcessState::Live,
            native_process_verified: true,
            supported_release_attested: true,
            effective_hook_engine_attested: true,
            actionable: true,
            owns_resources: true,
            local_config_ambiguous: false,
            interaction_ambiguous: false,
            subagent_set_complete: true,
            turn_id: Some("turn-1".to_string()),
            prompt_observed_at_ms: edge_ms,
            stop_observed_at_ms: edge_ms,
            tool_opened_at_ms: HashMap::new(),
            tool_classes: HashMap::new(),
            subagent_opened_at_ms: HashMap::new(),
            subagent_stopped_at_ms: HashMap::new(),
            candidate,
            observations: Vec::new(),
        };
        match &record.candidate {
            HookCandidate::ToolOpen(ids) => {
                record.tool_opened_at_ms = ids.iter().map(|id| (id.clone(), edge_ms)).collect();
                record.tool_classes = ids
                    .iter()
                    .map(|id| (id.clone(), HookToolClass::Ordinary))
                    .collect();
            }
            HookCandidate::SubagentOpen {
                active,
                provisional,
                root,
            } => {
                record.subagent_opened_at_ms = active
                    .iter()
                    .chain(provisional.iter())
                    .map(|id| (id.clone(), edge_ms))
                    .collect();
                record.subagent_stopped_at_ms =
                    provisional.iter().map(|id| (id.clone(), edge_ms)).collect();
                if let HookRootCandidate::ToolOpen(ids) = root {
                    record.tool_opened_at_ms = ids.iter().map(|id| (id.clone(), edge_ms)).collect();
                    record.tool_classes = ids
                        .iter()
                        .map(|id| (id.clone(), HookToolClass::Ordinary))
                        .collect();
                }
            }
            HookCandidate::Unknown(_)
            | HookCandidate::TurnOpen
            | HookCandidate::TurnStopped
            | HookCandidate::Ended => {}
        }
        record
    }

    fn production_turn_open_hook_state(now_ms: u64) -> HookSessionState {
        let edge_ms = now_ms.saturating_sub(1_000);
        HookSessionState {
            schema_version: 1,
            integration: IntegrationIdentity {
                hook_schema_revision: "test-schema".to_string(),
                helper_digest: "test-helper".to_string(),
                installation_id: "test-installation".to_string(),
                config_digest: "test-config".to_string(),
                complete_hook_set: true,
            },
            generation_id: "test-generation".to_string(),
            session_id: "hook-session".to_string(),
            cwd: "/home/user/project".to_string(),
            process: HookProcessIdentity {
                pid: 42,
                started_at_ms: now_ms.saturating_sub(20_000),
                incarnation: "test:codex:42".to_string(),
                shared_host: false,
                launch_config_ambiguous: false,
            },
            created_at_ms: now_ms.saturating_sub(10_000),
            updated_at_ms: now_ms,
            ended_at_ms: 0,
            first_confirmed_gone_at_ms: 0,
            last_event: HookEventKind::UserPromptSubmit,
            session_start_source: None,
            last_root_event: Some(HookEventKind::UserPromptSubmit),
            last_root_boundary_at_ms: edge_ms,
            active_turn_id: Some("turn-1".to_string()),
            prompt_observed_at_ms: edge_ms,
            stop_turn_id: None,
            stop_hook_active: None,
            stop_observed_at_ms: 0,
            prompt_accepted: true,
            open_tools: BTreeMap::new(),
            tool_opened_at_ms: BTreeMap::new(),
            closed_tools: BTreeSet::new(),
            completed_tool_order: Vec::new(),
            open_child_tools: BTreeMap::new(),
            child_tool_opened_at_ms: BTreeMap::new(),
            closed_child_tools: BTreeMap::new(),
            completed_child_tool_order: Vec::new(),
            open_subagents: BTreeSet::new(),
            subagent_opened_at_ms: BTreeMap::new(),
            provisional_stopped_subagents: BTreeSet::new(),
            subagent_stopped_at_ms: BTreeMap::new(),
            closed_subagents: BTreeSet::new(),
            completed_subagent_order: Vec::new(),
            open_questions: BTreeSet::new(),
            question_opened_at_ms: BTreeMap::new(),
            closed_questions: BTreeSet::new(),
            completed_question_order: Vec::new(),
            question_agents: BTreeMap::new(),
            permission_ambiguity: false,
            permission_observed_at_ms: 0,
            child_permission_ambiguities: BTreeSet::new(),
            child_permission_observed_at_ms: BTreeMap::new(),
            compaction_open: false,
            sticky_fault: None,
            completed_history_truncated: false,
            completed_ingests: Vec::new(),
            samples: Vec::new(),
        }
    }

    fn active_root_rollout(now_ms: u64) -> RolloutLifecycle {
        RolloutLifecycle {
            root_cli_version: plugin::MIN_SUPPORTED_CODEX_VERSION.to_string(),
            turn_active: true,
            active_turn_id: Some("turn-1".to_string()),
            turn_started_at_ms: now_ms.saturating_sub(2_000),
            latest_lifecycle_at_ms: now_ms.saturating_sub(500),
            ..Default::default()
        }
    }

    fn completed_root_rollout(now_ms: u64) -> RolloutLifecycle {
        RolloutLifecycle {
            root_cli_version: plugin::MIN_SUPPORTED_CODEX_VERSION.to_string(),
            task_complete: true,
            completed_turn_id: Some("turn-1".to_string()),
            turn_started_at_ms: now_ms.saturating_sub(2_000),
            latest_lifecycle_at_ms: now_ms.saturating_sub(500),
            task_completed_at_ms: now_ms.saturating_sub(500),
            ..Default::default()
        }
    }

    fn active_child_rollout(
        session_id: &str,
        direct_child: bool,
        now_ms: u64,
    ) -> DescendantRolloutLifecycle {
        DescendantRolloutLifecycle {
            session_id: session_id.to_string(),
            cli_version: plugin::MIN_SUPPORTED_CODEX_VERSION.to_string(),
            direct_child,
            lifecycle_valid: true,
            turn_active: true,
            task_complete: false,
            active_turn_id: Some("child-turn".to_string()),
            completed_turn_id: None,
            turn_started_at_ms: now_ms.saturating_sub(900),
            latest_lifecycle_at_ms: now_ms.saturating_sub(100),
            task_completed_at_ms: 0,
            open_tool_ids: HashSet::new(),
            open_tool_started_at_ms: HashMap::new(),
            open_tool_classes: HashMap::new(),
            live_code_mode_cells: 0,
            code_mode_correlation_ambiguous: false,
        }
    }

    fn terminal_child_rollout(
        session_id: &str,
        direct_child: bool,
        now_ms: u64,
    ) -> DescendantRolloutLifecycle {
        DescendantRolloutLifecycle {
            session_id: session_id.to_string(),
            cli_version: plugin::MIN_SUPPORTED_CODEX_VERSION.to_string(),
            direct_child,
            lifecycle_valid: true,
            turn_active: false,
            task_complete: true,
            active_turn_id: None,
            completed_turn_id: Some("child-turn".to_string()),
            turn_started_at_ms: now_ms.saturating_sub(900),
            latest_lifecycle_at_ms: now_ms.saturating_sub(100),
            task_completed_at_ms: now_ms.saturating_sub(100),
            open_tool_ids: HashSet::new(),
            open_tool_started_at_ms: HashMap::new(),
            open_tool_classes: HashMap::new(),
            live_code_mode_cells: 0,
            code_mode_correlation_ambiguous: false,
        }
    }

    fn hook_shared() -> super::super::SharedProcessData {
        let process_info = HashMap::from([(42, proc_info(42, 1, "/usr/local/bin/codex"))]);
        super::super::SharedProcessData {
            children_map: process::get_children_map(&process_info),
            process_info,
            ports: HashMap::new(),
            slow_tick: false,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        }
    }

    fn prepared_live_hook(
        collector: &CodexCollector,
        now_ms: u64,
        session_id: &str,
        generation_id: &str,
        process_incarnation: &str,
        cli_version: &str,
    ) -> (HookCollectorRecord, AgentSession) {
        let mut record = hook_record(HookCandidate::TurnOpen, now_ms);
        record.session_id = session_id.to_string();
        record.generation_id = generation_id.to_string();
        record.process_incarnation = Some(process_incarnation.to_string());
        let mut rollout = active_root_rollout(now_ms);
        rollout.root_cli_version = cli_version.to_string();
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert(session_id.to_string(), rollout);
        let mut session = collector.hook_placeholder(&record);
        session.pid = record.pid;
        session.version = cli_version.to_string();
        session.model = "gpt-test".to_string();
        session.effort = "high".to_string();
        session.context_percent = 42.0;
        session.total_input_tokens = 123;
        session.total_output_tokens = 45;
        session.total_cache_read = 67;
        session.turn_count = 9;
        session.git_branch = "sensitive-branch-name".to_string();
        session.git_added = 3;
        session.git_modified = 4;
        session.token_history = vec![10, 20, 30];
        session.initial_prompt = "sensitive prompt".to_string();
        session.first_assistant_text = "sensitive response".to_string();
        session.chat_messages.push(ChatMessage {
            role: ChatRole::User,
            text: "sensitive chat".to_string(),
        });
        session.tool_calls.push(ToolCall {
            name: "Read".to_string(),
            arg: "sensitive/path".to_string(),
            duration_ms: 1,
        });
        (record, session)
    }

    fn write_jsonl(path: &Path, lines: &[&str]) {
        let mut file = File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    fn append_jsonl(path: &Path, lines: &[&str]) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
    }

    fn set_modified(path: &Path, when: SystemTime) {
        // Open with write access: on Windows, setting timestamps through a
        // read-only handle fails with PermissionDenied.
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn find_codex_pids_windows_keeps_real_child_over_wrappers() {
        let mut process_info = HashMap::new();
        process_info.insert(
            10,
            proc_info(
                10,
                1,
                r#""C:\Program Files\Git\usr\bin\sh.exe" /c/Users/GK/AppData/Roaming/npm/codex -m gpt-5.5"#,
            ),
        );
        process_info.insert(
            20,
            proc_info(
                20,
                10,
                r#""C:\Program Files\nodejs\node.exe" C:\Users\GK\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.js -m gpt-5.5"#,
            ),
        );
        process_info.insert(
            30,
            proc_info(
                30,
                20,
                r#"C:\Users\GK\AppData\Roaming\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\codex\codex.exe -m gpt-5.5"#,
            ),
        );

        let pids = CodexCollector::find_codex_pids_from_shared(
            &process_info,
            &std::collections::HashSet::new(),
        );

        assert_eq!(pids, vec![(30, false)]);
    }

    #[test]
    fn find_codex_pids_excludes_app_server() {
        let mut process_info = HashMap::new();
        process_info.insert(10, proc_info(10, 1, "codex --resume abc"));
        process_info.insert(
            20,
            proc_info(
                20,
                1,
                "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled",
            ),
        );

        let pids = CodexCollector::find_codex_pids_from_shared(&process_info, &HashSet::new());

        assert_eq!(pids, vec![(10, false)]);
    }

    #[test]
    fn find_codex_pids_keeps_cli_with_app_server_in_path() {
        let mut process_info = HashMap::new();
        process_info.insert(
            10,
            proc_info(10, 1, "codex --cd /home/user/app-server --resume abc"),
        );

        let pids = CodexCollector::find_codex_pids_from_shared(&process_info, &HashSet::new());

        assert_eq!(pids, vec![(10, false)]);
    }

    #[test]
    fn find_codex_desktop_pids_detects_app_servers() {
        let mut process_info = HashMap::new();
        process_info.insert(
            10,
            proc_info(
                10,
                1,
                "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled",
            ),
        );
        process_info.insert(20, proc_info(20, 1, "codex app-server --listen stdio://"));

        let pids =
            CodexCollector::find_codex_desktop_pids_from_shared(&process_info, &HashSet::new());

        assert_eq!(pids, vec![10, 20]);
    }

    #[test]
    fn find_codex_desktop_pids_ignores_mcp_and_non_codex() {
        let mut process_info = HashMap::new();
        process_info.insert(10, proc_info(10, 1, "codex mcp-server"));
        process_info.insert(20, proc_info(20, 1, "node app-server"));
        process_info.insert(30, proc_info(30, 1, "grep codex app-server"));
        process_info.insert(40, proc_info(40, 1, "codex app-server --listen stdio://"));
        let mut mcp = HashSet::new();
        mcp.insert(10);

        let pids = CodexCollector::find_codex_desktop_pids_from_shared(&process_info, &mcp);

        assert_eq!(pids, vec![40]);
    }

    #[test]
    fn desktop_rollout_filter_requires_originator() {
        let mut desktop = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut desktop, &[DESKTOP_SESSION_META]);
        let mut cli = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut cli, &[SESSION_META]);

        assert!(CodexCollector::is_active_desktop_rollout(
            desktop.path(),
            super::super::mcp::ACTIVE_MTIME_SECS
        ));
        assert!(!CodexCollector::is_active_desktop_rollout(
            cli.path(),
            super::super::mcp::ACTIVE_MTIME_SECS
        ));
    }

    #[test]
    fn active_desktop_rollouts_filters_stale_seen_and_cli_files() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("rollout-active.jsonl");
        let stale = temp.path().join("rollout-stale.jsonl");
        let cli = temp.path().join("rollout-cli.jsonl");
        let seen = temp.path().join("rollout-seen.jsonl");
        write_jsonl(&active, &[DESKTOP_SESSION_META]);
        write_jsonl(&stale, &[DESKTOP_SESSION_META]);
        write_jsonl(&cli, &[SESSION_META]);
        write_jsonl(&seen, &[DESKTOP_SESSION_META]);
        set_modified(&stale, SystemTime::now() - Duration::from_secs(31 * 60));

        let mut pid_to_rollouts = HashMap::new();
        pid_to_rollouts.insert(
            99,
            vec![active.clone(), stale, cli, seen.clone(), active.clone()],
        );
        let seen_jsonl = HashSet::from([seen]);

        let rollouts = CodexCollector::active_desktop_rollouts(
            pid_to_rollouts,
            &seen_jsonl,
            &HashSet::new(),
            super::super::mcp::ACTIVE_MTIME_SECS,
        );

        assert_eq!(rollouts, vec![(99, active)]);
    }

    #[test]
    fn recent_desktop_rollouts_include_active_sessions_from_older_day_dirs() {
        let sessions = tempfile::tempdir().unwrap();
        let today = CodexCollector::today_session_dir(sessions.path()).unwrap_or_else(|| {
            let now = chrono::Local::now();
            sessions
                .path()
                .join(now.format("%Y").to_string())
                .join(now.format("%m").to_string())
                .join(now.format("%d").to_string())
        });
        let older = sessions.path().join("2026").join("05").join("20");
        fs::create_dir_all(&today).unwrap();
        fs::create_dir_all(&older).unwrap();
        let active = today.join("rollout-active.jsonl");
        let older_active = older.join("rollout-older-active.jsonl");
        let stale = today.join("rollout-stale.jsonl");
        let cli = today.join("rollout-cli.jsonl");
        write_jsonl(&active, &[DESKTOP_SESSION_META]);
        write_jsonl(&older_active, &[DESKTOP_SESSION_META]);
        write_jsonl(&stale, &[DESKTOP_SESSION_META]);
        write_jsonl(&cli, &[SESSION_META]);
        set_modified(&stale, SystemTime::now() - Duration::from_secs(31 * 60));

        let rollouts = CodexCollector::recent_desktop_rollouts(
            sessions.path(),
            &HashSet::new(),
            &HashSet::new(),
            super::super::mcp::ACTIVE_MTIME_SECS,
        );

        assert_eq!(rollouts.len(), 2);
        assert!(rollouts.contains(&active));
        assert!(rollouts.contains(&older_active));
    }

    #[test]
    fn desktop_pid_by_rollout_path_uses_active_fd_cache_only_for_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("rollout-active.jsonl");
        let stale = temp.path().join("rollout-stale.jsonl");
        write_jsonl(&active, &[DESKTOP_SESSION_META]);
        write_jsonl(&stale, &[DESKTOP_SESSION_META]);
        set_modified(&stale, SystemTime::now() - Duration::from_secs(31 * 60));
        let pid_to_rollouts = HashMap::from([(99, vec![active.clone(), stale])]);

        let by_path = CodexCollector::desktop_pid_by_rollout_path(
            &pid_to_rollouts,
            super::super::mcp::ACTIVE_MTIME_SECS,
        );

        assert_eq!(by_path, HashMap::from([(active, 99)]));
    }

    #[test]
    fn desktop_rollout_metrics_do_not_create_live_status_or_action_owner() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("rollout-active.jsonl");
        let stale = temp.path().join("rollout-stale.jsonl");
        write_jsonl(&active, &[DESKTOP_SESSION_META]);
        write_jsonl(&stale, &[DESKTOP_SESSION_META]);
        set_modified(&stale, SystemTime::now() - Duration::from_secs(31 * 60));

        let mut pid_to_rollouts = HashMap::new();
        pid_to_rollouts.insert(99, vec![active.clone(), stale]);
        let rollouts = CodexCollector::active_desktop_rollouts(
            pid_to_rollouts,
            &HashSet::new(),
            &HashSet::new(),
            super::super::mcp::ACTIVE_MTIME_SECS,
        );

        let collector = CodexCollector::new();
        let mut process_info = HashMap::new();
        process_info.insert(
            99,
            proc_info(
                99,
                1,
                "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled",
            ),
        );
        process_info.insert(100, proc_info(100, 99, "cargo test"));
        let children_map = HashMap::from([(99, vec![100])]);
        let ports = HashMap::from([(100, vec![3000])]);
        let sessions: Vec<AgentSession> = rollouts
            .iter()
            .filter_map(|(pid, path)| {
                collector
                    .load_session_with_rate_limit(
                        host_process(*pid),
                        path,
                        &process_info,
                        &children_map,
                        &ports,
                        &HashSet::new(),
                    )
                    .map(|(session, _)| session)
            })
            .collect();

        assert_eq!(sessions.len(), 1);
        let session = finalize_unintegrated(
            &collector,
            sessions.into_iter().next().unwrap(),
            process_info,
        );
        assert_eq!(session.pid, 99);
        assert_eq!(session.session_id, "desktop-123");
        assert_eq!(session.agent_cli, "codex");
        assert_eq!(session.status, SessionStatus::Unknown);
        assert!(session.action_process_incarnation.is_none());
        assert_eq!(session.mem_mb, 0);
        assert!(session.children.is_empty());
    }

    #[test]
    fn desktop_filesystem_only_rollout_is_unknown_without_fd_owner() {
        let sessions = tempfile::tempdir().unwrap();
        let today = sessions
            .path()
            .join(chrono::Local::now().format("%Y/%m/%d").to_string());
        fs::create_dir_all(&today).unwrap();
        let active = today.join("rollout-active.jsonl");
        write_jsonl(&active, &[DESKTOP_SESSION_META]);

        let mut collector = CodexCollector {
            sessions_dir: sessions.path().to_path_buf(),
            last_rate_limit: None,
            desktop_recent_scanner: DesktopRecentRolloutScanner::new(),
            parse_cache: RefCell::new(CodexParseCache::default()),
            rollout_lifecycle: RefCell::new(HashMap::new()),
            hook_process_states: RefCell::new(HashMap::new()),
            hook_exit_observations: RefCell::new(HashMap::new()),
            hook_process_rollout_bindings: RefCell::new(HashMap::new()),
            hook_live_session_snapshots: RefCell::new(HashMap::new()),
            hook_done_tombstones: RefCell::new(HashMap::new()),
            herdr_status_resolver: RefCell::new(HerdrStatusResolver::default()),
            herdr_working_continuity: RefCell::new(HashMap::new()),
        };
        let mut shared = super::super::SharedProcessData {
            process_info: HashMap::new(),
            children_map: HashMap::new(),
            ports: HashMap::new(),
            slow_tick: false,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        };
        shared.process_info.insert(
            99,
            proc_info(
                99,
                1,
                "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled",
            ),
        );

        let sessions = collector.collect_sessions(&shared);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 0);
        assert_eq!(sessions[0].session_id, "desktop-123");
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        assert_eq!(
            sessions[0].current_tasks,
            vec!["status evidence unavailable".to_string()]
        );
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
    fn root_cli_version_is_not_replaced_by_replayed_session_metadata() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"root","cwd":"/home/user/project","cli_version":"0.145.0","timestamp":"2026-03-28T15:00:00Z"}}"#,
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:01:00Z","payload":{"id":"stale-parent","cwd":"/home/user/other","cli_version":"0.146.0","timestamp":"2026-03-28T15:01:00Z"}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.session_id, "root");
        assert_eq!(result.cwd, "/home/user/project");
        assert_eq!(result.version, "0.145.0");
    }

    #[test]
    fn test_parse_codex_token_count() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"output_tokens":200,"cached_input_tokens":100,"total_tokens":700},"last_token_usage":{"input_tokens":50,"output_tokens":20,"cached_input_tokens":10,"total_tokens":70},"model_context_window":128000}}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.total_input, 400);
        assert_eq!(result.total_output, 200);
        assert_eq!(result.total_cache_read, 100);
        assert_eq!(result.last_context_tokens, 50);
        assert_eq!(result.context_window, 128000);
        assert_eq!(result.token_history.len(), 1);
        assert_eq!(result.token_history[0], 70);
    }

    #[test]
    fn test_parse_codex_context_does_not_double_count_cached_input() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":58140501,"cached_input_tokens":55267712,"output_tokens":114278,"total_tokens":58254779},"last_token_usage":{"input_tokens":151839,"cached_input_tokens":146816,"output_tokens":621,"total_tokens":152460},"model_context_window":258400}}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.last_context_tokens, 151_839);
        assert_eq!(result.context_window, 258_400);
        assert!(result.last_context_tokens < result.context_window);
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
        assert_eq!(rl.five_hour_window_minutes, Some(300));
        assert_eq!(rl.seven_day_pct, Some(14.0));
        assert_eq!(rl.seven_day_window_minutes, Some(10_080));
        assert_eq!(rl.windows.len(), 2);
        assert_eq!(rl.windows[0].id, "primary");
        assert_eq!(rl.windows[0].label, "5h");
        assert_eq!(rl.windows[0].used_pct, 9.0);
        assert_eq!(rl.windows[0].provenance, RateLimitProvenance::Native);
        assert_eq!(rl.windows[1].id, "secondary");
        assert_eq!(rl.windows[1].label, "7d");
        assert_eq!(rl.windows[1].used_pct, 14.0);
        assert_eq!(rl.windows[1].provenance, RateLimitProvenance::Native);
    }

    #[test]
    fn test_parse_codex_free_rate_limit_uses_thirty_day_window() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-06-17T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":1},"last_token_usage":{"input_tokens":1,"output_tokens":1}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":48.0,"window_minutes":43200,"resets_at":1780000000},"secondary":null,"plan_type":"free"}}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        let rl = result.rate_limit.expect("rate_limit should be Some");
        assert_eq!(rl.five_hour_pct, None);
        assert_eq!(rl.seven_day_pct, Some(48.0));
        assert_eq!(rl.seven_day_window_minutes, Some(43_200));
        assert_eq!(rl.windows.len(), 1);
        assert_eq!(rl.windows[0].id, "primary");
        assert_eq!(rl.windows[0].label, "30d");
        assert_eq!(rl.windows[0].used_pct, 48.0);
        assert_eq!(rl.windows[0].window_minutes, Some(43_200));
        assert_eq!(rl.windows[0].provenance, RateLimitProvenance::Native);
    }

    #[test]
    fn test_parse_codex_rate_limit_rejects_unbounded_native_duration() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-06-17T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":1},"last_token_usage":{"input_tokens":1,"output_tokens":1}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":48.0,"window_minutes":525601,"resets_at":1780000000},"secondary":null,"plan_type":"free"}}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert!(result.rate_limit.is_none());
    }

    #[test]
    fn test_parse_codex_rate_limits_ignores_model_specific_limits() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":1},"last_token_usage":{"input_tokens":1,"output_tokens":1}},"rate_limits":{"limit_id":"codex","primary":{"used_percent":25.0,"window_minutes":300,"resets_at":1774686045},"secondary":{"used_percent":4.0,"window_minutes":10080,"resets_at":1775186466}}}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1,"output_tokens":1},"last_token_usage":{"input_tokens":1,"output_tokens":1}},"rate_limits":{"limit_id":"codex_bengalfox","limit_name":"GPT-5.3-Codex-Spark","primary":{"used_percent":0.0,"window_minutes":300,"resets_at":1774686045},"secondary":{"used_percent":0.0,"window_minutes":10080,"resets_at":1775186466}}}}"#,
            ],
        );

        let result = parse_codex_jsonl(file.path()).unwrap();
        let rl = result.rate_limit.expect("account rate_limit should remain");
        assert_eq!(rl.five_hour_pct, Some(25.0));
        assert_eq!(rl.seven_day_pct, Some(4.0));
        assert_eq!(rl.seven_day_window_minutes, Some(10_080));
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
        assert_eq!(result.last_context_tokens, 20);
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
        // Metrics continue after a bad line, but lifecycle promotion fails closed.
        assert_eq!(result.turn_count, 1);
        assert!(!result.lifecycle_valid);
    }

    #[test]
    fn test_parse_codex_turn_active_after_user_message() {
        // Older rollouts may omit task_started, so a user message remains
        // useful activity metadata but cannot prove an exact current turn.
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
        assert!(result.turn_active);
        assert!(!result.lifecycle_valid);
    }

    #[test]
    fn test_parse_codex_agent_message_does_not_close_turn() {
        // Commentary/final text can be followed by tools. Only task_complete
        // or turn_aborted closes a Codex turn.
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
        assert!(result.turn_active);
    }

    #[test]
    fn test_parse_codex_chat_tail_from_user_and_agent_messages() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"user_message","message":"check \u0007auth\u202E sk-proj-secret"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:02:00Z","payload":{"type":"agent_message","message":"Auth guard\u0008 is the failing path."}}"#,
            ],
        );
        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.chat_messages.len(), 2);
        assert_eq!(result.chat_messages[0].role, ChatRole::User);
        assert_eq!(result.chat_messages[0].text, "check auth [REDACTED]");
        assert_eq!(result.chat_messages[1].role, ChatRole::Assistant);
        assert_eq!(
            result.chat_messages[1].text,
            "Auth guard is the failing path."
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
    fn rollout_open_call_needs_hook_correlation_before_executing() {
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
        let parsed = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(parsed.open_tool_ids, HashSet::from(["call_1".to_string()]));
        assert!(parsed.pending_since_ms > 0);

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
                owned_process(42),
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert_eq!(
            session.status_evidence.authority,
            StatusAuthority::Unavailable
        );
        assert_eq!(session.tool_calls.len(), 1);
        assert_eq!(session.tool_calls[0].name, "exec_command");
        assert!(session.tool_calls[0].arg.is_empty());
        assert_eq!(session.tool_calls[0].duration_ms, 0);
        assert_eq!(session.pending_since_ms, 0);
        assert!(!session.awaiting_input);
        assert_eq!(session.thinking_since_ms, 0);
    }

    #[test]
    fn rollout_request_user_input_never_promotes_live_status() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"user_message","message":"ask me first"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[{\"question\":\"Choose a mode\"}]}","call_id":"call_question"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let mut process_info = HashMap::new();
        process_info.insert(42, proc_info(42, 1, "codex"));
        let mut child = proc_info(43, 42, "cargo test");
        child.cpu_pct = 99.0;
        process_info.insert(43, child);
        let children_map = HashMap::from([(42, vec![43])]);

        let (session, _) = collector
            .load_session_with_rate_limit(
                owned_process(42),
                file.path(),
                &process_info,
                &children_map,
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert!(!session.awaiting_input);
        assert_eq!(
            session.status_evidence.authority,
            StatusAuthority::Unavailable
        );
        assert_eq!(session.tool_calls.len(), 1);
        assert_eq!(session.tool_calls[0].name, "request_user_input");
        assert_eq!(session.tool_calls[0].duration_ms, 0);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn rollout_request_user_input_resolution_remains_metadata_only() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[]}","call_id":"call_question"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"function_call_output","call_id":"call_question","output":"{\"answers\":{}}"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let process_info = HashMap::from([(42, proc_info(42, 1, "codex"))]);
        let (session, _) = collector
            .load_session_with_rate_limit(
                owned_process(42),
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert!(!session.awaiting_input);
        assert_eq!(session.pending_since_ms, 0);
        assert_eq!(session.tool_calls[0].duration_ms, 3_000);
    }

    #[test]
    fn test_codex_turn_boundaries_clear_stale_user_input_waits() {
        let boundaries = [
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"task_complete"}}"#,
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"turn_aborted"}}"#,
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"task_started","model_context_window":200000}}"#,
        ];

        for boundary in boundaries {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            write_lines(
                &mut file,
                &[
                    SESSION_META,
                    r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[]}","call_id":"call_question"}}"#,
                    boundary,
                ],
            );

            let result = parse_codex_jsonl(file.path()).unwrap();
            assert_eq!(result.awaiting_input_since_ms, 0);
            assert_eq!(result.pending_since_ms, 0);
            assert!(result.current_task.is_empty());
            assert_eq!(result.tool_calls[0].duration_ms, 3_000);
        }
    }

    #[test]
    fn rollout_exec_end_records_duration_but_stays_open_until_output() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"exec_command_end","call_id":"call_1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(parsed.open_tool_ids, HashSet::from(["call_1".to_string()]));
        assert!(parsed.pending_since_ms > 0);

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
                owned_process(42),
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert_eq!(session.tool_calls.len(), 1);
        assert_eq!(session.tool_calls[0].duration_ms, 3_000);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn rollout_exec_output_records_duration_but_not_live_status() {
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
                owned_process(42),
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert_eq!(session.tool_calls.len(), 1);
        assert_eq!(session.tool_calls[0].duration_ms, 3_000);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn rollout_background_exec_closure_is_metadata_only() {
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
                owned_process(42),
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert_eq!(session.tool_calls.len(), 2);
        assert_eq!(session.tool_calls[0].name, "exec_command");
        assert_eq!(session.tool_calls[0].duration_ms, 6_000);
        assert_eq!(session.tool_calls[1].name, "write_stdin");
        assert_eq!(session.tool_calls[1].duration_ms, 4_000);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn exact_exec_end_closes_a_background_process_after_its_running_output() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"function_call_output","call_id":"call_1","output":"Process running with session ID 12345"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"exec_command_end","call_id":"call_1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
            ],
        );

        let parsed = parse_codex_jsonl(file.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.is_exact_terminal_lifecycle());
        assert!(parsed.open_tool_ids.is_empty());
        assert_eq!(parsed.tool_calls[0].duration_ms, 2_000);
        assert_eq!(
            parsed
                .completed_tool_calls
                .get("call_1")
                .map(|call| call.class),
            Some(RolloutToolClass::Ordinary)
        );
    }

    #[test]
    fn test_codex_turn_boundaries_preserve_running_exec_sessions() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:07Z","payload":{"type":"function_call_output","call_id":"call_1","output":"Chunk ID: abc\nWall time: 1.0000 seconds\nProcess running with session ID 12345\nOutput:\ncompiling"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:08Z","payload":{"type":"task_complete"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"task_started","model_context_window":200000}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:10Z","payload":{"type":"function_call","name":"write_stdin","arguments":"{\"session_id\":12345,\"chars\":\"\"}","call_id":"call_2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:12Z","payload":{"type":"function_call_output","call_id":"call_2","output":"Chunk ID: abc\nWall time: 0.0000 seconds\nProcess exited with code 0\nOutput:\nok"}}"#,
            ],
        );

        let result = parse_codex_jsonl(file.path()).unwrap();
        assert_eq!(result.pending_since_ms, 0);
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].duration_ms, 6_000);
        assert_eq!(result.tool_calls[1].duration_ms, 2_000);
    }

    #[test]
    fn rollout_background_exec_alone_stays_unknown() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"function_call_output","call_id":"call_1","output":"Process running with session ID 12345"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"task_complete"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let process_info = HashMap::from([(42, proc_info(42, 1, "codex exec"))]);
        let mut process_ctx = owned_process(42);
        process_ctx.is_exec = true;
        let (session, _) = collector
            .load_session_with_rate_limit(
                process_ctx,
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert_eq!(session.pending_since_ms, 0);
    }

    #[test]
    fn rollout_task_complete_alone_stays_unknown() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","model_context_window":200000}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"agent_message","message":"Done."}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"task_complete"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let process_info = HashMap::from([(42, proc_info(42, 1, "codex"))]);
        let (session, _) = collector
            .load_session_with_rate_limit(
                owned_process(42),
                file.path(),
                &process_info,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
            )
            .unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.status, SessionStatus::Unknown);
        assert!(!session.awaiting_input);
        assert_eq!(session.pending_since_ms, 0);
        assert_eq!(session.thinking_since_ms, 0);
    }

    #[test]
    fn test_codex_parse_cache_invalidates_on_append_and_stays_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("rollout-first.jsonl");
        write_jsonl(&first, &[SESSION_META]);
        let collector = CodexCollector::new();

        assert!(!collector.parse_rollout_cached(&first).unwrap().turn_active);
        assert!(!collector.parse_rollout_cached(&first).unwrap().turn_active);
        {
            let cache = collector.parse_cache.borrow();
            assert_eq!(cache.entries.len(), 1);
            assert_eq!(cache.clock, 2);
        }

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&first)
            .unwrap();
        file.write_all(
            br#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started"}}
"#,
        )
        .unwrap();
        file.flush().unwrap();
        assert!(collector.parse_rollout_cached(&first).unwrap().turn_active);

        for index in 0..MAX_CODEX_PARSE_CACHE_ENTRIES + 4 {
            let path = temp.path().join(format!("rollout-{index}.jsonl"));
            write_jsonl(&path, &[SESSION_META]);
            collector.parse_rollout_cached(&path).unwrap();
        }
        assert_eq!(
            collector.parse_cache.borrow().entries.len(),
            MAX_CODEX_PARSE_CACHE_ENTRIES
        );
    }

    #[test]
    fn parse_cache_invalidates_same_length_rewrite_with_restored_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-rewritten.jsonl");
        write_jsonl(&path, &[SESSION_META]);
        let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
        let collector = CodexCollector::new();
        assert_eq!(
            collector.parse_rollout_cached(&path).unwrap().session_id,
            "sess-123"
        );

        let replacement = SESSION_META.replace("sess-123", "sess-456");
        assert_eq!(replacement.len(), SESSION_META.len());
        write_jsonl(&path, &[&replacement]);
        set_modified(&path, original_modified);
        assert_eq!(
            collector.parse_rollout_cached(&path).unwrap().session_id,
            "sess-456"
        );
    }

    #[test]
    fn parse_cache_invalidates_equal_metadata_atomic_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-replaced.jsonl");
        let replacement_path = temp.path().join("replacement.jsonl");
        write_jsonl(&path, &[SESSION_META]);
        let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
        let collector = CodexCollector::new();
        assert_eq!(
            collector.parse_rollout_cached(&path).unwrap().session_id,
            "sess-123"
        );

        let replacement = SESSION_META.replace("sess-123", "sess-789");
        write_jsonl(&replacement_path, &[&replacement]);
        set_modified(&replacement_path, original_modified);
        fs::rename(&replacement_path, &path).unwrap();
        assert_eq!(
            collector.parse_rollout_cached(&path).unwrap().session_id,
            "sess-789"
        );
    }

    #[test]
    fn global_codex_config_is_attested_but_project_config_and_lock_are_not() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let codex_home = home.join(".codex");
        let clean_project = home.join("clean");
        let configured_project = home.join("configured");
        let locked_project = home.join("locked");
        fs::create_dir_all(&codex_home).unwrap();
        fs::create_dir_all(&clean_project).unwrap();
        fs::create_dir_all(configured_project.join(".codex")).unwrap();
        fs::create_dir_all(locked_project.join(".codex")).unwrap();
        write_jsonl(&codex_home.join("config.toml"), &["[features]"]);
        write_jsonl(&configured_project.join(".codex/config.toml"), &["[hooks]"]);
        write_jsonl(
            &locked_project.join(".codex/.config.lock.toml"),
            &["version = 1"],
        );

        assert!(!cwd_has_unattested_codex_config(
            clean_project.to_str().unwrap(),
            &codex_home,
        ));
        assert!(cwd_has_unattested_codex_config(
            configured_project.to_str().unwrap(),
            &codex_home,
        ));
        assert!(cwd_has_unattested_codex_config(
            locked_project.to_str().unwrap(),
            &codex_home,
        ));
    }

    #[test]
    fn test_codex_custom_tool_call_lifecycle_is_exact_and_private() {
        let mut open = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut open,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"do not expose this raw freeform input","status":"completed","call_id":"custom_1"}}"#,
            ],
        );
        let result = parse_codex_jsonl(open.path()).unwrap();
        assert_eq!(result.pending_since_ms, 1_774_710_061_000);
        assert_eq!(result.current_task, "exec");
        assert_eq!(result.tool_calls.len(), 1);
        assert!(result.tool_calls[0].arg.is_empty());
        assert_eq!(
            result.open_tool_classes,
            HashMap::from([(
                "custom_1".to_string(),
                RolloutToolClass::CodeModeExec {
                    exec_started_at_ms: 1_774_710_061_000,
                },
            )])
        );

        write_lines(
            &mut open,
            &[
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"custom_tool_call_output","call_id":"custom_1","output":[{"type":"input_text","text":"Script completed\nWall time 3.0 seconds\nOutput:\n"}]}}"#,
            ],
        );
        let result = parse_codex_jsonl(open.path()).unwrap();
        assert!(result.lifecycle_valid);
        assert_eq!(result.pending_since_ms, 0);
        assert!(result.current_task.is_empty());
        assert!(result.open_tool_classes.is_empty());
        assert_eq!(result.tool_calls[0].duration_ms, 3_000);
        assert!(result.turn_active);
    }

    #[test]
    fn code_mode_wait_class_requires_an_exact_yielded_cell_link() {
        let mut linked = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut linked,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private code","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\",\"yield_time_ms\":10000}","call_id":"call_wait"}}"#,
            ],
        );
        let open = parse_codex_jsonl(linked.path()).unwrap();
        assert_eq!(open.open_tool_ids, HashSet::from(["call_wait".to_string()]));
        assert!(open
            .completed_tool_calls
            .get("call_exec")
            .is_some_and(
                |call| !call.code_mode_terminal && call.completed_at_ms == 1_774_710_062_000
            ));
        assert_eq!(
            open.open_tool_classes,
            HashMap::from([(
                "call_wait".to_string(),
                RolloutToolClass::CodeModeWait {
                    exec_started_at_ms: 1_774_710_061_000,
                    exec_yielded_at_ms: 1_774_710_062_000,
                },
            )])
        );

        write_lines(
            &mut linked,
            &[
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call_output","call_id":"call_wait","output":[{"type":"input_text","text":"Script completed\nWall time 3.0 seconds\nOutput:\n"}]}}"#,
            ],
        );
        let closed = parse_codex_jsonl(linked.path()).unwrap();
        assert!(closed.open_tool_ids.is_empty());
        assert!(closed.open_tool_classes.is_empty());
        assert!(closed
            .completed_tool_calls
            .get("call_exec")
            .is_some_and(|call| call.code_mode_terminal
                && call.started_at_ms == 1_774_710_061_000
                && call.completed_at_ms == 1_774_710_064_000));
        assert!(closed
            .completed_tool_calls
            .get("call_wait")
            .is_some_and(|call| !call.code_mode_terminal
                && matches!(call.class, RolloutToolClass::CodeModeWait { .. })));

        let mut unlinked = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut unlinked,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"2\"}","call_id":"call_wait"}}"#,
            ],
        );
        let unlinked = parse_codex_jsonl(unlinked.path()).unwrap();
        assert_eq!(
            unlinked.open_tool_classes,
            HashMap::from([(
                "call_wait".to_string(),
                RolloutToolClass::CodeModeUncorrelatable,
            )])
        );
        assert!(unlinked.code_mode_correlation_ambiguous);
    }

    #[test]
    fn linked_terminal_wait_completes_only_the_exact_origin_exec() {
        let temp = tempfile::tempdir().unwrap();
        let linked = temp.path().join("rollout-linked-waits.jsonl");
        write_jsonl(
            &linked,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call_output","call_id":"call_wait_1","output":"Script running with cell ID 1\nWall time 3.0 seconds\nOutput:\n"}}"#,
            ],
        );
        let running = parse_codex_jsonl(&linked).unwrap();
        assert!(running.lifecycle_valid);
        assert_eq!(running.live_code_mode_cells, 1);
        assert!(running
            .completed_tool_calls
            .get("call_exec")
            .is_some_and(
                |call| !call.code_mode_terminal && call.completed_at_ms == 1_774_710_062_000
            ));

        append_jsonl(
            &linked,
            &[
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait_2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call_output","call_id":"call_wait_2","output":"Script completed\nWall time 5.0 seconds\nOutput:\n"}}"#,
            ],
        );
        let terminal = parse_codex_jsonl(&linked).unwrap();
        assert!(terminal.lifecycle_valid);
        assert_eq!(terminal.live_code_mode_cells, 0);
        assert!(terminal
            .completed_tool_calls
            .get("call_exec")
            .is_some_and(
                |call| call.code_mode_terminal && call.completed_at_ms == 1_774_710_066_000
            ));

        let terminated = temp.path().join("rollout-terminated-wait.jsonl");
        write_jsonl(
            &terminated,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\",\"terminate\":true}","call_id":"call_wait"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call_output","call_id":"call_wait","output":"Script terminated\nWall time 3.0 seconds\nOutput:\n"}}"#,
            ],
        );
        let terminated = parse_codex_jsonl(&terminated).unwrap();
        assert!(terminated.lifecycle_valid);
        assert!(terminated
            .completed_tool_calls
            .get("call_exec")
            .is_some_and(|call| !call.code_mode_terminal));
    }

    #[test]
    fn linked_terminal_wait_reconciles_the_stale_hook_before_current_code_mode_work() {
        let rollout_path = tempfile::NamedTempFile::new().unwrap();
        write_jsonl(
            rollout_path.path(),
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"sess-123","cwd":"/home/user/project","cli_version":"0.146.0","timestamp":"2026-03-28T15:00:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_stale"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_stale","output":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call_output","call_id":"call_wait","output":"Script completed\nWall time 3.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_current"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(rollout_path.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed
            .completed_tool_calls
            .get("call_stale")
            .is_some_and(|call| call.code_mode_terminal));

        let now_ms = 1_774_710_066_000;
        let stale_hook = "exec-01234567-89ab-4def-8abc-0123456789ab".to_string();
        let current_hook = "exec-11111111-2222-4333-8444-555555555555".to_string();
        let mut record = hook_record(
            HookCandidate::ToolOpen(HashSet::from([stale_hook.clone(), current_hook.clone()])),
            now_ms,
        );
        record
            .tool_opened_at_ms
            .insert(stale_hook, 1_774_710_061_050);
        record
            .tool_opened_at_ms
            .insert(current_hook, 1_774_710_065_050);

        let rollout = RolloutLifecycle {
            root_cli_version: parsed.version,
            turn_active: parsed.turn_active,
            task_complete: parsed.task_complete,
            lifecycle_valid: parsed.lifecycle_valid,
            active_turn_id: parsed.active_turn_id,
            completed_turn_id: parsed.completed_turn_id,
            turn_started_at_ms: parsed.turn_started_at_ms,
            latest_lifecycle_at_ms: parsed.latest_lifecycle_at_ms,
            task_completed_at_ms: parsed.task_completed_at_ms,
            open_tool_ids: parsed.open_tool_ids,
            open_tool_started_at_ms: parsed.open_tool_started_at_ms,
            open_tool_classes: parsed.open_tool_classes,
            completed_tool_calls: parsed.completed_tool_calls,
            nested_code_mode_end_at_ms: parsed.nested_code_mode_end_at_ms,
            live_code_mode_cells: parsed.live_code_mode_cells,
            code_mode_correlation_ambiguous: parsed.code_mode_correlation_ambiguous,
            descendants: Vec::new(),
            relevant_process_descendant: false,
        };

        assert_eq!(
            project_hook_status(&record, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            ),
            "rollout completion remains non-authoritative without exact Herdr working evidence"
        );
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: 1_774_710_065_050,
                consecutive_matching: 0,
            }),
            "the linked terminal wait must close only the stale nested hook and retain current work"
        );

        let collector = CodexCollector::new();
        let binding_key = hook_done_key(&record).unwrap();
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert(record.session_id.clone(), rollout);
        let mut pidless = collector.hook_placeholder(&record);
        pidless.version = "0.146.0".to_string();
        let sessions = collector.finalize_hook_records_with_herdr(
            vec![pidless],
            vec![record.clone()],
            &hook_shared(),
            now_ms,
            HashMap::from([(
                (record.session_id.clone(), record.pid),
                HerdrObservation {
                    status: HerdrStatus::Working,
                    observed_at_ms: now_ms,
                    status_since_ms: now_ms - 1_000,
                    consecutive_matching: 2,
                },
            )]),
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Executing);
        assert_eq!(
            sessions[0].status_evidence.authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(
            sessions[0].status_evidence.reason,
            StatusReason::HerdrScreenWorking
        );
        assert!(sessions[0].action_process_incarnation.is_none());
        assert!(!collector
            .hook_process_rollout_bindings
            .borrow()
            .contains_key(&binding_key));
        assert!(!collector
            .hook_done_tombstones
            .borrow()
            .contains_key(&binding_key));
    }

    #[test]
    fn code_mode_cell_ids_and_status_headers_match_the_audited_provider_shape() {
        assert_eq!(bounded_code_mode_cell_id("1").as_deref(), Some("1"));
        assert_eq!(bounded_code_mode_cell_id("g2:1").as_deref(), Some("g2:1"));
        for invalid in ["", "0", "01", "g1:1", "g2:0", "cell1", "g2:01"] {
            assert_eq!(bounded_code_mode_cell_id(invalid), None, "{invalid}");
        }
        assert_eq!(
            code_mode_output_state(&serde_json::json!([{
                "type": "input_text",
                "text": "Script running with cell ID g2:1\nWall time 1.0 seconds\nOutput:\n",
            }])),
            Some(CodeModeOutputState::Running("g2:1".to_string()))
        );
        assert_eq!(
            code_mode_output_state(&serde_json::json!(
                "Script running with cell ID 1\nWall time 0.0 seconds\nOutput:\n"
            )),
            Some(CodeModeOutputState::Running("1".to_string()))
        );
        assert_eq!(
            code_mode_output_state(&serde_json::json!(
                "Script completed\nWall time 0.0 seconds\nOutput:\n"
            )),
            Some(CodeModeOutputState::Terminal)
        );
        for terminal in ["Script completed", "Script failed", "Script terminated"] {
            assert_eq!(
                code_mode_output_state(&serde_json::json!([{
                    "type": "input_text",
                    "text": format!("{terminal}\nWall time 1.0 seconds\nOutput:\n"),
                }, {
                    "type": "input_text",
                    "text": "subsequent provider output is not part of the status frame",
                }])),
                Some(CodeModeOutputState::Terminal)
            );
        }
        for invalid in [
            serde_json::json!([{
                "type": "input_text",
                "text": "Script completed",
            }]),
            serde_json::json!([{
                "type": "input_text",
                "text": "Script completed\nWall time 1.00 seconds\nOutput:\n",
            }]),
            serde_json::json!([{
                "type": "input_text",
                "text": "Script completed\nWall time 01.0 seconds\nOutput:\n",
            }]),
            serde_json::json!([{
                "type": "input_text",
                "text": "Script completed\nWall time 1.0 seconds\nOutput:\nforged",
            }]),
            serde_json::json!([{
                "type": "input_text",
                "text": "Script paused\nWall time 1.0 seconds\nOutput:\n",
            }]),
            serde_json::json!([{
                "type": "input_text",
                "text": "Script completed\nWall time 1.0 seconds\nOutput:\n",
                "extra": true,
            }]),
        ] {
            assert_eq!(code_mode_output_state(&invalid), None, "{invalid}");
        }
    }

    #[test]
    fn code_mode_wait_arguments_match_the_audited_provider_deserializer() {
        for (raw, cell_id, terminate) in [
            (r#"{"cell_id":"1"}"#, "1", false),
            (
                r#"{"cell_id":"g2:1","yield_time_ms":0,"max_tokens":0,"terminate":false}"#,
                "g2:1",
                false,
            ),
            (
                r#"{"cell_id":"1","max_tokens":null,"extra":{"ignored":true}}"#,
                "1",
                false,
            ),
            (r#"{"cell_id":"1","terminate":true}"#, "1", true),
        ] {
            assert_eq!(
                parse_code_mode_wait_args(raw),
                Some(ParsedCodeModeWaitArgs {
                    cell_id: cell_id.to_string(),
                    terminate,
                }),
                "{raw}"
            );
        }

        for raw in [
            r#"{}"#,
            r#"{"cell_id":"1","cell_id":"2"}"#,
            r#"{"cell_id":1}"#,
            r#"{"cell_id":"01"}"#,
            r#"{"cell_id":"1","yield_time_ms":"10"}"#,
            r#"{"cell_id":"1","yield_time_ms":null}"#,
            r#"{"cell_id":"1","max_tokens":-1}"#,
            r#"{"cell_id":"1","terminate":0}"#,
            r#"{"cell_id":"1","terminate":null}"#,
        ] {
            assert_eq!(parse_code_mode_wait_args(raw), None, "{raw}");
        }
    }

    #[test]
    fn terminating_code_mode_wait_never_projects_as_a_resumptive_wait() {
        let mut terminal = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut terminal,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\",\"terminate\":true}","call_id":"call_wait"}}"#,
            ],
        );
        let open = parse_codex_jsonl(terminal.path()).unwrap();
        assert!(open.lifecycle_valid);
        assert_eq!(open.live_code_mode_cells, 1);
        assert_eq!(
            open.open_tool_classes.get("call_wait"),
            Some(&RolloutToolClass::CodeModeUncorrelatable)
        );

        write_lines(
            &mut terminal,
            &[
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call_output","call_id":"call_wait","output":[{"type":"input_text","text":"Script terminated\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
            ],
        );
        let closed = parse_codex_jsonl(terminal.path()).unwrap();
        assert!(closed.lifecycle_valid);
        assert_eq!(closed.live_code_mode_cells, 0);
        assert!(closed.open_tool_classes.is_empty());

        let mut impossible_running = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut impossible_running,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\",\"terminate\":true}","call_id":"call_wait"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call_output","call_id":"call_wait","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
            ],
        );
        let rejected = parse_codex_jsonl(impossible_running.path()).unwrap();
        assert!(!rejected.lifecycle_valid);
        assert!(!rejected.is_exact_terminal_lifecycle());
    }

    #[test]
    fn rollout_open_call_metadata_is_bounded_and_closed_calls_do_not_accumulate() {
        let mut overflow = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut overflow,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ],
        );
        for index in 0..=MAX_OPEN_ROLLOUT_CALLS {
            writeln!(
                overflow,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "function_call",
                        "name": "read_file",
                        "arguments": "{}",
                        "call_id": format!("call_{index}"),
                    }
                })
            )
            .unwrap();
        }
        overflow.flush().unwrap();
        let parsed = parse_codex_jsonl(overflow.path()).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert_eq!(parsed.open_tool_ids.len(), MAX_OPEN_ROLLOUT_CALLS);
        assert_eq!(parsed.open_tool_started_at_ms.len(), MAX_OPEN_ROLLOUT_CALLS);
        assert_eq!(parsed.open_tool_classes.len(), MAX_OPEN_ROLLOUT_CALLS);

        let mut sequential = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut sequential,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ],
        );
        for index in 0..(MAX_OPEN_ROLLOUT_CALLS + 64) {
            let call_id = format!("call_{index}");
            writeln!(
                sequential,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "function_call",
                        "name": "read_file",
                        "arguments": "{}",
                        "call_id": call_id,
                    }
                })
            )
            .unwrap();
            writeln!(
                sequential,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": "ok",
                    }
                })
            )
            .unwrap();
        }
        sequential.flush().unwrap();
        let parsed = parse_codex_jsonl(sequential.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.open_tool_ids.is_empty());
        assert!(parsed.open_tool_started_at_ms.is_empty());
        assert!(parsed.open_tool_classes.is_empty());

        write_lines(
            &mut sequential,
            &[
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"read_file","arguments":"{}","call_id":"call_0"}}"#,
            ],
        );
        let duplicate = parse_codex_jsonl(sequential.path()).unwrap();
        assert!(!duplicate.lifecycle_valid);
        assert!(duplicate.open_tool_ids.is_empty());
    }

    #[test]
    fn rollout_call_history_resets_at_each_clean_turn_boundary() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut file, &[SESSION_META]);
        for index in 0..=MAX_TRACKED_ROLLOUT_CALL_IDS {
            let turn_id = format!("turn-{index}");
            let call_id = format!("call_{index}");
            for value in [
                serde_json::json!({
                    "type": "event_msg",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {"type": "task_started", "turn_id": turn_id},
                }),
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "function_call",
                        "name": "read_file",
                        "arguments": "{}",
                        "call_id": call_id,
                    },
                }),
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": "ok",
                    },
                }),
                serde_json::json!({
                    "type": "event_msg",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {"type": "task_complete", "turn_id": turn_id},
                }),
            ] {
                writeln!(file, "{value}").unwrap();
            }
        }
        file.flush().unwrap();

        let parsed = parse_codex_jsonl(file.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.task_complete);
        assert_eq!(
            parsed.completed_turn_id.as_deref(),
            Some(format!("turn-{MAX_TRACKED_ROLLOUT_CALL_IDS}").as_str())
        );
        assert!(parsed.open_tool_ids.is_empty());
    }

    #[test]
    fn rollout_lifecycle_ids_and_tool_names_are_bounded_before_retention() {
        let mut oversized_turn = tempfile::NamedTempFile::new().unwrap();
        write_lines(&mut oversized_turn, &[SESSION_META]);
        writeln!(
            oversized_turn,
            "{}",
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-03-28T15:01:00Z",
                "payload": {
                    "type": "task_started",
                    "turn_id": "x".repeat(MAX_ROLLOUT_LIFECYCLE_ID_BYTES + 1),
                },
            })
        )
        .unwrap();
        for index in 1..=MAX_TRACKED_CODE_MODE_CELLS {
            let call_id = format!("call_exec_{index}");
            writeln!(
                oversized_turn,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "custom_tool_call",
                        "name": "exec",
                        "input": "private",
                        "call_id": call_id,
                    },
                })
            )
            .unwrap();
            writeln!(
                oversized_turn,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "custom_tool_call_output",
                        "call_id": call_id,
                        "output": [{
                            "type": "input_text",
                            "text": format!(
                                "Script running with cell ID {index}\nWall time 1.0 seconds\nOutput:\n"
                            ),
                        }],
                    },
                })
            )
            .unwrap();
        }
        oversized_turn.flush().unwrap();
        let parsed = parse_codex_jsonl(oversized_turn.path()).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert!(parsed.active_turn_id.is_none());
        assert_eq!(parsed.live_code_mode_cells, 0);

        let mut unsafe_names = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut unsafe_names,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ],
        );
        for (index, name) in [
            "read\u{1b}[31m".to_string(),
            "x".repeat(MAX_ROLLOUT_TOOL_NAME_BYTES + 1),
        ]
        .into_iter()
        .enumerate()
        {
            writeln!(
                unsafe_names,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "function_call",
                        "name": name,
                        "arguments": "{}",
                        "call_id": format!("call_{index}"),
                    },
                })
            )
            .unwrap();
        }
        unsafe_names.flush().unwrap();
        let parsed = parse_codex_jsonl(unsafe_names.path()).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert!(parsed.current_task.is_empty());
        assert!(parsed.tool_calls.is_empty());
        assert!(parsed.open_tool_ids.is_empty());
    }

    #[test]
    fn multiple_or_cross_turn_code_mode_cells_remain_valid_but_uncorrelatable() {
        let mut multiple = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut multiple,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec_1","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec_2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec_2","output":[{"type":"input_text","text":"Script running with cell ID 2\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(multiple.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert_eq!(parsed.live_code_mode_cells, 2);
        assert!(parsed.code_mode_correlation_ambiguous);
        assert_eq!(
            parsed.open_tool_classes.get("call_wait"),
            Some(&RolloutToolClass::CodeModeUncorrelatable)
        );

        let mut cross_turn = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut cross_turn,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(cross_turn.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert_eq!(parsed.live_code_mode_cells, 1);
        assert!(parsed.code_mode_correlation_ambiguous);
        assert_eq!(
            parsed.open_tool_classes.get("call_wait"),
            Some(&RolloutToolClass::CodeModeUncorrelatable)
        );
    }

    #[test]
    fn concurrent_code_mode_waits_and_unknown_outputs_fail_closed() {
        let mut concurrent = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut concurrent,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait_2"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(concurrent.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.code_mode_correlation_ambiguous);
        assert!(parsed
            .open_tool_classes
            .values()
            .all(|class| { *class == RolloutToolClass::CodeModeUncorrelatable }));

        write_lines(
            &mut concurrent,
            &[
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"function_call_output","call_id":"call_wait_2","output":[{"type":"input_text","text":"Script completed\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call_output","call_id":"call_wait_1","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:07Z","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
            ],
        );
        let raced = parse_codex_jsonl(concurrent.path()).unwrap();
        assert!(!raced.lifecycle_valid);
        assert!(!raced.is_exact_terminal_lifecycle());

        let mut resolved_ambiguity = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut resolved_ambiguity,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec_1","output":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec_2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec_2","output":"Script running with cell ID 2\nWall time 1.0 seconds\nOutput:\n"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call_output","call_id":"call_wait_1","output":[{"type":"input_text","text":"Script completed\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:07Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"2\"}","call_id":"call_wait_2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:08Z","payload":{"type":"function_call_output","call_id":"call_wait_2","output":[{"type":"input_text","text":"Script completed\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:09Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec_3"}}"#,
            ],
        );
        let recovered = parse_codex_jsonl(resolved_ambiguity.path()).unwrap();
        assert!(recovered.lifecycle_valid);
        assert_eq!(recovered.live_code_mode_cells, 0);
        assert!(!recovered.code_mode_correlation_ambiguous);
        assert!(matches!(
            recovered.open_tool_classes.get("call_exec_3"),
            Some(RolloutToolClass::CodeModeExec { .. })
        ));

        let mut unknown_output = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut unknown_output,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\"}","call_id":"call_wait"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"function_call_output","call_id":"call_wait","output":"Script paused\nOutput:\n"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(unknown_output.path()).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert_eq!(parsed.live_code_mode_cells, 0);
        assert!(parsed.open_tool_ids.is_empty());
    }

    #[test]
    fn code_mode_cell_provenance_is_bounded_and_blocks_terminal_promotion() {
        let mut overflow = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut overflow,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ],
        );
        for index in 1..=(MAX_TRACKED_CODE_MODE_CELLS + 1) {
            let call_id = format!("call_exec_{index}");
            writeln!(
                overflow,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "custom_tool_call",
                        "name": "exec",
                        "input": "private",
                        "call_id": call_id,
                    }
                })
            )
            .unwrap();
            writeln!(
                overflow,
                "{}",
                serde_json::json!({
                    "type": "response_item",
                    "timestamp": "2026-03-28T15:01:01Z",
                    "payload": {
                        "type": "custom_tool_call_output",
                        "call_id": call_id,
                        "output": [{
                            "type": "input_text",
                            "text": format!(
                                "Script running with cell ID {index}\nWall time 1.0 seconds\nOutput:\n"
                            ),
                        }],
                    }
                })
            )
            .unwrap();
        }
        overflow.flush().unwrap();
        let parsed = parse_codex_jsonl(overflow.path()).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert_eq!(parsed.live_code_mode_cells, MAX_TRACKED_CODE_MODE_CELLS);

        let mut live_at_completion = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut live_at_completion,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_exec"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call_exec","output":[{"type":"input_text","text":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}]}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(live_at_completion.path()).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.task_complete);
        assert_eq!(parsed.live_code_mode_cells, 1);
        assert!(!parsed.is_exact_terminal_lifecycle());
    }

    #[test]
    fn rollout_group_aggregates_subagent_metadata_but_not_status() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("rollout-root.jsonl");
        let child = temp.path().join("rollout-child.jsonl");
        write_jsonl(
            &root,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"root","cwd":"/home/user/project","timestamp":"2026-03-28T15:00:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:00:02Z","payload":{"type":"task_complete"}}"#,
            ],
        );
        write_jsonl(
            &child,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:01:00Z","payload":{"id":"child","parent_thread_id":"root","agent_nickname":"reviewer","cwd":"/home/user/project","timestamp":"2026-03-28T15:01:00Z"}}"#,
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"root","cwd":"/home/user/project","timestamp":"2026-03-28T15:00:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"task_started"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let process_info = HashMap::from([(42, proc_info(42, 1, "codex"))]);
        let loaded = collector.load_cli_session_group(
            owned_process(42),
            &[child.clone(), root.clone()],
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );
        let session = loaded.session.unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.session_id, "root");
        assert_eq!(session.status, SessionStatus::Unknown);
        assert_eq!(session.subagents.len(), 1);
        assert_eq!(session.subagents[0].name, "reviewer");
        assert_eq!(session.subagents[0].status, "working");
        assert_eq!(
            HashSet::from_iter(loaded.owned_paths),
            HashSet::from([root, child])
        );
    }

    #[test]
    fn rollout_group_rejects_duplicate_cycle_and_conflicting_parent_graphs() {
        let temp = tempfile::tempdir().unwrap();
        let make = |name: &str, id: &str, parent: Option<&str>| {
            let path = temp.path().join(name);
            let parent_field = parent
                .map(|parent| format!(",\"parent_thread_id\":\"{parent}\""))
                .unwrap_or_default();
            let line = format!(
                r#"{{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{{"id":"{id}"{parent_field},"cwd":"/tmp","timestamp":"2026-03-28T15:00:00Z"}}}}"#
            );
            write_jsonl(&path, &[&line]);
            (path.clone(), parse_codex_jsonl(&path).unwrap())
        };

        let duplicate = select_codex_rollout_group(vec![
            make("duplicate-a.jsonl", "same", None),
            make("duplicate-b.jsonl", "same", None),
        ])
        .unwrap();
        assert!(!duplicate.lifecycle_valid);

        let cycle = select_codex_rollout_group(vec![
            make("cycle-a.jsonl", "a", Some("b")),
            make("cycle-b.jsonl", "b", Some("a")),
        ])
        .unwrap();
        assert!(!cycle.lifecycle_valid);

        let conflicting = select_codex_rollout_group(vec![
            make("parent-a.jsonl", "parent-a", None),
            make("parent-b.jsonl", "parent-b", None),
            make("child-a.jsonl", "child", Some("parent-a")),
            make("child-b.jsonl", "child", Some("parent-b")),
        ])
        .unwrap();
        assert!(!conflicting.lifecycle_valid);
    }

    #[test]
    fn unparseable_open_descriptor_invalidates_the_selected_rollout_group() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("rollout-root.jsonl");
        let unparseable = temp.path().join("rollout-unparseable.jsonl");
        write_jsonl(
            &root,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"root","cwd":"/home/user/project","cli_version":"0.146.0","timestamp":"2026-03-28T15:00:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:00:01Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ],
        );
        write_jsonl(&unparseable, &["not-json"]);

        let collector = CodexCollector::new();
        let process_info = HashMap::from([(42, proc_info(42, 1, "codex"))]);
        let loaded = collector.load_cli_session_group(
            owned_process(42),
            &[root.clone(), unparseable.clone()],
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );
        let session = loaded.session.unwrap();
        assert_eq!(
            HashSet::from_iter(loaded.owned_paths),
            HashSet::from([root.clone(), unparseable.clone()]),
            "every descriptor owned by the live PID must remain suppressed from fallback scans"
        );
        let lifecycle = collector
            .rollout_lifecycle
            .borrow()
            .get("root")
            .cloned()
            .unwrap();
        assert!(!lifecycle.lifecycle_valid);
        let now_ms = unix_now_ms();
        let mut record = hook_record(HookCandidate::TurnOpen, now_ms);
        record.session_id = "root".to_string();
        let public = collector.finalize_hook_records(
            vec![session],
            vec![record.clone()],
            &hook_shared(),
            now_ms,
        );
        assert_eq!(public[0].status, SessionStatus::Unknown);
        assert_eq!(
            public[0].status_evidence.reason,
            StatusReason::HookIntegrationUnverified
        );
        assert_eq!(
            project_hook_status(&record, Some(&lifecycle), now_ms,),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookEventGap,
            )
        );

        let all_unparseable = collector.load_cli_session_group(
            owned_process(42),
            std::slice::from_ref(&unparseable),
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );
        assert!(all_unparseable.session.is_none());
        assert_eq!(all_unparseable.owned_paths, vec![unparseable]);
    }

    #[test]
    fn multiple_rollout_roots_require_every_nonselected_tree_to_be_exact_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("rollout-selected.jsonl");
        let terminal = temp.path().join("rollout-terminal.jsonl");
        let active = temp.path().join("rollout-active.jsonl");
        write_jsonl(
            &selected,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:03:00Z","payload":{"id":"selected","cwd":"/tmp","cli_version":"0.146.0","timestamp":"2026-03-28T15:03:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:03:01Z","payload":{"type":"task_started","turn_id":"selected-turn"}}"#,
            ],
        );
        write_jsonl(
            &terminal,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"terminal","cwd":"/tmp","cli_version":"0.146.0","timestamp":"2026-03-28T15:00:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:00:01Z","payload":{"type":"task_started","turn_id":"terminal-turn"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:00:02Z","payload":{"type":"task_complete","turn_id":"terminal-turn"}}"#,
            ],
        );
        write_jsonl(
            &active,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:01:00Z","payload":{"id":"active","cwd":"/tmp","cli_version":"0.146.0","timestamp":"2026-03-28T15:01:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"task_started","turn_id":"active-turn"}}"#,
            ],
        );

        let parse = |path: &Path| (path.to_path_buf(), parse_codex_jsonl(path).unwrap());
        let safe = select_codex_rollout_group(vec![parse(&selected), parse(&terminal)]).unwrap();
        assert_eq!(safe.root.session_id, "selected");
        assert!(safe.lifecycle_valid);

        let ambiguous = select_codex_rollout_group(vec![parse(&selected), parse(&active)]).unwrap();
        assert_eq!(ambiguous.root.session_id, "selected");
        assert!(!ambiguous.lifecycle_valid);

        let collector = CodexCollector::new();
        let process_info = HashMap::from([(42, proc_info(42, 1, "codex"))]);
        let loaded = collector.load_cli_session_group(
            owned_process(42),
            &[selected, active],
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );
        let session = loaded.session.unwrap();
        let now_ms = unix_now_ms();
        let mut record = hook_record(HookCandidate::TurnOpen, now_ms);
        record.session_id = "selected".to_string();
        record.cwd = "/tmp".to_string();
        record.turn_id = Some("selected-turn".to_string());
        let public =
            collector.finalize_hook_records(vec![session], vec![record], &hook_shared(), now_ms);
        assert_eq!(public[0].status, SessionStatus::Unknown);
        assert_eq!(
            public[0].status_evidence.reason,
            StatusReason::HookIntegrationUnverified
        );
    }

    #[test]
    fn child_rollout_user_input_never_promotes_live_status() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("rollout-root.jsonl");
        let child = temp.path().join("rollout-child.jsonl");
        write_jsonl(
            &root,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"root","cwd":"/home/user/project","timestamp":"2026-03-28T15:00:00Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:00:01Z","payload":{"type":"task_started"}}"#,
            ],
        );
        write_jsonl(
            &child,
            &[
                r#"{"type":"session_meta","timestamp":"2026-03-28T15:01:00Z","payload":{"id":"child","parent_thread_id":"root","agent_nickname":"reviewer","cwd":"/home/user/project","timestamp":"2026-03-28T15:01:00Z"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"request_user_input","input":"{\"questions\":[]}","call_id":"question_1"}}"#,
            ],
        );

        let collector = CodexCollector::new();
        let process_info = HashMap::from([(42, proc_info(42, 1, "codex"))]);
        let loaded = collector.load_cli_session_group(
            owned_process(42),
            &[root, child],
            &process_info,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );
        let session = loaded.session.unwrap();
        let session = finalize_unintegrated(&collector, session, process_info);

        assert_eq!(session.session_id, "root");
        assert_eq!(session.status, SessionStatus::Unknown);
        assert!(!session.awaiting_input);
    }

    #[test]
    fn root_tool_open_is_unknown_without_effective_permission_attestation() {
        let now_ms = 100_000;
        let tool = hook_record(
            HookCandidate::ToolOpen(HashSet::from(["call-1".to_string()])),
            now_ms,
        );
        let mismatch = RolloutLifecycle {
            root_cli_version: plugin::MIN_SUPPORTED_CODEX_VERSION.to_string(),
            turn_active: true,
            active_turn_id: Some("turn-1".to_string()),
            open_tool_ids: HashSet::from(["call-2".to_string()]),
            ..Default::default()
        };
        assert_eq!(
            project_hook_status(&tool, Some(&mismatch), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            )
        );

        let mut matched = active_root_rollout(now_ms);
        matched.open_tool_ids = HashSet::from(["call-1".to_string()]);
        matched.open_tool_started_at_ms =
            HashMap::from([("call-1".to_string(), now_ms.saturating_sub(1_500))]);
        assert_eq!(
            project_hook_status(&tool, Some(&matched), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            )
        );

        let thinking = hook_record(HookCandidate::TurnOpen, now_ms);
        assert_eq!(
            project_hook_status(&thinking, Some(&matched), now_ms).0,
            SessionStatus::Unknown,
            "an unresolved rollout tool prevents Thinking"
        );
        assert_eq!(
            project_hook_status(&thinking, Some(&active_root_rollout(now_ms)), now_ms,),
            (
                SessionStatus::Thinking,
                StatusAuthority::Heuristic,
                StatusReason::HookTurnOpen,
            )
        );
    }

    #[test]
    fn hook_interaction_ambiguity_is_unknown_even_with_background_work() {
        let now_ms = 100_000;
        let mut record = hook_record(
            HookCandidate::ToolOpen(HashSet::from(["call-1".to_string()])),
            now_ms,
        );
        record.interaction_ambiguous = true;
        let rollout = RolloutLifecycle {
            root_cli_version: plugin::MIN_SUPPORTED_CODEX_VERSION.to_string(),
            turn_active: true,
            open_tool_ids: HashSet::from(["call-1".to_string()]),
            ..Default::default()
        };
        assert_eq!(
            project_hook_status(&record, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            )
        );
    }

    #[test]
    fn hook_projection_precedence_table_is_fail_closed() {
        let now_ms = 100_000;
        let complete = completed_root_rollout(now_ms);
        let cases = [
            (
                HookCandidate::Unknown(StatusReason::HookConfigChanged),
                Some(complete.clone()),
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
            ),
            (
                HookCandidate::SubagentOpen {
                    active: HashSet::from(["child-1".to_string()]),
                    provisional: HashSet::new(),
                    root: HookRootCandidate::TurnOpen,
                },
                None,
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
            ),
            (
                HookCandidate::TurnOpen,
                None,
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
            ),
            (
                HookCandidate::ToolOpen(HashSet::from(["missing".to_string()])),
                Some(complete.clone()),
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
            ),
            (
                HookCandidate::TurnStopped,
                Some(complete),
                SessionStatus::Idle,
                StatusAuthority::Heuristic,
            ),
        ];

        for (candidate, rollout, expected_status, expected_authority) in cases {
            let record = hook_record(candidate, now_ms);
            let (status, authority, _) = project_hook_status(&record, rollout.as_ref(), now_ms);
            assert_eq!(status, expected_status);
            assert_eq!(authority, expected_authority);
            assert!(!matches!(
                status,
                SessionStatus::Waiting | SessionStatus::Error | SessionStatus::RateLimited
            ));
        }
    }

    #[test]
    fn hook_session_start_is_unknown_and_stop_idle_requires_rollout_completion() {
        let now_ms = 100_000;
        assert_eq!(
            project_hook_status(
                &hook_record(HookCandidate::Unknown(StatusReason::HookEventGap), now_ms),
                None,
                now_ms,
            ),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookEventGap,
            )
        );

        let stopped = hook_record(HookCandidate::TurnStopped, now_ms);
        assert_eq!(
            project_hook_status(&stopped, None, now_ms).0,
            SessionStatus::Unknown
        );
        let complete = completed_root_rollout(now_ms);
        assert_eq!(
            project_hook_status(&stopped, Some(&complete), now_ms).0,
            SessionStatus::Idle
        );
    }

    #[test]
    fn aborted_or_stale_rollout_boundary_cannot_promote_stop_to_idle() {
        let now_ms = 100_000;
        let stopped = hook_record(HookCandidate::TurnStopped, now_ms);
        let aborted = RolloutLifecycle {
            lifecycle_valid: false,
            completed_turn_id: Some("turn-1".to_string()),
            task_completed_at_ms: 99_999,
            ..Default::default()
        };
        assert_eq!(
            project_hook_status(&stopped, Some(&aborted), now_ms).0,
            SessionStatus::Unknown
        );

        let stale = RolloutLifecycle {
            task_complete: true,
            completed_turn_id: Some("older-turn".to_string()),
            task_completed_at_ms: 99_999,
            ..Default::default()
        };
        assert_eq!(
            project_hook_status(&stopped, Some(&stale), now_ms).0,
            SessionStatus::Unknown
        );
    }

    #[test]
    fn idle_completion_must_follow_the_exact_stop_and_current_turn() {
        let now_ms = 100_000;
        let stopped = hook_record(HookCandidate::TurnStopped, now_ms);
        let mut before_stop = completed_root_rollout(now_ms);
        before_stop.latest_lifecycle_at_ms = now_ms.saturating_sub(1_500);
        before_stop.task_completed_at_ms = now_ms.saturating_sub(1_500);
        assert_eq!(
            project_hook_status(&stopped, Some(&before_stop), now_ms).0,
            SessionStatus::Unknown
        );

        let mut before_turn = completed_root_rollout(now_ms);
        before_turn.turn_started_at_ms = before_turn.task_completed_at_ms.saturating_add(1);
        assert_eq!(
            project_hook_status(&stopped, Some(&before_turn), now_ms).0,
            SessionStatus::Unknown
        );

        let mut future = completed_root_rollout(now_ms);
        future.latest_lifecycle_at_ms = now_ms.saturating_add(1);
        future.task_completed_at_ms = now_ms.saturating_add(1);
        assert_eq!(
            project_hook_status(&stopped, Some(&future), now_ms).0,
            SessionStatus::Unknown
        );
    }

    #[test]
    fn live_background_process_blocks_thinking_and_idle() {
        let now_ms = 100_000;
        let mut active = active_root_rollout(now_ms);
        active.relevant_process_descendant = true;
        assert_eq!(
            project_hook_status(
                &hook_record(HookCandidate::TurnOpen, now_ms),
                Some(&active),
                now_ms,
            )
            .0,
            SessionStatus::Unknown
        );

        let mut complete = completed_root_rollout(now_ms);
        complete.relevant_process_descendant = true;
        assert_eq!(
            project_hook_status(
                &hook_record(HookCandidate::TurnStopped, now_ms),
                Some(&complete),
                now_ms,
            )
            .0,
            SessionStatus::Unknown
        );
    }

    #[test]
    fn process_descendant_filter_traverses_code_host_and_skips_proven_mcp_subtree() {
        let process_info = HashMap::from([
            (2, proc_info(2, 1, "codex-code-mode-host")),
            (3, proc_info(3, 2, "sleep 300")),
        ]);
        let with_background = HashMap::from([(1, vec![2]), (2, vec![3])]);
        assert!(has_relevant_codex_process_descendant(
            1,
            &process_info,
            &with_background,
            &HashSet::new(),
        ));
        assert!(!has_relevant_codex_process_descendant(
            1,
            &process_info,
            &HashMap::from([(1, vec![2])]),
            &HashSet::new(),
        ));
        assert!(!has_relevant_codex_process_descendant(
            1,
            &process_info,
            &with_background,
            &HashSet::from([2]),
        ));
    }

    #[test]
    fn malformed_and_uncovered_rollout_tools_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let malformed = temp.path().join("rollout-malformed.jsonl");
        write_jsonl(
            &malformed,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                "not-json",
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&malformed).unwrap();
        assert!(!parsed.lifecycle_valid);

        let hosted = temp.path().join("rollout-hosted.jsonl");
        write_jsonl(
            &hosted,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"hosted_search_call","id":"call-1","status":"in_progress"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&hosted).unwrap();
        assert!(!parsed.lifecycle_valid);
    }

    #[test]
    fn known_codex_end_events_require_exact_correlation_and_canonical_output() {
        const AUDITED_META: &str = r#"{"type":"session_meta","timestamp":"2026-03-28T15:00:00Z","payload":{"id":"sess-audited","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-03-28T15:00:00Z"}}"#;
        let temp = tempfile::tempdir().unwrap();
        let half_closed = temp.path().join("rollout-known-end-half-closed.jsonl");
        write_jsonl(
            &half_closed,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call_1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"exec_command_end","call_id":"call_1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&half_closed).unwrap();
        assert!(parsed.lifecycle_valid);
        assert_eq!(parsed.open_tool_ids, HashSet::from(["call_1".to_string()]));
        assert_eq!(parsed.tool_calls[0].duration_ms, 1_000);

        let completed = temp.path().join("rollout-known-end-completed.jsonl");
        write_jsonl(
            &completed,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call_1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"exec_command_end","call_id":"call_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&completed).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.open_tool_ids.is_empty());
        assert_eq!(parsed.tool_calls[0].duration_ms, 1_000);
        let completed = parsed.completed_tool_calls.get("call_1").unwrap();
        assert_eq!(completed.class, RolloutToolClass::Ordinary);
        assert_eq!(completed.completed_at_ms - completed.started_at_ms, 2_000);
        assert!(!completed.code_mode_terminal);

        let output_before_end = temp.path().join("rollout-output-before-known-end.jsonl");
        write_jsonl(
            &output_before_end,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call_1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"exec_command_end","call_id":"call_1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&output_before_end).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert!(parsed.open_tool_ids.is_empty());

        let nested = temp.path().join("rollout-known-nested-end.jsonl");
        write_jsonl(
            &nested,
            &[
                AUDITED_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call_outer"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"web_search_end","call_id":"exec-01234567-89ab-4def-8abc-0123456789ab"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"custom_tool_call_output","call_id":"call_outer","output":"Script completed\nWall time 2.0 seconds\nOutput:\n"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&nested).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.open_tool_ids.is_empty());
        assert!(parsed
            .completed_tool_calls
            .get("call_outer")
            .is_some_and(|call| call.code_mode_terminal));
        assert_eq!(
            parsed.nested_code_mode_end_at_ms,
            HashMap::from([(
                "exec-01234567-89ab-4def-8abc-0123456789ab".to_string(),
                1_774_710_062_000,
            )])
        );

        let unknown = temp.path().join("rollout-unknown-end.jsonl");
        write_jsonl(
            &unknown,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"future_tool_end","call_id":"call-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&unknown).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert_eq!(parsed.open_tool_ids, HashSet::from(["call-1".to_string()]));

        for (name, end) in [
            (
                "mismatched-kind",
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"patch_apply_end","call_id":"call-1"}}"#,
            ),
            (
                "unmatched-id",
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"exec_command_end","call_id":"unmatched"}}"#,
            ),
        ] {
            let path = temp.path().join(format!("rollout-{name}.jsonl"));
            write_jsonl(
                &path,
                &[
                    SESSION_META,
                    r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                    r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
                    end,
                ],
            );
            let parsed = parse_codex_jsonl(&path).unwrap();
            assert!(!parsed.lifecycle_valid, "{name} must fail closed");
            assert_eq!(parsed.open_tool_ids, HashSet::from(["call-1".to_string()]));
        }

        let duplicate = temp.path().join("rollout-duplicate-known-end.jsonl");
        write_jsonl(
            &duplicate,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"exec_command_end","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"exec_command_end","call_id":"call-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&duplicate).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert_eq!(parsed.open_tool_ids, HashSet::from(["call-1".to_string()]));
    }

    #[test]
    fn known_codex_end_kinds_match_the_audited_direct_tool_namespaces() {
        let temp = tempfile::tempdir().unwrap();
        let cases = [
            (
                "exec-command",
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"exec_command_end","call_id":"call-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
            ),
            (
                "apply-patch",
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"apply_patch","input":"private","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"patch_apply_end","call_id":"call-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"custom_tool_call_output","call_id":"call-1","output":"ok"}}"#,
            ),
            (
                "web-run",
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","namespace":"web","name":"run","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"web_search_end","call_id":"call-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
            ),
            (
                "image-generation",
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","namespace":"image_gen","name":"imagegen","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"image_generation_end","call_id":"call-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
            ),
            (
                "mcp-tool",
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","namespace":"mcp__server","name":"tool","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"mcp_tool_call_end","call_id":"call-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
            ),
        ];

        for (name, call, end, output) in cases {
            let path = temp.path().join(format!("rollout-{name}.jsonl"));
            write_jsonl(
                &path,
                &[
                    SESSION_META,
                    r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                    call,
                    end,
                    output,
                ],
            );
            let parsed = parse_codex_jsonl(&path).unwrap();
            assert!(parsed.lifecycle_valid, "{name} should remain exact");
            assert!(
                parsed.open_tool_ids.is_empty(),
                "{name} should close on output"
            );
            assert_eq!(parsed.tool_calls[0].duration_ms, 1_000);
        }
    }

    #[test]
    fn future_and_pre_epoch_rollout_timestamps_are_invalid() {
        let temp = tempfile::tempdir().unwrap();
        let future = temp.path().join("rollout-future.jsonl");
        write_jsonl(
            &future,
            &[
                r#"{"type":"session_meta","timestamp":"2999-01-01T00:00:00Z","payload":{"id":"future","cwd":"/tmp","timestamp":"2999-01-01T00:00:00Z"}}"#,
            ],
        );
        assert!(!parse_codex_jsonl(&future).unwrap().lifecycle_valid);

        let pre_epoch = temp.path().join("rollout-pre-epoch.jsonl");
        write_jsonl(
            &pre_epoch,
            &[
                r#"{"type":"session_meta","timestamp":"1960-01-01T00:00:00Z","payload":{"id":"past","cwd":"/tmp","timestamp":"1960-01-01T00:00:00Z"}}"#,
            ],
        );
        assert!(!parse_codex_jsonl(&pre_epoch).unwrap().lifecycle_valid);
    }

    #[test]
    fn rollout_error_records_permanently_invalidate_positive_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let cases = [
            (
                "stream-error",
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"stream_error","message":"must remain private"}}"#,
            ),
            (
                "event-error",
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"error","message":"must remain private"}}"#,
            ),
            (
                "top-level-error",
                r#"{"type":"error","timestamp":"2026-03-28T15:01:01Z","message":"must remain private"}"#,
            ),
        ];
        for (name, error_record) in cases {
            let path = temp.path().join(format!("rollout-{name}.jsonl"));
            write_jsonl(
                &path,
                &[
                    SESSION_META,
                    r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                    error_record,
                    r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
                ],
            );
            let parsed = parse_codex_jsonl(&path).unwrap();
            assert!(!parsed.lifecycle_valid, "{name} must fail closed");
            assert!(!parsed.task_complete, "{name} cannot retain completion");
            assert!(!parsed.current_task.contains("must remain private"));
            assert!(parsed
                .chat_messages
                .iter()
                .all(|message| !message.text.contains("must remain private")));
        }

        let parsed = parse_codex_jsonl(&temp.path().join("rollout-stream-error.jsonl")).unwrap();
        let rollout = RolloutLifecycle {
            root_cli_version: plugin::MIN_SUPPORTED_CODEX_VERSION.to_string(),
            turn_active: parsed.turn_active,
            task_complete: parsed.task_complete,
            lifecycle_valid: parsed.lifecycle_valid,
            active_turn_id: parsed.active_turn_id,
            completed_turn_id: parsed.completed_turn_id,
            turn_started_at_ms: parsed.turn_started_at_ms,
            latest_lifecycle_at_ms: parsed.latest_lifecycle_at_ms,
            task_completed_at_ms: parsed.task_completed_at_ms,
            open_tool_ids: parsed.open_tool_ids,
            open_tool_started_at_ms: parsed.open_tool_started_at_ms,
            ..Default::default()
        };
        let now_ms = unix_now_ms();
        assert_eq!(
            project_hook_status(
                &hook_record(HookCandidate::TurnOpen, now_ms),
                Some(&rollout),
                now_ms,
            ),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookEventGap,
            ),
            "an errored active rollout cannot remain Thinking"
        );

        let failed_completion = temp.path().join("rollout-failed-completion.jsonl");
        write_jsonl(
            &failed_completion,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:02:00Z","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:02:01Z","payload":{"type":"task_complete","turn_id":"turn-2","error":{"message":"must remain private"}}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&failed_completion).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert!(!parsed.task_complete);
        assert!(parsed
            .chat_messages
            .iter()
            .all(|message| !message.text.contains("must remain private")));
    }

    #[test]
    fn incomplete_rollout_tail_fails_closed_and_recovers_after_append() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write_lines(
            &mut file,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            ],
        );
        file.write_all(
            br#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"agent_message""#,
        )
        .unwrap();
        file.flush().unwrap();

        let partial = parse_codex_jsonl(file.path()).unwrap();
        assert!(!partial.lifecycle_valid);
        let now_ms = unix_now_ms();
        let mut rollout = active_root_rollout(now_ms);
        rollout.lifecycle_valid = partial.lifecycle_valid;
        assert_eq!(
            project_hook_status(
                &hook_record(HookCandidate::TurnOpen, now_ms),
                Some(&rollout),
                now_ms,
            ),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookEventGap,
            )
        );

        file.write_all(br#", "message":"ok"}}"#).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let complete = parse_codex_jsonl(file.path()).unwrap();
        assert!(complete.lifecycle_valid);
        assert!(complete.turn_active);
    }

    #[test]
    fn rollout_lifecycle_preserves_exact_active_and_completed_turn_ids() {
        let temp = tempfile::tempdir().unwrap();
        let active = temp.path().join("rollout-active.jsonl");
        write_jsonl(
            &active,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&active).unwrap();
        assert!(parsed.lifecycle_valid);
        assert_eq!(parsed.active_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(parsed.open_tool_ids, HashSet::from(["call-1".to_string()]));

        let completed = temp.path().join("rollout-completed.jsonl");
        write_jsonl(
            &completed,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"apply_patch","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"task_complete","turn_id":"turn-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&completed).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.task_complete);
        assert_eq!(parsed.completed_turn_id.as_deref(), Some("turn-1"));
        assert!(parsed.active_turn_id.is_none());

        let aborted = temp.path().join("rollout-aborted.jsonl");
        write_jsonl(
            &aborted,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"turn_aborted","turn_id":"turn-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&aborted).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(!parsed.turn_active);
        assert!(!parsed.task_complete);
        assert!(parsed.active_turn_id.is_none());
        assert!(parsed.completed_turn_id.is_none());
    }

    #[test]
    fn exact_abort_allows_only_a_later_clean_turn_to_recover() {
        let temp = tempfile::tempdir().unwrap();
        let recovered = temp.path().join("rollout-abort-recovered.jsonl");
        write_jsonl(
            &recovered,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"turn_aborted","turn_id":"turn-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"custom_tool_call","name":"exec","input":"const result = await tools.exec_command({cmd: \"cargo test\"});\ntext(result.output);","call_id":"call-2"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&recovered).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.turn_active);
        assert_eq!(parsed.active_turn_id.as_deref(), Some("turn-2"));
        assert_eq!(parsed.open_tool_ids, HashSet::from(["call-2".to_string()]));
        assert!(matches!(
            parsed.open_tool_classes.get("call-2"),
            Some(RolloutToolClass::CodeModeExec { .. })
        ));

        for (name, fault) in [
            (
                "mismatched-abort",
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"turn_aborted","turn_id":"other-turn"}}"#,
            ),
            (
                "stream-error",
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"stream_error"}}"#,
            ),
        ] {
            let path = temp.path().join(format!("rollout-{name}.jsonl"));
            write_jsonl(
                &path,
                &[
                    SESSION_META,
                    r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                    fault,
                    r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
                ],
            );
            let parsed = parse_codex_jsonl(&path).unwrap();
            assert!(!parsed.lifecycle_valid, "{name} must remain fail-closed");
        }
    }

    #[test]
    fn abort_preserves_yielded_code_mode_cells_until_exact_termination() {
        let temp = tempfile::tempdir().unwrap();
        let unresolved = temp.path().join("rollout-aborted-yielded-cell.jsonl");
        let prefix = [
            SESSION_META,
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"private","call_id":"call-exec"}}"#,
            r#"{"type":"response_item","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"custom_tool_call_output","call_id":"call-exec","output":"Script running with cell ID 1\nWall time 1.0 seconds\nOutput:\n"}}"#,
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:03Z","payload":{"type":"turn_aborted","turn_id":"turn-1"}}"#,
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
        ];
        write_jsonl(&unresolved, &prefix);

        let parsed = parse_codex_jsonl(&unresolved).unwrap();
        assert!(parsed.lifecycle_valid);
        assert_eq!(parsed.live_code_mode_cells, 1);
        assert!(parsed.code_mode_correlation_ambiguous);
        assert!(!parsed.is_exact_terminal_lifecycle());

        let completed_without_cell = temp.path().join("rollout-aborted-live-cell-complete.jsonl");
        let mut lines = prefix.to_vec();
        lines.push(
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"task_complete","turn_id":"turn-2"}}"#,
        );
        write_jsonl(&completed_without_cell, &lines);
        let parsed = parse_codex_jsonl(&completed_without_cell).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.task_complete);
        assert_eq!(parsed.live_code_mode_cells, 1);
        assert!(parsed.code_mode_correlation_ambiguous);
        assert!(!parsed.is_exact_terminal_lifecycle());

        let terminated = temp.path().join("rollout-aborted-cell-terminated.jsonl");
        let mut lines = prefix.to_vec();
        lines.extend([
            r#"{"type":"response_item","timestamp":"2026-03-28T15:01:05Z","payload":{"type":"function_call","name":"wait","arguments":"{\"cell_id\":\"1\",\"terminate\":true}","call_id":"call-wait"}}"#,
            r#"{"type":"response_item","timestamp":"2026-03-28T15:01:06Z","payload":{"type":"function_call_output","call_id":"call-wait","output":"Script terminated\nWall time 1.0 seconds\nOutput:\n"}}"#,
            r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:07Z","payload":{"type":"task_complete","turn_id":"turn-2"}}"#,
        ]);
        write_jsonl(&terminated, &lines);
        let parsed = parse_codex_jsonl(&terminated).unwrap();
        assert!(parsed.lifecycle_valid);
        assert_eq!(parsed.live_code_mode_cells, 0);
        assert!(!parsed.code_mode_correlation_ambiguous);
        assert!(parsed.is_exact_terminal_lifecycle());
    }

    #[test]
    fn copied_subagent_history_starts_a_distinct_exact_lifecycle_epoch() {
        const CHILD_ID: &str = "019fc46e-53c0-7861-95ad-b379c660fb69";
        const PARENT_ID: &str = "019fc2c5-df6e-78f0-8773-9ab7a5fb6d84";
        const CHILD_META: &str = r#"{"type":"session_meta","timestamp":"2026-08-02T21:43:12.695Z","payload":{"id":"019fc46e-53c0-7861-95ad-b379c660fb69","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T21:43:12.695Z"}}"#;
        const PARENT_META: &str = r#"{"type":"session_meta","timestamp":"2026-08-02T21:43:12.695Z","payload":{"id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T13:59:35.564Z"}}"#;

        let temp = tempfile::tempdir().unwrap();
        let copied = temp.path().join("rollout-copied-child.jsonl");
        write_jsonl(
            &copied,
            &[
                CHILD_META,
                PARENT_META,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.700Z","payload":{"type":"task_started","turn_id":"019fc46d-6979-7711-86f3-823997623d47"}}"#,
                r#"{"type":"response_item","timestamp":"2026-08-02T21:43:12.710Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"parent-call"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.730Z","payload":{"type":"thread_settings_applied"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.736Z","payload":{"type":"task_started","turn_id":"019fc46e-545f-73f3-a201-88b6a34e78f0"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:45:17.200Z","payload":{"type":"turn_aborted","turn_id":"019fc46e-545f-73f3-a201-88b6a34e78f0"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:45:35.627Z","payload":{"type":"task_started","turn_id":"019fc470-828a-79a0-9903-d5f02a8c437d"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:45:40.000Z","payload":{"type":"agent_message","message":"done"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:45:57.017Z","payload":{"type":"task_complete","turn_id":"019fc470-828a-79a0-9903-d5f02a8c437d"}}"#,
            ],
        );

        let parsed = parse_codex_jsonl(&copied).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.task_complete);
        assert!(!parsed.turn_active);
        assert_eq!(parsed.session_id, CHILD_ID);
        assert_eq!(parsed.parent_thread_id.as_deref(), Some(PARENT_ID));
        assert_eq!(
            parsed.completed_turn_id.as_deref(),
            Some("019fc470-828a-79a0-9903-d5f02a8c437d")
        );
        assert_eq!(parsed.turn_count, 1);
        assert!(parsed.open_tool_ids.is_empty());
        assert!(parsed.tool_calls.is_empty());

        let legacy = temp.path().join("rollout-copied-child-0.145.jsonl");
        write_jsonl(
            &legacy,
            &[
                r#"{"type":"session_meta","timestamp":"2026-08-02T21:43:12.695Z","payload":{"id":"019fc46e-53c0-7861-95ad-b379c660fb69","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.145.0","timestamp":"2026-08-02T21:43:12.695Z"}}"#,
                r#"{"type":"session_meta","timestamp":"2026-08-02T21:43:12.695Z","payload":{"id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.145.0","timestamp":"2026-08-02T13:59:35.564Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.700Z","payload":{"type":"task_started","turn_id":"019fc46d-6979-7711-86f3-823997623d47"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.736Z","payload":{"type":"task_started","turn_id":"019fc46e-545f-73f3-a201-88b6a34e78f0"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T21:45:17.200Z","payload":{"type":"task_complete","turn_id":"019fc46e-545f-73f3-a201-88b6a34e78f0"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&legacy).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert!(!parsed.is_exact_terminal_lifecycle());
    }

    #[test]
    fn copied_rollout_metadata_is_bounded_and_excess_fails_closed() {
        fn uuid_v7_at(timestamp_ms: u64, serial: usize) -> String {
            format!(
                "{:08x}-{:04x}-7000-8000-{:012x}",
                timestamp_ms >> 16,
                timestamp_ms & 0xffff,
                serial
            )
        }

        fn rfc3339(timestamp_ms: u64) -> String {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp_ms as i64)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        }

        fn write_copied_chain(path: &Path, replayed_count: usize) {
            let base_ms = 1_774_710_000_000_u64;
            let replayed_ids = (0..replayed_count)
                .map(|index| uuid_v7_at(base_ms + index as u64 * 1_000, index))
                .collect::<Vec<_>>();
            let child_timestamp_ms = base_ms + replayed_count as u64 * 1_000;
            let child_id = uuid_v7_at(child_timestamp_ms, replayed_count);
            let child_timestamp = rfc3339(child_timestamp_ms);
            let mut file = File::create(path).unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "session_meta",
                    "timestamp": child_timestamp,
                    "payload": {
                        "id": child_id,
                        "parent_thread_id": replayed_ids.last().unwrap(),
                        "cwd": "/work",
                        "cli_version": "0.146.0",
                        "timestamp": child_timestamp,
                    }
                })
            )
            .unwrap();

            for (index, replayed_id) in replayed_ids.iter().enumerate() {
                let metadata_timestamp = rfc3339(base_ms + index as u64 * 1_000);
                let mut payload = serde_json::json!({
                    "id": replayed_id,
                    "cwd": "/work",
                    "cli_version": "0.146.0",
                    "timestamp": metadata_timestamp,
                });
                if index > 0 {
                    payload["parent_thread_id"] = Value::String(replayed_ids[index - 1].clone());
                }
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "type": "session_meta",
                        "timestamp": metadata_timestamp,
                        "payload": payload,
                    })
                )
                .unwrap();
            }

            let epoch_thresholds = (1..replayed_count)
                .map(|index| base_ms + index as u64 * 1_000)
                .chain(std::iter::once(child_timestamp_ms));
            for (index, threshold_ms) in epoch_thresholds.enumerate() {
                let turn_id = uuid_v7_at(threshold_ms + 100, replayed_count + index + 1);
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "type": "event_msg",
                        "timestamp": rfc3339(threshold_ms + 50),
                        "payload": { "type": "thread_settings_applied" },
                    })
                )
                .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "type": "event_msg",
                        "timestamp": rfc3339(threshold_ms + 150),
                        "payload": { "type": "task_started", "turn_id": turn_id },
                    })
                )
                .unwrap();
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "type": "event_msg",
                        "timestamp": rfc3339(threshold_ms + 200),
                        "payload": { "type": "task_complete", "turn_id": turn_id },
                    })
                )
                .unwrap();
            }
            file.flush().unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let at_limit = temp.path().join("rollout-copied-meta-at-limit.jsonl");
        write_copied_chain(&at_limit, MAX_COPIED_ROLLOUT_SESSION_META);
        let parsed = parse_codex_jsonl(&at_limit).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.is_exact_terminal_lifecycle());

        let over_limit = temp.path().join("rollout-copied-meta-over-limit.jsonl");
        write_copied_chain(&over_limit, MAX_COPIED_ROLLOUT_SESSION_META + 1);
        let parsed = parse_codex_jsonl(&over_limit).unwrap();
        assert!(!parsed.lifecycle_valid);
        assert!(!parsed.is_exact_terminal_lifecycle());
    }

    #[test]
    fn attested_fork_metadata_delimits_copied_history_without_replayed_parent_meta() {
        let temp = tempfile::tempdir().unwrap();
        let copied = temp.path().join("rollout-attested-fork-child.jsonl");
        write_jsonl(
            &copied,
            &[
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.920Z","payload":{"type":"task_started","turn_id":"019fc46d-6979-7711-86f3-823997623d47"}}"#,
                r#"{"type":"response_item","timestamp":"2026-08-02T22:02:12.925Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"parent-call"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.930Z","payload":{"type":"thread_settings_applied"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.935Z","payload":{"type":"task_started","turn_id":"019fc47f-b8b7-7210-99b2-d8b4d8bc86b7"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.950Z","payload":{"type":"agent_message","message":"child done"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:13.000Z","payload":{"type":"task_complete","turn_id":"019fc47f-b8b7-7210-99b2-d8b4d8bc86b7"}}"#,
            ],
        );

        let parsed = parse_codex_jsonl(&copied).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.is_exact_terminal_lifecycle());
        assert_eq!(
            parsed.completed_turn_id.as_deref(),
            Some("019fc47f-b8b7-7210-99b2-d8b4d8bc86b7")
        );
        assert_eq!(parsed.turn_count, 1);
        assert!(parsed.tool_calls.is_empty());

        let conflicting = temp.path().join("rollout-conflicting-fork-child.jsonl");
        write_jsonl(
            &conflicting,
            &[
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c0-0000-7000-8000-000000000000"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.930Z","payload":{"type":"thread_settings_applied"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.935Z","payload":{"type":"task_started","turn_id":"019fc47f-b8b7-7210-99b2-d8b4d8bc86b7"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:13.000Z","payload":{"type":"task_complete","turn_id":"019fc47f-b8b7-7210-99b2-d8b4d8bc86b7"}}"#,
            ],
        );
        assert!(!parse_codex_jsonl(&conflicting).unwrap().lifecycle_valid);
    }

    #[test]
    fn direct_fork_epoch_attestation_requires_the_complete_exact_0146_shape() {
        const SETTINGS: &str = r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.930Z","payload":{"type":"thread_settings_applied"}}"#;
        const START: &str = r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.935Z","payload":{"type":"task_started","turn_id":"019fc47f-b8b7-7210-99b2-d8b4d8bc86b7"}}"#;
        const COMPLETE: &str = r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:13.000Z","payload":{"type":"task_complete","turn_id":"019fc47f-b8b7-7210-99b2-d8b4d8bc86b7"}}"#;
        let cases = [
            (
                "missing-direct-parent",
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
            ),
            (
                "missing-source-parent",
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
            ),
            (
                "mismatched-fork-parent",
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c0-0000-7000-8000-000000000000","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
            ),
            (
                "non-string-direct-parent",
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":42,"forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
            ),
            (
                "unreviewed-patch-release",
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.1","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
            ),
            (
                "parent-not-older-than-child",
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc480-0000-7000-8000-000000000000","forked_from_id":"019fc480-0000-7000-8000-000000000000","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc480-0000-7000-8000-000000000000"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
            ),
            (
                "malformed-child-uuid",
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"not-a-uuid","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
            ),
        ];

        let temp = tempfile::tempdir().unwrap();
        for (name, metadata) in cases {
            let path = temp.path().join(format!("rollout-{name}.jsonl"));
            write_jsonl(&path, &[metadata, SETTINGS, START, COMPLETE]);
            assert!(
                !parse_codex_jsonl(&path).unwrap().lifecycle_valid,
                "{name} must remain fail-closed"
            );
        }

        let interrupted = temp.path().join("rollout-interrupted-copied-prefix.jsonl");
        write_jsonl(
            &interrupted,
            &[
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T22:02:12.920Z","payload":{"type":"stream_error"}}"#,
                SETTINGS,
                START,
                COMPLETE,
            ],
        );
        assert!(!parse_codex_jsonl(&interrupted).unwrap().lifecycle_valid);

        let incomplete = temp.path().join("rollout-incomplete-direct-fork.jsonl");
        write_jsonl(
            &incomplete,
            &[
                r#"{"type":"session_meta","timestamp":"2026-08-02T22:02:12.914Z","payload":{"id":"019fc47f-b832-75b1-9427-5f024c502791","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","forked_from_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","source":{"subagent":{"thread_spawn":{"parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84"}}},"cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T22:02:12.914Z"}}"#,
                SETTINGS,
            ],
        );
        assert!(!parse_codex_jsonl(&incomplete).unwrap().lifecycle_valid);
    }

    #[test]
    fn copied_subagent_epoch_recovery_requires_the_exact_provider_delimiter() {
        const CHILD_META: &str = r#"{"type":"session_meta","timestamp":"2026-08-02T21:43:12.695Z","payload":{"id":"019fc46e-53c0-7861-95ad-b379c660fb69","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T21:43:12.695Z"}}"#;
        const PARENT_META: &str = r#"{"type":"session_meta","timestamp":"2026-08-02T21:43:12.695Z","payload":{"id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T13:59:35.564Z"}}"#;
        const PARENT_START: &str = r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.700Z","payload":{"type":"task_started","turn_id":"019fc46d-6979-7711-86f3-823997623d47"}}"#;
        const SETTINGS: &str = r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.730Z","payload":{"type":"thread_settings_applied"}}"#;
        const CHILD_START: &str = r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.736Z","payload":{"type":"task_started","turn_id":"019fc46e-545f-73f3-a201-88b6a34e78f0"}}"#;

        let temp = tempfile::tempdir().unwrap();
        let cases: [(&str, &[&str]); 8] = [
            (
                "missing-parent-meta",
                &[CHILD_META, PARENT_START, SETTINGS, CHILD_START],
            ),
            (
                "non-adjacent-delimiter",
                &[
                    CHILD_META,
                    PARENT_META,
                    PARENT_START,
                    SETTINGS,
                    r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.732Z","payload":{"type":"token_count"}}"#,
                    CHILD_START,
                ],
            ),
            (
                "missing-0.146-delimiter",
                &[CHILD_META, PARENT_META, PARENT_START, CHILD_START],
            ),
            (
                "malformed-copied-stream",
                &[
                    CHILD_META,
                    PARENT_META,
                    "NOT JSON",
                    PARENT_START,
                    SETTINGS,
                    CHILD_START,
                ],
            ),
            (
                "missing-child-local-task",
                &[CHILD_META, PARENT_META, PARENT_START, SETTINGS],
            ),
            (
                "failed-first-candidate-cannot-recover",
                &[
                    CHILD_META,
                    PARENT_META,
                    PARENT_START,
                    CHILD_START,
                    SETTINGS,
                    r#"{"type":"event_msg","timestamp":"2026-08-02T21:45:35.627Z","payload":{"type":"task_started","turn_id":"019fc470-828a-79a0-9903-d5f02a8c437d"}}"#,
                ],
            ),
            (
                "equal-uuidv7-threshold",
                &[
                    CHILD_META,
                    PARENT_META,
                    PARENT_START,
                    SETTINGS,
                    r#"{"type":"event_msg","timestamp":"2026-08-02T21:43:12.736Z","payload":{"type":"task_started","turn_id":"019fc46e-53c0-7abc-8def-0123456789ab"}}"#,
                ],
            ),
            (
                "incomplete-ancestor-chain",
                &[
                    CHILD_META,
                    r#"{"type":"session_meta","timestamp":"2026-08-02T21:43:12.695Z","payload":{"id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","parent_thread_id":"019fc2c0-0000-7000-8000-000000000000","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T13:59:35.564Z"}}"#,
                    PARENT_START,
                    SETTINGS,
                    CHILD_START,
                ],
            ),
        ];

        for (name, lines) in cases {
            let path = temp.path().join(format!("rollout-{name}.jsonl"));
            write_jsonl(&path, lines);
            let parsed = parse_codex_jsonl(&path).unwrap();
            assert!(!parsed.lifecycle_valid, "{name} must remain fail-closed");
        }
    }

    #[test]
    fn nested_copied_subagent_epochs_follow_the_exact_metadata_chain_once() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("rollout-nested-copied-child.jsonl");
        write_jsonl(
            &nested,
            &[
                r#"{"type":"session_meta","timestamp":"2026-08-02T18:04:28.102Z","payload":{"id":"019fc3a6-0fd2-77c1-8a24-2a0330b1ab2e","parent_thread_id":"019fc3a5-ecd7-75a3-80c9-f278fa72e5c6","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T18:04:28.007Z"}}"#,
                r#"{"type":"session_meta","timestamp":"2026-08-02T18:04:28.102Z","payload":{"id":"019fc3a5-ecd7-75a3-80c9-f278fa72e5c6","parent_thread_id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T18:04:19.051Z"}}"#,
                r#"{"type":"session_meta","timestamp":"2026-08-02T18:04:28.102Z","payload":{"id":"019fc2c5-df6e-78f0-8773-9ab7a5fb6d84","cwd":"/work","cli_version":"0.146.0","timestamp":"2026-08-02T13:59:35.564Z"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.106Z","payload":{"type":"task_started","turn_id":"019fc3a3-fd5f-7b51-9657-5240320e5f8d"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.115Z","payload":{"type":"thread_settings_applied"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.116Z","payload":{"type":"task_started","turn_id":"019fc3a5-ed5f-7081-bcb2-f12e3e6d791c"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.117Z","payload":{"type":"task_complete","turn_id":"019fc3a5-ed5f-7081-bcb2-f12e3e6d791c"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.117Z","payload":{"type":"thread_settings_applied"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.118Z","payload":{"type":"task_started","turn_id":"019fc3a5-f000-7660-8b33-6d31a98fa13b"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.119Z","payload":{"type":"task_complete","turn_id":"019fc3a5-f000-7660-8b33-6d31a98fa13b"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.119Z","payload":{"type":"thread_settings_applied"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:28.120Z","payload":{"type":"task_started","turn_id":"019fc3a6-1058-7800-a69d-82ec56600245"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:04:29.000Z","payload":{"type":"agent_message","message":"child done"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-08-02T18:08:29.254Z","payload":{"type":"task_complete","turn_id":"019fc3a6-1058-7800-a69d-82ec56600245"}}"#,
            ],
        );

        let parsed = parse_codex_jsonl(&nested).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.is_exact_terminal_lifecycle());
        assert_eq!(
            parsed.completed_turn_id.as_deref(),
            Some("019fc3a6-1058-7800-a69d-82ec56600245")
        );
        assert_eq!(parsed.turn_count, 1);
    }

    #[test]
    fn root_subagent_set_executes_and_interaction_ambiguity_wins() {
        let now_ms = 100_000;
        let mut root = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::TurnOpen,
            },
            now_ms,
        );
        let mut rollout = active_root_rollout(now_ms);
        rollout
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&root, Some(&rollout), now_ms),
            (
                SessionStatus::Executing,
                StatusAuthority::Heuristic,
                StatusReason::HookSubagentActive,
            )
        );

        root.interaction_ambiguous = true;
        assert_eq!(
            project_hook_status(&root, Some(&rollout), now_ms).0,
            SessionStatus::Unknown
        );
    }

    #[test]
    fn positive_hook_status_accepts_compatible_stable_root_cli_versions() {
        let now_ms = 100_000;
        let record = hook_record(HookCandidate::TurnOpen, now_ms);
        for supported in ["0.145.0", "0.146.1", "1.0.0"] {
            let mut rollout = active_root_rollout(now_ms);
            rollout.root_cli_version = supported.to_string();
            assert_eq!(
                project_hook_status(&record, Some(&rollout), now_ms),
                (
                    SessionStatus::Thinking,
                    StatusAuthority::Heuristic,
                    StatusReason::HookTurnOpen,
                ),
                "compatible root cli_version {supported:?} must retain lifecycle evidence"
            );
        }
        for unsupported in ["0.144.999", "0.145.0-beta.1", ""] {
            let mut rollout = active_root_rollout(now_ms);
            rollout.root_cli_version = unsupported.to_string();
            assert_eq!(
                project_hook_status(&record, Some(&rollout), now_ms),
                (
                    SessionStatus::Unknown,
                    StatusAuthority::Unavailable,
                    StatusReason::HookIntegrationUnverified,
                ),
                "unsupported root cli_version {unsupported:?} must fail closed"
            );
        }

        let mut unattested = record;
        unattested.supported_release_attested = false;
        assert_eq!(
            project_hook_status(&unattested, Some(&active_root_rollout(now_ms)), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookIntegrationUnverified,
            ),
            "rollout metadata cannot replace exact process/root correlation"
        );
    }

    #[test]
    fn production_hook_conversion_is_heuristic_unactionable_and_preserves_exact_done_proof() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let mut record = hook_record_from_state(production_turn_open_hook_state(now_ms));
        assert!(matches!(record.candidate, HookCandidate::TurnOpen));
        assert!(
            !record.effective_hook_engine_attested,
            "supported Codex releases cannot attest the effective hook engine for one live thread"
        );

        // The OS probes are isolated from conversion in this unit test. Make
        // every other ownership input exact so the live row can be displayed
        // heuristically while action ownership remains unavailable.
        record.process_state = HookProcessState::Live;
        record.native_process_verified = true;
        record.actionable = true;
        record.owns_resources = true;
        let (_, session) = prepared_live_hook(
            &collector,
            now_ms,
            "hook-session",
            "test-generation",
            "test:codex:42",
            "0.147.3",
        );
        let key = hook_done_key(&record).unwrap();
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, now_ms);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].status, SessionStatus::Thinking);
        assert_eq!(
            live[0].status_evidence.authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(live[0].status_evidence.reason, StatusReason::HookTurnOpen);
        assert_eq!(live[0].version, "0.147.3");
        assert!(live[0].action_process_incarnation.is_none());
        assert!(collector
            .hook_process_rollout_bindings
            .borrow()
            .contains_key(&key));
        assert!(collector
            .hook_live_session_snapshots
            .borrow()
            .contains_key(&key));

        record.process_state = HookProcessState::Gone;
        record.native_process_verified = false;
        let done = collector.finalize_hook_records(Vec::new(), vec![record], &shared, 101_000);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].status, SessionStatus::Done);
        assert_eq!(
            done[0].status_evidence.authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(done[0].status_evidence.reason, StatusReason::ProcessExited);
        assert_eq!(done[0].version, "0.147.3");
        assert!(done[0].action_process_incarnation.is_none());
    }

    #[test]
    fn herdr_working_refines_unique_tool_across_codex_id_namespaces() {
        let now_ms = 100_000;
        let hook_id = "exec-f842a710-1234-4abc-8def-000000000000".to_string();
        let rollout_id = "call_qOSAD01CGeN305j2BKf2D6vV".to_string();
        let mut record = hook_record(
            HookCandidate::ToolOpen(HashSet::from([hook_id.clone()])),
            now_ms,
        );
        record.effective_hook_engine_attested = false;
        let mut rollout = active_root_rollout(now_ms);
        rollout.open_tool_ids = HashSet::from([rollout_id.clone()]);
        rollout
            .open_tool_started_at_ms
            .insert(rollout_id.clone(), now_ms - 1_042);
        rollout.open_tool_classes.insert(
            rollout_id,
            RolloutToolClass::CodeModeExec {
                exec_started_at_ms: now_ms - 1_042,
            },
        );

        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: now_ms - 1_000,
                consecutive_matching: 0,
            })
        );

        record
            .tool_opened_at_ms
            .insert(hook_id.clone(), now_ms - 1_043);
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "a sole stale hook from before the outer exec cannot refine current work"
        );
        record.tool_opened_at_ms.insert(hook_id, now_ms - 1_000);

        record.interaction_ambiguous = true;
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "a permission edge keeps visible work unclassified"
        );

        record.interaction_ambiguous = false;
        record.observed_at_ms = now_ms + 1;
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "Herdr working must not bypass malformed hook state"
        );
    }

    #[test]
    fn herdr_working_refines_a_later_unique_wait_without_reusing_ids() {
        let now_ms = 100_000;
        let hook_id = "exec-f842a710-1234-4abc-8def-000000000000".to_string();
        let rollout_id = "call_wait".to_string();
        let mut record = hook_record(
            HookCandidate::ToolOpen(HashSet::from([hook_id.clone()])),
            now_ms,
        );
        record.status_since_ms = now_ms - 1_500;
        record
            .tool_opened_at_ms
            .insert(hook_id.clone(), now_ms - 1_500);

        let mut rollout = active_root_rollout(now_ms);
        rollout.open_tool_ids = HashSet::from([rollout_id.clone()]);
        rollout
            .open_tool_started_at_ms
            .insert(rollout_id.clone(), now_ms - 500);
        rollout.open_tool_classes.insert(
            rollout_id,
            RolloutToolClass::CodeModeWait {
                exec_started_at_ms: now_ms - 1_500,
                exec_yielded_at_ms: now_ms - 1_000,
            },
        );
        rollout.live_code_mode_cells = 1;
        rollout.latest_lifecycle_at_ms = now_ms - 100;

        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: now_ms - 500,
                consecutive_matching: 0,
            })
        );

        record
            .tool_opened_at_ms
            .insert(hook_id.clone(), now_ms - 1_501);
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "a sole stale hook from before the yielded exec cannot corroborate its later wait"
        );

        record.tool_opened_at_ms.insert(hook_id, now_ms - 999);
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "a hook opened after the yield cannot be reused as the origin of its wait"
        );
    }

    #[test]
    fn herdr_working_reconciles_only_exact_canonical_root_completions() {
        let now_ms = 100_000;
        let stale_id = "call_Stale".to_string();
        let current_id = "call_Current".to_string();
        let mut record = hook_record(
            HookCandidate::ToolOpen(HashSet::from([stale_id.clone(), current_id.clone()])),
            now_ms,
        );
        record
            .tool_opened_at_ms
            .insert(stale_id.clone(), now_ms - 1_000);
        record
            .tool_opened_at_ms
            .insert(current_id.clone(), now_ms - 300);

        let mut rollout = active_root_rollout(now_ms);
        rollout.latest_lifecycle_at_ms = now_ms - 100;
        rollout.open_tool_ids = HashSet::from([current_id.clone()]);
        rollout
            .open_tool_started_at_ms
            .insert(current_id.clone(), now_ms - 250);
        rollout
            .open_tool_classes
            .insert(current_id, RolloutToolClass::Ordinary);
        rollout.completed_tool_calls.insert(
            stale_id,
            CompletedRolloutCall {
                started_at_ms: now_ms - 1_100,
                completed_at_ms: now_ms - 900,
                class: RolloutToolClass::Ordinary,
                code_mode_terminal: false,
            },
        );

        assert_eq!(
            project_hook_status(&record, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            ),
            "rollout completion must not weaken the general hook projection"
        );
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: now_ms - 250,
                consecutive_matching: 0,
            })
        );

        let completion = rollout.completed_tool_calls.get_mut("call_Stale").unwrap();
        completion.started_at_ms = now_ms - 950;
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "a provider completion that starts after the hook edge cannot close it"
        );
    }

    #[test]
    fn herdr_working_reconciles_one_terminal_code_mode_interval_without_guessing() {
        let now_ms = 100_000;
        let stale_hook = "exec-01234567-89ab-4def-8abc-0123456789ab".to_string();
        let current_hook = "exec-11111111-2222-4333-8444-555555555555".to_string();
        let current_outer = "call_Current".to_string();
        let mut record = hook_record(
            HookCandidate::ToolOpen(HashSet::from([stale_hook.clone(), current_hook.clone()])),
            now_ms,
        );
        record
            .tool_opened_at_ms
            .insert(stale_hook.clone(), now_ms - 1_000);
        record.tool_opened_at_ms.insert(current_hook, now_ms - 200);

        let mut rollout = active_root_rollout(now_ms);
        rollout.latest_lifecycle_at_ms = now_ms - 100;
        rollout.open_tool_ids = HashSet::from([current_outer.clone()]);
        rollout
            .open_tool_started_at_ms
            .insert(current_outer.clone(), now_ms - 250);
        rollout.open_tool_classes.insert(
            current_outer,
            RolloutToolClass::CodeModeExec {
                exec_started_at_ms: now_ms - 250,
            },
        );
        rollout.completed_tool_calls.insert(
            "call_Completed".to_string(),
            CompletedRolloutCall {
                started_at_ms: now_ms - 1_100,
                completed_at_ms: now_ms - 900,
                class: RolloutToolClass::CodeModeExec {
                    exec_started_at_ms: now_ms - 1_100,
                },
                code_mode_terminal: true,
            },
        );

        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: now_ms - 200,
                consecutive_matching: 0,
            })
        );

        rollout
            .completed_tool_calls
            .get_mut("call_Completed")
            .unwrap()
            .code_mode_terminal = false;
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "a yielded Code Mode cell is not a terminal nested-tool completion"
        );

        rollout
            .completed_tool_calls
            .get_mut("call_Completed")
            .unwrap()
            .code_mode_terminal = true;
        rollout.nested_code_mode_end_at_ms.insert(
            "exec-aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".to_string(),
            now_ms - 950,
        );
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "an exposed nested end identity cannot close a different stale hook"
        );

        rollout.nested_code_mode_end_at_ms.clear();
        rollout
            .nested_code_mode_end_at_ms
            .insert(stale_hook, now_ms - 950);
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: now_ms - 200,
                consecutive_matching: 0,
            }),
            "an exact nested end identity may reconcile only its matching stale hook"
        );
        rollout.nested_code_mode_end_at_ms.clear();
        rollout.completed_tool_calls.insert(
            "call_Overlap".to_string(),
            CompletedRolloutCall {
                started_at_ms: now_ms - 1_050,
                completed_at_ms: now_ms - 850,
                class: RolloutToolClass::CodeModeExec {
                    exec_started_at_ms: now_ms - 1_050,
                },
                code_mode_terminal: true,
            },
        );
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "overlapping Code Mode intervals have no exact hook bijection"
        );

        rollout.completed_tool_calls.remove("call_Overlap");
        rollout.root_cli_version = "0.147.0".to_string();
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "an unaudited Code Mode release cannot reuse older correlation rules"
        );
    }

    #[test]
    fn herdr_completion_reconciliation_preserves_exact_subagent_projection() {
        let now_ms = 100_000;
        let stale_id = "call_Stale".to_string();
        let mut active = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::ToolOpen(HashSet::from([stale_id.clone()])),
            },
            now_ms,
        );
        active
            .tool_opened_at_ms
            .insert(stale_id.clone(), now_ms - 1_000);
        let mut rollout = active_root_rollout(now_ms);
        rollout.completed_tool_calls.insert(
            stale_id.clone(),
            CompletedRolloutCall {
                started_at_ms: now_ms - 1_100,
                completed_at_ms: now_ms - 900,
                class: RolloutToolClass::Ordinary,
                code_mode_terminal: false,
            },
        );
        rollout
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_herdr_working_status(&active, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: now_ms - 1_000,
                consecutive_matching: 0,
            })
        );

        let mut stopped = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::new(),
                provisional: HashSet::from(["child-1".to_string()]),
                root: HookRootCandidate::ToolOpen(HashSet::from([stale_id.clone()])),
            },
            now_ms,
        );
        stopped.tool_opened_at_ms.insert(stale_id, now_ms - 1_000);
        rollout.descendants = vec![terminal_child_rollout("child-1", true, now_ms)];
        assert_eq!(
            project_herdr_working_status(&stopped, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Thinking,
                status_since_ms: now_ms - 1_000,
                consecutive_matching: 0,
            })
        );

        let current_id = "call_Current".to_string();
        let terminal_tool = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::new(),
                provisional: HashSet::from(["child-1".to_string()]),
                root: HookRootCandidate::ToolOpen(HashSet::from([
                    "call_Stale".to_string(),
                    current_id.clone(),
                ])),
            },
            now_ms,
        );
        rollout.latest_lifecycle_at_ms = now_ms - 100;
        rollout.open_tool_ids = HashSet::from([current_id.clone()]);
        rollout
            .open_tool_started_at_ms
            .insert(current_id.clone(), now_ms - 250);
        rollout
            .open_tool_classes
            .insert(current_id, RolloutToolClass::Ordinary);
        assert_eq!(
            project_herdr_working_status(&terminal_tool, Some(&rollout), now_ms),
            Some(HerdrWorkingProjection {
                status: SessionStatus::Executing,
                status_since_ms: now_ms - 250,
                consecutive_matching: 0,
            })
        );

        rollout.descendants.clear();
        assert_eq!(
            project_herdr_working_status(&terminal_tool, Some(&rollout), now_ms),
            None,
            "missing provisional descendant completion remains unavailable"
        );

        stopped
            .tool_classes
            .insert("unexpected".to_string(), HookToolClass::Ordinary);
        assert_eq!(
            project_herdr_working_status(&stopped, Some(&rollout), now_ms),
            None,
            "malformed hook maps remain unavailable"
        );
    }

    #[test]
    fn herdr_working_rejects_interaction_and_parallel_tool_ambiguity() {
        let now_ms = 100_000;
        let hook_id = "exec-f842a710-1234-4abc-8def-000000000000".to_string();
        let record = hook_record(HookCandidate::ToolOpen(HashSet::from([hook_id])), now_ms);
        let rollout_id = "call_exec".to_string();
        let mut rollout = active_root_rollout(now_ms);
        rollout.open_tool_ids = HashSet::from([rollout_id.clone()]);
        rollout
            .open_tool_started_at_ms
            .insert(rollout_id.clone(), now_ms - 900);
        rollout
            .open_tool_classes
            .insert(rollout_id.clone(), RolloutToolClass::RequestUserInput);
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "an open interaction is never execution"
        );

        rollout.open_tool_classes.insert(
            rollout_id,
            RolloutToolClass::CodeModeExec {
                exec_started_at_ms: now_ms - 900,
            },
        );
        rollout.open_tool_ids.insert("call_parallel".to_string());
        rollout
            .open_tool_started_at_ms
            .insert("call_parallel".to_string(), now_ms - 800);
        rollout
            .open_tool_classes
            .insert("call_parallel".to_string(), RolloutToolClass::Ordinary);
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "parallel provider calls have no cross-namespace bijection"
        );

        rollout.open_tool_ids.remove("call_parallel");
        rollout.open_tool_started_at_ms.remove("call_parallel");
        rollout.open_tool_classes.remove("call_parallel");
        rollout.open_tool_classes.clear();
        assert_eq!(
            project_herdr_working_status(&record, Some(&rollout), now_ms),
            None,
            "missing content-free classes fail closed"
        );
    }

    #[test]
    fn herdr_working_keeps_direct_ids_and_rejects_unreviewed_code_mode_shapes() {
        let now_ms = 100_000;
        let direct_id = "call_direct".to_string();
        let direct = hook_record(
            HookCandidate::ToolOpen(HashSet::from([direct_id.clone()])),
            now_ms,
        );
        let mut rollout = active_root_rollout(now_ms);
        rollout.open_tool_ids = HashSet::from([direct_id.clone()]);
        rollout
            .open_tool_started_at_ms
            .insert(direct_id.clone(), now_ms - 900);
        rollout
            .open_tool_classes
            .insert(direct_id, RolloutToolClass::Ordinary);
        assert!(project_herdr_working_status(&direct, Some(&rollout), now_ms).is_some());

        let code_mode = hook_record(
            HookCandidate::ToolOpen(HashSet::from([
                "exec-f842a710-1234-4abc-8def-000000000000".to_string()
            ])),
            now_ms,
        );
        rollout.root_cli_version = "0.147.0".to_string();
        rollout.open_tool_ids = HashSet::from(["call_outer".to_string()]);
        rollout.open_tool_started_at_ms = HashMap::from([("call_outer".to_string(), now_ms - 900)]);
        rollout.open_tool_classes = HashMap::from([(
            "call_outer".to_string(),
            RolloutToolClass::CodeModeExec {
                exec_started_at_ms: now_ms - 900,
            },
        )]);
        assert_eq!(
            project_herdr_working_status(&code_mode, Some(&rollout), now_ms),
            None,
            "future Code Mode shapes require an explicit source audit"
        );

        rollout.root_cli_version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let malformed = hook_record(
            HookCandidate::ToolOpen(HashSet::from(["exec-not-a-uuid".to_string()])),
            now_ms,
        );
        assert_eq!(
            project_herdr_working_status(&malformed, Some(&rollout), now_ms),
            None,
            "only the exact nested Code Mode UUID shape crosses ID namespaces"
        );
    }

    #[test]
    fn hook_conversion_preserves_content_free_open_tool_classes() {
        let now_ms = 100_000;
        let mut state = production_turn_open_hook_state(now_ms);
        state
            .open_tools
            .insert("exec-f842".to_string(), HookToolClass::Ordinary);
        state
            .tool_opened_at_ms
            .insert("exec-f842".to_string(), now_ms - 500);
        state.last_event = HookEventKind::PreToolUse;
        state.updated_at_ms = now_ms;

        let record = hook_record_from_state(state);
        assert_eq!(
            record.tool_classes,
            HashMap::from([("exec-f842".to_string(), HookToolClass::Ordinary)])
        );
    }

    #[test]
    fn herdr_overlay_maps_attention_idle_and_coarse_work_without_actions() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let record = hook_record(HookCandidate::TurnOpen, now_ms);
        let observation = |status| HerdrObservation {
            status,
            observed_at_ms: now_ms,
            status_since_ms: now_ms - 5_000,
            consecutive_matching: 3,
        };

        let mut blocked = collector.hook_placeholder(&record);
        blocked.pid = record.pid;
        apply_herdr_observation(&mut blocked, observation(HerdrStatus::Blocked), None, None);
        assert_eq!(blocked.status, SessionStatus::Waiting);
        assert_eq!(
            blocked.status_evidence.reason,
            StatusReason::HerdrScreenBlocked
        );
        assert!(blocked.awaiting_input);
        assert!(blocked.action_process_incarnation.is_none());

        let mut idle = collector.hook_placeholder(&record);
        idle.pid = record.pid;
        apply_herdr_observation(&mut idle, observation(HerdrStatus::Idle), None, None);
        assert_eq!(idle.status, SessionStatus::Idle);
        assert_eq!(idle.status_evidence.status_since_ms, now_ms - 5_000);
        assert_eq!(idle.status_evidence.consecutive_matching, 3);
        assert!(!idle.awaiting_input);
        assert!(idle.action_process_incarnation.is_none());

        let mut unrefined = collector.hook_placeholder(&record);
        unrefined.pid = record.pid;
        apply_herdr_observation(
            &mut unrefined,
            observation(HerdrStatus::Working),
            None,
            None,
        );
        assert_eq!(unrefined.status, SessionStatus::Working);
        assert_eq!(
            unrefined.status_evidence.authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(
            unrefined.status_evidence.reason,
            StatusReason::HerdrWorkingUnrefined
        );
        assert!(unrefined.action_process_incarnation.is_none());
    }

    #[test]
    fn herdr_selects_the_current_same_process_generation_and_tracks_its_phase() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let mut current = hook_record(
            HookCandidate::ToolOpen(HashSet::from([
                "exec-f842a710-1234-4abc-8def-000000000000".to_string()
            ])),
            now_ms,
        );
        current.session_id = "current-session".to_string();
        current.generation_id = "current-generation".to_string();
        let mut stale = hook_record(HookCandidate::TurnOpen, now_ms);
        stale.session_id = "stale-session".to_string();
        stale.generation_id = "stale-generation".to_string();
        stale.started_at_ms = now_ms.saturating_sub(20_000);
        let stale_key = hook_done_key(&stale).unwrap();
        let current_key = hook_done_key(&current).unwrap();

        let mut rollout = active_root_rollout(now_ms);
        rollout.open_tool_ids = HashSet::from(["call_qOS".to_string()]);
        rollout
            .open_tool_started_at_ms
            .insert("call_qOS".to_string(), now_ms - 1_042);
        rollout.open_tool_classes.insert(
            "call_qOS".to_string(),
            RolloutToolClass::CodeModeExec {
                exec_started_at_ms: now_ms - 1_042,
            },
        );
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert(current.session_id.clone(), rollout);
        let mut session = collector.hook_placeholder(&current);
        session.pid = 0;
        session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let mut stale_session = collector.hook_placeholder(&stale);
        stale_session.pid = 0;
        stale_session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let working = HerdrObservation {
            status: HerdrStatus::Working,
            observed_at_ms: now_ms,
            status_since_ms: now_ms - 2_000,
            consecutive_matching: 2,
        };
        let observations = HashMap::from([((current.session_id.clone(), current.pid), working)]);

        let executing = collector.finalize_hook_records_with_herdr(
            vec![stale_session, session],
            vec![stale.clone(), current.clone()],
            &hook_shared(),
            now_ms,
            observations.clone(),
        );
        assert_eq!(executing.len(), 1);
        assert_eq!(executing[0].session_id, "current-session");
        assert_eq!(executing[0].pid, 42);
        assert_eq!(executing[0].status, SessionStatus::Executing);
        assert_eq!(
            executing[0].status_evidence.authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(
            executing[0].status_evidence.reason,
            StatusReason::HerdrScreenWorking
        );
        assert_eq!(executing[0].status_evidence.status_since_ms, now_ms - 1_000);
        assert_eq!(executing[0].status_evidence.consecutive_matching, 1);
        assert_eq!(executing[0].pending_since_ms, now_ms - 1_000);
        assert!(executing[0].action_process_incarnation.is_none());
        assert!(!collector
            .hook_process_states
            .borrow()
            .contains_key(&stale_key));
        assert!(!collector
            .hook_process_rollout_bindings
            .borrow()
            .contains_key(&current_key));

        let mut stale_gone = stale.clone();
        let mut thinking = hook_record(HookCandidate::TurnOpen, now_ms + 1_000);
        thinking.session_id = "current-session".to_string();
        thinking.generation_id = "current-generation".to_string();
        let mut stale_next = stale;
        stale_next.observed_at_ms = now_ms + 1_000;
        stale_next.status_since_ms = now_ms;
        let mut thinking_continued = thinking.clone();
        let mut stale_continued = stale_next.clone();
        collector.rollout_lifecycle.borrow_mut().insert(
            thinking.session_id.clone(),
            active_root_rollout(now_ms + 1_000),
        );
        let mut session = collector.hook_placeholder(&thinking);
        session.pid = 0;
        session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let mut stale_session = collector.hook_placeholder(&stale_next);
        stale_session.pid = 0;
        stale_session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let observations = HashMap::from([(
            (thinking.session_id.clone(), thinking.pid),
            HerdrObservation {
                observed_at_ms: now_ms + 1_000,
                ..working
            },
        )]);
        let thinking = collector.finalize_hook_records_with_herdr(
            vec![stale_session, session],
            vec![stale_next, thinking],
            &hook_shared(),
            now_ms + 1_000,
            observations,
        );
        assert_eq!(thinking.len(), 1);
        assert_eq!(thinking[0].status, SessionStatus::Thinking);
        assert_eq!(
            thinking[0].status_evidence.reason,
            StatusReason::HerdrScreenWorking
        );
        assert_eq!(thinking[0].status_evidence.status_since_ms, now_ms);
        assert_eq!(thinking[0].status_evidence.consecutive_matching, 1);
        assert_eq!(thinking[0].thinking_since_ms, now_ms);
        assert!(thinking[0].action_process_incarnation.is_none());

        thinking_continued.observed_at_ms = now_ms + 2_000;
        stale_continued.observed_at_ms = now_ms + 2_000;
        collector.rollout_lifecycle.borrow_mut().insert(
            thinking_continued.session_id.clone(),
            active_root_rollout(now_ms + 2_000),
        );
        let mut session = collector.hook_placeholder(&thinking_continued);
        session.pid = 0;
        session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let mut stale_session = collector.hook_placeholder(&stale_continued);
        stale_session.pid = 0;
        stale_session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let observations = HashMap::from([(
            (
                thinking_continued.session_id.clone(),
                thinking_continued.pid,
            ),
            HerdrObservation {
                observed_at_ms: now_ms + 2_000,
                consecutive_matching: 4,
                ..working
            },
        )]);
        let continued = collector.finalize_hook_records_with_herdr(
            vec![stale_session, session],
            vec![stale_continued, thinking_continued],
            &hook_shared(),
            now_ms + 2_000,
            observations,
        );
        assert_eq!(continued.len(), 1);
        assert_eq!(continued[0].status, SessionStatus::Thinking);
        assert_eq!(continued[0].status_evidence.status_since_ms, now_ms);
        assert_eq!(continued[0].status_evidence.consecutive_matching, 2);

        stale_gone.candidate = HookCandidate::Ended;
        stale_gone.process_state = HookProcessState::Gone;
        stale_gone.native_process_verified = false;
        stale_gone.observed_at_ms = now_ms + 3_000;
        stale_gone.ended_at_ms = now_ms + 2_500;
        stale_gone.exit_observed_at_ms = now_ms + 2_500;
        stale_gone.exit_supported_rollout_correlated = false;
        let exited = collector.finalize_hook_records_with_herdr(
            Vec::new(),
            vec![stale_gone],
            &hook_shared(),
            now_ms + 3_000,
            HashMap::new(),
        );
        assert!(exited.is_empty());
        assert!(!collector
            .hook_exit_observations
            .borrow()
            .contains_key(&stale_key));
        assert!(!collector
            .hook_done_tombstones
            .borrow()
            .contains_key(&stale_key));
    }

    #[test]
    fn herdr_working_preserves_fault_evidence_while_reporting_coarse_work() {
        let now_ms = 100_000;
        for (index, reason) in [StatusReason::HookStateMalformed, StatusReason::HookEventGap]
            .into_iter()
            .enumerate()
        {
            let collector = CodexCollector::new();
            let tool_ids = HashSet::from(["tool-1".to_string()]);
            let mut record = hook_record(HookCandidate::Unknown(reason), now_ms);
            record.session_id = format!("unavailable-session-{index}");
            record.generation_id = format!("unavailable-generation-{index}");

            let mut rollout = active_root_rollout(now_ms);
            rollout.open_tool_ids = tool_ids;
            rollout
                .open_tool_started_at_ms
                .insert("tool-1".to_string(), now_ms - 900);
            collector
                .rollout_lifecycle
                .borrow_mut()
                .insert(record.session_id.clone(), rollout);

            let mut session = collector.hook_placeholder(&record);
            session.pid = record.pid;
            session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
            let observations = HashMap::from([(
                (record.session_id.clone(), record.pid),
                HerdrObservation {
                    status: HerdrStatus::Working,
                    observed_at_ms: now_ms,
                    status_since_ms: now_ms - 2_000,
                    consecutive_matching: 2,
                },
            )]);

            let sessions = collector.finalize_hook_records_with_herdr(
                vec![session],
                vec![record],
                &hook_shared(),
                now_ms,
                observations,
            );

            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].status, SessionStatus::Working);
            assert_eq!(
                sessions[0].status_evidence.authority,
                StatusAuthority::Heuristic
            );
            assert_eq!(
                sessions[0].status_evidence.reason,
                StatusReason::HerdrWorkingUnrefined
            );
            assert!(sessions[0]
                .status_evidence
                .observations
                .iter()
                .any(|observation| observation.reason == reason));
            assert!(sessions[0].action_process_incarnation.is_none());
        }
    }

    #[test]
    fn herdr_working_reports_coarse_activity_without_a_current_hook_record() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let record = hook_record(HookCandidate::TurnOpen, now_ms);
        let mut rollout_only = collector.hook_placeholder(&record);
        rollout_only.pid = record.pid;
        rollout_only.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let observations = HashMap::from([(
            (record.session_id.clone(), record.pid),
            HerdrObservation {
                status: HerdrStatus::Working,
                observed_at_ms: now_ms,
                status_since_ms: now_ms - 2_000,
                consecutive_matching: 2,
            },
        )]);

        let sessions = collector.finalize_hook_records_with_herdr(
            vec![rollout_only],
            Vec::new(),
            &hook_shared(),
            now_ms,
            observations,
        );

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Working);
        assert_eq!(
            sessions[0].status_evidence.authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(
            sessions[0].status_evidence.reason,
            StatusReason::HerdrWorkingUnrefined
        );
        assert!(sessions[0]
            .status_evidence
            .observations
            .iter()
            .any(|sample| {
                sample.reason == StatusReason::HookIntegrationUnverified
                    && sample.authority == StatusAuthority::Unavailable
            }));
        assert_eq!(sessions[0].current_tasks, vec!["working"]);
        assert!(sessions[0].action_process_incarnation.is_none());
    }

    #[test]
    fn pidless_rollout_recovers_herdr_status_without_actions_or_exit_proof() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let tool_ids = HashSet::from(["tool-1".to_string()]);
        let record = hook_record(HookCandidate::ToolOpen(tool_ids.clone()), now_ms);
        let binding_key = hook_done_key(&record).unwrap();
        let mut rollout = active_root_rollout(now_ms);
        rollout.open_tool_ids = tool_ids;
        rollout
            .open_tool_started_at_ms
            .insert("tool-1".to_string(), now_ms - 900);
        rollout
            .open_tool_classes
            .insert("tool-1".to_string(), RolloutToolClass::Ordinary);
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert(record.session_id.clone(), rollout);
        let mut pidless = collector.hook_placeholder(&record);
        pidless.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let observations = HashMap::from([(
            (record.session_id.clone(), record.pid),
            HerdrObservation {
                status: HerdrStatus::Working,
                observed_at_ms: now_ms,
                status_since_ms: now_ms - 2_000,
                consecutive_matching: 2,
            },
        )]);

        let sessions = collector.finalize_hook_records_with_herdr(
            vec![pidless],
            vec![record],
            &hook_shared(),
            now_ms,
            observations,
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 42);
        assert_eq!(sessions[0].status, SessionStatus::Executing);
        assert!(sessions[0].action_process_incarnation.is_none());
        assert!(!collector
            .hook_process_rollout_bindings
            .borrow()
            .contains_key(&binding_key));
    }

    #[test]
    fn current_generation_suppression_is_scoped_to_the_stale_process_claim() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let mut current = hook_record(HookCandidate::TurnOpen, now_ms);
        current.session_id = "current-session".to_string();
        current.generation_id = "current-generation".to_string();
        let mut stale = hook_record(HookCandidate::TurnOpen, now_ms);
        stale.session_id = "reused-session-id".to_string();
        stale.generation_id = "stale-generation".to_string();

        let current_session = collector.hook_placeholder(&current);
        let stale_pidless = collector.hook_placeholder(&stale);
        let mut independent = collector.hook_placeholder(&stale);
        independent.pid = 99;
        let mut sessions = vec![stale_pidless, independent, current_session];
        let mut records = vec![stale, current.clone()];
        let observations = HashMap::from([(
            (current.session_id.clone(), current.pid),
            HerdrObservation {
                status: HerdrStatus::Working,
                observed_at_ms: now_ms,
                status_since_ms: now_ms,
                consecutive_matching: 1,
            },
        )]);
        let shared = hook_shared();

        reconcile_current_hook_generations(
            &mut records,
            &mut sessions,
            &HashMap::new(),
            &observations,
            &shared.process_info,
            &HashSet::from([current.pid]),
        );

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "current-session");
        assert!(sessions
            .iter()
            .any(|session| session.session_id == "current-session" && session.pid == 0));
        assert!(sessions
            .iter()
            .any(|session| session.session_id == "reused-session-id" && session.pid == 99));
        assert!(!sessions
            .iter()
            .any(|session| session.session_id == "reused-session-id" && session.pid == 0));
    }

    #[test]
    fn rollout_selects_only_the_current_generation_and_only_it_can_transition_to_done() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let mut current = hook_record(HookCandidate::TurnOpen, now_ms);
        current.session_id = "current-session".to_string();
        current.generation_id = "current-generation".to_string();
        current.effective_hook_engine_attested = false;
        let mut stale = hook_record(HookCandidate::TurnOpen, now_ms);
        stale.session_id = "stale-session".to_string();
        stale.generation_id = "stale-generation".to_string();
        stale.started_at_ms = now_ms - 20_000;
        stale.effective_hook_engine_attested = false;
        let current_key = hook_done_key(&current).unwrap();
        let stale_key = hook_done_key(&stale).unwrap();

        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert(current.session_id.clone(), active_root_rollout(now_ms));
        let mut current_session = collector.hook_placeholder(&current);
        current_session.pid = current.pid;
        current_session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        current_session.mem_mb = 777;
        current_session.children.push(ChildProcess {
            pid: 77,
            command: "codex-child".to_string(),
            mem_kb: 1_024,
            port: None,
        });
        let stale_session = collector.hook_placeholder(&stale);

        let live = collector.finalize_hook_records_with_herdr(
            vec![stale_session, current_session],
            vec![stale.clone(), current.clone()],
            &hook_shared(),
            now_ms,
            HashMap::new(),
        );
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].session_id, "current-session");
        assert_eq!(live[0].status, SessionStatus::Thinking);
        assert_eq!(live[0].status_evidence.reason, StatusReason::HookTurnOpen);
        assert!(live[0].action_process_incarnation.is_none());
        assert_eq!(live[0].mem_mb, 0);
        assert!(live[0].children.is_empty());
        assert!(collector
            .hook_process_rollout_bindings
            .borrow()
            .contains_key(&current_key));
        assert!(!collector
            .hook_process_rollout_bindings
            .borrow()
            .contains_key(&stale_key));

        let mut current_gone = current;
        current_gone.process_state = HookProcessState::Gone;
        current_gone.native_process_verified = false;
        current_gone.observed_at_ms = now_ms + 1_000;
        let mut stale_gone = stale;
        stale_gone.process_state = HookProcessState::Gone;
        stale_gone.native_process_verified = false;
        stale_gone.observed_at_ms = now_ms + 1_000;
        let exited = collector.finalize_hook_records_with_herdr(
            Vec::new(),
            vec![stale_gone, current_gone],
            &hook_shared(),
            now_ms + 1_000,
            HashMap::new(),
        );
        let current = exited
            .iter()
            .find(|session| session.session_id == "current-session")
            .expect("the selected generation should retain its exact exit row");
        assert_eq!(current.status, SessionStatus::Done);
        assert_eq!(current.status_evidence.reason, StatusReason::ProcessExited);
        assert!(exited.iter().all(|session| {
            session.session_id != "stale-session" || session.status != SessionStatus::Done
        }));
        assert!(!collector
            .hook_done_tombstones
            .borrow()
            .contains_key(&stale_key));
    }

    #[test]
    fn disagreeing_rollout_and_herdr_current_sessions_remain_unknown() {
        let now_ms = 100_000;
        for status in [
            HerdrStatus::Working,
            HerdrStatus::Blocked,
            HerdrStatus::Idle,
        ] {
            let collector = CodexCollector::new();
            let mut rollout_selected = hook_record(HookCandidate::TurnOpen, now_ms);
            rollout_selected.session_id = "rollout-session".to_string();
            rollout_selected.generation_id = "rollout-generation".to_string();
            let mut herdr_selected = hook_record(HookCandidate::TurnOpen, now_ms);
            herdr_selected.session_id = "herdr-session".to_string();
            herdr_selected.generation_id = "herdr-generation".to_string();
            collector.rollout_lifecycle.borrow_mut().insert(
                rollout_selected.session_id.clone(),
                active_root_rollout(now_ms),
            );
            let mut session = collector.hook_placeholder(&rollout_selected);
            session.pid = rollout_selected.pid;
            session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
            let observations = HashMap::from([(
                (herdr_selected.session_id.clone(), herdr_selected.pid),
                HerdrObservation {
                    status,
                    observed_at_ms: now_ms,
                    status_since_ms: now_ms - 2_000,
                    consecutive_matching: 2,
                },
            )]);

            let sessions = collector.finalize_hook_records_with_herdr(
                vec![session],
                vec![rollout_selected, herdr_selected],
                &hook_shared(),
                now_ms,
                observations,
            );
            assert_eq!(sessions.len(), 2);
            assert!(sessions.iter().all(|session| {
                session.status == SessionStatus::Unknown
                    && session.status_evidence.reason == StatusReason::OwnershipUnconfirmed
                    && session.action_process_incarnation.is_none()
            }));
        }
    }

    #[test]
    fn child_version_cannot_supply_or_contradict_the_root_release() {
        let now_ms = 100_000;
        let record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::TurnOpen,
            },
            now_ms,
        );
        let mut child = active_child_rollout("child-1", true, now_ms);

        let mut missing_root = active_root_rollout(now_ms);
        missing_root.root_cli_version.clear();
        missing_root.descendants.push(child.clone());
        assert_eq!(
            project_hook_status(&record, Some(&missing_root), now_ms).2,
            StatusReason::HookIntegrationUnverified,
            "a supported child must not supply missing root version evidence"
        );

        child.cli_version = "0.146.1".to_string();
        let mut mismatched_child = active_root_rollout(now_ms);
        mismatched_child.descendants.push(child);
        assert_eq!(
            project_hook_status(&record, Some(&mismatched_child), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookIntegrationUnverified,
            ),
            "a child from a different release invalidates child lifecycle proof"
        );
    }

    #[test]
    fn subagent_exec_requires_the_exact_active_direct_child_set() {
        let now_ms = 100_000;
        let record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::TurnOpen,
            },
            now_ms,
        );

        let mut exact = active_root_rollout(now_ms);
        exact
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&exact), now_ms).0,
            SessionStatus::Executing
        );
        assert_eq!(
            project_hook_status(&record, None, now_ms).0,
            SessionStatus::Unknown,
            "a hook child without a rollout is not execution proof"
        );

        let mut mismatched = active_root_rollout(now_ms);
        mismatched
            .descendants
            .push(active_child_rollout("child-2", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&mismatched), now_ms).0,
            SessionStatus::Unknown
        );

        let mut extra = exact.clone();
        extra
            .descendants
            .push(active_child_rollout("child-2", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&extra), now_ms).0,
            SessionStatus::Unknown
        );

        let mut uncovered_terminal = exact.clone();
        uncovered_terminal
            .descendants
            .push(terminal_child_rollout("child-2", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&uncovered_terminal), now_ms).0,
            SessionStatus::Unknown,
            "an uncovered terminal direct child invalidates the complete hook set"
        );

        let mut nested = active_root_rollout(now_ms);
        nested
            .descendants
            .push(active_child_rollout("child-1", false, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&nested), now_ms).0,
            SessionStatus::Unknown
        );

        let mut direct_with_terminal_nested = exact.clone();
        direct_with_terminal_nested
            .descendants
            .push(terminal_child_rollout("nested-terminal", false, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&direct_with_terminal_nested), now_ms).0,
            SessionStatus::Unknown,
            "a flat hook child set cannot prove a non-direct subagent execution tree"
        );

        let mut terminal = active_root_rollout(now_ms);
        terminal
            .descendants
            .push(terminal_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&terminal), now_ms).0,
            SessionStatus::Unknown,
            "a terminal rollout cannot satisfy an active hook child"
        );
    }

    #[test]
    fn active_child_cannot_hide_an_unknown_or_ended_root_candidate() {
        let now_ms = 100_000;
        let mut rollout = active_root_rollout(now_ms);
        rollout
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));

        let unknown = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::Unknown(StatusReason::HookConfigChanged),
            },
            now_ms,
        );
        assert_eq!(
            project_hook_status(&unknown, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookConfigChanged,
            )
        );

        let ended = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::Ended,
            },
            now_ms,
        );
        assert_eq!(
            project_hook_status(&ended, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookEventGap,
            )
        );
    }

    #[test]
    fn exact_terminal_nested_descendants_allow_root_thinking_and_idle() {
        let now_ms = 100_000;
        let mut thinking_rollout = active_root_rollout(now_ms);
        thinking_rollout
            .descendants
            .push(terminal_child_rollout("nested-terminal", false, now_ms));
        assert_eq!(
            project_hook_status(
                &hook_record(HookCandidate::TurnOpen, now_ms),
                Some(&thinking_rollout),
                now_ms,
            ),
            (
                SessionStatus::Thinking,
                StatusAuthority::Heuristic,
                StatusReason::HookTurnOpen,
            )
        );

        let mut idle_rollout = completed_root_rollout(now_ms);
        idle_rollout
            .descendants
            .push(terminal_child_rollout("nested-terminal", false, now_ms));
        assert_eq!(
            project_hook_status(
                &hook_record(HookCandidate::TurnStopped, now_ms),
                Some(&idle_rollout),
                now_ms,
            ),
            (
                SessionStatus::Idle,
                StatusAuthority::Heuristic,
                StatusReason::HookTurnComplete,
            )
        );
    }

    #[test]
    fn provisional_subagent_stop_needs_exact_later_child_evidence() {
        let now_ms = 100_000;
        let record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::new(),
                provisional: HashSet::from(["child-1".to_string()]),
                root: HookRootCandidate::TurnStopped,
            },
            now_ms,
        );

        let mut continued = completed_root_rollout(now_ms);
        continued
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&continued), now_ms).0,
            SessionStatus::Executing,
            "same-child activity after provisional SubagentStop remains work"
        );

        let mut closed = completed_root_rollout(now_ms);
        closed
            .descendants
            .push(terminal_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&closed), now_ms).0,
            SessionStatus::Idle
        );

        let mut aborted = completed_root_rollout(now_ms);
        let mut child = terminal_child_rollout("child-1", true, now_ms);
        child.lifecycle_valid = false;
        child.task_complete = false;
        aborted.descendants.push(child);
        assert_eq!(
            project_hook_status(&record, Some(&aborted), now_ms).0,
            SessionStatus::Unknown
        );
    }

    #[test]
    fn root_open_tool_keeps_provisional_child_work_unknown() {
        let now_ms = 100_000;
        let record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::new(),
                provisional: HashSet::from(["child-1".to_string()]),
                root: HookRootCandidate::ToolOpen(HashSet::from(["close-agent-call".to_string()])),
            },
            now_ms,
        );
        let mut continuing = active_root_rollout(now_ms);
        continuing
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&continuing), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            ),
            "a possibly blocked root interaction wins over active child work"
        );

        let mut incomplete = record.clone();
        incomplete.subagent_set_complete = false;
        assert_eq!(
            project_hook_status(&incomplete, Some(&continuing), now_ms).2,
            StatusReason::HookInteractionResolutionUnavailable,
            "root tool ambiguity wins before incomplete child-set projection"
        );

        let mut terminal = active_root_rollout(now_ms);
        terminal
            .descendants
            .push(terminal_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&terminal), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            ),
            "the root close_agent tool remains approval-ambiguous"
        );
    }

    #[test]
    fn root_rollout_open_tool_keeps_active_child_work_unknown() {
        let now_ms = 100_000;
        let record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::TurnOpen,
            },
            now_ms,
        );
        let mut rollout = active_root_rollout(now_ms);
        rollout
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        rollout.open_tool_ids = HashSet::from(["root-call".to_string()]);
        rollout.open_tool_started_at_ms =
            HashMap::from([("root-call".to_string(), now_ms.saturating_sub(500))]);
        assert_eq!(
            project_hook_status(&record, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            ),
            "an unhooked root rollout call may still be waiting for approval"
        );

        rollout.open_tool_ids.clear();
        assert_eq!(
            project_hook_status(&record, Some(&rollout), now_ms).2,
            StatusReason::HookInteractionResolutionUnavailable,
            "orphaned root call timestamps cannot be hidden by child execution"
        );

        let stopped_record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::new(),
                provisional: HashSet::from(["child-1".to_string()]),
                root: HookRootCandidate::TurnStopped,
            },
            now_ms,
        );
        let mut stopped_rollout = completed_root_rollout(now_ms);
        stopped_rollout
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        stopped_rollout.open_tool_ids = HashSet::from(["root-call".to_string()]);
        stopped_rollout.open_tool_started_at_ms =
            HashMap::from([("root-call".to_string(), now_ms.saturating_sub(500))]);
        assert_eq!(
            project_hook_status(&stopped_record, Some(&stopped_rollout), now_ms).2,
            StatusReason::HookInteractionResolutionUnavailable,
            "a provisional root stop cannot override an open root rollout call"
        );
    }

    #[test]
    fn child_open_tool_is_unknown_because_approval_coverage_is_unobservable() {
        let now_ms = 100_000;
        let record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::TurnOpen,
            },
            now_ms,
        );
        let mut rollout = active_root_rollout(now_ms);
        let mut child = active_child_rollout("child-1", true, now_ms);
        child.open_tool_ids = HashSet::from(["child-call".to_string()]);
        child.open_tool_started_at_ms =
            HashMap::from([("child-call".to_string(), now_ms.saturating_sub(500))]);
        child.open_tool_classes =
            HashMap::from([("child-call".to_string(), RolloutToolClass::Ordinary)]);
        rollout.descendants.push(child);
        assert_eq!(
            project_hook_status(&record, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookInteractionResolutionUnavailable,
            )
        );
    }

    #[test]
    fn production_child_pre_tool_ambiguity_cannot_promote_execution() {
        let now_ms = 100_000;
        let mut state = production_turn_open_hook_state(now_ms);
        state.open_subagents.insert("child-1".to_string());
        state
            .subagent_opened_at_ms
            .insert("child-1".to_string(), now_ms.saturating_sub(800));
        state
            .open_child_tools
            .insert("child-call".to_string(), "child-1".to_string());
        state
            .child_tool_opened_at_ms
            .insert("child-call".to_string(), now_ms.saturating_sub(500));
        let mut record = hook_record_from_state(state);
        assert!(matches!(
            record.candidate,
            HookCandidate::Unknown(StatusReason::HookToolOpen)
        ));

        // Even a hypothetical future effective-engine attestation cannot turn
        // the explicit child interaction ambiguity into execution.
        record.process_state = HookProcessState::Live;
        record.native_process_verified = true;
        record.supported_release_attested = true;
        record.effective_hook_engine_attested = true;
        let mut rollout = active_root_rollout(now_ms);
        rollout
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        assert_eq!(
            project_hook_status(&record, Some(&rollout), now_ms),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookToolOpen,
            )
        );

        // The state is still exact process/session evidence. It must stay
        // Unknown while live without preventing a later exact process-exit
        // tombstone for the same incarnation.
        record.effective_hook_engine_attested = false;
        let collector = CodexCollector::new();
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert(record.session_id.clone(), rollout);
        let mut session = collector.hook_placeholder(&record);
        session.pid = record.pid;
        session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let shared = hook_shared();
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, now_ms);
        assert_eq!(live[0].status, SessionStatus::Unknown);
        assert_eq!(live[0].status_evidence.reason, StatusReason::HookToolOpen);

        record.process_state = HookProcessState::Gone;
        record.native_process_verified = false;
        let done = collector.finalize_hook_records(Vec::new(), vec![record], &shared, 101_000);
        assert_eq!(done[0].status, SessionStatus::Done);
        assert_eq!(done[0].status_evidence.reason, StatusReason::ProcessExited);
    }

    #[test]
    fn malformed_child_prevents_root_idle() {
        let now_ms = 100_000;
        let stopped = hook_record(HookCandidate::TurnStopped, now_ms);
        let mut rollout = completed_root_rollout(now_ms);
        let mut child = terminal_child_rollout("child-1", true, now_ms);
        child.lifecycle_valid = false;
        rollout.descendants.push(child);
        assert_eq!(
            project_hook_status(&stopped, Some(&rollout), now_ms).0,
            SessionStatus::Unknown
        );
    }

    #[test]
    fn live_to_gone_transition_is_required_for_done() {
        let collector = CodexCollector::new();
        let mut live = vec![hook_record(HookCandidate::TurnOpen, 100_000)];
        collector.observe_hook_process_transitions(&mut live, 100_000, true);
        assert_eq!(live[0].exit_observed_at_ms, 0);
        let live_key = hook_done_key(&live[0]).unwrap();
        collector.hook_process_rollout_bindings.borrow_mut().insert(
            live_key,
            HookProcessRolloutBinding {
                session_id: "hook-session".to_string(),
                supported_release: true,
            },
        );

        let mut gone = live.clone();
        gone[0].process_state = HookProcessState::Gone;
        gone[0].native_process_verified = false;
        collector.observe_hook_process_transitions(&mut gone, 101_000, true);
        assert!(matches!(gone[0].candidate, HookCandidate::Ended));
        assert_eq!(gone[0].exit_observed_at_ms, 101_000);

        let fresh_collector = CodexCollector::new();
        let mut already_gone = vec![gone[0].clone()];
        already_gone[0].exit_observed_at_ms = 0;
        already_gone[0].candidate = HookCandidate::Ended;
        fresh_collector.observe_hook_process_transitions(&mut already_gone, 101_000, true);
        assert_eq!(already_gone[0].exit_observed_at_ms, 0);
    }

    #[test]
    fn unsupported_release_binding_cannot_become_done() {
        let collector = CodexCollector::new();
        let mut live = vec![hook_record(HookCandidate::TurnOpen, 100_000)];
        collector.observe_hook_process_transitions(&mut live, 100_000, true);
        let live_key = hook_done_key(&live[0]).unwrap();
        collector.hook_process_rollout_bindings.borrow_mut().insert(
            live_key,
            HookProcessRolloutBinding {
                session_id: "hook-session".to_string(),
                supported_release: false,
            },
        );

        let mut gone = live;
        gone[0].process_state = HookProcessState::Gone;
        gone[0].native_process_verified = false;
        collector.observe_hook_process_transitions(&mut gone, 101_000, true);
        assert!(!gone[0].exit_supported_rollout_correlated);
        assert!(matches!(
            gone[0].candidate,
            HookCandidate::Unknown(StatusReason::HookIntegrationUnverified)
        ));
        assert_eq!(
            project_hook_status(&gone[0], None, 101_000),
            (
                SessionStatus::Unknown,
                StatusAuthority::Unavailable,
                StatusReason::HookIntegrationUnverified,
            )
        );
    }

    #[test]
    fn done_tombstone_survives_source_disappearance_for_the_full_window() {
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let (record, session) = prepared_live_hook(
            &collector,
            100_000,
            "hook-session",
            "generation-one",
            "test:codex:42",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let key = hook_done_key(&record).unwrap();
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, 100_000);
        assert_eq!(live[0].status, SessionStatus::Thinking);
        assert!(collector
            .hook_live_session_snapshots
            .borrow()
            .contains_key(&key));

        let mut gone = record;
        gone.process_state = HookProcessState::Gone;
        gone.native_process_verified = false;
        let first_done = collector.finalize_hook_records(Vec::new(), vec![gone], &shared, 101_000);
        assert_eq!(first_done.len(), 1);
        let done = &first_done[0];
        assert_eq!(done.status, SessionStatus::Done);
        assert_eq!(
            done.pid, 42,
            "a tombstone always retains its exact nonzero PID"
        );
        assert!(done.action_process_incarnation.is_none());
        assert_eq!(done.total_input_tokens, 123);
        assert_eq!(done.total_output_tokens, 45);
        assert_eq!(done.total_cache_read, 67);
        assert_eq!(done.turn_count, 9);
        assert_eq!(done.model, "gpt-test");
        assert_eq!(done.effort, "high");
        assert_eq!(done.current_tasks, vec!["finished"]);
        assert!(done.initial_prompt.is_empty());
        assert!(done.first_assistant_text.is_empty());
        assert!(done.chat_messages.is_empty());
        assert!(done.tool_calls.is_empty());
        assert!(done.children.is_empty());
        assert!(done.subagents.is_empty());
        assert!(done.git_branch.is_empty());
        assert_eq!(
            done.status_evidence.observed_at_ms, 101_000,
            "the exit observation is immutable across the retention window"
        );
        assert!(collector.hook_done_tombstones.borrow().contains_key(&key));

        let vanished = collector.finalize_hook_records(Vec::new(), Vec::new(), &shared, 101_001);
        assert_eq!(vanished.len(), 1);
        assert_eq!(vanished[0].status, SessionStatus::Done);
        assert_eq!(vanished[0].pid, 42);
        assert!(vanished[0].action_process_incarnation.is_none());

        let boundary = collector.finalize_hook_records(Vec::new(), Vec::new(), &shared, 131_000);
        assert_eq!(
            boundary.len(),
            1,
            "Done is visible through exactly 30 seconds"
        );
        assert_eq!(boundary[0].status, SessionStatus::Done);

        let expired = collector.finalize_hook_records(Vec::new(), Vec::new(), &shared, 131_001);
        assert!(expired.is_empty());
        assert!(collector.hook_done_tombstones.borrow().is_empty());
    }

    #[test]
    fn unavailable_scan_preserves_proof_but_never_promotes_live_status() {
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let (record, session) = prepared_live_hook(
            &collector,
            100_000,
            "hook-session",
            "generation-one",
            "test:codex:42",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let key = hook_done_key(&record).unwrap();
        let fallback_session = session.clone();
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, 100_000);
        assert_eq!(live[0].status, SessionStatus::Thinking);

        let unavailable = collector.finalize_hook_records_with_scan(
            vec![fallback_session],
            Vec::new(),
            &shared,
            100_500,
            false,
        );
        assert_eq!(unavailable[0].status, SessionStatus::Unknown);
        assert!(collector.hook_process_states.borrow().contains_key(&key));
        assert!(collector
            .hook_process_rollout_bindings
            .borrow()
            .contains_key(&key));
        assert!(collector
            .hook_live_session_snapshots
            .borrow()
            .contains_key(&key));

        let mut gone = record;
        gone.process_state = HookProcessState::Gone;
        gone.native_process_verified = false;
        let done = collector.finalize_hook_records(Vec::new(), vec![gone], &shared, 101_000);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].status, SessionStatus::Done);
    }

    #[test]
    fn fresh_already_gone_or_sticky_fault_never_creates_a_tombstone() {
        let shared = hook_shared();
        let fresh_collector = CodexCollector::new();
        let mut already_gone = hook_record(HookCandidate::TurnOpen, 100_000);
        already_gone.process_state = HookProcessState::Gone;
        already_gone.native_process_verified = false;
        let mut pidless_alias = fresh_collector.hook_placeholder(&already_gone);
        pidless_alias.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let fresh = fresh_collector.finalize_hook_records(
            vec![pidless_alias],
            vec![already_gone.clone()],
            &shared,
            100_000,
        );
        assert!(fresh.is_empty());
        assert!(fresh_collector.hook_done_tombstones.borrow().is_empty());
        assert!(fresh_collector
            .finalize_hook_records(Vec::new(), vec![already_gone], &shared, 100_001)
            .is_empty());

        for reason in [
            StatusReason::HookStateMalformed,
            StatusReason::HookConfigChanged,
            StatusReason::OwnershipUnconfirmed,
            StatusReason::HookEventGap,
            StatusReason::HookIntegrationUnverified,
        ] {
            let collector = CodexCollector::new();
            let (record, session) = prepared_live_hook(
                &collector,
                100_000,
                "hook-session",
                "generation-one",
                "test:codex:42",
                plugin::MIN_SUPPORTED_CODEX_VERSION,
            );
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, 100_000);
            let mut faulty_gone = record;
            faulty_gone.process_state = HookProcessState::Gone;
            faulty_gone.native_process_verified = false;
            faulty_gone.candidate = HookCandidate::Unknown(reason);
            let faulty =
                collector.finalize_hook_records(Vec::new(), vec![faulty_gone], &shared, 101_000);
            assert!(faulty.is_empty(), "gone {reason:?} rows must be omitted");
            assert!(collector.hook_done_tombstones.borrow().is_empty());
        }
    }

    #[test]
    fn suppressed_gone_hook_record_preserves_an_independently_live_rollout() {
        let mut gone = hook_record(HookCandidate::TurnOpen, 100_000);
        gone.process_state = HookProcessState::Gone;
        gone.native_process_verified = false;

        let collector = CodexCollector::new();
        let mut live_rollout = collector.hook_placeholder(&gone);
        live_rollout.pid = 99;
        live_rollout.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let process_info = HashMap::from([
            (42, proc_info(42, 1, "/usr/local/bin/codex")),
            (99, proc_info(99, 1, "/usr/local/bin/codex")),
        ]);
        let shared = super::super::SharedProcessData {
            children_map: process::get_children_map(&process_info),
            process_info,
            ports: HashMap::new(),
            slow_tick: false,
            mcp_server_pids: HashSet::new(),
            mcp_owned_rollouts: HashSet::new(),
            mcp_suppress: true,
            desktop_rollout_fd_map: HashMap::new(),
        };

        let sessions =
            collector.finalize_hook_records(vec![live_rollout], vec![gone], &shared, 100_000);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 99);
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        assert_eq!(
            sessions[0].status_evidence.reason,
            StatusReason::HookIntegrationUnverified
        );
    }

    #[test]
    fn interaction_resolution_unknown_can_still_become_exact_done() {
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let (mut record, session) = prepared_live_hook(
            &collector,
            100_000,
            "hook-session",
            "generation-one",
            "test:codex:42",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        record.candidate =
            HookCandidate::Unknown(StatusReason::HookInteractionResolutionUnavailable);
        record.effective_hook_engine_attested = false;
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, 100_000);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].status, SessionStatus::Unknown);
        assert_eq!(
            live[0].status_evidence.reason,
            StatusReason::HookInteractionResolutionUnavailable
        );

        record.process_state = HookProcessState::Gone;
        record.native_process_verified = false;
        let done = collector.finalize_hook_records(Vec::new(), vec![record], &shared, 101_000);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].status, SessionStatus::Done);
        assert_eq!(done[0].status_evidence.reason, StatusReason::ProcessExited);
        assert_eq!(done[0].pid, 42);
        assert!(done[0].action_process_incarnation.is_none());
        assert_eq!(collector.hook_done_tombstones.borrow().len(), 1);
    }

    #[test]
    fn unsupported_release_never_creates_or_replays_a_done_tombstone() {
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let (record, session) = prepared_live_hook(
            &collector,
            100_000,
            "hook-session",
            "generation-one",
            "test:codex:42",
            "0.144.0",
        );
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, 100_000);
        assert_eq!(live[0].status, SessionStatus::Unknown);

        let mut gone = record;
        gone.process_state = HookProcessState::Gone;
        gone.native_process_verified = false;
        let gone = collector.finalize_hook_records(Vec::new(), vec![gone], &shared, 101_000);
        assert!(gone.is_empty());
        assert!(collector.hook_done_tombstones.borrow().is_empty());
        assert!(collector
            .finalize_hook_records(Vec::new(), Vec::new(), &shared, 101_001)
            .is_empty());
    }

    #[test]
    fn done_tombstone_cache_is_bounded_and_prunes_the_oldest_exit() {
        let collector = CodexCollector::new();
        let now_ms = 100_000;
        let template_record = hook_record(HookCandidate::TurnOpen, now_ms);
        let template_session = collector.hook_placeholder(&template_record);
        let snapshot = HookSessionSnapshot::capture(&template_session);
        for index in 0..=MAX_CODEX_DONE_TOMBSTONES {
            let key = HookDoneKey {
                session_id: format!("session-{index}"),
                generation_id: format!("generation-{index}"),
                pid: u32::try_from(index + 1).unwrap(),
                process_incarnation: format!("incarnation-{index}"),
            };
            collector.hook_done_tombstones.borrow_mut().insert(
                key,
                HookDoneTombstone {
                    exit_observed_at_ms: 90_000 + u64::try_from(index).unwrap(),
                    snapshot: snapshot.clone(),
                },
            );
        }

        collector.prune_hook_done_tombstones(now_ms);
        let tombstones = collector.hook_done_tombstones.borrow();
        assert_eq!(tombstones.len(), MAX_CODEX_DONE_TOMBSTONES);
        assert!(!tombstones.keys().any(|key| key.session_id == "session-0"));
        assert!(tombstones
            .keys()
            .any(|key| key.session_id == format!("session-{MAX_CODEX_DONE_TOMBSTONES}")));
    }

    #[test]
    fn generation_rotation_cannot_inherit_prior_live_or_done_proof() {
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let (old_record, old_session) = prepared_live_hook(
            &collector,
            100_000,
            "hook-session",
            "generation-old",
            "test:codex:42",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let old_key = hook_done_key(&old_record).unwrap();
        collector.finalize_hook_records(
            vec![old_session],
            vec![old_record.clone()],
            &shared,
            100_000,
        );

        let (new_record, new_session) = prepared_live_hook(
            &collector,
            100_500,
            "hook-session",
            "generation-new",
            "test:codex:42",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let new_key = hook_done_key(&new_record).unwrap();
        let new_live = collector.finalize_hook_records(
            vec![new_session],
            vec![new_record.clone()],
            &shared,
            100_500,
        );
        assert_eq!(new_live[0].status, SessionStatus::Thinking);
        assert!(!collector
            .hook_process_states
            .borrow()
            .contains_key(&old_key));
        assert!(collector
            .hook_process_states
            .borrow()
            .contains_key(&new_key));

        let mut stale_old_gone = old_record;
        stale_old_gone.process_state = HookProcessState::Gone;
        stale_old_gone.native_process_verified = false;
        let (_, refreshed_new_session) = prepared_live_hook(
            &collector,
            101_000,
            "hook-session",
            "generation-new",
            "test:codex:42",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let sessions = collector.finalize_hook_records(
            vec![refreshed_new_session],
            vec![stale_old_gone, new_record],
            &shared,
            101_000,
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Thinking);
        assert!(collector.hook_done_tombstones.borrow().is_empty());
    }

    #[test]
    fn pid_reuse_with_a_new_incarnation_cannot_inherit_done_proof() {
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let (old_record, old_session) = prepared_live_hook(
            &collector,
            100_000,
            "old-session",
            "old-generation",
            "old-incarnation",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let old_key = hook_done_key(&old_record).unwrap();
        collector.finalize_hook_records(
            vec![old_session],
            vec![old_record.clone()],
            &shared,
            100_000,
        );

        let (new_record, new_session) = prepared_live_hook(
            &collector,
            100_500,
            "new-session",
            "new-generation",
            "new-incarnation",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let new_key = hook_done_key(&new_record).unwrap();
        collector.finalize_hook_records(
            vec![new_session],
            vec![new_record.clone()],
            &shared,
            100_500,
        );
        assert!(!collector
            .hook_process_states
            .borrow()
            .contains_key(&old_key));
        assert!(collector
            .hook_process_states
            .borrow()
            .contains_key(&new_key));

        let mut stale_old_gone = old_record;
        stale_old_gone.process_state = HookProcessState::Gone;
        stale_old_gone.native_process_verified = false;
        let (_, refreshed_new_session) = prepared_live_hook(
            &collector,
            101_000,
            "new-session",
            "new-generation",
            "new-incarnation",
            plugin::MIN_SUPPORTED_CODEX_VERSION,
        );
        let sessions = collector.finalize_hook_records(
            vec![refreshed_new_session],
            vec![stale_old_gone, new_record],
            &shared,
            101_000,
        );
        assert!(sessions
            .iter()
            .any(|session| session.session_id == "new-session"
                && session.status == SessionStatus::Thinking));
        assert!(sessions
            .iter()
            .all(|session| session.status != SessionStatus::Done));
        assert!(collector.hook_done_tombstones.borrow().is_empty());
    }

    #[test]
    fn live_hook_rollout_binding_mismatch_is_unknown_and_unactionable() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let record = hook_record(HookCandidate::TurnOpen, now_ms);
        let mut wrong = collector.hook_placeholder(&record);
        wrong.pid = 99;
        wrong.cwd = "/home/user/other".to_string();
        let sessions =
            collector.finalize_hook_records(vec![wrong], vec![record], &hook_shared(), now_ms);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        assert_eq!(
            sessions[0].status_evidence.reason,
            StatusReason::OwnershipUnconfirmed
        );
        assert_eq!(sessions[0].pid, 0);
        assert!(sessions[0].action_process_incarnation.is_none());
    }

    #[test]
    fn live_rollout_with_same_pid_and_different_session_blocks_promotion() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::TurnOpen,
            },
            now_ms,
        );
        let mut conflicting = collector.hook_placeholder(&record);
        conflicting.session_id = "other-session".to_string();
        conflicting.pid = record.pid;
        let sessions = collector.finalize_hook_records(
            vec![conflicting],
            vec![record],
            &hook_shared(),
            now_ms,
        );
        let hook = sessions
            .iter()
            .find(|session| session.session_id == "hook-session")
            .unwrap();
        assert_eq!(hook.status, SessionStatus::Unknown);
        assert_eq!(
            hook.status_evidence.reason,
            StatusReason::OwnershipUnconfirmed
        );
        assert!(hook.action_process_incarnation.is_none());
    }

    #[test]
    fn hook_done_requires_confirmed_exit_and_expires_after_thirty_seconds() {
        let now_ms = 100_000;
        let mut ended = hook_record(HookCandidate::Ended, now_ms);
        ended.ended_at_ms = 90_000;
        ended.process_state = HookProcessState::Unverified;
        assert_eq!(
            project_hook_status(&ended, None, now_ms).0,
            SessionStatus::Unknown
        );
        ended.process_state = HookProcessState::Gone;
        ended.exit_observed_at_ms = 90_000;
        assert_eq!(
            project_hook_status(&ended, None, now_ms).0,
            SessionStatus::Unknown,
            "process exit without the last exact rollout binding is not Done proof"
        );
        ended.exit_supported_rollout_correlated = true;
        assert_eq!(
            project_hook_status(&ended, None, now_ms),
            (
                SessionStatus::Done,
                StatusAuthority::Heuristic,
                StatusReason::ProcessExited,
            )
        );
        assert_eq!(
            project_hook_status(&ended, None, 120_001).0,
            SessionStatus::Unknown
        );
        ended.process_incarnation = Some(String::new());
        assert_eq!(
            project_hook_status(&ended, None, now_ms).0,
            SessionStatus::Unknown
        );
    }

    #[test]
    fn hook_overlay_is_never_provider_authoritative_or_actionable_when_ambiguous() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let mut rollout = active_root_rollout(now_ms);
        rollout
            .descendants
            .push(active_child_rollout("child-1", true, now_ms));
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert("hook-session".to_string(), rollout);
        let mut record = hook_record(
            HookCandidate::SubagentOpen {
                active: HashSet::from(["child-1".to_string()]),
                provisional: HashSet::new(),
                root: HookRootCandidate::TurnOpen,
            },
            now_ms,
        );
        record.observations.push(StatusObservation::new(
            SessionStatus::Executing,
            StatusAuthority::Heuristic,
            StatusReason::HookSubagentActive,
            now_ms.saturating_sub(500),
            0,
        ));
        let mut session = collector.hook_placeholder(&record);
        session.pid = 42;
        session.version = plugin::MIN_SUPPORTED_CODEX_VERSION.to_string();
        let sessions = collector.finalize_hook_records(
            vec![session],
            vec![record.clone()],
            &hook_shared(),
            now_ms,
        );
        assert_eq!(sessions[0].status, SessionStatus::Executing);
        assert_eq!(
            sessions[0].status_evidence.authority,
            StatusAuthority::Heuristic
        );
        assert_eq!(sessions[0].status_evidence.connection_generation, 0);
        assert_eq!(
            sessions[0].status_evidence.observations[0].status,
            SessionStatus::Unknown,
            "uncorrelated historical candidates must stay Unknown"
        );
        assert_eq!(
            sessions[0].action_process_incarnation.as_deref(),
            Some("test:codex:42")
        );

        record.interaction_ambiguous = true;
        record.actionable = false;
        let sessions =
            collector.finalize_hook_records(Vec::new(), vec![record], &hook_shared(), now_ms);
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        assert_eq!(
            sessions[0].status_evidence.authority,
            StatusAuthority::Unavailable
        );
        assert!(sessions[0].action_process_incarnation.is_none());
        assert!(!sessions[0].awaiting_input);
    }

    #[test]
    fn unsupported_process_owned_root_rollout_is_unknown_and_unactionable() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let mut rollout = active_root_rollout(now_ms);
        rollout.root_cli_version = "0.144.0".to_string();
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert("hook-session".to_string(), rollout);
        let record = hook_record(HookCandidate::TurnOpen, now_ms);
        let binding_key = hook_done_key(&record).unwrap();
        let mut session = collector.hook_placeholder(&record);
        session.pid = 42;
        session.version = "0.144.0".to_string();

        let sessions =
            collector.finalize_hook_records(vec![session], vec![record], &hook_shared(), now_ms);
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        assert_eq!(
            sessions[0].status_evidence.reason,
            StatusReason::HookIntegrationUnverified
        );
        assert!(sessions[0].action_process_incarnation.is_none());
        assert_eq!(
            collector
                .hook_process_rollout_bindings
                .borrow()
                .get(&binding_key),
            Some(&HookProcessRolloutBinding {
                session_id: "hook-session".to_string(),
                supported_release: false,
            })
        );
    }

    #[test]
    fn duplicate_hook_ownership_collapses_to_one_unknown_unactionable_row() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let first = hook_record(HookCandidate::TurnOpen, now_ms);
        let mut second = first.clone();
        second.observed_at_ms = now_ms.saturating_sub(1);

        let sessions = collector.finalize_hook_records(
            Vec::new(),
            vec![first, second],
            &hook_shared(),
            now_ms,
        );
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Unknown);
        assert_eq!(
            sessions[0].status_evidence.reason,
            StatusReason::OwnershipUnconfirmed
        );
        assert!(sessions[0].action_process_incarnation.is_none());
    }

    #[test]
    fn live_generation_suppresses_ended_generation_for_the_same_process_identity() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let live = hook_record(HookCandidate::Unknown(StatusReason::HookEventGap), now_ms);
        let mut ended = live.clone();
        ended.generation_id = "older-generation".to_string();
        ended.session_id = "older-session".to_string();
        ended.started_at_ms = now_ms.saturating_sub(20_000);
        ended.ended_at_ms = now_ms.saturating_sub(500);
        ended.candidate = HookCandidate::Ended;

        let sessions =
            collector.finalize_hook_records(Vec::new(), vec![ended, live], &hook_shared(), now_ms);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "hook-session");
        assert_ne!(
            sessions[0].status_evidence.reason,
            StatusReason::OwnershipUnconfirmed
        );
    }

    #[test]
    fn multiple_active_generations_for_one_pid_remain_unknown() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let first = hook_record(HookCandidate::TurnOpen, now_ms);
        let mut second = first.clone();
        second.generation_id = "second-generation".to_string();
        second.session_id = "second-session".to_string();

        let sessions = collector.finalize_hook_records(
            Vec::new(),
            vec![first, second],
            &hook_shared(),
            now_ms,
        );
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|session| {
            session.status == SessionStatus::Unknown
                && session.status_evidence.reason == StatusReason::OwnershipUnconfirmed
                && session.action_process_incarnation.is_none()
        }));
    }

    #[test]
    fn gone_process_generations_collapse_to_the_newest_session() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let mut older = hook_record(HookCandidate::Ended, now_ms);
        older.generation_id = "older-generation".to_string();
        older.session_id = "older-session".to_string();
        older.started_at_ms = 80_000;
        older.ended_at_ms = 90_000;
        older.exit_observed_at_ms = 99_000;
        older.exit_supported_rollout_correlated = true;
        older.process_state = HookProcessState::Gone;
        older.native_process_verified = false;

        let mut newer = older.clone();
        newer.generation_id = "newer-generation".to_string();
        newer.session_id = "newer-session".to_string();
        newer.started_at_ms = 90_000;
        newer.ended_at_ms = 95_000;

        let sessions =
            collector.finalize_hook_records(Vec::new(), vec![older, newer], &hook_shared(), now_ms);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "newer-session");
        assert_eq!(sessions[0].status, SessionStatus::Done);
    }

    #[test]
    fn pid_reuse_does_not_suppress_a_different_ended_incarnation() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let live = hook_record(HookCandidate::Unknown(StatusReason::HookEventGap), now_ms);
        let mut ended = live.clone();
        ended.generation_id = "ended-generation".to_string();
        ended.session_id = "ended-session".to_string();
        ended.started_at_ms = 80_000;
        ended.ended_at_ms = 90_000;
        ended.exit_observed_at_ms = 99_000;
        ended.exit_supported_rollout_correlated = true;
        ended.process_incarnation = Some("test:codex:old-42".to_string());
        ended.process_state = HookProcessState::Gone;
        ended.native_process_verified = false;
        ended.candidate = HookCandidate::Ended;

        let sessions =
            collector.finalize_hook_records(Vec::new(), vec![ended, live], &hook_shared(), now_ms);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| {
            session.session_id == "ended-session" && session.status == SessionStatus::Done
        }));
    }

    #[test]
    fn test_parse_codex_empty_returns_none() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(parse_codex_jsonl(file.path()).is_none());
    }
}
