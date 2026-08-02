use super::process::{self, ProcInfo};
use crate::codex_hooks::{
    plugin::{self, PluginPaths},
    state::{
        HookProjection, HookRootProjection, HookSessionState, HookStateStore, IntegrationIdentity,
    },
};
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, RateLimitInfo, SessionStatus,
    StatusAuthority, StatusEvidence, StatusObservation, StatusReason, SubAgent, ToolCall,
    MAX_CHAT_MESSAGES,
};
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
}

const MAX_CODEX_PARSE_CACHE_ENTRIES: usize = 256;
const MAX_CODEX_DONE_TOMBSTONES: usize = 128;
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
    }

    fn is_exact_active(&self, now_ms: u64) -> bool {
        self.has_exact_active_shape(now_ms) && self.open_tool_ids.is_empty()
    }

    fn is_exact_active_with_open_tool(&self, now_ms: u64) -> bool {
        self.has_exact_active_shape(now_ms) && !self.open_tool_ids.is_empty()
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
    }
}

impl RolloutLifecycle {
    fn has_exact_supported_release(&self) -> bool {
        // Only the selected root attests the process release. Descendant
        // metadata cannot supply a missing root version, and disagreement
        // makes the selected lifecycle tree internally inconsistent.
        self.root_cli_version == plugin::SUPPORTED_CODEX_VERSION
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
    /// The last exact live root binding used the audited Codex release.
    exit_supported_rollout_correlated: bool,
    pid: u32,
    process_incarnation: Option<String>,
    process_state: HookProcessState,
    native_process_verified: bool,
    /// Current process ownership and the matched root rollout both attest the audited release.
    supported_release_attested: bool,
    /// Exact, thread-bound proof that this live Codex process actually loaded
    /// and enabled abtop's complete hook engine. Codex 0.146 exposes no such
    /// proof, so production state conversion always leaves this false. Tests
    /// may set it to true to exercise the hypothetical lifecycle projector.
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
fn native_codex_process_is_exact(pid: u32, expected_incarnation: &str) -> bool {
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
        && (!record.effective_hook_engine_attested
            || !record.supported_release_attested
            || rollout.is_none_or(|state| !state.has_exact_supported_release()))
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
                    !state.open_tool_ids.is_empty() || !state.open_tool_started_at_ms.is_empty()
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

fn hook_task_label(status: SessionStatus, rollout_preview: Option<String>) -> String {
    match status {
        SessionStatus::Thinking => "thinking".to_string(),
        SessionStatus::Executing => rollout_preview.unwrap_or_else(|| "executing".to_string()),
        SessionStatus::Idle => "idle".to_string(),
        SessionStatus::Done => "finished".to_string(),
        SessionStatus::Unknown => "status evidence unavailable".to_string(),
        // Codex 0.146 hook evidence never emits these live states.
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
        snapshot.version = plugin::SUPPORTED_CODEX_VERSION.to_string();
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
        mut records: Vec<HookCollectorRecord>,
        shared: &super::SharedProcessData,
        now_ms: u64,
        hook_scan_available: bool,
    ) -> Vec<AgentSession> {
        let eligible_pids =
            Self::find_codex_pids_from_shared(&shared.process_info, &shared.mcp_server_pids)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect::<HashSet<_>>();
        self.observe_hook_process_transitions(&mut records, now_ms, hook_scan_available);
        self.prune_hook_done_tombstones(now_ms);

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
        for session in remaining.iter_mut().flatten() {
            mark_codex_status_unavailable(session, now_ms, StatusReason::HookIntegrationUnverified);
        }

        records.retain(|record| {
            !record.session_id.is_empty()
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
                        session.pid == 0 || session.pid != record.pid || session.cwd != record.cwd
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
            let rollout_binding_exact = matching_rollouts.len() == 1
                && selected.is_some_and(|index| {
                    remaining[index].as_ref().is_some_and(|session| {
                        session.pid == record.pid
                            && session.cwd == record.cwd
                            && shared.process_info.contains_key(&session.pid)
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
                    .is_some_and(RolloutLifecycle::has_exact_supported_release);
            record.supported_release_attested = active_generation
                && process_visible
                && !ownership_conflict
                && !record.local_config_ambiguous
                && supported_release;
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
                .filter(|session| !rollout_only_done_ids.contains(&session.session_id)),
        );
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

fn close_codex_turn_calls(
    end_ms: u64,
    tool_calls: &mut [ToolCall],
    call_indices: &HashMap<String, usize>,
    call_starts: &mut HashMap<String, u64>,
    pending_tasks: &mut Vec<(String, String)>,
    running_exec_by_session: &HashMap<String, String>,
) {
    let background_execs: HashSet<&str> = running_exec_by_session
        .values()
        .map(String::as_str)
        .collect();
    let call_ids: Vec<String> = call_starts
        .keys()
        .filter(|call_id| !background_execs.contains(call_id.as_str()))
        .cloned()
        .collect();
    for call_id in call_ids {
        close_codex_tool_call(
            &call_id,
            end_ms,
            tool_calls,
            call_indices,
            call_starts,
            pending_tasks,
        );
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
        awaiting_input_since_ms: 0,
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

        match val["type"].as_str() {
            Some("session_meta") => {
                // A forked subagent rollout starts with its own metadata, then
                // may replay parent/root session_meta records as inherited
                // history. The first valid identity belongs to this file;
                // replacing it with a later parent collapses the rollout graph
                // and hides active child work from the root session.
                if !result.session_id.is_empty() {
                    continue;
                }
                let payload = &val["payload"];
                if let Some(id) = payload["id"].as_str() {
                    result.session_id = id.to_string();
                }
                result.parent_thread_id = payload["parent_thread_id"]
                    .as_str()
                    .or_else(|| {
                        payload["source"]["subagent"]["thread_spawn"]["parent_thread_id"].as_str()
                    })
                    .map(str::to_string);
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
                            .filter(|turn_id| !turn_id.is_empty())
                            .map(str::to_string);
                        close_codex_turn_calls(
                            boundary_ms,
                            &mut result.tool_calls,
                            &call_indices,
                            &mut call_starts,
                            &mut pending_tasks,
                            &running_exec_by_session,
                        );
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
                        // Rate limits: assign to short/long slots based on window_minutes.
                        // Plus plans: primary=5h(300min), secondary=7d(10080min).
                        // Free plans: primary can be a longer window, such as 30d(43200min).
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
                                let mins = w["window_minutes"].as_u64().unwrap_or(0);
                                let pct = w["used_percent"].as_f64();
                                let resets = w["resets_at"].as_u64();
                                if mins <= 300 {
                                    info.five_hour_pct = pct;
                                    info.five_hour_resets_at = resets;
                                    info.five_hour_window_minutes = Some(mins);
                                } else {
                                    info.seven_day_pct = pct;
                                    info.seven_day_resets_at = resets;
                                    info.seven_day_window_minutes = Some(mins);
                                }
                            }
                            result.rate_limit = Some(info);
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
                            .filter(|turn_id| !turn_id.is_empty())
                            .map(str::to_string);
                        let exact_boundary = payload["error"].is_null()
                            && boundary_ms > 0
                            && completed_turn_id.is_some()
                            && completed_turn_id == result.active_turn_id
                            && result.turn_started_at_ms > 0
                            && result.turn_started_at_ms <= result.latest_lifecycle_at_ms
                            && result.latest_lifecycle_at_ms <= boundary_ms
                            && call_starts.is_empty();
                        close_codex_turn_calls(
                            boundary_ms,
                            &mut result.tool_calls,
                            &call_indices,
                            &mut call_starts,
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
                        advance_rollout_lifecycle(&mut result, boundary_ms);
                        close_codex_turn_calls(
                            boundary_ms,
                            &mut result.tool_calls,
                            &call_indices,
                            &mut call_starts,
                            &mut pending_tasks,
                            &running_exec_by_session,
                        );
                        result.lifecycle_valid = false;
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
                    }
                    Some(
                        "exec_command_end"
                        | "image_generation_end"
                        | "mcp_tool_call_end"
                        | "patch_apply_end"
                        | "web_search_end",
                    ) => {
                        if let Some(call_id) = payload["call_id"].as_str() {
                            if !call_starts.contains_key(call_id) {
                                result.lifecycle_valid = false;
                            }
                            let end_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                            advance_rollout_lifecycle(&mut result, end_ms);
                            close_codex_tool_call(
                                call_id,
                                end_ms,
                                &mut result.tool_calls,
                                &call_indices,
                                &mut call_starts,
                                &mut pending_tasks,
                            );
                        } else {
                            result.lifecycle_valid = false;
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
                    if let Some(name) = payload["name"].as_str() {
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
                            if call_id.is_empty()
                                || start_ms == 0
                                || call_starts.contains_key(call_id)
                            {
                                result.lifecycle_valid = false;
                            }
                            call_names.insert(call_id.to_string(), name.to_string());
                            if name == "write_stdin" {
                                if let Some(session_id) =
                                    raw_input.as_str().and_then(parse_codex_tool_session_id)
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
                        if !call_starts.contains_key(call_id) {
                            result.lifecycle_valid = false;
                        }
                        let end_ms = event_timestamp_ms(&val, parse_now_ms).unwrap_or(0);
                        advance_rollout_lifecycle(&mut result, end_ms);
                        if end_ms == 0 {
                            result.lifecycle_valid = false;
                        }
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
    result.open_tool_ids = call_starts.keys().cloned().collect();
    result.open_tool_started_at_ms = call_starts.clone();
    result.awaiting_input_since_ms = call_starts
        .iter()
        .filter_map(|(call_id, started_at)| {
            call_names
                .get(call_id)
                .is_some_and(|name| codex_tool_waits_for_user(name))
                .then_some(*started_at)
        })
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
            subagent_opened_at_ms: HashMap::new(),
            subagent_stopped_at_ms: HashMap::new(),
            candidate,
            observations: Vec::new(),
        };
        match &record.candidate {
            HookCandidate::ToolOpen(ids) => {
                record.tool_opened_at_ms = ids.iter().map(|id| (id.clone(), edge_ms)).collect();
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
            open_child_tools: BTreeMap::new(),
            child_tool_opened_at_ms: BTreeMap::new(),
            closed_child_tools: BTreeMap::new(),
            open_subagents: BTreeSet::new(),
            subagent_opened_at_ms: BTreeMap::new(),
            provisional_stopped_subagents: BTreeSet::new(),
            subagent_stopped_at_ms: BTreeMap::new(),
            closed_subagents: BTreeSet::new(),
            open_questions: BTreeSet::new(),
            question_opened_at_ms: BTreeMap::new(),
            closed_questions: BTreeSet::new(),
            question_agents: BTreeMap::new(),
            permission_ambiguity: false,
            permission_observed_at_ms: 0,
            child_permission_ambiguities: BTreeSet::new(),
            child_permission_observed_at_ms: BTreeMap::new(),
            compaction_open: false,
            sticky_fault: None,
            completed_ingests: Vec::new(),
            samples: Vec::new(),
        }
    }

    fn active_root_rollout(now_ms: u64) -> RolloutLifecycle {
        RolloutLifecycle {
            root_cli_version: plugin::SUPPORTED_CODEX_VERSION.to_string(),
            turn_active: true,
            active_turn_id: Some("turn-1".to_string()),
            turn_started_at_ms: now_ms.saturating_sub(2_000),
            latest_lifecycle_at_ms: now_ms.saturating_sub(500),
            ..Default::default()
        }
    }

    fn completed_root_rollout(now_ms: u64) -> RolloutLifecycle {
        RolloutLifecycle {
            root_cli_version: plugin::SUPPORTED_CODEX_VERSION.to_string(),
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
            cli_version: plugin::SUPPORTED_CODEX_VERSION.to_string(),
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
        }
    }

    fn terminal_child_rollout(
        session_id: &str,
        direct_child: bool,
        now_ms: u64,
    ) -> DescendantRolloutLifecycle {
        DescendantRolloutLifecycle {
            session_id: session_id.to_string(),
            cli_version: plugin::SUPPORTED_CODEX_VERSION.to_string(),
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
    fn rollout_exec_end_records_duration_but_not_live_status() {
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
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started"}}"#,
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
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"custom_tool_call","name":"exec","input":"do not expose this raw freeform input","status":"completed","call_id":"custom_1"}}"#,
            ],
        );
        let result = parse_codex_jsonl(open.path()).unwrap();
        assert_eq!(result.pending_since_ms, 1_774_710_061_000);
        assert_eq!(result.current_task, "exec");
        assert_eq!(result.tool_calls.len(), 1);
        assert!(result.tool_calls[0].arg.is_empty());

        write_lines(
            &mut open,
            &[
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:04Z","payload":{"type":"custom_tool_call_output","call_id":"custom_1","output":[]}}"#,
            ],
        );
        let result = parse_codex_jsonl(open.path()).unwrap();
        assert_eq!(result.pending_since_ms, 0);
        assert!(result.current_task.is_empty());
        assert_eq!(result.tool_calls[0].duration_ms, 3_000);
        assert!(result.turn_active);
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
            root_cli_version: plugin::SUPPORTED_CODEX_VERSION.to_string(),
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
            root_cli_version: plugin::SUPPORTED_CODEX_VERSION.to_string(),
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
    fn only_known_codex_end_events_close_their_matching_call() {
        let temp = tempfile::tempdir().unwrap();
        let known = temp.path().join("rollout-known-end.jsonl");
        write_jsonl(
            &known,
            &[
                SESSION_META,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:00Z","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
                r#"{"type":"response_item","timestamp":"2026-03-28T15:01:01Z","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"call-1"}}"#,
                r#"{"type":"event_msg","timestamp":"2026-03-28T15:01:02Z","payload":{"type":"exec_command_end","call_id":"call-1"}}"#,
            ],
        );
        let parsed = parse_codex_jsonl(&known).unwrap();
        assert!(parsed.lifecycle_valid);
        assert!(parsed.open_tool_ids.is_empty());

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
            root_cli_version: plugin::SUPPORTED_CODEX_VERSION.to_string(),
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
        assert!(!parsed.lifecycle_valid);
        assert!(!parsed.task_complete);
        assert!(parsed.completed_turn_id.is_none());
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
    fn positive_hook_status_requires_the_exact_supported_root_cli_version() {
        let now_ms = 100_000;
        let record = hook_record(HookCandidate::TurnOpen, now_ms);
        for unsupported in ["0.145.0", "0.146.1", ""] {
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
    fn production_hook_conversion_cannot_promote_live_but_preserves_exact_done_proof() {
        let now_ms = 100_000;
        let collector = CodexCollector::new();
        let shared = hook_shared();
        let mut record = hook_record_from_state(production_turn_open_hook_state(now_ms));
        assert!(matches!(record.candidate, HookCandidate::TurnOpen));
        assert!(
            !record.effective_hook_engine_attested,
            "Codex 0.146 cannot attest the effective hook engine for one live thread"
        );

        // The OS probes are isolated from conversion in this unit test. Make
        // every other ownership input exact so the missing native attestation
        // is the only reason live status cannot be promoted.
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
            plugin::SUPPORTED_CODEX_VERSION,
        );
        let key = hook_done_key(&record).unwrap();
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, now_ms);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].status, SessionStatus::Unknown);
        assert_eq!(
            live[0].status_evidence.authority,
            StatusAuthority::Unavailable
        );
        assert_eq!(
            live[0].status_evidence.reason,
            StatusReason::HookIntegrationUnverified
        );
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
        assert!(done[0].action_process_incarnation.is_none());
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
            "a child from an unaudited release invalidates child lifecycle proof"
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
        session.version = plugin::SUPPORTED_CODEX_VERSION.to_string();
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
            plugin::SUPPORTED_CODEX_VERSION,
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
            plugin::SUPPORTED_CODEX_VERSION,
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
        let fresh =
            fresh_collector.finalize_hook_records(Vec::new(), vec![already_gone], &shared, 100_000);
        assert_eq!(fresh[0].status, SessionStatus::Unknown);
        assert!(fresh_collector.hook_done_tombstones.borrow().is_empty());

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
                plugin::SUPPORTED_CODEX_VERSION,
            );
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, 100_000);
            let mut faulty_gone = record;
            faulty_gone.process_state = HookProcessState::Gone;
            faulty_gone.native_process_verified = false;
            faulty_gone.candidate = HookCandidate::Unknown(reason);
            let faulty =
                collector.finalize_hook_records(Vec::new(), vec![faulty_gone], &shared, 101_000);
            assert_eq!(faulty[0].status, SessionStatus::Unknown);
            assert_eq!(faulty[0].status_evidence.reason, reason);
            assert!(collector.hook_done_tombstones.borrow().is_empty());
        }
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
            plugin::SUPPORTED_CODEX_VERSION,
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
            "0.146.1",
        );
        let live =
            collector.finalize_hook_records(vec![session], vec![record.clone()], &shared, 100_000);
        assert_eq!(live[0].status, SessionStatus::Unknown);

        let mut gone = record;
        gone.process_state = HookProcessState::Gone;
        gone.native_process_verified = false;
        let gone = collector.finalize_hook_records(Vec::new(), vec![gone], &shared, 101_000);
        assert_eq!(gone[0].status, SessionStatus::Unknown);
        assert_eq!(
            gone[0].status_evidence.reason,
            StatusReason::HookIntegrationUnverified
        );
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
            plugin::SUPPORTED_CODEX_VERSION,
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
            plugin::SUPPORTED_CODEX_VERSION,
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
            plugin::SUPPORTED_CODEX_VERSION,
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
            plugin::SUPPORTED_CODEX_VERSION,
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
            plugin::SUPPORTED_CODEX_VERSION,
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
            plugin::SUPPORTED_CODEX_VERSION,
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
        session.version = plugin::SUPPORTED_CODEX_VERSION.to_string();
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
        rollout.root_cli_version = "0.146.1".to_string();
        collector
            .rollout_lifecycle
            .borrow_mut()
            .insert("hook-session".to_string(), rollout);
        let record = hook_record(HookCandidate::TurnOpen, now_ms);
        let binding_key = hook_done_key(&record).unwrap();
        let mut session = collector.hook_placeholder(&record);
        session.pid = 42;
        session.version = "0.146.1".to_string();

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
