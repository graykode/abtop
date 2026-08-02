//! Private, content-free Codex hook state.
//!
//! Hook payloads are reduced before this module sees them. The state format
//! intentionally contains only lifecycle identifiers, canonical event/tool
//! classes, timestamps, an exact process incarnation, and bounded faults.

use crate::collector::process;
use crate::model::{SessionStatus, StatusReason};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{OsStr, OsString};
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const HOOK_STATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_STATE_SAMPLES: usize = 128;
const MAX_OPEN_ITEMS: usize = 256;
const MAX_STATE_FILES: usize = 256;
const MAX_FAULT_FILES: usize = 128;
const MAX_TEMP_FILES: usize = 8;
const MAX_DIRECTORY_ENTRIES: usize = MAX_STATE_FILES + 16;
const MAX_FAULT_DIRECTORY_ENTRIES: usize = MAX_FAULT_FILES + 8;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_FAULT_BYTES: u64 = 32 * 1024;
const MAX_ID_BYTES: usize = 512;
const MAX_CWD_BYTES: usize = 16 * 1024;
const STATE_DIR_NAME: &str = "states";
const FAULT_DIR_NAME: &str = "faults";
const FAULT_PREFIX: &str = "hook-";
const FAULT_OVERFLOW_NAME: &str = "overflow.json";
const LAUNCH_FAULT_PREFIX: &str = "launch-";
const LAUNCH_FAULT_SUFFIX: &str = ".pending";
const LAUNCH_UNIQUE_SEPARATOR: &str = "-pending.";
const LAUNCH_UNIQUE_NONCE_LEN: usize = 16;
const LAUNCH_FAULT_SLOT_COUNT: u8 = 16;
const LAUNCH_FAULT_SLOT_NONCE: &str = "abtopv1";
const INGEST_COMMIT_ID_HEX_BYTES: usize = 16;
const INGEST_COMMIT_ID_LEN: usize = INGEST_COMMIT_ID_HEX_BYTES * 2;
const INGEST_COMMIT_PROOF_SEPARATOR: char = ':';
const TERMINAL_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const MIN_TERMINAL_AGE_BEFORE_PRESSURE_MS: u64 = 30 * 1000;
const PROCESS_DEATH_OBSERVATION_GRACE_MS: u64 = 30 * 1000;

pub const fn hook_state_platform_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationIdentity {
    pub hook_schema_revision: String,
    pub helper_digest: String,
    pub installation_id: String,
    pub config_digest: String,
    pub complete_hook_set: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookProcessIdentity {
    pub pid: u32,
    pub started_at_ms: u64,
    pub incarnation: String,
    pub shared_host: bool,
    pub launch_config_ambiguous: bool,
}

impl HookProcessIdentity {
    pub fn matches_live_process(&self) -> bool {
        self.pid != 0
            && !self.incarnation.is_empty()
            && process::get_process_incarnation(self.pid).as_deref()
                == Some(self.incarnation.as_str())
    }

    pub fn confirmed_gone(&self) -> bool {
        if self.pid == 0 || self.incarnation.is_empty() {
            return false;
        }
        if let Some(current) = process::get_process_incarnation(self.pid) {
            return current != self.incarnation;
        }
        #[cfg(unix)]
        {
            let Ok(pid) = libc::pid_t::try_from(self.pid) else {
                return false;
            };
            // SAFETY: signal zero performs no mutation and only queries the
            // exact numeric PID. ESRCH proves that incarnation is gone.
            let result = unsafe { libc::kill(pid, 0) };
            result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    pub fn actionable(&self) -> bool {
        !self.shared_host && !self.launch_config_ambiguous && self.matches_live_process()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    SubagentStart,
    SubagentStop,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookToolClass {
    Ordinary,
    RequestUserInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEvent {
    pub kind: HookEventKind,
    pub session_id: String,
    pub cwd: String,
    pub turn_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_class: Option<HookToolClass>,
    pub agent_id: Option<String>,
    pub session_start_source: Option<SessionStartSource>,
    /// Exact Codex Stop/SubagentStop recursion guard. Codex sets this when
    /// another Stop hook previously blocked the same turn from stopping.
    pub stop_hook_active: Option<bool>,
    /// Opaque proof binding the durable marker basename to this exact marker
    /// adoption. It is generated by abtop and contains no provider content.
    pub ingest_marker_id: String,
    pub observed_at_ms: u64,
    pub process: HookProcessIdentity,
    pub integration: IntegrationIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookStateSample {
    pub event: HookEventKind,
    pub observed_at_ms: u64,
    #[serde(default)]
    pub status: SessionStatus,
    #[serde(default)]
    pub reason: StatusReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSessionState {
    pub schema_version: u32,
    pub integration: IntegrationIdentity,
    pub generation_id: String,
    pub session_id: String,
    pub cwd: String,
    pub process: HookProcessIdentity,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub ended_at_ms: u64,
    /// First writer-side confirmation that this generation's exact recorded
    /// process incarnation is gone. This maintenance timestamp is
    /// never accepted from hook/provider input and may be later than
    /// `updated_at_ms`.
    #[serde(default)]
    pub first_confirmed_gone_at_ms: u64,
    pub last_event: HookEventKind,
    #[serde(default)]
    pub session_start_source: Option<SessionStartSource>,
    #[serde(default)]
    pub last_root_event: Option<HookEventKind>,
    #[serde(default)]
    pub last_root_boundary_at_ms: u64,
    pub active_turn_id: Option<String>,
    #[serde(default)]
    pub prompt_observed_at_ms: u64,
    pub stop_turn_id: Option<String>,
    #[serde(default)]
    pub stop_hook_active: Option<bool>,
    #[serde(default)]
    pub stop_observed_at_ms: u64,
    pub prompt_accepted: bool,
    pub open_tools: BTreeMap<String, HookToolClass>,
    #[serde(default)]
    pub tool_opened_at_ms: BTreeMap<String, u64>,
    pub closed_tools: BTreeSet<String>,
    /// Exact ordinary child-tool ownership. Child tools cannot be projected
    /// as execution because Codex does not attest their approval coverage.
    #[serde(default)]
    pub open_child_tools: BTreeMap<String, String>,
    #[serde(default)]
    pub child_tool_opened_at_ms: BTreeMap<String, u64>,
    #[serde(default)]
    pub closed_child_tools: BTreeMap<String, String>,
    pub open_subagents: BTreeSet<String>,
    #[serde(default)]
    pub subagent_opened_at_ms: BTreeMap<String, u64>,
    #[serde(default)]
    pub provisional_stopped_subagents: BTreeSet<String>,
    #[serde(default)]
    pub subagent_stopped_at_ms: BTreeMap<String, u64>,
    pub closed_subagents: BTreeSet<String>,
    pub open_questions: BTreeSet<String>,
    #[serde(default)]
    pub question_opened_at_ms: BTreeMap<String, u64>,
    pub closed_questions: BTreeSet<String>,
    #[serde(default)]
    pub question_agents: BTreeMap<String, String>,
    pub permission_ambiguity: bool,
    #[serde(default)]
    pub permission_observed_at_ms: u64,
    #[serde(default)]
    pub child_permission_ambiguities: BTreeSet<String>,
    #[serde(default)]
    pub child_permission_observed_at_ms: BTreeMap<String, u64>,
    pub compaction_open: bool,
    pub sticky_fault: Option<StatusReason>,
    #[serde(default)]
    pub completed_ingests: Vec<String>,
    pub samples: Vec<HookStateSample>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookProjection {
    Unknown(StatusReason),
    TurnOpen,
    ToolOpen(BTreeSet<String>),
    SubagentOpen {
        active: BTreeSet<String>,
        provisional: BTreeSet<String>,
        root: HookRootProjection,
    },
    TurnStopped,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookRootProjection {
    Unknown(StatusReason),
    TurnOpen,
    ToolOpen(BTreeSet<String>),
    TurnStopped,
    Ended,
}

impl HookSessionState {
    fn child_tool_owner(&self, tool_id: &str) -> Option<&str> {
        self.open_child_tools.get(tool_id).map(String::as_str)
    }

    fn has_open_child_tools(&self) -> bool {
        self.open_child_tools
            .keys()
            .any(|tool_id| self.child_tool_owner(tool_id).is_some())
    }

    pub fn interaction_ambiguous(&self) -> bool {
        self.permission_ambiguity
            || !self.child_permission_ambiguities.is_empty()
            || !self.open_questions.is_empty()
    }

    pub fn projection(&self) -> HookProjection {
        if !self.integration.complete_hook_set {
            return HookProjection::Unknown(StatusReason::HookIntegrationUnverified);
        }
        if self.process.shared_host || self.process.launch_config_ambiguous {
            return HookProjection::Unknown(StatusReason::OwnershipUnconfirmed);
        }
        if let Some(reason) = self.sticky_fault {
            return HookProjection::Unknown(reason);
        }
        if self.interaction_ambiguous() {
            return HookProjection::Unknown(StatusReason::HookInteractionResolutionUnavailable);
        }
        if self.has_open_child_tools() {
            // Codex 0.146 does not attest whether an ordinary child tool can
            // surface an approval. Exact open-tool evidence therefore blocks
            // the tool-free child-model projection and stays Unknown.
            return HookProjection::Unknown(StatusReason::HookToolOpen);
        }
        if !self.open_subagents.is_empty() {
            let active = self
                .open_subagents
                .difference(&self.provisional_stopped_subagents)
                .cloned()
                .collect();
            return HookProjection::SubagentOpen {
                active,
                provisional: self.provisional_stopped_subagents.clone(),
                root: self.root_projection(),
            };
        }
        match self.root_projection() {
            HookRootProjection::Unknown(reason) => HookProjection::Unknown(reason),
            HookRootProjection::TurnOpen => HookProjection::TurnOpen,
            HookRootProjection::ToolOpen(tools) => HookProjection::ToolOpen(tools),
            HookRootProjection::TurnStopped => HookProjection::TurnStopped,
            HookRootProjection::Ended => HookProjection::Ended,
        }
    }

    fn root_projection(&self) -> HookRootProjection {
        if self.ended_at_ms != 0 || self.last_root_event == Some(HookEventKind::SessionEnd) {
            return HookRootProjection::Ended;
        }
        if !self.open_tools.is_empty() {
            return HookRootProjection::ToolOpen(self.open_tools.keys().cloned().collect());
        }
        match self.last_root_event {
            // Codex 0.146 queues every SessionStart source and runs the hook
            // from inside the next turn, immediately before UserPromptSubmit.
            // It therefore proves a clean generation boundary, not Idle.
            Some(HookEventKind::SessionStart)
                if self.session_start_source == Some(SessionStartSource::Compact)
                    && self.active_turn_id.is_some()
                    && self.prompt_accepted =>
            {
                HookRootProjection::TurnOpen
            }
            Some(HookEventKind::SessionStart) => {
                HookRootProjection::Unknown(StatusReason::HookEventGap)
            }
            Some(HookEventKind::Stop) if self.stop_turn_id.is_some() => {
                HookRootProjection::TurnStopped
            }
            _ if self.active_turn_id.is_some() && self.prompt_accepted => {
                HookRootProjection::TurnOpen
            }
            _ => HookRootProjection::Unknown(StatusReason::HookEventGap),
        }
    }

    fn new(event: &HookEvent) -> Self {
        let generation_id = state_key(event);
        Self {
            schema_version: HOOK_STATE_SCHEMA_VERSION,
            integration: event.integration.clone(),
            generation_id,
            session_id: event.session_id.clone(),
            cwd: event.cwd.clone(),
            process: event.process.clone(),
            created_at_ms: event.observed_at_ms,
            updated_at_ms: event.observed_at_ms,
            ended_at_ms: 0,
            first_confirmed_gone_at_ms: 0,
            last_event: event.kind,
            session_start_source: event.session_start_source,
            last_root_event: event.agent_id.is_none().then_some(event.kind),
            last_root_boundary_at_ms: if is_root_boundary_event(event) {
                event.observed_at_ms
            } else {
                0
            },
            active_turn_id: None,
            prompt_observed_at_ms: 0,
            stop_turn_id: None,
            stop_hook_active: None,
            stop_observed_at_ms: 0,
            prompt_accepted: false,
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

    fn apply(&mut self, event: &HookEvent) {
        if self.integration != event.integration {
            self.sticky_fault = Some(StatusReason::HookConfigChanged);
        }
        if self.process != event.process || self.session_id != event.session_id {
            self.sticky_fault = Some(StatusReason::OwnershipUnconfirmed);
        }
        if !event.cwd.is_empty() && self.cwd != event.cwd {
            self.sticky_fault = Some(StatusReason::OwnershipUnconfirmed);
        }
        let root_boundary = is_root_boundary_event(event);
        let root_boundary_regressed =
            root_boundary && event.observed_at_ms < self.last_root_boundary_at_ms;
        if root_boundary_regressed {
            self.sticky_fault = Some(StatusReason::HookEventGap);
        }
        if event.agent_id.is_some()
            && matches!(
                event.kind,
                HookEventKind::PreToolUse
                    | HookEventKind::PermissionRequest
                    | HookEventKind::PostToolUse
                    | HookEventKind::PreCompact
                    | HookEventKind::PostCompact
                    | HookEventKind::UserPromptSubmit
                    | HookEventKind::SubagentStart
                    | HookEventKind::SubagentStop
            )
            && self.prompt_observed_at_ms != 0
            && event.observed_at_ms < self.prompt_observed_at_ms
        {
            self.sticky_fault = Some(StatusReason::HookEventGap);
        }
        if event.agent_id.is_none()
            && matches!(
                event.kind,
                HookEventKind::PreToolUse
                    | HookEventKind::PermissionRequest
                    | HookEventKind::PostToolUse
            )
            && self.prompt_observed_at_ms != 0
            && event.observed_at_ms < self.prompt_observed_at_ms
        {
            self.sticky_fault = Some(StatusReason::HookEventGap);
        }

        // Stop is provisional: another provider hook can block it and Codex
        // then continues the same turn. Any exact later non-Stop lifecycle
        // edge proves that continuation and invalidates the old candidate.
        // UserPromptSubmit handles this itself so it can distinguish a steer
        // from the first prompt of a genuinely new turn.
        if self.stop_turn_id.is_some()
            && event.agent_id.is_none()
            && !matches!(
                event.kind,
                HookEventKind::Stop
                    | HookEventKind::UserPromptSubmit
                    | HookEventKind::SessionEnd
                    | HookEventKind::SessionStart
            )
        {
            if event.observed_at_ms < self.stop_observed_at_ms {
                self.sticky_fault = Some(StatusReason::HookEventGap);
            }
            self.stop_turn_id = None;
            self.stop_hook_active = None;
            self.stop_observed_at_ms = 0;
        }
        if let Some(agent) = event.agent_id.as_deref() {
            if event.kind != HookEventKind::SubagentStart
                && self
                    .subagent_opened_at_ms
                    .get(agent)
                    .is_some_and(|opened_at| event.observed_at_ms < *opened_at)
            {
                self.sticky_fault = Some(StatusReason::HookEventGap);
            }
            if event.kind != HookEventKind::SubagentStop {
                let delayed_before_stop = self
                    .subagent_stopped_at_ms
                    .get(agent)
                    .is_some_and(|stopped_at| event.observed_at_ms < *stopped_at);
                if delayed_before_stop {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                } else {
                    self.provisional_stopped_subagents.remove(agent);
                    self.subagent_stopped_at_ms.remove(agent);
                }
            }
        }

        match event.kind {
            HookEventKind::SessionStart => {
                if event.session_start_source == Some(SessionStartSource::Compact) {
                    // Compact is queued and dispatched inside the current
                    // turn after PostCompact. It is not a new generation and
                    // must preserve root and descendant work state.
                    if event.agent_id.is_some()
                        || self.last_root_event != Some(HookEventKind::PostCompact)
                        || self.active_turn_id.is_none()
                        || self.compaction_open
                    {
                        self.sticky_fault = Some(StatusReason::HookEventGap);
                    }
                    self.session_start_source = event.session_start_source;
                } else {
                    // startup/resume/clear is an exact root generation
                    // boundary. Opening an empty TUI emits no event; this
                    // boundary is queued until the first turn and is not Idle.
                    if event.agent_id.is_some()
                        || !matches!(
                            event.session_start_source,
                            Some(
                                SessionStartSource::Startup
                                    | SessionStartSource::Resume
                                    | SessionStartSource::Clear
                            )
                        )
                    {
                        self.sticky_fault = Some(StatusReason::HookEventGap);
                    } else if !root_boundary_regressed {
                        self.integration = event.integration.clone();
                        self.cwd = event.cwd.clone();
                        self.process = event.process.clone();
                        self.created_at_ms = event.observed_at_ms;
                        self.ended_at_ms = 0;
                        self.first_confirmed_gone_at_ms = 0;
                        self.active_turn_id = None;
                        self.prompt_observed_at_ms = 0;
                        self.stop_turn_id = None;
                        self.stop_hook_active = None;
                        self.stop_observed_at_ms = 0;
                        self.prompt_accepted = false;
                        self.open_tools.clear();
                        self.tool_opened_at_ms.clear();
                        self.closed_tools.clear();
                        self.open_child_tools.clear();
                        self.child_tool_opened_at_ms.clear();
                        self.closed_child_tools.clear();
                        self.open_subagents.clear();
                        self.subagent_opened_at_ms.clear();
                        self.provisional_stopped_subagents.clear();
                        self.subagent_stopped_at_ms.clear();
                        self.closed_subagents.clear();
                        self.open_questions.clear();
                        self.question_opened_at_ms.clear();
                        self.closed_questions.clear();
                        self.question_agents.clear();
                        self.permission_ambiguity = false;
                        self.permission_observed_at_ms = 0;
                        self.child_permission_ambiguities.clear();
                        self.child_permission_observed_at_ms.clear();
                        self.compaction_open = false;
                        self.sticky_fault = None;
                        // A marker may have been durably committed immediately
                        // before this boundary and then survived a crash before
                        // unlink. Retain every valid exact commit proof so the
                        // boundary cannot turn that success into a false gap.
                        // Legacy basename-only entries are not commit proofs
                        // and are recoverable only at this exact clean edge.
                        self.completed_ingests
                            .retain(|proof| valid_ingest_commit_proof(proof));
                        self.samples.clear();
                        self.session_start_source = event.session_start_source;
                    }
                }
            }
            HookEventKind::UserPromptSubmit => {
                if let Some(agent) = event.agent_id.as_deref() {
                    if !self.open_subagents.contains(agent) {
                        self.sticky_fault = Some(StatusReason::HookEventGap);
                    }
                    self.finish_sample(event);
                    return;
                }
                let turn = required_id(event.turn_id.as_deref());
                let same_turn = turn.is_some() && self.active_turn_id.as_deref() == turn;
                let may_start_after_stop = turn.is_some()
                    && self.active_turn_id.is_some()
                    && self.active_turn_id.as_deref() != turn
                    && self.stop_turn_id == self.active_turn_id
                    && self.last_root_event == Some(HookEventKind::Stop)
                    && self.open_tools.is_empty()
                    && !self.compaction_open;
                let first_turn = turn.is_some() && self.active_turn_id.is_none();
                if !(same_turn || may_start_after_stop || first_turn) {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                }
                if first_turn || may_start_after_stop {
                    self.active_turn_id = turn.map(ToOwned::to_owned);
                    clear_root_questions(self);
                    self.closed_tools.clear();
                    self.closed_child_tools.clear();
                    self.permission_ambiguity = false;
                    self.permission_observed_at_ms = 0;
                }
                // A repeated/steered prompt in the same turn must not clear
                // open work, closed-edge history, or interaction ambiguity.
                self.stop_turn_id = None;
                self.stop_hook_active = None;
                self.stop_observed_at_ms = 0;
                self.prompt_accepted = turn.is_some();
                self.prompt_observed_at_ms = self.prompt_observed_at_ms.max(event.observed_at_ms);
            }
            HookEventKind::PreToolUse => {
                let child = event.agent_id.as_deref();
                if child.is_some_and(|agent| !self.open_subagents.contains(agent))
                    || (child.is_none() && !turn_matches(self, event.turn_id.as_deref()))
                {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                }
                let Some(tool_id) = required_id(event.tool_use_id.as_deref()) else {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                    self.finish_sample(event);
                    return;
                };
                match event.tool_class {
                    Some(HookToolClass::RequestUserInput) => {
                        if self.open_tools.contains_key(tool_id)
                            || self.closed_tools.contains(tool_id)
                            || self.open_child_tools.contains_key(tool_id)
                            || self.closed_child_tools.contains_key(tool_id)
                            || self.closed_questions.contains(tool_id)
                        {
                            self.sticky_fault = Some(StatusReason::HookEventGap);
                        } else if self.open_questions.contains(tool_id) {
                            let same_actor = match (child, self.question_agents.get(tool_id)) {
                                (None, None) => true,
                                (Some(agent), Some(recorded)) => recorded == agent,
                                _ => false,
                            };
                            if !same_actor {
                                self.sticky_fault = Some(StatusReason::HookEventGap);
                            }
                        } else {
                            self.open_questions.insert(tool_id.to_string());
                            self.question_opened_at_ms
                                .insert(tool_id.to_string(), event.observed_at_ms);
                            if let Some(agent) = child {
                                self.question_agents
                                    .insert(tool_id.to_string(), agent.to_string());
                            }
                        }
                    }
                    Some(HookToolClass::Ordinary) => {
                        if let Some(agent) = child {
                            let conflicts_with_root = self.open_tools.contains_key(tool_id)
                                || self.closed_tools.contains(tool_id);
                            let owner_conflict = self
                                .open_child_tools
                                .get(tool_id)
                                .is_some_and(|recorded| recorded != agent);
                            if conflicts_with_root
                                || owner_conflict
                                || self.closed_child_tools.contains_key(tool_id)
                                || self.open_questions.contains(tool_id)
                                || self.closed_questions.contains(tool_id)
                            {
                                self.sticky_fault = Some(StatusReason::HookEventGap);
                            } else {
                                // Exact duplicate child PreToolUse is
                                // idempotent only for the same child owner.
                                self.open_child_tools
                                    .entry(tool_id.to_string())
                                    .or_insert_with(|| agent.to_string());
                                self.child_tool_opened_at_ms
                                    .entry(tool_id.to_string())
                                    .and_modify(|at| *at = (*at).min(event.observed_at_ms))
                                    .or_insert(event.observed_at_ms);
                            }
                        } else if self.closed_tools.contains(tool_id)
                            || self.open_questions.contains(tool_id)
                            || self.closed_questions.contains(tool_id)
                            || self.open_child_tools.contains_key(tool_id)
                            || self.closed_child_tools.contains_key(tool_id)
                        {
                            self.sticky_fault = Some(StatusReason::HookEventGap);
                        } else {
                            // An exact duplicate root PreToolUse is an
                            // idempotent retry.
                            self.open_tools
                                .entry(tool_id.to_string())
                                .or_insert(HookToolClass::Ordinary);
                            self.tool_opened_at_ms
                                .entry(tool_id.to_string())
                                .and_modify(|at| *at = (*at).min(event.observed_at_ms))
                                .or_insert(event.observed_at_ms);
                        }
                    }
                    None => self.sticky_fault = Some(StatusReason::HookEventGap),
                }
            }
            HookEventKind::PermissionRequest => {
                if let Some(agent) = event.agent_id.as_deref() {
                    if !self.open_subagents.contains(agent) {
                        self.sticky_fault = Some(StatusReason::HookEventGap);
                    }
                    self.child_permission_ambiguities.insert(agent.to_string());
                    self.child_permission_observed_at_ms
                        .entry(agent.to_string())
                        .and_modify(|at| *at = (*at).min(event.observed_at_ms))
                        .or_insert(event.observed_at_ms);
                } else if !turn_matches(self, event.turn_id.as_deref()) {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                    self.permission_ambiguity = true;
                    self.permission_observed_at_ms =
                        min_nonzero(self.permission_observed_at_ms, event.observed_at_ms);
                } else {
                    self.permission_ambiguity = true;
                    self.permission_observed_at_ms =
                        min_nonzero(self.permission_observed_at_ms, event.observed_at_ms);
                }
            }
            HookEventKind::PostToolUse => {
                let child = event.agent_id.as_deref();
                if child.is_some_and(|agent| !self.open_subagents.contains(agent))
                    || (child.is_none() && !turn_matches(self, event.turn_id.as_deref()))
                {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                }
                let Some(tool_id) = required_id(event.tool_use_id.as_deref()) else {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                    self.finish_sample(event);
                    return;
                };
                match event.tool_class {
                    Some(HookToolClass::RequestUserInput) => {
                        let actor_matches = match (child, self.question_agents.get(tool_id)) {
                            (None, None) => true,
                            (Some(agent), Some(recorded)) => recorded == agent,
                            _ => false,
                        };
                        if self
                            .question_opened_at_ms
                            .get(tool_id)
                            .is_some_and(|opened_at| event.observed_at_ms < *opened_at)
                        {
                            self.sticky_fault = Some(StatusReason::HookEventGap);
                        }
                        if actor_matches && self.open_questions.remove(tool_id) {
                            self.question_agents.remove(tool_id);
                            self.question_opened_at_ms.remove(tool_id);
                            self.closed_questions.insert(tool_id.to_string());
                        } else if !self.closed_questions.contains(tool_id) {
                            self.sticky_fault = Some(StatusReason::HookEventGap);
                        }
                    }
                    Some(HookToolClass::Ordinary) => {
                        if let Some(agent) = child {
                            if self.open_tools.contains_key(tool_id)
                                || self.closed_tools.contains(tool_id)
                            {
                                self.sticky_fault = Some(StatusReason::HookEventGap);
                            } else if self
                                .open_child_tools
                                .get(tool_id)
                                .is_some_and(|recorded| recorded == agent)
                            {
                                if self
                                    .child_tool_opened_at_ms
                                    .get(tool_id)
                                    .is_some_and(|opened_at| event.observed_at_ms < *opened_at)
                                {
                                    self.sticky_fault = Some(StatusReason::HookEventGap);
                                }
                                self.open_child_tools.remove(tool_id);
                                self.child_tool_opened_at_ms.remove(tool_id);
                                self.closed_child_tools
                                    .insert(tool_id.to_string(), agent.to_string());
                            } else if self
                                .closed_child_tools
                                .get(tool_id)
                                .is_none_or(|recorded| recorded != agent)
                            {
                                self.sticky_fault = Some(StatusReason::HookEventGap);
                            }
                        } else if self.open_child_tools.contains_key(tool_id)
                            || self.closed_child_tools.contains_key(tool_id)
                        {
                            self.sticky_fault = Some(StatusReason::HookEventGap);
                        } else if self.open_tools.remove(tool_id).is_some() {
                            if self
                                .tool_opened_at_ms
                                .get(tool_id)
                                .is_some_and(|opened_at| event.observed_at_ms < *opened_at)
                            {
                                self.sticky_fault = Some(StatusReason::HookEventGap);
                            }
                            self.tool_opened_at_ms.remove(tool_id);
                            self.closed_tools.insert(tool_id.to_string());
                        } else if !self.closed_tools.contains(tool_id) {
                            self.sticky_fault = Some(StatusReason::HookEventGap);
                        }
                    }
                    None => self.sticky_fault = Some(StatusReason::HookEventGap),
                }
            }
            HookEventKind::SubagentStart => {
                // All descendants share the root session_id. agent_id is the
                // exact child thread identity and the start/stop pair is the
                // only complete descendant-work boundary exposed to hooks.
                let root_turn_active = self.active_turn_id.is_some()
                    && self.prompt_accepted
                    && self.prompt_observed_at_ms != 0
                    && self.stop_turn_id.is_none()
                    && self.ended_at_ms == 0;
                match (
                    required_id(event.agent_id.as_deref()),
                    required_id(event.turn_id.as_deref()),
                ) {
                    (Some(agent), Some(_))
                        if root_turn_active
                            && event.observed_at_ms >= self.prompt_observed_at_ms
                            && !self.closed_subagents.contains(agent)
                            && !self.provisional_stopped_subagents.contains(agent) =>
                    {
                        self.open_subagents.insert(agent.to_string());
                        self.subagent_opened_at_ms
                            .entry(agent.to_string())
                            .and_modify(|at| *at = (*at).min(event.observed_at_ms))
                            .or_insert(event.observed_at_ms);
                    }
                    _ => self.sticky_fault = Some(StatusReason::HookEventGap),
                }
            }
            HookEventKind::SubagentStop => match required_id(event.agent_id.as_deref()) {
                Some(agent) if self.open_subagents.contains(agent) => {
                    if self
                        .open_child_tools
                        .values()
                        .any(|recorded| recorded == agent)
                    {
                        self.sticky_fault = Some(StatusReason::HookEventGap);
                    }
                    self.provisional_stopped_subagents.insert(agent.to_string());
                    self.subagent_stopped_at_ms
                        .entry(agent.to_string())
                        .and_modify(|at| *at = (*at).max(event.observed_at_ms))
                        .or_insert(event.observed_at_ms);
                    self.child_permission_ambiguities.remove(agent);
                    self.child_permission_observed_at_ms.remove(agent);
                    clear_agent_questions(self, agent);
                }
                _ => self.sticky_fault = Some(StatusReason::HookEventGap),
            },
            HookEventKind::PreCompact => {
                if let Some(agent) = event.agent_id.as_deref() {
                    if !self.open_subagents.contains(agent) {
                        self.sticky_fault = Some(StatusReason::HookEventGap);
                    }
                    self.finish_sample(event);
                    return;
                }
                if self.compaction_open || !turn_matches(self, event.turn_id.as_deref()) {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                }
                self.compaction_open = true;
            }
            HookEventKind::PostCompact => {
                if let Some(agent) = event.agent_id.as_deref() {
                    if !self.open_subagents.contains(agent) {
                        self.sticky_fault = Some(StatusReason::HookEventGap);
                    }
                    self.finish_sample(event);
                    return;
                }
                if !self.compaction_open || !turn_matches(self, event.turn_id.as_deref()) {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                }
                self.compaction_open = false;
            }
            HookEventKind::Stop => {
                let turn = required_id(event.turn_id.as_deref());
                if turn.is_none()
                    || self.active_turn_id.as_deref() != turn
                    || !self.open_tools.is_empty()
                    || self.compaction_open
                    || event.stop_hook_active.is_none()
                {
                    self.sticky_fault = Some(StatusReason::HookEventGap);
                }
                self.stop_turn_id = turn.map(ToOwned::to_owned);
                self.stop_hook_active = event.stop_hook_active;
                self.stop_observed_at_ms = event.observed_at_ms;
                // Stop is the first exact later turn boundary available for
                // PermissionRequest and cancelled question ambiguity.
                self.permission_ambiguity = false;
                self.permission_observed_at_ms = 0;
                clear_root_questions(self);
            }
            HookEventKind::SessionEnd => {
                self.ended_at_ms = event.observed_at_ms;
                self.active_turn_id = None;
                self.prompt_observed_at_ms = 0;
                self.stop_turn_id = None;
                self.stop_hook_active = None;
                self.stop_observed_at_ms = 0;
                self.prompt_accepted = false;
                self.open_tools.clear();
                self.tool_opened_at_ms.clear();
                self.closed_tools.clear();
                self.open_child_tools.clear();
                self.child_tool_opened_at_ms.clear();
                self.closed_child_tools.clear();
                self.open_subagents.clear();
                self.subagent_opened_at_ms.clear();
                self.provisional_stopped_subagents.clear();
                self.subagent_stopped_at_ms.clear();
                self.closed_subagents.clear();
                self.open_questions.clear();
                self.question_opened_at_ms.clear();
                self.closed_questions.clear();
                self.question_agents.clear();
                self.permission_ambiguity = false;
                self.permission_observed_at_ms = 0;
                self.child_permission_ambiguities.clear();
                self.child_permission_observed_at_ms.clear();
                self.compaction_open = false;
            }
        }
        self.finish_sample(event);
    }

    fn finish_sample(&mut self, event: &HookEvent) {
        self.last_event = event.kind;
        if event.agent_id.is_none() {
            self.last_root_event = Some(event.kind);
        }
        if is_root_boundary_event(event) {
            self.last_root_boundary_at_ms = self.last_root_boundary_at_ms.max(event.observed_at_ms);
        }
        if !self.completed_ingests.contains(&event.ingest_marker_id) {
            self.completed_ingests.push(event.ingest_marker_id.clone());
        }
        if self.completed_ingests.len() > MAX_STATE_SAMPLES {
            let remove = self.completed_ingests.len() - MAX_STATE_SAMPLES;
            self.completed_ingests.drain(..remove);
        }
        self.updated_at_ms = self.updated_at_ms.max(event.observed_at_ms);
        // Historical hook candidates are not independently sufficient for a
        // public status. The collector promotes only the current candidate
        // after exact rollout/process correlation, so persisted history stays
        // conservatively Unknown.
        let reason = match self.projection() {
            HookProjection::Unknown(reason) => reason,
            HookProjection::TurnOpen => StatusReason::HookTurnOpen,
            HookProjection::ToolOpen(_) | HookProjection::SubagentOpen { .. } => {
                StatusReason::HookToolOpen
            }
            HookProjection::TurnStopped => StatusReason::HookTurnComplete,
            HookProjection::Ended => StatusReason::OwnershipUnconfirmed,
        };
        self.samples.push(HookStateSample {
            event: event.kind,
            observed_at_ms: event.observed_at_ms,
            status: SessionStatus::Unknown,
            reason,
        });
        self.samples.sort_by_key(|sample| sample.observed_at_ms);
        if self.samples.len() > MAX_STATE_SAMPLES {
            let remove = self.samples.len() - MAX_STATE_SAMPLES;
            self.samples.drain(..remove);
        }
        if self.open_tools.len() > MAX_OPEN_ITEMS
            || self.tool_opened_at_ms.len() > MAX_OPEN_ITEMS
            || self.open_child_tools.len() > MAX_OPEN_ITEMS
            || self.child_tool_opened_at_ms.len() > MAX_OPEN_ITEMS
            || self.closed_child_tools.len() > MAX_OPEN_ITEMS
            || self.open_subagents.len() > MAX_OPEN_ITEMS
            || self.subagent_opened_at_ms.len() > MAX_OPEN_ITEMS
            || self.provisional_stopped_subagents.len() > MAX_OPEN_ITEMS
            || self.subagent_stopped_at_ms.len() > MAX_OPEN_ITEMS
            || self.open_questions.len() > MAX_OPEN_ITEMS
            || self.question_opened_at_ms.len() > MAX_OPEN_ITEMS
            || self.closed_tools.len() > MAX_OPEN_ITEMS
            || self.closed_subagents.len() > MAX_OPEN_ITEMS
            || self.closed_questions.len() > MAX_OPEN_ITEMS
            || self.question_agents.len() > MAX_OPEN_ITEMS
            || self.child_permission_ambiguities.len() > MAX_OPEN_ITEMS
            || self.child_permission_observed_at_ms.len() > MAX_OPEN_ITEMS
        {
            self.sticky_fault = Some(StatusReason::HookStateMalformed);
            while self.open_tools.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.open_tools.keys().next_back().cloned() {
                    self.open_tools.remove(&key);
                    self.tool_opened_at_ms.remove(&key);
                }
            }
            while self.open_child_tools.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.open_child_tools.keys().next_back().cloned() {
                    self.open_child_tools.remove(&key);
                    self.child_tool_opened_at_ms.remove(&key);
                }
            }
            while self.closed_child_tools.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.closed_child_tools.keys().next_back().cloned() {
                    self.closed_child_tools.remove(&key);
                }
            }
            while self.open_subagents.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.open_subagents.iter().next_back().cloned() {
                    self.open_subagents.remove(&key);
                    self.subagent_opened_at_ms.remove(&key);
                    self.provisional_stopped_subagents.remove(&key);
                    self.subagent_stopped_at_ms.remove(&key);
                    let owned_tools = self
                        .open_child_tools
                        .iter()
                        .filter_map(|(tool, owner)| (owner == &key).then_some(tool.clone()))
                        .collect::<Vec<_>>();
                    for tool in owned_tools {
                        self.open_child_tools.remove(&tool);
                        self.child_tool_opened_at_ms.remove(&tool);
                    }
                    self.closed_child_tools.retain(|_, owner| owner != &key);
                }
            }
            while self.open_questions.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.open_questions.iter().next_back().cloned() {
                    self.open_questions.remove(&key);
                    self.question_opened_at_ms.remove(&key);
                    self.question_agents.remove(&key);
                }
            }
            while self.closed_tools.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.closed_tools.iter().next_back().cloned() {
                    self.closed_tools.remove(&key);
                }
            }
            while self.closed_subagents.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.closed_subagents.iter().next_back().cloned() {
                    self.closed_subagents.remove(&key);
                }
            }
            while self.closed_questions.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self.closed_questions.iter().next_back().cloned() {
                    self.closed_questions.remove(&key);
                }
            }
            while self.child_permission_ambiguities.len() > MAX_OPEN_ITEMS {
                if let Some(key) = self
                    .child_permission_ambiguities
                    .iter()
                    .next_back()
                    .cloned()
                {
                    self.child_permission_ambiguities.remove(&key);
                    self.child_permission_observed_at_ms.remove(&key);
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct HookStateScan {
    pub states: Vec<HookSessionState>,
    pub rejected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct HookIngestFault {
    schema_version: u32,
    #[serde(default)]
    integration: Option<IntegrationIdentity>,
    observed_at_ms: u64,
    /// Random per-adoption identity. Legacy records deserialize with an empty
    /// value and fail closed instead of inheriting a basename-only commit.
    #[serde(default)]
    commit_id: String,
}

fn new_ingest_commit_id() -> io::Result<String> {
    let mut random = [0_u8; INGEST_COMMIT_ID_HEX_BYTES];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    Ok(hex(&random))
}

fn valid_ingest_commit_id(value: &str) -> bool {
    value.len() == INGEST_COMMIT_ID_LEN && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn ingest_commit_proof(marker_name: &str, commit_id: &str) -> io::Result<String> {
    if (!valid_fault_filename(marker_name) && !valid_launcher_fault_filename(marker_name))
        || !valid_ingest_commit_id(commit_id)
    {
        return Err(invalid_data("invalid hook ingest commit proof"));
    }
    Ok(format!(
        "{marker_name}{INGEST_COMMIT_PROOF_SEPARATOR}{commit_id}"
    ))
}

fn parse_ingest_commit_proof(value: &str) -> Option<(&str, &str)> {
    let (marker_name, commit_id) = value.split_once(INGEST_COMMIT_PROOF_SEPARATOR)?;
    ((!marker_name.contains(INGEST_COMMIT_PROOF_SEPARATOR))
        && (valid_fault_filename(marker_name) || valid_launcher_fault_filename(marker_name))
        && valid_ingest_commit_id(commit_id))
    .then_some((marker_name, commit_id))
}

fn valid_ingest_commit_proof(value: &str) -> bool {
    parse_ingest_commit_proof(value).is_some()
}

fn valid_ingest_fault(fault: &HookIngestFault) -> bool {
    fault.schema_version == HOOK_STATE_SCHEMA_VERSION
        && fault.observed_at_ms != 0
        && valid_ingest_commit_id(&fault.commit_id)
        && fault
            .integration
            .as_ref()
            .is_none_or(|identity| validate_identity(identity).is_ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmedGoneGcDecision {
    Keep,
    PersistFirstConfirmation,
    Remove,
}

fn confirmed_gone_gc_decision(
    state: &mut HookSessionState,
    now_ms: u64,
) -> ConfirmedGoneGcDecision {
    if now_ms < state.updated_at_ms || !state.process.confirmed_gone() {
        return ConfirmedGoneGcDecision::Keep;
    }
    if state.first_confirmed_gone_at_ms == 0 {
        state.first_confirmed_gone_at_ms = now_ms;
        return ConfirmedGoneGcDecision::PersistFirstConfirmation;
    }
    if now_ms.saturating_sub(state.first_confirmed_gone_at_ms) > PROCESS_DEATH_OBSERVATION_GRACE_MS
    {
        // `confirmed_gone` above is a fresh exact-incarnation check for this
        // deletion attempt. A reused numeric PID cannot satisfy Live again.
        ConfirmedGoneGcDecision::Remove
    } else {
        ConfirmedGoneGcDecision::Keep
    }
}

/// A durable marker created before parsing/folding one hook invocation.
/// Dropping an armed guard deliberately leaves the marker behind.
pub struct HookIngestGuard {
    fault_dir: Arc<SecureDirectory>,
    name: OsString,
    commit_proof: String,
    file: File,
    armed: bool,
    remove_on_success: bool,
}

impl HookIngestGuard {
    pub fn marker_id(&self) -> io::Result<&str> {
        Ok(&self.commit_proof)
    }

    pub fn succeed(mut self) -> io::Result<()> {
        if self.remove_on_success {
            self.fault_dir.remove_if_same(&self.name, &self.file)?;
        }
        self.armed = false;
        Ok(())
    }
}

fn ingest_guard(
    fault_dir: &Arc<SecureDirectory>,
    name: OsString,
    file: File,
    commit_id: &str,
    remove_on_success: bool,
) -> io::Result<HookIngestGuard> {
    let marker_name = name
        .to_str()
        .ok_or_else(|| invalid_data("invalid hook marker encoding"))?;
    Ok(HookIngestGuard {
        fault_dir: Arc::clone(fault_dir),
        commit_proof: ingest_commit_proof(marker_name, commit_id)?,
        name,
        file,
        armed: true,
        remove_on_success,
    })
}

impl Drop for HookIngestGuard {
    fn drop(&mut self) {
        // An armed marker is the fail-closed result. It must not be removed
        // from Drop because unwinding/errors are exactly what it records.
        let _ = self.armed;
    }
}

/// A safely anchored store root used before attestation and process/input
/// preflight. Its generic marker deliberately contains no provider content.
pub struct HookStateIngress {
    state_dir: Arc<SecureDirectory>,
    fault_dir: Arc<SecureDirectory>,
}

pub struct HookStateStore {
    state_dir: Arc<SecureDirectory>,
    fault_dir: Arc<SecureDirectory>,
    expected: IntegrationIdentity,
}

impl HookStateStore {
    /// Resolve and anchor the private state directories before any expensive
    /// hook preflight. On Unix every later operation is relative to retained
    /// directory descriptors, so replacing an ancestor cannot redirect I/O.
    pub fn prepare(plugin_data: &Path) -> io::Result<HookStateIngress> {
        if !hook_state_platform_supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure Codex hook state is unsupported on this platform",
            ));
        }
        validate_plugin_data_path(plugin_data)?;
        let plugin_dir = SecureDirectory::open_path(plugin_data, true)?;
        let state_dir = Arc::new(plugin_dir.open_or_create_child(STATE_DIR_NAME)?);
        let fault_dir = Arc::new(state_dir.open_or_create_child(FAULT_DIR_NAME)?);
        Ok(HookStateIngress {
            state_dir,
            fault_dir,
        })
    }

    #[cfg(all(test, unix))]
    pub fn new(plugin_data: &Path, expected: IntegrationIdentity) -> io::Result<Self> {
        Self::prepare(plugin_data)?.bind(expected)
    }

    /// Open the collector side without creating directories or repairing
    /// permissions. Missing or unsafe state must remain a read-only failure.
    pub fn open_existing(plugin_data: &Path, expected: IntegrationIdentity) -> io::Result<Self> {
        if !hook_state_platform_supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure Codex hook state is unsupported on this platform",
            ));
        }
        validate_plugin_data_path(plugin_data)?;
        validate_identity(&expected)?;
        let plugin_dir = SecureDirectory::open_path(plugin_data, false)?;
        let state_dir = Arc::new(plugin_dir.open_existing_child(STATE_DIR_NAME)?);
        let fault_dir = Arc::new(state_dir.open_existing_child(FAULT_DIR_NAME)?);
        Ok(Self {
            state_dir,
            fault_dir,
            expected,
        })
    }

    pub fn fold(&self, event: HookEvent) -> io::Result<HookSessionState> {
        validate_event(&event)?;
        if event.integration != self.expected {
            return Err(invalid_data("hook integration identity changed"));
        }
        let name = OsString::from(format!("state-{}.json", state_key(&event)));
        let state = {
            let _lock = StateLock::acquire(&self.state_dir)?;
            let mut state = match read_state_file(&self.state_dir, &name)? {
                Some(state) => state,
                None => {
                    self.ensure_state_capacity(event.observed_at_ms)?;
                    let mut state = HookSessionState::new(&event);
                    if event.kind != HookEventKind::SessionStart {
                        state.sticky_fault = Some(StatusReason::HookEventGap);
                    }
                    state
                }
            };
            state.apply(&event);
            validate_state(&state)?;
            write_state_file(&self.state_dir, &name, &state)?;
            state
        };
        // Terminal retention is intentionally outside the hot fold lock.
        // Files are immutable terminal generations and removal verifies the
        // exact opened inode before unlinking.
        if event.kind == HookEventKind::SessionEnd {
            self.cleanup_terminal_states(event.observed_at_ms)?;
        }
        Ok(state)
    }

    #[cfg(all(test, unix))]
    pub fn begin_ingest(&self, observed_at_ms: u64) -> io::Result<HookIngestGuard> {
        self.begin_marker(observed_at_ms, Some(self.expected.clone()))
    }

    pub fn read_all(&self, now_ms: u64) -> io::Result<HookStateScan> {
        let mut scan = HookStateScan::default();
        let mut latest_failure_ms = 0_u64;
        let mut poison_all = false;
        let mut saw_lock = false;
        let mut saw_state_file = false;
        let (entries, truncated) = self.state_dir.list_names(MAX_DIRECTORY_ENTRIES)?;
        if truncated {
            scan.rejected += 1;
            poison_all = true;
        }
        for name in entries {
            let Some(name) = name.to_str() else {
                scan.rejected += 1;
                poison_all = true;
                continue;
            };
            if name == ".lock" {
                saw_lock = true;
                let lock_is_unsafe = match self
                    .state_dir
                    .open_private_read(OsStr::new(name))
                    .and_then(|file| file.metadata())
                {
                    Ok(metadata) => metadata.len() != 0,
                    Err(_) => true,
                };
                if lock_is_unsafe {
                    scan.rejected += 1;
                    poison_all = true;
                }
                continue;
            }
            if name == FAULT_DIR_NAME {
                continue;
            }
            if valid_temporary_filename(name) {
                // An in-flight atomic replacement still leaves the prior
                // state readable. Count unexpected temp accumulation so it
                // cannot be used to bypass the directory bound.
                scan.rejected += 1;
                poison_all = true;
                continue;
            }
            if !valid_state_filename(name) {
                scan.rejected += 1;
                poison_all = true;
                continue;
            }
            saw_state_file = true;
            match read_state_file(&self.state_dir, OsStr::new(name)) {
                Ok(Some(mut state)) => {
                    if validate_state(&state).is_err() {
                        scan.rejected += 1;
                        poison_all = true;
                        continue;
                    }
                    let expected_name = format!("state-{}.json", state.generation_id);
                    if name != expected_name {
                        scan.rejected += 1;
                        poison_all = true;
                        continue;
                    }
                    if state.integration.installation_id != self.expected.installation_id {
                        // A prior installation is retained for bounded audit
                        // and GC, but is not malformed evidence for the
                        // current integration and cannot produce a live row.
                        continue;
                    }
                    if state.integration != self.expected {
                        state.sticky_fault = Some(StatusReason::HookConfigChanged);
                    }
                    if state.updated_at_ms > now_ms.saturating_add(60_000) {
                        state.sticky_fault = Some(StatusReason::HookStateMalformed);
                    }
                    if state.first_confirmed_gone_at_ms > now_ms.saturating_add(60_000) {
                        state.sticky_fault = Some(StatusReason::HookStateMalformed);
                    }
                    scan.states.push(state);
                }
                _ => {
                    scan.rejected += 1;
                    poison_all = true;
                }
            }
        }
        if scan.states.len() > MAX_STATE_FILES {
            scan.rejected += 1;
            scan.states.truncate(MAX_STATE_FILES);
            poison_all = true;
        }
        if saw_state_file && !saw_lock {
            scan.rejected += 1;
            poison_all = true;
        }

        let (faults, faults_truncated) = self.fault_dir.list_names(MAX_FAULT_DIRECTORY_ENTRIES)?;
        if faults_truncated {
            scan.rejected += 1;
            poison_all = true;
        }
        for name in faults {
            let Some(name) = name.to_str() else {
                scan.rejected += 1;
                poison_all = true;
                continue;
            };
            if valid_temporary_filename(name) {
                scan.rejected += 1;
                poison_all = true;
                continue;
            }
            if !valid_fault_filename(name) && !valid_launcher_fault_filename(name) {
                scan.rejected += 1;
                poison_all = true;
                continue;
            }
            match read_fault_file(&self.fault_dir, OsStr::new(name)) {
                Ok(fault) => {
                    if !valid_ingest_fault(&fault) {
                        scan.rejected += 1;
                        poison_all = true;
                        continue;
                    }
                    if fault.integration.as_ref().is_some_and(|integration| {
                        integration.installation_id != self.expected.installation_id
                    }) {
                        // A structurally valid older integration cannot poison
                        // or exhaust the current installation.
                        continue;
                    }
                    if fault.observed_at_ms > now_ms.saturating_add(60_000) {
                        scan.rejected += 1;
                        poison_all = true;
                        continue;
                    }
                    let Ok(commit_proof) = ingest_commit_proof(name, &fault.commit_id) else {
                        scan.rejected += 1;
                        poison_all = true;
                        continue;
                    };
                    let committed = name != FAULT_OVERFLOW_NAME
                        && scan.states.iter().any(|state| {
                            state.integration == self.expected
                                && state
                                    .completed_ingests
                                    .iter()
                                    .any(|proof| proof == &commit_proof)
                                && state.updated_at_ms >= fault.observed_at_ms
                        });
                    if committed {
                        continue;
                    }
                    match fault.integration.as_ref() {
                        None => {
                            latest_failure_ms = latest_failure_ms.max(fault.observed_at_ms);
                        }
                        Some(integration) if integration == &self.expected => {
                            latest_failure_ms = latest_failure_ms.max(fault.observed_at_ms);
                        }
                        Some(_) => {
                            scan.rejected += 1;
                            poison_all = true;
                        }
                    }
                }
                Err(_) => {
                    // Empty launcher markers, partial writes, future times,
                    // and schema mismatches are durable global uncertainty.
                    scan.rejected += 1;
                    poison_all = true;
                }
            }
        }

        for state in &mut scan.states {
            if poison_all {
                state.sticky_fault = Some(StatusReason::HookStateMalformed);
            } else if latest_failure_ms > 0 && state.created_at_ms <= latest_failure_ms {
                state.sticky_fault = Some(StatusReason::HookEventGap);
            }
        }
        Ok(scan)
    }

    #[cfg(all(test, unix))]
    fn state_path(&self, key: &str) -> PathBuf {
        self.state_dir.path.join(format!("state-{key}.json"))
    }

    fn cleanup_terminal_states(&self, now_ms: u64) -> io::Result<()> {
        let (entries, _) = self.state_dir.list_names(MAX_DIRECTORY_ENTRIES)?;
        let mut candidates = Vec::new();
        for name in entries {
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if !valid_state_filename(name_str) {
                continue;
            }
            let Some(state) = read_state_file(&self.state_dir, &name)? else {
                continue;
            };
            if validate_state(&state).is_err() {
                // Malformed evidence is never garbage-collected as if it
                // were a trustworthy terminal generation.
                continue;
            }
            let retention_anchor = if state.ended_at_ms != 0 {
                state.ended_at_ms
            } else {
                state.updated_at_ms
            };
            if now_ms.saturating_sub(retention_anchor) > TERMINAL_RETENTION_MS
                && state.process.confirmed_gone()
            {
                candidates.push((name, state));
            }
        }

        // Slow process probes above do not hold the writer lock. Revalidate
        // both the exact state snapshot and exact process incarnation before
        // persisting a first-death observation or deleting anything.
        let _lock = StateLock::acquire(&self.state_dir)?;
        for (name, snapshot) in candidates {
            let Some((mut current, file)) = read_state_file_with_file(&self.state_dir, &name)?
            else {
                continue;
            };
            if current != snapshot || validate_state(&current).is_err() {
                continue;
            }
            match confirmed_gone_gc_decision(&mut current, now_ms) {
                ConfirmedGoneGcDecision::Keep => {}
                ConfirmedGoneGcDecision::PersistFirstConfirmation => {
                    validate_state(&current)?;
                    write_state_file(&self.state_dir, &name, &current)?;
                }
                ConfirmedGoneGcDecision::Remove => {
                    let _ = self.state_dir.remove_if_same(&name, &file);
                }
            }
        }
        Ok(())
    }

    fn ensure_state_capacity(&self, now_ms: u64) -> io::Result<()> {
        let (entries, truncated) = self.state_dir.list_names(MAX_DIRECTORY_ENTRIES)?;
        let mut state_names = entries
            .into_iter()
            .filter(|name| name.to_str().is_some_and(valid_state_filename))
            .collect::<Vec<_>>();
        if !truncated && state_names.len() < MAX_STATE_FILES {
            return Ok(());
        }

        // Pressure never anchors the observation opportunity to SessionEnd:
        // a process can remain live long after that hook. The first exact
        // death confirmation is persisted, and removal needs a later pass
        // strictly beyond its grace interval.
        state_names.sort();
        for name in &state_names {
            let Ok(Some((mut state, file))) = read_state_file_with_file(&self.state_dir, name)
            else {
                continue;
            };
            if validate_state(&state).is_err() {
                continue;
            }
            let terminal_reclaimable = state.ended_at_ms != 0
                && now_ms.saturating_sub(state.ended_at_ms) >= MIN_TERMINAL_AGE_BEFORE_PRESSURE_MS;
            let crashed_reclaimable = state.ended_at_ms == 0
                && now_ms.saturating_sub(state.updated_at_ms) > TERMINAL_RETENTION_MS;
            if terminal_reclaimable || crashed_reclaimable {
                match confirmed_gone_gc_decision(&mut state, now_ms) {
                    ConfirmedGoneGcDecision::Keep => {}
                    ConfirmedGoneGcDecision::PersistFirstConfirmation => {
                        validate_state(&state)?;
                        write_state_file(&self.state_dir, name, &state)?;
                    }
                    ConfirmedGoneGcDecision::Remove => {
                        let _ = self.state_dir.remove_if_same(name, &file);
                    }
                }
            }
            let (remaining, still_truncated) = self.state_dir.list_names(MAX_DIRECTORY_ENTRIES)?;
            let count = remaining
                .iter()
                .filter(|candidate| candidate.to_str().is_some_and(valid_state_filename))
                .count();
            if !still_truncated && count < MAX_STATE_FILES {
                return Ok(());
            }
        }

        self.record_overflow_fault(now_ms, Some(self.expected.clone()))?;
        Err(invalid_data("hook-state generation capacity exhausted"))
    }

    #[cfg(all(test, unix))]
    fn begin_marker(
        &self,
        observed_at_ms: u64,
        integration: Option<IntegrationIdentity>,
    ) -> io::Result<HookIngestGuard> {
        begin_marker(
            &self.state_dir,
            &self.fault_dir,
            observed_at_ms,
            integration,
        )
    }

    fn record_overflow_fault(
        &self,
        observed_at_ms: u64,
        integration: Option<IntegrationIdentity>,
    ) -> io::Result<()> {
        record_overflow_fault(&self.fault_dir, observed_at_ms, integration).map(|_| ())
    }
}

impl HookStateIngress {
    pub fn bind(&self, expected: IntegrationIdentity) -> io::Result<HookStateStore> {
        validate_identity(&expected)?;
        Ok(HookStateStore {
            state_dir: Arc::clone(&self.state_dir),
            fault_dir: Arc::clone(&self.fault_dir),
            expected,
        })
    }

    pub fn begin_ingest(&self, observed_at_ms: u64) -> io::Result<HookIngestGuard> {
        begin_marker(&self.state_dir, &self.fault_dir, observed_at_ms, None)
    }

    /// Reclaim crash artifacts after the hook payload has been drained.
    ///
    /// This intentionally performs process-incarnation probes and therefore
    /// must not run on the latency-critical path before stdin drain or durable
    /// marker adoption. Collector reads never call this mutation.
    pub fn reclaim_stale_artifacts_after_drain(&self, now_ms: u64) -> io::Result<()> {
        if now_ms == 0 {
            return Err(invalid_data("invalid hook-ingest cleanup time"));
        }
        let Some(states_before_probe) = generations_for_artifact_reclamation(&self.state_dir)?
        else {
            return Ok(());
        };
        // Process inspection is deliberately outside the global state lock:
        // another concurrent hook must still be able to adopt its durable
        // marker and drain stdin within Codex's one-second timeout.
        let process_cache = process_death_snapshot(&states_before_probe);
        let _lock = StateLock::acquire(&self.state_dir)?;
        let Some(states_after_probe) = generations_for_artifact_reclamation(&self.state_dir)?
        else {
            return Ok(());
        };
        if states_before_probe != states_after_probe {
            // Never apply process observations to a state set that changed
            // while those observations were collected.
            return Ok(());
        }
        reclaim_stale_ingest_artifacts(
            &self.state_dir,
            &self.fault_dir,
            &states_after_probe,
            now_ms,
            &process_cache,
        )?;
        reclaim_expired_faults(&self.fault_dir, &states_after_probe, now_ms, &process_cache)
    }

    /// Adopt only the unique launcher token grammar (or its bounded fixed-slot
    /// fallback) below the already anchored faults directory. An arbitrary
    /// path is never accepted.
    pub fn adopt_launcher_marker(
        &self,
        token: &OsStr,
        observed_at_ms: u64,
    ) -> io::Result<HookIngestGuard> {
        let Some(name) = token.to_str() else {
            return Err(invalid_data("invalid launcher fault token encoding"));
        };
        if !valid_launcher_fault_filename(name) {
            return Err(invalid_data("invalid launcher fault token"));
        }
        if observed_at_ms == 0 {
            return Err(invalid_data("invalid hook-ingest fault time"));
        }
        let _lock = StateLock::acquire(&self.state_dir)?;
        let mut file = self.fault_dir.open_private_rw_existing(token)?;
        if file.metadata()?.len() != 0 {
            return Err(invalid_data("launcher fault marker was already adopted"));
        }
        lock_marker(&file)?;
        let commit_id = new_ingest_commit_id()?;
        let fault = HookIngestFault {
            schema_version: HOOK_STATE_SCHEMA_VERSION,
            integration: None,
            observed_at_ms,
            commit_id: commit_id.clone(),
        };
        let bytes = encode_fault(&fault)?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&bytes)?;
        file.set_len(bytes.len() as u64)?;
        file.sync_all()?;
        self.fault_dir.sync()?;
        ingest_guard(
            &self.fault_dir,
            token.to_os_string(),
            file,
            &commit_id,
            true,
        )
    }
}

fn begin_marker(
    state_dir: &Arc<SecureDirectory>,
    fault_dir: &Arc<SecureDirectory>,
    observed_at_ms: u64,
    integration: Option<IntegrationIdentity>,
) -> io::Result<HookIngestGuard> {
    if observed_at_ms == 0 {
        return Err(invalid_data("invalid hook-ingest fault time"));
    }
    if let Some(identity) = integration.as_ref() {
        validate_identity(identity)?;
    }
    let _lock = StateLock::acquire(state_dir)?;
    let (entries, truncated) = fault_dir.list_names(MAX_FAULT_DIRECTORY_ENTRIES)?;
    let overflow_present = entries
        .iter()
        .any(|name| name == OsStr::new(FAULT_OVERFLOW_NAME));
    let overflow_invalid = overflow_present
        && match read_fault_file(fault_dir, OsStr::new(FAULT_OVERFLOW_NAME)) {
            Ok(fault) => !valid_ingest_fault(&fault),
            Err(_) => true,
        };
    if overflow_invalid {
        let commit_id = record_overflow_fault(fault_dir, observed_at_ms, integration)?;
        let file = fault_dir.open_private_rw_existing(OsStr::new(FAULT_OVERFLOW_NAME))?;
        lock_marker(&file)?;
        return ingest_guard(
            fault_dir,
            OsString::from(FAULT_OVERFLOW_NAME),
            file,
            &commit_id,
            false,
        );
    }
    let count = entries
        .iter()
        .filter(|name| {
            name.to_str().is_some_and(|name| {
                valid_fault_filename(name) || valid_launcher_fault_filename(name)
            })
        })
        .count();
    if truncated || count >= MAX_FAULT_FILES {
        let commit_id = record_overflow_fault(fault_dir, observed_at_ms, integration)?;
        let file = fault_dir.open_private_rw_existing(OsStr::new(FAULT_OVERFLOW_NAME))?;
        lock_marker(&file)?;
        return ingest_guard(
            fault_dir,
            OsString::from(FAULT_OVERFLOW_NAME),
            file,
            &commit_id,
            false,
        );
    }
    let commit_id = new_ingest_commit_id()?;
    let fault = HookIngestFault {
        schema_version: HOOK_STATE_SCHEMA_VERSION,
        integration,
        observed_at_ms,
        commit_id: commit_id.clone(),
    };
    let bytes = encode_fault(&fault)?;
    for _ in 0..8 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let name = OsString::from(format!("{FAULT_PREFIX}{}.json", hex(&random)));
        match fault_dir.create_private_new(&name, &bytes) {
            Ok(file) => {
                lock_marker(&file)?;
                return ingest_guard(fault_dir, name, file, &commit_id, true);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    record_overflow_fault(fault_dir, observed_at_ms, fault.integration)?;
    Err(invalid_data("cannot allocate a unique hook fault marker"))
}

/// Return a complete, validated generation set for conservative artifact GC.
/// A malformed/truncated directory makes reclamation ineligible rather than
/// letting cleanup erase evidence it cannot associate with a dead process.
fn generations_for_artifact_reclamation(
    state_dir: &SecureDirectory,
) -> io::Result<Option<Vec<HookSessionState>>> {
    let (names, truncated) = state_dir.list_names(MAX_DIRECTORY_ENTRIES)?;
    if truncated {
        return Ok(None);
    }

    let mut states = Vec::new();
    let mut saw_lock = false;
    let mut saw_state = false;
    for name in names {
        let Some(name_str) = name.to_str() else {
            return Ok(None);
        };
        if name_str == ".lock" {
            saw_lock = true;
            let Ok(file) = state_dir.open_private_read(&name) else {
                return Ok(None);
            };
            if file.metadata()?.len() != 0 {
                return Ok(None);
            }
            continue;
        }
        if name_str == FAULT_DIR_NAME || valid_temporary_filename(name_str) {
            continue;
        }
        if !valid_state_filename(name_str) {
            return Ok(None);
        }
        saw_state = true;
        let Some(state) = read_state_file(state_dir, &name)? else {
            return Ok(None);
        };
        if validate_state(&state).is_err()
            || name_str != format!("state-{}.json", state.generation_id)
        {
            return Ok(None);
        }
        states.push(state);
    }
    if saw_state && !saw_lock {
        return Ok(None);
    }
    states.sort_by(|left, right| left.generation_id.cmp(&right.generation_id));
    Ok(Some(states))
}

fn process_death_snapshot(states: &[HookSessionState]) -> BTreeMap<(u32, String), bool> {
    let mut process_cache = BTreeMap::new();
    for state in states {
        let key = (state.process.pid, state.process.incarnation.clone());
        process_cache
            .entry(key)
            .or_insert_with(|| state.process.confirmed_gone());
    }
    process_cache
}

fn stale_artifact_cutoff(file: &File, now_ms: u64) -> Option<u64> {
    let modified_at_ms = file
        .metadata()
        .ok()?
        .modified()
        .ok()
        .and_then(system_time_ms)?;
    (modified_at_ms != 0 && now_ms.saturating_sub(modified_at_ms) > TERMINAL_RETENTION_MS)
        .then_some(modified_at_ms)
}

fn affected_generations_confirmed_gone(
    states: &[HookSessionState],
    cutoff_ms: u64,
    process_cache: &BTreeMap<(u32, String), bool>,
) -> bool {
    states
        .iter()
        .filter(|state| state.created_at_ms <= cutoff_ms)
        .all(|state| {
            let key = (state.process.pid, state.process.incarnation.clone());
            process_cache.get(&key).copied().unwrap_or(false)
        })
}

fn reclaim_stale_temporary_files(
    directory: &SecureDirectory,
    listing_bound: usize,
    states: &[HookSessionState],
    now_ms: u64,
    process_cache: &BTreeMap<(u32, String), bool>,
) -> io::Result<()> {
    let (names, _) = directory.list_names(listing_bound)?;
    for name in names {
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !valid_temporary_filename(name_str) {
            continue;
        }
        let Ok(file) = directory.open_private_read(&name) else {
            continue;
        };
        let Some(cutoff_ms) = stale_artifact_cutoff(&file, now_ms) else {
            continue;
        };
        if affected_generations_confirmed_gone(states, cutoff_ms, process_cache) {
            let _ = directory.remove_if_same(&name, &file);
        }
    }
    Ok(())
}

fn reclaim_stale_launcher_markers(
    fault_dir: &SecureDirectory,
    states: &[HookSessionState],
    now_ms: u64,
    process_cache: &BTreeMap<(u32, String), bool>,
) -> io::Result<()> {
    let (names, _) = fault_dir.list_names(MAX_FAULT_DIRECTORY_ENTRIES)?;
    for name in names {
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !valid_launcher_fault_filename(name_str) {
            continue;
        }
        let Ok((file, bytes)) = fault_dir.read_private_bounded(&name, MAX_FAULT_BYTES) else {
            continue;
        };
        let Some(mut cutoff_ms) = stale_artifact_cutoff(&file, now_ms) else {
            continue;
        };

        match serde_json::from_slice::<HookIngestFault>(&bytes) {
            Ok(fault) if valid_ingest_fault(&fault) => {
                // A reused fixed fallback slot or theoretically colliding
                // unique basename has a new inode/mtime. Require both its
                // durable record and current inode to have crossed the 24h
                // eligibility window before considering it abandoned.
                if now_ms.saturating_sub(fault.observed_at_ms) <= TERMINAL_RETENTION_MS {
                    continue;
                }
                cutoff_ms = cutoff_ms.max(fault.observed_at_ms);
            }
            _ => {
                // Launchers necessarily leave an empty marker if the helper
                // never starts, and can leave a partial/legacy marker if it
                // dies while adopting it. The inode mtime is the only safe
                // content-free age for those malformed launcher records.
            }
        }

        if affected_generations_confirmed_gone(states, cutoff_ms, process_cache)
            && try_lock_marker(&file)?
        {
            let _ = fault_dir.remove_if_same(&name, &file);
        }
    }
    Ok(())
}

fn reclaim_stale_ingest_artifacts(
    state_dir: &SecureDirectory,
    fault_dir: &SecureDirectory,
    states: &[HookSessionState],
    now_ms: u64,
    process_cache: &BTreeMap<(u32, String), bool>,
) -> io::Result<()> {
    reclaim_stale_temporary_files(
        state_dir,
        MAX_DIRECTORY_ENTRIES,
        states,
        now_ms,
        process_cache,
    )?;
    reclaim_stale_temporary_files(
        fault_dir,
        MAX_FAULT_DIRECTORY_ENTRIES,
        states,
        now_ms,
        process_cache,
    )?;
    reclaim_stale_launcher_markers(fault_dir, states, now_ms, process_cache)
}

fn reclaim_expired_faults(
    fault_dir: &SecureDirectory,
    states: &[HookSessionState],
    now_ms: u64,
    process_cache: &BTreeMap<(u32, String), bool>,
) -> io::Result<()> {
    let (fault_names, _) = fault_dir.list_names(MAX_FAULT_DIRECTORY_ENTRIES)?;
    for name in fault_names {
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if valid_launcher_fault_filename(name_str) {
            // Launcher records have inode-age and fixed-slot reuse rules that
            // are stricter than ordinary hook faults. They are handled by
            // reclaim_stale_launcher_markers and never by this path.
            continue;
        }
        if !valid_fault_filename(name_str) || name_str == FAULT_OVERFLOW_NAME {
            continue;
        }
        let Ok((file, bytes)) = fault_dir.read_private_bounded(&name, MAX_FAULT_BYTES) else {
            continue;
        };
        let Ok(fault) = serde_json::from_slice::<HookIngestFault>(&bytes) else {
            continue;
        };
        if fault.schema_version != HOOK_STATE_SCHEMA_VERSION
            || fault.observed_at_ms == 0
            || fault
                .integration
                .as_ref()
                .is_some_and(|integration| validate_identity(integration).is_err())
            || now_ms.saturating_sub(fault.observed_at_ms) <= TERMINAL_RETENTION_MS
        {
            continue;
        }
        let still_relevant = states.iter().any(|state| {
            let integration_matches = fault.integration.as_ref().is_none_or(|integration| {
                integration.installation_id == state.integration.installation_id
            });
            let process_key = (state.process.pid, state.process.incarnation.clone());
            integration_matches
                && state.created_at_ms <= fault.observed_at_ms
                && !process_cache.get(&process_key).copied().unwrap_or(false)
        });
        if !still_relevant && try_lock_marker(&file)? {
            let _ = fault_dir.remove_if_same(&name, &file);
        }
    }
    Ok(())
}

fn system_time_ms(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

fn encode_fault(fault: &HookIngestFault) -> io::Result<Vec<u8>> {
    if !valid_ingest_fault(fault) {
        return Err(invalid_data("invalid hook fault shape"));
    }
    let bytes = serde_json::to_vec(fault)
        .map_err(|error| invalid_data(format!("cannot encode hook fault: {error}")))?;
    if bytes.len() as u64 > MAX_FAULT_BYTES {
        return Err(invalid_data("hook fault exceeds its storage bound"));
    }
    Ok(bytes)
}

fn record_overflow_fault(
    fault_dir: &Arc<SecureDirectory>,
    observed_at_ms: u64,
    integration: Option<IntegrationIdentity>,
) -> io::Result<String> {
    let old = read_fault_file(fault_dir, OsStr::new(FAULT_OVERFLOW_NAME))
        .ok()
        .filter(valid_ingest_fault);
    let commit_id = new_ingest_commit_id()?;
    let fault = HookIngestFault {
        schema_version: HOOK_STATE_SCHEMA_VERSION,
        integration: match old.as_ref() {
            // Once an overflow covers every integration, a later scoped
            // failure must never narrow it. Doing so could make an older
            // uncommitted launcher failure disappear for another generation.
            Some(HookIngestFault {
                integration: None, ..
            }) => None,
            Some(HookIngestFault {
                integration: Some(old_identity),
                ..
            }) if integration.as_ref() == Some(old_identity) => Some(old_identity.clone()),
            Some(_) => None,
            None => integration,
        },
        observed_at_ms: old
            .map(|fault| fault.observed_at_ms)
            .unwrap_or_default()
            .max(observed_at_ms),
        commit_id: commit_id.clone(),
    };
    fault_dir.atomic_replace(OsStr::new(FAULT_OVERFLOW_NAME), &encode_fault(&fault)?)?;
    Ok(commit_id)
}

fn is_root_boundary_event(event: &HookEvent) -> bool {
    event.agent_id.is_none()
        && matches!(
            event.kind,
            HookEventKind::SessionStart
                | HookEventKind::UserPromptSubmit
                | HookEventKind::PreCompact
                | HookEventKind::PostCompact
                | HookEventKind::Stop
                | HookEventKind::SessionEnd
        )
}

fn turn_matches(state: &HookSessionState, turn: Option<&str>) -> bool {
    required_id(turn).is_some() && state.active_turn_id.as_deref() == turn
}

fn clear_root_questions(state: &mut HookSessionState) {
    let root_questions = state
        .open_questions
        .iter()
        .filter(|tool_id| !state.question_agents.contains_key(*tool_id))
        .cloned()
        .collect::<Vec<_>>();
    for tool_id in root_questions {
        state.open_questions.remove(&tool_id);
        state.question_opened_at_ms.remove(&tool_id);
    }
    state.closed_questions.clear();
}

fn clear_agent_questions(state: &mut HookSessionState, agent: &str) {
    let agent_questions = state
        .question_agents
        .iter()
        .filter_map(|(tool_id, owner)| {
            (owner == agent && state.open_questions.contains(tool_id)).then_some(tool_id.clone())
        })
        .collect::<Vec<_>>();
    for tool_id in agent_questions {
        state.question_agents.remove(&tool_id);
        state.open_questions.remove(&tool_id);
        state.question_opened_at_ms.remove(&tool_id);
    }
}

fn required_id(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty() && value.len() <= MAX_ID_BYTES)
}

fn min_nonzero(current: u64, candidate: u64) -> u64 {
    if current == 0 {
        candidate
    } else {
        current.min(candidate)
    }
}

fn validate_event(event: &HookEvent) -> io::Result<()> {
    validate_identity(&event.integration)?;
    validate_id(&event.session_id, "session ID")?;
    if event.cwd.is_empty()
        || event.cwd.len() > MAX_CWD_BYTES
        || !Path::new(&event.cwd).is_absolute()
    {
        return Err(invalid_data("invalid hook cwd"));
    }
    for (value, label) in [
        (event.turn_id.as_deref(), "turn ID"),
        (event.tool_use_id.as_deref(), "tool-use ID"),
        (event.agent_id.as_deref(), "agent ID"),
    ] {
        if let Some(value) = value {
            validate_id(value, label)?;
        }
    }
    if event.observed_at_ms == 0
        || event.process.pid == 0
        || event.process.started_at_ms == 0
        || event.process.started_at_ms > event.observed_at_ms.saturating_add(60_000)
    {
        return Err(invalid_data(
            "hook event has no exact time/process identity",
        ));
    }
    validate_id(&event.process.incarnation, "process incarnation")?;
    if matches!(
        event.kind,
        HookEventKind::Stop | HookEventKind::SubagentStop
    ) != event.stop_hook_active.is_some()
    {
        return Err(invalid_data("invalid stop_hook_active lifecycle shape"));
    }
    if !valid_ingest_commit_proof(&event.ingest_marker_id) {
        return Err(invalid_data("invalid hook ingest commit proof"));
    }
    Ok(())
}

fn validate_identity(identity: &IntegrationIdentity) -> io::Result<()> {
    validate_id(&identity.hook_schema_revision, "hook schema revision")?;
    validate_digest(&identity.helper_digest)?;
    validate_id(&identity.installation_id, "installation ID")?;
    validate_digest(&identity.config_digest)?;
    Ok(())
}

fn validate_state(state: &HookSessionState) -> io::Result<()> {
    let invalid_optional_time =
        |value: u64| value != 0 && (value < state.created_at_ms || value > state.updated_at_ms);
    let expected_generation = state_key_parts(
        &state.integration.installation_id,
        &state.session_id,
        state.process.pid,
        &state.process.incarnation,
    );
    if state.schema_version != HOOK_STATE_SCHEMA_VERSION
        || state.generation_id.len() != 64
        || !state
            .generation_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || state.generation_id != expected_generation
        || state.created_at_ms == 0
        || state.updated_at_ms < state.created_at_ms
        || state.process.pid == 0
        || state.process.started_at_ms == 0
        || state.process.started_at_ms > state.created_at_ms.saturating_add(60_000)
        || state.last_root_event.is_none()
        || invalid_optional_time(state.last_root_boundary_at_ms)
        || state.last_root_boundary_at_ms > state.updated_at_ms
        || invalid_optional_time(state.ended_at_ms)
        || (state.first_confirmed_gone_at_ms != 0
            && state.first_confirmed_gone_at_ms < state.updated_at_ms)
        || invalid_optional_time(state.prompt_observed_at_ms)
        || invalid_optional_time(state.stop_observed_at_ms)
        || invalid_optional_time(state.permission_observed_at_ms)
        || (state.stop_turn_id.is_some() != state.stop_hook_active.is_some())
        || state
            .stop_turn_id
            .as_ref()
            .is_some_and(|turn| state.active_turn_id.as_ref() != Some(turn))
        || state.samples.len() > MAX_STATE_SAMPLES
        || state.open_tools.len() > MAX_OPEN_ITEMS
        || state.tool_opened_at_ms.len() > MAX_OPEN_ITEMS
        || state.open_child_tools.len() > MAX_OPEN_ITEMS
        || state.child_tool_opened_at_ms.len() > MAX_OPEN_ITEMS
        || state.closed_child_tools.len() > MAX_OPEN_ITEMS
        || state.open_subagents.len() > MAX_OPEN_ITEMS
        || state.subagent_opened_at_ms.len() > MAX_OPEN_ITEMS
        || state.provisional_stopped_subagents.len() > MAX_OPEN_ITEMS
        || state.subagent_stopped_at_ms.len() > MAX_OPEN_ITEMS
        || state.open_questions.len() > MAX_OPEN_ITEMS
        || state.question_opened_at_ms.len() > MAX_OPEN_ITEMS
        || state.closed_tools.len() > MAX_OPEN_ITEMS
        || state.closed_subagents.len() > MAX_OPEN_ITEMS
        || state.closed_questions.len() > MAX_OPEN_ITEMS
        || state.question_agents.len() > MAX_OPEN_ITEMS
        || state.child_permission_ambiguities.len() > MAX_OPEN_ITEMS
        || state.child_permission_observed_at_ms.len() > MAX_OPEN_ITEMS
        || (state.prompt_accepted != (state.prompt_observed_at_ms != 0))
        || (state.permission_ambiguity != (state.permission_observed_at_ms != 0))
        || (state.stop_turn_id.is_some() != (state.stop_observed_at_ms != 0))
        || state.samples.is_empty()
        || !state.tool_opened_at_ms.keys().eq(state.open_tools.keys())
        || !state
            .child_tool_opened_at_ms
            .keys()
            .eq(state.open_child_tools.keys())
        || !state
            .subagent_opened_at_ms
            .keys()
            .eq(state.open_subagents.iter())
        || !state
            .subagent_stopped_at_ms
            .keys()
            .eq(state.provisional_stopped_subagents.iter())
        || !state
            .question_opened_at_ms
            .keys()
            .eq(state.open_questions.iter())
        || !state
            .child_permission_observed_at_ms
            .keys()
            .eq(state.child_permission_ambiguities.iter())
        || state
            .open_tools
            .values()
            .any(|class| *class != HookToolClass::Ordinary)
        || state
            .open_child_tools
            .values()
            .chain(state.closed_child_tools.values())
            .any(|agent| !state.open_subagents.contains(agent))
        || state
            .question_agents
            .keys()
            .any(|tool_id| !state.open_questions.contains(tool_id))
        || state
            .question_agents
            .values()
            .any(|agent| !state.open_subagents.contains(agent))
        || state
            .child_permission_ambiguities
            .iter()
            .any(|agent| !state.open_subagents.contains(agent))
        || state
            .provisional_stopped_subagents
            .iter()
            .any(|agent| !state.open_subagents.contains(agent))
        || state.open_tools.keys().any(|key| {
            state.closed_tools.contains(key)
                || state.open_questions.contains(key)
                || state.closed_questions.contains(key)
        })
        || state
            .closed_tools
            .iter()
            .any(|key| state.open_questions.contains(key) || state.closed_questions.contains(key))
        || state.open_child_tools.keys().any(|key| {
            state.closed_child_tools.contains_key(key)
                || state.open_tools.contains_key(key)
                || state.closed_tools.contains(key)
                || state.open_questions.contains(key)
                || state.closed_questions.contains(key)
        })
        || state.closed_child_tools.keys().any(|key| {
            state.open_tools.contains_key(key)
                || state.closed_tools.contains(key)
                || state.open_questions.contains(key)
                || state.closed_questions.contains(key)
        })
        || state
            .open_subagents
            .iter()
            .any(|key| state.closed_subagents.contains(key))
        || state
            .open_questions
            .iter()
            .any(|key| state.closed_questions.contains(key))
        || state
            .tool_opened_at_ms
            .values()
            .chain(state.child_tool_opened_at_ms.values())
            .chain(state.subagent_opened_at_ms.values())
            .chain(state.subagent_stopped_at_ms.values())
            .chain(state.question_opened_at_ms.values())
            .chain(state.child_permission_observed_at_ms.values())
            .any(|at| *at < state.created_at_ms || *at > state.updated_at_ms)
        || state.samples.iter().any(|sample| {
            sample.observed_at_ms < state.created_at_ms
                || sample.observed_at_ms > state.updated_at_ms
                || sample.status != SessionStatus::Unknown
        })
        || state
            .samples
            .windows(2)
            .any(|samples| samples[0].observed_at_ms > samples[1].observed_at_ms)
    {
        return Err(invalid_data("invalid hook state shape"));
    }
    validate_identity(&state.integration)?;
    validate_id(&state.session_id, "session ID")?;
    validate_id(&state.process.incarnation, "process incarnation")?;
    for (value, label) in [
        (state.stop_turn_id.as_deref(), "stop turn ID"),
        (state.active_turn_id.as_deref(), "active turn ID"),
    ] {
        if let Some(value) = value {
            validate_id(value, label)?;
        }
    }
    if state.completed_ingests.is_empty()
        || state.completed_ingests.len() > MAX_STATE_SAMPLES
        || state
            .completed_ingests
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != state.completed_ingests.len()
        || state
            .completed_ingests
            .iter()
            .any(|proof| !valid_ingest_commit_proof(proof))
    {
        return Err(invalid_data("invalid completed hook marker identity"));
    }
    if state.cwd.is_empty()
        || state.cwd.len() > MAX_CWD_BYTES
        || !Path::new(&state.cwd).is_absolute()
    {
        return Err(invalid_data("invalid hook state cwd"));
    }
    for key in state
        .open_tools
        .keys()
        .chain(state.open_child_tools.keys())
        .chain(state.open_child_tools.values())
        .chain(state.closed_child_tools.keys())
        .chain(state.closed_child_tools.values())
        .chain(state.open_subagents.iter())
        .chain(state.provisional_stopped_subagents.iter())
        .chain(state.open_questions.iter())
        .chain(state.closed_tools.iter())
        .chain(state.closed_subagents.iter())
        .chain(state.closed_questions.iter())
        .chain(state.question_agents.values())
        .chain(state.child_permission_ambiguities.iter())
    {
        validate_id(key, "open lifecycle ID")?;
    }
    Ok(())
}

fn validate_id(value: &str, label: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(|ch| ch.is_control()) {
        Err(invalid_data(format!("invalid {label}")))
    } else {
        Ok(())
    }
}

fn validate_digest(value: &str) -> io::Result<()> {
    if value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(invalid_data("invalid SHA-256 identity"))
    }
}

fn state_key(event: &HookEvent) -> String {
    state_key_parts(
        &event.integration.installation_id,
        &event.session_id,
        event.process.pid,
        &event.process.incarnation,
    )
}

fn state_key_parts(installation_id: &str, session_id: &str, pid: u32, incarnation: &str) -> String {
    let mut hash = Sha256::new();
    for value in [
        installation_id.as_bytes(),
        session_id.as_bytes(),
        pid.to_string().as_bytes(),
        incarnation.as_bytes(),
    ] {
        hash.update(value);
        hash.update([0]);
    }
    hex(&hash.finalize())
}

fn valid_state_filename(name: &str) -> bool {
    name.len() == "state-.json".len() + 64
        && name.starts_with("state-")
        && name.ends_with(".json")
        && name[6..name.len() - 5]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn valid_fault_filename(name: &str) -> bool {
    name == FAULT_OVERFLOW_NAME
        || (name.len() == FAULT_PREFIX.len() + 32 + ".json".len()
            && name.starts_with(FAULT_PREFIX)
            && name.ends_with(".json")
            && name[FAULT_PREFIX.len()..name.len() - ".json".len()]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_launcher_fault_filename(name: &str) -> bool {
    if fixed_launcher_slot(name).is_some() {
        return true;
    }
    let Some(body) = name.strip_prefix(LAUNCH_FAULT_PREFIX) else {
        return false;
    };
    let Some((pid, nonce)) = body.split_once(LAUNCH_UNIQUE_SEPARATOR) else {
        return false;
    };
    let Ok(parsed_pid) = pid.parse::<u32>() else {
        return false;
    };
    parsed_pid != 0
        && parsed_pid.to_string() == pid
        && nonce.len() == LAUNCH_UNIQUE_NONCE_LEN
        && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn fixed_launcher_slot(name: &str) -> Option<u8> {
    let body = name
        .strip_prefix(LAUNCH_FAULT_PREFIX)?
        .strip_suffix(LAUNCH_FAULT_SUFFIX)?;
    let (slot_text, nonce) = body.split_once('-')?;
    if nonce != LAUNCH_FAULT_SLOT_NONCE {
        return None;
    }
    let slot = slot_text.parse::<u8>().ok()?;
    (slot < LAUNCH_FAULT_SLOT_COUNT && slot.to_string() == slot_text).then_some(slot)
}

fn valid_temporary_filename(name: &str) -> bool {
    name.len() == ".tmp-".len() + 32
        && name.starts_with(".tmp-")
        && name[".tmp-".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn validate_plugin_data_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some("abtop-abtop-local")
        || path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("data")
        || path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("plugins")
    {
        return Err(invalid_data("invalid abtop Codex plugin-data root"));
    }
    Ok(())
}

fn read_fault_file(dir: &SecureDirectory, name: &OsStr) -> io::Result<HookIngestFault> {
    let (_, bytes) = dir.read_private_bounded(name, MAX_FAULT_BYTES)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("invalid hook fault JSON: {error}")))
}

fn read_state_file(dir: &SecureDirectory, name: &OsStr) -> io::Result<Option<HookSessionState>> {
    Ok(read_state_file_with_file(dir, name)?.map(|(state, _)| state))
}

fn read_state_file_with_file(
    dir: &SecureDirectory,
    name: &OsStr,
) -> io::Result<Option<(HookSessionState, File)>> {
    let (file, bytes) = match dir.read_private_bounded(name, MAX_STATE_BYTES) {
        Ok(result) => result,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let state = serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("invalid hook state JSON: {error}")))?;
    Ok(Some((state, file)))
}

fn write_state_file(
    dir: &SecureDirectory,
    name: &OsStr,
    state: &HookSessionState,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(state)
        .map_err(|error| invalid_data(format!("cannot encode hook state: {error}")))?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(invalid_data("hook state exceeds its storage bound"));
    }
    dir.atomic_replace(name, &bytes)
}

struct SecureDirectory {
    path: PathBuf,
    #[cfg(unix)]
    file: File,
}

impl SecureDirectory {
    fn open_path(path: &Path, allow_fix_mode: bool) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd;
            use std::os::unix::ffi::OsStrExt;
            let mut components = path.components();
            if components.next() != Some(std::path::Component::RootDir) {
                return Err(invalid_data("private directory path is not absolute"));
            }
            let root = CString::new("/").expect("root has no NUL");
            // SAFETY: root is a valid NUL-terminated path and the returned fd
            // is immediately owned by File.
            let root_fd = unsafe {
                libc::open(
                    root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if root_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: open returned a new owned descriptor.
            let mut file = unsafe { File::from_raw_fd(root_fd) };
            for component in components {
                let std::path::Component::Normal(name) = component else {
                    return Err(invalid_data("unsafe private directory component"));
                };
                let name = CString::new(name.as_bytes())
                    .map_err(|_| invalid_data("private directory component contains NUL"))?;
                file = openat_directory(&file, &name)?;
            }
            validate_private_directory_file(path, &file, allow_fix_mode)?;
            Ok(Self {
                path: path.to_path_buf(),
                file,
            })
        }
        #[cfg(not(unix))]
        {
            validate_private_directory(path, allow_fix_mode)?;
            Ok(Self {
                path: path.to_path_buf(),
            })
        }
    }

    fn open_or_create_child(&self, name: &str) -> io::Result<Self> {
        validate_component_name(OsStr::new(name))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name_c = os_cstring(OsStr::new(name))?;
            // SAFETY: descriptor/name are valid; mkdirat is contained by the
            // retained parent descriptor.
            let result = unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            let file = openat_directory(&self.file, &name_c)?;
            let path = self.path.join(name);
            validate_private_directory_file(&path, &file, false)?;
            self.sync()?;
            Ok(Self { path, file })
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            ensure_private_directory(&path)?;
            Ok(Self { path })
        }
    }

    fn open_existing_child(&self, name: &str) -> io::Result<Self> {
        validate_component_name(OsStr::new(name))?;
        #[cfg(unix)]
        {
            let name_c = os_cstring(OsStr::new(name))?;
            let file = openat_directory(&self.file, &name_c)?;
            let path = self.path.join(name);
            validate_private_directory_file(&path, &file, false)?;
            Ok(Self { path, file })
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            validate_private_directory(&path, false)?;
            Ok(Self { path })
        }
    }

    fn list_names(&self, maximum: usize) -> io::Result<(Vec<OsString>, bool)> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStringExt;
            let dot = CString::new(".").expect("dot has no NUL");
            // SAFETY: openat on `.` creates a new open file description with
            // an independent directory cursor; fdopendir consumes it below.
            let listing_fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if listing_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: listing_fd is a valid directory descriptor.
            let directory = unsafe { libc::fdopendir(listing_fd) };
            if directory.is_null() {
                // SAFETY: fdopendir failed and did not consume the descriptor.
                unsafe { libc::close(listing_fd) };
                return Err(io::Error::last_os_error());
            }
            let mut names = Vec::new();
            let mut truncated = false;
            loop {
                clear_errno();
                // SAFETY: directory remains valid until closedir.
                let entry = unsafe { libc::readdir(directory) };
                if entry.is_null() {
                    let error = io::Error::last_os_error();
                    // SAFETY: directory is live and uniquely owned here.
                    unsafe { libc::closedir(directory) };
                    if error.raw_os_error().unwrap_or(0) != 0 {
                        return Err(error);
                    }
                    break;
                }
                // SAFETY: readdir returns a live dirent with a NUL-terminated
                // d_name for the duration of this iteration.
                let bytes = unsafe {
                    std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())
                        .to_bytes()
                        .to_vec()
                };
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                if names.len() >= maximum {
                    truncated = true;
                    continue;
                }
                names.push(OsString::from_vec(bytes));
            }
            Ok((names, truncated))
        }
        #[cfg(not(unix))]
        {
            let mut names = Vec::new();
            let mut truncated = false;
            for entry in fs::read_dir(&self.path)? {
                let entry = entry?;
                if names.len() >= maximum {
                    truncated = true;
                } else {
                    names.push(entry.file_name());
                }
            }
            Ok((names, truncated))
        }
    }

    fn read_private_bounded(&self, name: &OsStr, maximum: u64) -> io::Result<(File, Vec<u8>)> {
        let file = self.open_private_read(name)?;
        let metadata = file.metadata()?;
        validate_private_regular_metadata(&self.path.join(name), &metadata)?;
        if metadata.len() > maximum {
            return Err(invalid_data("oversized private hook file"));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&file).take(maximum + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > maximum {
            return Err(invalid_data("oversized private hook file"));
        }
        Ok((file, bytes))
    }

    fn open_private_read(&self, name: &OsStr) -> io::Result<File> {
        validate_component_name(name)?;
        #[cfg(unix)]
        {
            openat_regular(&self.file, name, libc::O_RDONLY, 0)
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            let file = OpenOptions::new().read(true).open(&path)?;
            validate_private_regular_metadata(&path, &file.metadata()?)?;
            Ok(file)
        }
    }

    fn open_private_rw_existing(&self, name: &OsStr) -> io::Result<File> {
        validate_component_name(name)?;
        #[cfg(unix)]
        {
            openat_regular(&self.file, name, libc::O_RDWR, 0)
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            validate_private_regular_metadata(&path, &file.metadata()?)?;
            Ok(file)
        }
    }

    fn create_private_new(&self, name: &OsStr, bytes: &[u8]) -> io::Result<File> {
        validate_component_name(name)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(invalid_data("private hook file exceeds storage bound"));
        }
        #[cfg(unix)]
        let mut file = openat_regular(
            &self.file,
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        #[cfg(not(unix))]
        let mut file = {
            let path = self.path.join(name);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            validate_private_regular_metadata(&path, &file.metadata()?)?;
            file
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        self.sync()?;
        Ok(file)
    }

    fn atomic_replace(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
        validate_component_name(name)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return Err(invalid_data("private hook file exceeds storage bound"));
        }
        match self.open_private_read(name) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let (entries, truncated) = self.list_names(MAX_DIRECTORY_ENTRIES)?;
        let temporary_count = entries
            .iter()
            .filter(|entry| entry.to_str().is_some_and(valid_temporary_filename))
            .count();
        if truncated || temporary_count >= MAX_TEMP_FILES {
            return Err(invalid_data(
                "private hook temporary-file capacity exhausted",
            ));
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let temporary_name = OsString::from(format!(".tmp-{}", hex(&random)));
        let temporary = self.create_private_new(&temporary_name, bytes)?;
        drop(temporary);
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let old = os_cstring(&temporary_name)?;
            let new = os_cstring(name)?;
            // SAFETY: both names are validated single components and both
            // descriptors are the same retained private directory.
            let result = unsafe {
                libc::renameat(
                    self.file.as_raw_fd(),
                    old.as_ptr(),
                    self.file.as_raw_fd(),
                    new.as_ptr(),
                )
            };
            if result != 0 {
                let error = io::Error::last_os_error();
                let _ = self.remove(&temporary_name);
                return Err(error);
            }
        }
        #[cfg(not(unix))]
        {
            let temporary_path = self.path.join(&temporary_name);
            if let Err(error) = fs::rename(&temporary_path, self.path.join(name)) {
                let _ = fs::remove_file(temporary_path);
                return Err(error);
            }
        }
        self.sync()
    }

    fn remove_if_same(&self, name: &OsStr, expected: &File) -> io::Result<()> {
        let current = self.open_private_read(name)?;
        if !same_file(&current.metadata()?, &expected.metadata()?) {
            return Err(invalid_data("private hook file changed before removal"));
        }
        drop(current);
        self.remove(name)
    }

    fn remove(&self, name: &OsStr) -> io::Result<()> {
        validate_component_name(name)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = os_cstring(name)?;
            // SAFETY: name is a validated component and dirfd is retained.
            let result = unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(unix))]
        fs::remove_file(self.path.join(name))?;
        self.sync()
    }

    fn sync(&self) -> io::Result<()> {
        #[cfg(unix)]
        self.file.sync_all()?;
        #[cfg(not(unix))]
        File::open(&self.path)?.sync_all()?;
        Ok(())
    }
}

#[cfg(unix)]
fn openat_directory(parent: &File, name: &CString) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: parent/name are valid and returned fd is immediately owned.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn openat_regular(parent: &File, name: &OsStr, flags: libc::c_int, mode: u32) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = os_cstring(name)?;
    // SAFETY: parent/name are valid and returned fd is immediately owned.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    validate_private_regular_metadata(
        Path::new(name.to_str().unwrap_or("<private>")),
        &file.metadata()?,
    )?;
    Ok(file)
}

#[cfg(unix)]
fn os_cstring(name: &OsStr) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(name.as_bytes()).map_err(|_| invalid_data("private filename contains NUL"))
}

fn validate_component_name(name: &OsStr) -> io::Result<()> {
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(invalid_data("invalid private filename"));
    }
    let path = Path::new(name);
    if path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(invalid_data("private filename is not one component"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_directory_file(
    path: &Path,
    file: &File,
    allow_fix_mode: bool,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mut metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(invalid_data(format!("unsafe directory {}", path.display())));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        if !allow_fix_mode {
            return Err(invalid_data(format!(
                "directory {} is not private",
                path.display()
            )));
        }
        // SAFETY: descriptor is retained and mode contains only permission bits.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } != 0 {
            return Err(io::Error::last_os_error());
        }
        metadata = file.metadata()?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid_data(format!(
                "directory {} is not private",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_directory(path: &Path, _allow_fix_mode: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_data(format!("unsafe directory {}", path.display())));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_directory(path, false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            validate_private_directory(path, false)
        }
        Err(error) => Err(error),
    }
}

fn validate_private_regular_metadata(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(invalid_data(format!(
            "unsafe state file {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions or side effects.
        let expected = unsafe { libc::geteuid() };
        if metadata.uid() != expected
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid_data(format!(
                "unsafe state file {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        left.len() == right.len() && left.modified().ok() == right.modified().ok()
    }
}

#[cfg(unix)]
fn clear_errno() {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    unsafe {
        *libc::__error() = 0;
    }
}

struct StateLock {
    #[cfg(unix)]
    file: File,
    #[cfg(not(unix))]
    _file: File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl StateLock {
    fn acquire(directory: &SecureDirectory) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let file = match directory.open_private_rw_existing(OsStr::new(".lock")) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match directory.create_private_new(OsStr::new(".lock"), &[]) {
                        Ok(file) => file,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            directory.open_private_rw_existing(OsStr::new(".lock"))?
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
            lock_with_budget(&file)?;
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            let path = directory.path.join(".lock");
            for _ in 0..160 {
                match OpenOptions::new().write(true).create_new(true).open(&path) {
                    Ok(file) => return Ok(Self { _file: file, path }),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "hook-state lock is busy",
            ))
        }
    }
}

#[cfg(unix)]
fn lock_with_budget(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    for _ in 0..160 {
        // SAFETY: descriptor is live for the duration of the caller's guard.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK)
            && error.raw_os_error() != Some(libc::EAGAIN)
        {
            return Err(error);
        }
        thread::sleep(Duration::from_millis(5));
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "hook-state lock is busy",
    ))
}

fn lock_marker(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    lock_with_budget(file)?;
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn try_lock_marker(file: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: descriptor remains live through the guarded removal. The
        // acquired lock is released automatically when that descriptor drops.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK)
            || error.raw_os_error() == Some(libc::EAGAIN)
        {
            return Ok(false);
        }
        Err(error)
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(false)
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: this guard owns the live descriptor.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
        #[cfg(not(unix))]
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> IntegrationIdentity {
        IntegrationIdentity {
            hook_schema_revision: "1".into(),
            helper_digest: format!("sha256:{}", "1".repeat(64)),
            installation_id: "a".repeat(32),
            config_digest: format!("sha256:{}", "2".repeat(64)),
            complete_hook_set: true,
        }
    }

    fn process_identity() -> HookProcessIdentity {
        HookProcessIdentity {
            pid: std::process::id(),
            started_at_ms: 1,
            incarnation: process::get_process_incarnation(std::process::id())
                .unwrap_or_else(|| "test-incarnation".into()),
            shared_host: false,
            launch_config_ambiguous: false,
        }
    }

    fn test_commit_proof(marker_name: &str) -> String {
        ingest_commit_proof(marker_name, &"c".repeat(INGEST_COMMIT_ID_LEN)).unwrap()
    }

    #[cfg(unix)]
    fn marker_name_from_proof(proof: &str) -> &str {
        parse_ingest_commit_proof(proof).unwrap().0
    }

    fn event(kind: HookEventKind, at: u64) -> HookEvent {
        HookEvent {
            kind,
            session_id: "session-a".into(),
            cwd: "/tmp/project".into(),
            turn_id: None,
            tool_use_id: None,
            tool_class: None,
            agent_id: None,
            session_start_source: (kind == HookEventKind::SessionStart)
                .then_some(SessionStartSource::Startup),
            stop_hook_active: matches!(kind, HookEventKind::Stop | HookEventKind::SubagentStop)
                .then_some(false),
            ingest_marker_id: test_commit_proof(&format!("{FAULT_PREFIX}{}.json", "0".repeat(32))),
            observed_at_ms: at,
            process: process_identity(),
            integration: identity(),
        }
    }

    #[cfg(unix)]
    fn private_plugin_data(temp: &tempfile::TempDir) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let root = fs::canonicalize(temp.path()).unwrap();
        let plugin_data = root.join("plugins/data/abtop-abtop-local");
        fs::create_dir_all(&plugin_data).unwrap();
        fs::set_permissions(&plugin_data, fs::Permissions::from_mode(0o700)).unwrap();
        plugin_data
    }

    #[cfg(unix)]
    fn set_modified_ms(path: &Path, at_ms: u64) {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis(at_ms))
            .unwrap();
    }

    #[cfg(unix)]
    fn gone_process_identity() -> HookProcessIdentity {
        HookProcessIdentity {
            pid: 2_000_000_000,
            started_at_ms: 1,
            incarnation: "gone-incarnation".into(),
            shared_host: false,
            launch_config_ambiguous: false,
        }
    }

    #[test]
    fn lifecycle_projection_is_strict() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookEventGap)
        );

        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);
        assert_eq!(state.projection(), HookProjection::TurnOpen);
        assert_eq!(state.prompt_observed_at_ms, 20);

        let mut pre = event(HookEventKind::PreToolUse, 30);
        pre.turn_id = Some("turn-a".into());
        pre.tool_use_id = Some("call-a".into());
        pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&pre);
        assert!(matches!(state.projection(), HookProjection::ToolOpen(_)));
        assert_eq!(state.tool_opened_at_ms.get("call-a"), Some(&30));

        let mut permission = event(HookEventKind::PermissionRequest, 40);
        permission.turn_id = Some("turn-a".into());
        state.apply(&permission);
        assert_eq!(state.permission_observed_at_ms, 40);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookInteractionResolutionUnavailable)
        );
    }

    #[test]
    fn missing_tool_close_is_a_sticky_gap() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);
        let mut stop = event(HookEventKind::Stop, 30);
        stop.turn_id = Some("turn-b".into());
        state.apply(&stop);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookEventGap)
        );
    }

    #[test]
    fn exact_duplicate_tool_edges_are_idempotent() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);

        let mut pre = event(HookEventKind::PreToolUse, 30);
        pre.turn_id = Some("turn-a".into());
        pre.tool_use_id = Some("call-a".into());
        pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&pre);
        state.apply(&pre);
        assert!(matches!(state.projection(), HookProjection::ToolOpen(_)));

        let mut post = event(HookEventKind::PostToolUse, 40);
        post.turn_id = Some("turn-a".into());
        post.tool_use_id = Some("call-a".into());
        post.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&post);
        state.apply(&post);
        assert_eq!(state.projection(), HookProjection::TurnOpen);
    }

    #[test]
    fn same_turn_steer_preserves_open_work() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);
        let mut pre = event(HookEventKind::PreToolUse, 30);
        pre.turn_id = Some("turn-a".into());
        pre.tool_use_id = Some("call-a".into());
        pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&pre);

        let mut steer = event(HookEventKind::UserPromptSubmit, 40);
        steer.turn_id = Some("turn-a".into());
        state.apply(&steer);
        assert_eq!(
            state.open_tools.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from(["call-a".to_string()])
        );
        assert_eq!(state.sticky_fault, None);
        assert!(matches!(state.projection(), HookProjection::ToolOpen(_)));
    }

    #[test]
    fn stop_is_provisional_and_same_turn_work_reopens_it() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);

        let mut stop = event(HookEventKind::Stop, 30);
        stop.turn_id = Some("turn-a".into());
        state.apply(&stop);
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-a"));
        assert_eq!(state.projection(), HookProjection::TurnStopped);

        let mut pre = event(HookEventKind::PreToolUse, 40);
        pre.turn_id = Some("turn-a".into());
        pre.tool_use_id = Some("call-a".into());
        pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&pre);
        assert_eq!(state.stop_turn_id, None);
        assert!(matches!(state.projection(), HookProjection::ToolOpen(_)));

        let mut post = event(HookEventKind::PostToolUse, 50);
        post.turn_id = Some("turn-a".into());
        post.tool_use_id = Some("call-a".into());
        post.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&post);
        let mut repeated_stop = event(HookEventKind::Stop, 60);
        repeated_stop.turn_id = Some("turn-a".into());
        repeated_stop.stop_hook_active = Some(true);
        state.apply(&repeated_stop);
        state.apply(&repeated_stop);
        assert_eq!(state.projection(), HookProjection::TurnStopped);

        let mut next_prompt = event(HookEventKind::UserPromptSubmit, 70);
        next_prompt.turn_id = Some("turn-b".into());
        state.apply(&next_prompt);
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-b"));
        assert_eq!(state.stop_turn_id, None);
        assert_eq!(state.sticky_fault, None);
        assert_eq!(state.projection(), HookProjection::TurnOpen);
    }

    #[test]
    fn distinct_tool_edges_can_arrive_in_reverse_timestamp_order() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);

        let mut later_pre = event(HookEventKind::PreToolUse, 40);
        later_pre.turn_id = Some("turn-a".into());
        later_pre.tool_use_id = Some("call-b".into());
        later_pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&later_pre);
        let mut earlier_pre = event(HookEventKind::PreToolUse, 30);
        earlier_pre.turn_id = Some("turn-a".into());
        earlier_pre.tool_use_id = Some("call-a".into());
        earlier_pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&earlier_pre);

        let mut later_post = event(HookEventKind::PostToolUse, 60);
        later_post.turn_id = Some("turn-a".into());
        later_post.tool_use_id = Some("call-b".into());
        later_post.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&later_post);
        let mut earlier_post = event(HookEventKind::PostToolUse, 50);
        earlier_post.turn_id = Some("turn-a".into());
        earlier_post.tool_use_id = Some("call-a".into());
        earlier_post.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&earlier_post);

        assert_eq!(state.sticky_fault, None);
        assert_eq!(state.updated_at_ms, 60);
        assert_eq!(state.projection(), HookProjection::TurnOpen);
    }

    #[test]
    fn one_tool_cannot_close_before_its_own_open_edge() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);

        let mut pre = event(HookEventKind::PreToolUse, 40);
        pre.turn_id = Some("turn-a".into());
        pre.tool_use_id = Some("call-a".into());
        pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&pre);
        let mut post = event(HookEventKind::PostToolUse, 30);
        post.turn_id = Some("turn-a".into());
        post.tool_use_id = Some("call-a".into());
        post.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&post);

        assert_eq!(state.sticky_fault, Some(StatusReason::HookEventGap));
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookEventGap)
        );
    }

    #[test]
    fn persisted_state_is_bound_to_its_generation_and_time_envelope() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        assert!(validate_state(&state).is_ok());

        let mut wrong_generation = state.clone();
        wrong_generation.process.pid = wrong_generation.process.pid.saturating_add(1);
        assert!(validate_state(&wrong_generation).is_err());

        let mut future_edge = state;
        future_edge.last_root_boundary_at_ms = future_edge.updated_at_ms.saturating_add(1);
        assert!(validate_state(&future_edge).is_err());
    }

    #[test]
    fn persisted_turn_and_child_tool_ids_use_the_full_id_contract() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);

        let mut invalid_active = state.clone();
        invalid_active.active_turn_id = Some("turn\ncontrol".into());
        let error = validate_state(&invalid_active).unwrap_err();
        assert!(error.to_string().contains("active turn ID"));

        let oversized = "x".repeat(MAX_ID_BYTES + 1);
        let mut invalid_stop = state.clone();
        invalid_stop.active_turn_id = Some(oversized.clone());
        invalid_stop.stop_turn_id = Some(oversized);
        invalid_stop.stop_hook_active = Some(false);
        invalid_stop.stop_observed_at_ms = 20;
        let error = validate_state(&invalid_stop).unwrap_err();
        assert!(error.to_string().contains("stop turn ID"));

        let mut invalid_child = state;
        invalid_child.open_subagents.insert("child-a".into());
        invalid_child
            .subagent_opened_at_ms
            .insert("child-a".into(), 20);
        invalid_child
            .open_child_tools
            .insert("call-a".into(), "child\ncontrol".into());
        invalid_child
            .child_tool_opened_at_ms
            .insert("call-a".into(), 20);
        assert!(validate_state(&invalid_child).is_err());
    }

    #[test]
    fn permission_remains_unknown_until_the_root_turn_ends() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);
        let mut permission = event(HookEventKind::PermissionRequest, 30);
        permission.turn_id = Some("turn-a".into());
        state.apply(&permission);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookInteractionResolutionUnavailable)
        );

        let mut pre = event(HookEventKind::PreToolUse, 40);
        pre.turn_id = Some("turn-a".into());
        pre.tool_use_id = Some("call-a".into());
        pre.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&pre);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookInteractionResolutionUnavailable)
        );

        let mut post = event(HookEventKind::PostToolUse, 50);
        post.turn_id = Some("turn-a".into());
        post.tool_use_id = Some("call-a".into());
        post.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&post);
        let mut stop = event(HookEventKind::Stop, 60);
        stop.turn_id = Some("turn-a".into());
        state.apply(&stop);
        assert_eq!(state.projection(), HookProjection::TurnStopped);
    }

    #[test]
    fn compact_session_start_is_not_idle_proof() {
        let mut start = event(HookEventKind::SessionStart, 10);
        start.session_start_source = Some(SessionStartSource::Compact);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookEventGap)
        );

        let start = event(HookEventKind::SessionStart, 20);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 30);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);
        let mut pre_compact = event(HookEventKind::PreCompact, 40);
        pre_compact.turn_id = Some("turn-a".into());
        state.apply(&pre_compact);
        let mut post_compact = event(HookEventKind::PostCompact, 50);
        post_compact.turn_id = Some("turn-a".into());
        state.apply(&post_compact);
        let mut compact_start = event(HookEventKind::SessionStart, 60);
        compact_start.session_start_source = Some(SessionStartSource::Compact);
        state.apply(&compact_start);
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-a"));
        assert_eq!(state.projection(), HookProjection::TurnOpen);
    }

    #[test]
    fn child_hooks_may_interleave_with_root_compact_boundary() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);
        let mut pre = event(HookEventKind::PreCompact, 30);
        pre.turn_id = Some("turn-a".into());
        state.apply(&pre);
        let mut post = event(HookEventKind::PostCompact, 40);
        post.turn_id = Some("turn-a".into());
        state.apply(&post);

        let mut child_start = event(HookEventKind::SubagentStart, 45);
        child_start.agent_id = Some("child-a".into());
        child_start.turn_id = Some("child-turn".into());
        state.apply(&child_start);
        let mut child_prompt = event(HookEventKind::UserPromptSubmit, 46);
        child_prompt.agent_id = Some("child-a".into());
        child_prompt.turn_id = Some("child-turn".into());
        state.apply(&child_prompt);

        let mut compact_start = event(HookEventKind::SessionStart, 50);
        compact_start.session_start_source = Some(SessionStartSource::Compact);
        state.apply(&compact_start);
        assert_eq!(state.sticky_fault, None);
        assert_eq!(state.active_turn_id.as_deref(), Some("turn-a"));
        assert_eq!(
            state.projection(),
            HookProjection::SubagentOpen {
                active: BTreeSet::from(["child-a".to_string()]),
                provisional: BTreeSet::new(),
                root: HookRootProjection::TurnOpen,
            }
        );
    }

    #[test]
    fn subagent_events_fold_into_the_shared_root_state() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut root_prompt = event(HookEventKind::UserPromptSubmit, 20);
        root_prompt.turn_id = Some("turn-root".into());
        state.apply(&root_prompt);

        let mut child_start = event(HookEventKind::SubagentStart, 30);
        child_start.agent_id = Some("child-a".into());
        child_start.turn_id = Some("turn-child".into());
        state.apply(&child_start);
        assert_eq!(
            state.projection(),
            HookProjection::SubagentOpen {
                active: BTreeSet::from(["child-a".to_string()]),
                provisional: BTreeSet::new(),
                root: HookRootProjection::TurnOpen,
            }
        );

        let mut prompt = event(HookEventKind::UserPromptSubmit, 40);
        prompt.agent_id = Some("child-a".into());
        prompt.turn_id = Some("turn-child".into());
        state.apply(&prompt);
        assert_eq!(
            state.projection(),
            HookProjection::SubagentOpen {
                active: BTreeSet::from(["child-a".to_string()]),
                provisional: BTreeSet::new(),
                root: HookRootProjection::TurnOpen,
            }
        );

        let mut question = event(HookEventKind::PreToolUse, 50);
        question.agent_id = Some("child-a".into());
        question.turn_id = Some("turn-child".into());
        question.tool_use_id = Some("question-a".into());
        question.tool_class = Some(HookToolClass::RequestUserInput);
        state.apply(&question);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookInteractionResolutionUnavailable)
        );

        let mut stop = event(HookEventKind::SubagentStop, 60);
        stop.agent_id = Some("child-a".into());
        stop.turn_id = Some("turn-child".into());
        state.apply(&stop);
        assert_eq!(state.subagent_opened_at_ms.get("child-a"), Some(&30));
        assert_eq!(state.subagent_stopped_at_ms.get("child-a"), Some(&60));
        assert_eq!(
            state.projection(),
            HookProjection::SubagentOpen {
                active: BTreeSet::new(),
                provisional: BTreeSet::from(["child-a".to_string()]),
                root: HookRootProjection::TurnOpen,
            }
        );
        let mut repeated_stop = stop.clone();
        repeated_stop.observed_at_ms = 61;
        repeated_stop.stop_hook_active = Some(true);
        state.apply(&repeated_stop);
        assert_eq!(state.sticky_fault, None);

        let mut continuation = event(HookEventKind::UserPromptSubmit, 70);
        continuation.agent_id = Some("child-a".into());
        continuation.turn_id = Some("turn-child".into());
        state.apply(&continuation);
        assert!(!state.subagent_stopped_at_ms.contains_key("child-a"));
        assert_eq!(
            state.projection(),
            HookProjection::SubagentOpen {
                active: BTreeSet::from(["child-a".to_string()]),
                provisional: BTreeSet::new(),
                root: HookRootProjection::TurnOpen,
            }
        );
    }

    #[test]
    fn subagent_start_requires_an_active_root_prompt_and_child_turn() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut without_prompt = HookSessionState::new(&start);
        without_prompt.apply(&start);
        let mut child = event(HookEventKind::SubagentStart, 20);
        child.agent_id = Some("child-a".into());
        child.turn_id = Some("child-turn".into());
        without_prompt.apply(&child);
        assert_eq!(
            without_prompt.sticky_fault,
            Some(StatusReason::HookEventGap)
        );
        assert!(without_prompt.open_subagents.is_empty());

        let mut missing_child_turn = HookSessionState::new(&start);
        missing_child_turn.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("root-turn".into());
        missing_child_turn.apply(&prompt);
        let mut child = event(HookEventKind::SubagentStart, 30);
        child.agent_id = Some("child-a".into());
        missing_child_turn.apply(&child);
        assert_eq!(
            missing_child_turn.sticky_fault,
            Some(StatusReason::HookEventGap)
        );
        assert!(missing_child_turn.open_subagents.is_empty());

        let mut after_stop = HookSessionState::new(&start);
        after_stop.apply(&start);
        after_stop.apply(&prompt);
        let mut stop = event(HookEventKind::Stop, 30);
        stop.turn_id = Some("root-turn".into());
        after_stop.apply(&stop);
        let mut child = event(HookEventKind::SubagentStart, 40);
        child.agent_id = Some("child-a".into());
        child.turn_id = Some("child-turn".into());
        after_stop.apply(&child);
        assert_eq!(after_stop.sticky_fault, Some(StatusReason::HookEventGap));
        assert!(after_stop.open_subagents.is_empty());
    }

    #[test]
    fn multiple_child_tools_are_exact_and_block_tool_free_execution() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("root-turn".into());
        state.apply(&prompt);
        let mut child = event(HookEventKind::SubagentStart, 30);
        child.agent_id = Some("child-a".into());
        child.turn_id = Some("child-turn".into());
        state.apply(&child);

        for (tool_id, at) in [("child-call-a", 40), ("child-call-b", 41)] {
            let mut pre = event(HookEventKind::PreToolUse, at);
            pre.agent_id = Some("child-a".into());
            pre.turn_id = Some("child-turn".into());
            pre.tool_use_id = Some(tool_id.into());
            pre.tool_class = Some(HookToolClass::Ordinary);
            state.apply(&pre);
        }
        assert_eq!(state.open_child_tools.len(), 2);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookToolOpen)
        );
        assert!(validate_state(&state).is_ok());

        let mut first_post = event(HookEventKind::PostToolUse, 50);
        first_post.agent_id = Some("child-a".into());
        first_post.turn_id = Some("child-turn".into());
        first_post.tool_use_id = Some("child-call-a".into());
        first_post.tool_class = Some(HookToolClass::Ordinary);
        state.apply(&first_post);
        assert_eq!(state.open_child_tools.len(), 1);
        assert_eq!(
            state.projection(),
            HookProjection::Unknown(StatusReason::HookToolOpen)
        );

        let mut second_post = first_post.clone();
        second_post.observed_at_ms = 51;
        second_post.tool_use_id = Some("child-call-b".into());
        state.apply(&second_post);
        assert!(state.open_child_tools.is_empty());
        assert_eq!(state.closed_child_tools.len(), 2);
        assert_eq!(
            state.projection(),
            HookProjection::SubagentOpen {
                active: BTreeSet::from(["child-a".to_string()]),
                provisional: BTreeSet::new(),
                root: HookRootProjection::TurnOpen,
            }
        );
        assert!(validate_state(&state).is_ok());
    }

    #[test]
    fn child_stop_preserves_the_root_stop_fallback() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-root".into());
        state.apply(&prompt);
        let mut child_start = event(HookEventKind::SubagentStart, 30);
        child_start.agent_id = Some("child-a".into());
        child_start.turn_id = Some("turn-child".into());
        state.apply(&child_start);
        let mut root_stop = event(HookEventKind::Stop, 40);
        root_stop.turn_id = Some("turn-root".into());
        state.apply(&root_stop);
        let mut child_stop = event(HookEventKind::SubagentStop, 50);
        child_stop.agent_id = Some("child-a".into());
        child_stop.turn_id = Some("turn-child".into());
        state.apply(&child_stop);

        assert_eq!(state.stop_turn_id.as_deref(), Some("turn-root"));
        assert_eq!(
            state.projection(),
            HookProjection::SubagentOpen {
                active: BTreeSet::new(),
                provisional: BTreeSet::from(["child-a".to_string()]),
                root: HookRootProjection::TurnStopped,
            }
        );
    }

    #[test]
    fn sample_history_is_bounded() {
        let start = event(HookEventKind::SessionStart, 1);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        for at in 2..300 {
            let mut prompt = event(HookEventKind::UserPromptSubmit, at);
            prompt.turn_id = Some(format!("turn-{at}"));
            state.apply(&prompt);
        }
        assert_eq!(state.samples.len(), MAX_STATE_SAMPLES);
    }

    #[test]
    fn state_filenames_are_strict() {
        assert!(valid_state_filename(&format!(
            "state-{}.json",
            "a".repeat(64)
        )));
        assert!(!valid_state_filename("state-../secret.json"));
        assert!(!valid_state_filename(&format!(
            "state-{}.jsonl",
            "a".repeat(64)
        )));
        assert!(valid_launcher_fault_filename(
            "launch-12345-pending.AbCdEf0123456789"
        ));
        assert!(valid_launcher_fault_filename("launch-0-abtopv1.pending"));
        assert!(!valid_launcher_fault_filename(
            "launch-12345-AbCdEf0123456789.pending"
        ));
        assert!(!valid_launcher_fault_filename(
            "launch-0-pending.AbCdEf0123456789"
        ));
        assert!(!valid_launcher_fault_filename(
            "launch-012-pending.AbCdEf0123456789"
        ));
        let marker = format!("{FAULT_PREFIX}{}.json", "a".repeat(32));
        assert!(valid_ingest_commit_proof(&test_commit_proof(&marker)));
        assert!(!valid_ingest_commit_proof(&marker));
    }

    #[cfg(unix)]
    #[test]
    fn store_round_trip_is_private_and_rejects_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        let start = event(HookEventKind::SessionStart, 10);
        let state = store.fold(start).unwrap();

        let path = store.state_path(&state.generation_id);
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let scan = store.read_all(10).unwrap();
        assert_eq!(scan.rejected, 0);
        assert_eq!(scan.states, vec![state]);

        fs::remove_file(&path).unwrap();
        let target = temp.path().join("outside.json");
        fs::write(&target, b"{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        let scan = store.read_all(10).unwrap();
        assert_eq!(scan.rejected, 1);
        assert!(scan.states.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn failed_ingest_poison_persists_until_a_clean_generation() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();

        let failed = store.begin_ingest(20).unwrap();
        drop(failed);
        let poisoned = store.read_all(30).unwrap();
        assert_eq!(poisoned.states.len(), 1);
        assert_eq!(
            poisoned.states[0].sticky_fault,
            Some(StatusReason::HookEventGap)
        );

        store.fold(event(HookEventKind::SessionStart, 40)).unwrap();
        let recovered = store.read_all(50).unwrap();
        assert_eq!(recovered.states.len(), 1);
        assert_eq!(recovered.states[0].sticky_fault, None);

        let successful = store.begin_ingest(60).unwrap();
        successful.succeed().unwrap();
        let still_clean = store.read_all(70).unwrap();
        assert_eq!(still_clean.states[0].sticky_fault, None);
    }

    #[cfg(unix)]
    #[test]
    fn one_session_boundary_never_erases_another_sessions_failure() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        let mut first = event(HookEventKind::SessionStart, 10);
        first.session_id = "session-a".into();
        store.fold(first).unwrap();
        let mut second = event(HookEventKind::SessionStart, 11);
        second.session_id = "session-b".into();
        store.fold(second).unwrap();

        let failed = store.begin_ingest(20).unwrap();
        let marker = marker_name_from_proof(failed.marker_id().unwrap()).to_string();
        drop(failed);
        let mut clean_first = event(HookEventKind::SessionStart, 40);
        clean_first.session_id = "session-a".into();
        store.fold(clean_first).unwrap();

        assert!(store.fault_dir.path.join(marker).exists());
        let scan = store.read_all(50).unwrap();
        let first = scan
            .states
            .iter()
            .find(|state| state.session_id == "session-a")
            .unwrap();
        let second = scan
            .states
            .iter()
            .find(|state| state.session_id == "session-b")
            .unwrap();
        assert_eq!(first.sticky_fault, None);
        assert_eq!(second.sticky_fault, Some(StatusReason::HookEventGap));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_inflight_marker_survives_a_clean_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let guard = store.begin_ingest(20).unwrap();
        let marker = marker_name_from_proof(guard.marker_id().unwrap()).to_string();
        store.fold(event(HookEventKind::SessionStart, 30)).unwrap();
        assert!(store.fault_dir.path.join(&marker).exists());
        drop(guard);
        assert!(store.fault_dir.path.join(marker).exists());
    }

    #[cfg(unix)]
    #[test]
    fn committed_marker_is_ignored_after_crash_before_unlink() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let guard = store.begin_ingest(20).unwrap();
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        prompt.ingest_marker_id = guard.marker_id().unwrap().to_string();
        store.fold(prompt).unwrap();
        drop(guard);

        let scan = store.read_all(30).unwrap();
        assert_eq!(scan.rejected, 0);
        assert_eq!(scan.states.len(), 1);
        assert_eq!(scan.states[0].sticky_fault, None);
        assert_eq!(scan.states[0].projection(), HookProjection::TurnOpen);
    }

    #[cfg(unix)]
    #[test]
    fn reused_launcher_basename_cannot_inherit_an_old_commit() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let ingress = HookStateStore::prepare(&plugin_data).unwrap();
        let store = ingress.bind(identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let token = OsStr::new("launch-0-abtopv1.pending");

        drop(store.fault_dir.create_private_new(token, &[]).unwrap());
        let first_guard = ingress.adopt_launcher_marker(token, 20).unwrap();
        let first_proof = first_guard.marker_id().unwrap().to_string();
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        prompt.ingest_marker_id = first_proof.clone();
        store.fold(prompt).unwrap();
        first_guard.succeed().unwrap();

        // Reuse the fixed fallback basename for an invocation that fails
        // before folding. A later unrelated successful update must not let
        // the old basename-only proof hide this new failure.
        drop(store.fault_dir.create_private_new(token, &[]).unwrap());
        let failed_guard = ingress.adopt_launcher_marker(token, 30).unwrap();
        let failed_proof = failed_guard.marker_id().unwrap().to_string();
        assert_ne!(first_proof, failed_proof);
        drop(failed_guard);

        let later_guard = store.begin_ingest(40).unwrap();
        let mut later = event(HookEventKind::UserPromptSubmit, 40);
        later.turn_id = Some("turn-a".into());
        later.ingest_marker_id = later_guard.marker_id().unwrap().to_string();
        store.fold(later).unwrap();
        later_guard.succeed().unwrap();

        let scan = store.read_all(50).unwrap();
        assert_eq!(scan.rejected, 0);
        assert_eq!(
            scan.states[0].sticky_fault,
            Some(StatusReason::HookEventGap)
        );
        assert!(scan.states[0].completed_ingests.contains(&first_proof));
        assert!(!scan.states[0].completed_ingests.contains(&failed_proof));
    }

    #[cfg(unix)]
    #[test]
    fn clean_boundary_retains_commit_proof_for_other_live_generations() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        let mut first_start = event(HookEventKind::SessionStart, 10);
        first_start.session_id = "session-a".into();
        store.fold(first_start).unwrap();
        let mut second_start = event(HookEventKind::SessionStart, 11);
        second_start.session_id = "session-b".into();
        store.fold(second_start).unwrap();

        let guard = store.begin_ingest(20).unwrap();
        let committed_proof = guard.marker_id().unwrap().to_string();
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.session_id = "session-a".into();
        prompt.turn_id = Some("turn-a".into());
        prompt.ingest_marker_id = committed_proof.clone();
        store.fold(prompt).unwrap();

        let mut boundary = event(HookEventKind::SessionStart, 30);
        boundary.session_id = "session-a".into();
        let boundary_state = store.fold(boundary).unwrap();
        assert!(boundary_state.completed_ingests.contains(&committed_proof));
        drop(guard);

        let scan = store.read_all(40).unwrap();
        assert_eq!(scan.rejected, 0);
        assert!(scan.states.iter().all(|state| state.sticky_fault.is_none()));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_fault_without_commit_id_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let name = OsString::from(format!("{FAULT_PREFIX}{}.json", "9".repeat(32)));
        let legacy = serde_json::json!({
            "schema_version": HOOK_STATE_SCHEMA_VERSION,
            "integration": identity(),
            "observed_at_ms": 20
        });
        let path = store.fault_dir.path.join(&name);
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let scan = store.read_all(30).unwrap();
        assert_eq!(scan.rejected, 1);
        assert_eq!(
            scan.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn parallel_committed_markers_remain_independently_auditable() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let first_guard = store.begin_ingest(20).unwrap();
        let second_guard = store.begin_ingest(30).unwrap();

        let mut first = event(HookEventKind::UserPromptSubmit, 20);
        first.turn_id = Some("turn-a".into());
        first.ingest_marker_id = first_guard.marker_id().unwrap().to_string();
        store.fold(first).unwrap();
        let mut second = event(HookEventKind::UserPromptSubmit, 30);
        second.turn_id = Some("turn-a".into());
        second.ingest_marker_id = second_guard.marker_id().unwrap().to_string();
        store.fold(second).unwrap();
        drop(first_guard);
        drop(second_guard);

        let scan = store.read_all(40).unwrap();
        assert_eq!(scan.rejected, 0);
        assert_eq!(scan.states[0].sticky_fault, None);
        assert_eq!(scan.states[0].completed_ingests.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_fixed_slot_marker_is_global_unknown_until_adopted() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let ingress = HookStateStore::prepare(&plugin_data).unwrap();
        let store = ingress.bind(identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let token = "launch-0-abtopv1.pending";
        let marker_path = store.fault_dir.path.join(token);
        fs::write(&marker_path, []).unwrap();
        fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o600)).unwrap();
        let orphaned = store.read_all(15).unwrap();
        assert_eq!(orphaned.rejected, 1);
        assert_eq!(
            orphaned.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );

        let guard = ingress
            .adopt_launcher_marker(OsStr::new(token), 20)
            .unwrap();
        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        prompt.ingest_marker_id = guard.marker_id().unwrap().to_string();
        store.fold(prompt).unwrap();
        drop(guard);
        let committed = store.read_all(30).unwrap();
        assert_eq!(committed.rejected, 0);
        assert_eq!(committed.states[0].sticky_fault, None);
    }

    #[cfg(unix)]
    #[test]
    fn stale_fixed_slot_recovers_only_after_dead_generation_and_strict_grace() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let ingress = HookStateStore::prepare(&plugin_data).unwrap();
        let store = ingress.bind(identity()).unwrap();
        let mut start = event(HookEventKind::SessionStart, 1_000);
        start.process = gone_process_identity();
        store.fold(start).unwrap();

        let token = OsStr::new("launch-0-abtopv1.pending");
        let marker_path = store.fault_dir.path.join(token);
        drop(store.fault_dir.create_private_new(token, &[]).unwrap());
        set_modified_ms(&marker_path, 2_000);
        let marker_time = fs::metadata(&marker_path)
            .unwrap()
            .modified()
            .ok()
            .and_then(system_time_ms)
            .unwrap();

        let poisoned = store.read_all(marker_time).unwrap();
        assert_eq!(poisoned.rejected, 1);
        assert_eq!(
            poisoned.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );

        ingress
            .reclaim_stale_artifacts_after_drain(marker_time + TERMINAL_RETENTION_MS)
            .unwrap();
        assert!(
            marker_path.exists(),
            "the 24h boundary is still fail-closed"
        );
        ingress
            .reclaim_stale_artifacts_after_drain(marker_time + TERMINAL_RETENTION_MS + 1)
            .unwrap();
        assert!(!marker_path.exists());
        let recovered = store
            .read_all(marker_time + TERMINAL_RETENTION_MS + 1)
            .unwrap();
        assert_eq!(recovered.rejected, 0);
        assert_eq!(recovered.states[0].sticky_fault, None);

        // A later launcher can now create a new inode in the freed fixed slot
        // and its own helper can adopt that fresh marker.
        drop(store.fault_dir.create_private_new(token, &[]).unwrap());
        let guard = ingress
            .adopt_launcher_marker(token, marker_time + TERMINAL_RETENTION_MS + 2)
            .unwrap();
        assert_eq!(
            marker_name_from_proof(guard.marker_id().unwrap()),
            token.to_str().unwrap()
        );
        guard.succeed().unwrap();
        assert!(!marker_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn stale_artifacts_remain_poisoning_while_an_affected_generation_is_live() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let ingress = HookStateStore::prepare(&plugin_data).unwrap();
        let store = ingress.bind(identity()).unwrap();
        store
            .fold(event(HookEventKind::SessionStart, 1_000))
            .unwrap();

        let token = OsStr::new("launch-1-abtopv1.pending");
        let marker_path = store.fault_dir.path.join(token);
        drop(store.fault_dir.create_private_new(token, &[]).unwrap());
        set_modified_ms(&marker_path, 2_000);
        let marker_time = fs::metadata(&marker_path)
            .unwrap()
            .modified()
            .ok()
            .and_then(system_time_ms)
            .unwrap();

        ingress
            .reclaim_stale_artifacts_after_drain(marker_time + TERMINAL_RETENTION_MS + 1)
            .unwrap();
        assert!(marker_path.exists());
        let scan = store
            .read_all(marker_time + TERMINAL_RETENTION_MS + 1)
            .unwrap();
        assert_eq!(scan.rejected, 1);
        assert_eq!(
            scan.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_atomic_temps_recover_after_dead_generation_and_strict_grace() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let ingress = HookStateStore::prepare(&plugin_data).unwrap();
        let store = ingress.bind(identity()).unwrap();
        let mut start = event(HookEventKind::SessionStart, 1_000);
        start.process = gone_process_identity();
        store.fold(start).unwrap();

        let state_temp = OsStr::new(".tmp-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let fault_temp = OsStr::new(".tmp-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let state_temp_path = store.state_dir.path.join(state_temp);
        let fault_temp_path = store.fault_dir.path.join(fault_temp);
        drop(
            store
                .state_dir
                .create_private_new(state_temp, b"partial")
                .unwrap(),
        );
        drop(
            store
                .fault_dir
                .create_private_new(fault_temp, b"partial")
                .unwrap(),
        );
        set_modified_ms(&state_temp_path, 2_000);
        set_modified_ms(&fault_temp_path, 2_000);
        let artifact_time = fs::metadata(&state_temp_path)
            .unwrap()
            .modified()
            .ok()
            .and_then(system_time_ms)
            .unwrap();

        let poisoned = store.read_all(artifact_time).unwrap();
        assert_eq!(poisoned.rejected, 2);
        assert_eq!(
            poisoned.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );

        ingress
            .reclaim_stale_artifacts_after_drain(artifact_time + TERMINAL_RETENTION_MS)
            .unwrap();
        assert!(state_temp_path.exists());
        assert!(fault_temp_path.exists());
        ingress
            .reclaim_stale_artifacts_after_drain(artifact_time + TERMINAL_RETENTION_MS + 1)
            .unwrap();
        assert!(!state_temp_path.exists());
        assert!(!fault_temp_path.exists());
        let recovered = store
            .read_all(artifact_time + TERMINAL_RETENTION_MS + 1)
            .unwrap();
        assert_eq!(recovered.rejected, 0);
        assert_eq!(recovered.states[0].sticky_fault, None);
    }

    #[cfg(unix)]
    #[test]
    fn overflow_sentinel_is_never_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let ingress = HookStateStore::prepare(&plugin_data).unwrap();
        let store = ingress.bind(identity()).unwrap();
        store.record_overflow_fault(10, None).unwrap();

        ingress
            .reclaim_stale_artifacts_after_drain(TERMINAL_RETENTION_MS + 100)
            .unwrap();
        let overflow = read_fault_file(&store.fault_dir, OsStr::new(FAULT_OVERFLOW_NAME)).unwrap();
        assert_eq!(overflow.observed_at_ms, 10);
        assert_eq!(overflow.integration, None);
    }

    #[cfg(unix)]
    #[test]
    fn overflow_timestamp_is_monotonic_and_malformed_markers_are_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 1)).unwrap();
        for at in 2..(MAX_FAULT_FILES as u64 + 2) {
            drop(store.begin_ingest(at).unwrap());
        }
        drop(store.begin_ingest(1_000).unwrap());
        drop(store.begin_ingest(900).unwrap());
        let overflow = read_fault_file(&store.fault_dir, OsStr::new(FAULT_OVERFLOW_NAME)).unwrap();
        assert_eq!(overflow.observed_at_ms, 1_000);

        let malformed_name = format!("{FAULT_PREFIX}{}.json", "f".repeat(32));
        let malformed_path = store.fault_dir.path.join(malformed_name);
        fs::write(&malformed_path, b"{").unwrap();
        fs::set_permissions(&malformed_path, fs::Permissions::from_mode(0o600)).unwrap();
        let scan = store.read_all(1_100).unwrap();
        assert!(scan.rejected >= 1);
        assert_eq!(
            scan.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn generic_overflow_can_never_be_narrowed_to_one_integration() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();

        store.record_overflow_fault(10, None).unwrap();
        store.record_overflow_fault(20, Some(identity())).unwrap();

        let overflow = read_fault_file(&store.fault_dir, OsStr::new(FAULT_OVERFLOW_NAME)).unwrap();
        assert_eq!(overflow.observed_at_ms, 20);
        assert_eq!(overflow.integration, None);
    }

    #[cfg(unix)]
    #[test]
    fn same_installation_mismatched_fault_is_rejected_globally() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let mut changed = identity();
        changed.config_digest = format!("sha256:{}", "9".repeat(64));
        let fault = HookIngestFault {
            schema_version: HOOK_STATE_SCHEMA_VERSION,
            integration: Some(changed),
            observed_at_ms: 20,
            commit_id: "d".repeat(INGEST_COMMIT_ID_LEN),
        };
        let name = OsString::from(format!("{FAULT_PREFIX}{}.json", "e".repeat(32)));
        store
            .fault_dir
            .create_private_new(&name, &encode_fault(&fault).unwrap())
            .unwrap();

        let scan = store.read_all(30).unwrap();
        assert_eq!(scan.rejected, 1);
        assert_eq!(
            scan.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_launcher_overflow_is_refreshed_without_becoming_success() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let overflow_path = store.fault_dir.path.join(FAULT_OVERFLOW_NAME);
        fs::write(&overflow_path, []).unwrap();
        fs::set_permissions(&overflow_path, fs::Permissions::from_mode(0o600)).unwrap();

        let guard = store.begin_ingest(20).unwrap();
        assert_eq!(
            marker_name_from_proof(guard.marker_id().unwrap()),
            FAULT_OVERFLOW_NAME
        );
        drop(guard);
        let fault = read_fault_file(&store.fault_dir, OsStr::new(FAULT_OVERFLOW_NAME)).unwrap();
        assert_eq!(fault.observed_at_ms, 20);
        let scan = store.read_all(30).unwrap();
        assert_eq!(scan.rejected, 0);
        assert_eq!(
            scan.states[0].sticky_fault,
            Some(StatusReason::HookEventGap)
        );
    }

    #[cfg(unix)]
    #[test]
    fn retained_directory_descriptors_defeat_ancestor_replacement() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        let original_plugins = root.join("plugins");
        let retained_plugins = root.join("plugins-retained");
        fs::rename(&original_plugins, &retained_plugins).unwrap();

        let replacement = root.join("replacement");
        fs::create_dir(&replacement).unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&replacement, &original_plugins).unwrap();

        let state = store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let retained_state = retained_plugins
            .join("data/abtop-abtop-local/states")
            .join(format!("state-{}.json", state.generation_id));
        assert!(retained_state.exists());
        assert!(!replacement.join("data").exists());
        assert_eq!(store.read_all(20).unwrap().states.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn collector_open_existing_never_creates_state_directories() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        assert!(HookStateStore::open_existing(&plugin_data, identity()).is_err());
        assert!(!plugin_data.join(STATE_DIR_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn prior_installation_state_is_retained_but_not_current_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let mut old_identity = identity();
        old_identity.installation_id = "b".repeat(32);
        let old_store = HookStateStore::new(&plugin_data, old_identity.clone()).unwrap();
        let mut old_start = event(HookEventKind::SessionStart, 10);
        old_start.integration = old_identity;
        old_store.fold(old_start).unwrap();
        drop(old_store.begin_ingest(15).unwrap());

        let current = HookStateStore::open_existing(&plugin_data, identity()).unwrap();
        let scan = current.read_all(20).unwrap();
        assert_eq!(scan.rejected, 0);
        assert!(scan.states.is_empty());
        assert_eq!(
            current
                .state_dir
                .list_names(MAX_DIRECTORY_ENTRIES)
                .unwrap()
                .0
                .iter()
                .filter(|name| name.to_str().is_some_and(valid_state_filename))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn normal_gc_starts_grace_at_first_confirmed_death_and_uses_strict_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();

        let mut start = event(HookEventKind::SessionStart, 1);
        start.process = gone_process_identity();
        let started = store.fold(start).unwrap();
        let mut end = event(HookEventKind::SessionEnd, 2);
        end.process = gone_process_identity();
        store.fold(end).unwrap();
        let name = OsString::from(format!("state-{}.json", started.generation_id));

        // SessionEnd can precede process exit by an arbitrary interval. The
        // retention age must not substitute for the first exact Gone poll.
        let first_confirmation = 2 + TERMINAL_RETENTION_MS + 1;
        store.cleanup_terminal_states(first_confirmation).unwrap();
        let stamped = read_state_file(&store.state_dir, &name).unwrap().unwrap();
        assert_eq!(stamped.first_confirmed_gone_at_ms, first_confirmation);

        store
            .cleanup_terminal_states(first_confirmation + PROCESS_DEATH_OBSERVATION_GRACE_MS)
            .unwrap();
        let at_boundary = read_state_file(&store.state_dir, &name).unwrap().unwrap();
        assert_eq!(at_boundary.first_confirmed_gone_at_ms, first_confirmation);

        store
            .cleanup_terminal_states(first_confirmation + PROCESS_DEATH_OBSERVATION_GRACE_MS + 1)
            .unwrap();
        assert!(read_state_file(&store.state_dir, &name).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn crashed_generations_need_persisted_death_grace_under_capacity_pressure() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();

        let mut crashed = event(HookEventKind::SessionStart, 1);
        crashed.session_id = "crashed".into();
        crashed.process = HookProcessIdentity {
            pid: 2_000_000_000,
            started_at_ms: 1,
            incarnation: "gone-incarnation".into(),
            shared_host: false,
            launch_config_ambiguous: false,
        };
        store.fold(crashed).unwrap();
        for index in 1..MAX_STATE_FILES {
            let mut live = event(HookEventKind::SessionStart, index as u64 + 2);
            live.session_id = format!("live-{index}");
            store.fold(live).unwrap();
        }

        let after_retention = TERMINAL_RETENTION_MS + 10;
        let mut replacement = event(HookEventKind::SessionStart, after_retention);
        replacement.session_id = "replacement".into();
        assert!(store.fold(replacement.clone()).is_err());
        let scan = store.read_all(after_retention).unwrap();
        assert_eq!(scan.rejected, 0);
        assert_eq!(scan.states.len(), MAX_STATE_FILES);
        let crashed = scan
            .states
            .iter()
            .find(|state| state.session_id == "crashed")
            .unwrap();
        assert_eq!(crashed.ended_at_ms, 0);
        assert_eq!(crashed.first_confirmed_gone_at_ms, after_retention);

        replacement.observed_at_ms = after_retention + PROCESS_DEATH_OBSERVATION_GRACE_MS;
        assert!(store.fold(replacement.clone()).is_err());
        let at_boundary = store
            .read_all(replacement.observed_at_ms)
            .unwrap()
            .states
            .into_iter()
            .find(|state| state.session_id == "crashed")
            .unwrap();
        assert_eq!(at_boundary.first_confirmed_gone_at_ms, after_retention);

        replacement.observed_at_ms += 1;
        store.fold(replacement.clone()).unwrap();
        let after_grace = store.read_all(replacement.observed_at_ms).unwrap();
        assert_eq!(after_grace.states.len(), MAX_STATE_FILES);
        assert!(after_grace
            .states
            .iter()
            .all(|state| state.session_id != "crashed"));
        assert!(after_grace
            .states
            .iter()
            .any(|state| state.session_id == "replacement"));

        let mut overflow = event(HookEventKind::SessionStart, replacement.observed_at_ms + 1);
        overflow.session_id = "overflow".into();
        assert!(store.fold(overflow).is_err());
        assert!(store.fault_dir.path.join(FAULT_OVERFLOW_NAME).exists());
    }

    #[test]
    fn pid_reuse_is_exact_gone_evidence_and_still_observes_grace() {
        let mut reused = process_identity();
        reused.incarnation = "retired-test-incarnation".into();
        assert!(!reused.matches_live_process());
        assert!(reused.confirmed_gone());

        let mut start = event(HookEventKind::SessionStart, 10);
        start.process = reused;
        let mut state = HookSessionState::new(&start);
        state.apply(&start);

        let first_confirmation = TERMINAL_RETENTION_MS + 20;
        assert_eq!(
            confirmed_gone_gc_decision(&mut state, first_confirmation),
            ConfirmedGoneGcDecision::PersistFirstConfirmation
        );
        assert_eq!(
            confirmed_gone_gc_decision(
                &mut state,
                first_confirmation + PROCESS_DEATH_OBSERVATION_GRACE_MS
            ),
            ConfirmedGoneGcDecision::Keep
        );
        assert_eq!(
            confirmed_gone_gc_decision(
                &mut state,
                first_confirmation + PROCESS_DEATH_OBSERVATION_GRACE_MS + 1
            ),
            ConfirmedGoneGcDecision::Remove
        );
    }

    #[test]
    fn only_a_clean_generation_boundary_resets_death_confirmation() {
        let start = event(HookEventKind::SessionStart, 10);
        let mut state = HookSessionState::new(&start);
        state.apply(&start);
        state.first_confirmed_gone_at_ms = 40;

        let mut prompt = event(HookEventKind::UserPromptSubmit, 20);
        prompt.turn_id = Some("turn-a".into());
        state.apply(&prompt);
        assert_eq!(state.first_confirmed_gone_at_ms, 40);

        let resume = HookEvent {
            session_start_source: Some(SessionStartSource::Resume),
            ..event(HookEventKind::SessionStart, 50)
        };
        state.apply(&resume);
        assert_eq!(state.first_confirmed_gone_at_ms, 0);
    }

    #[cfg(unix)]
    #[test]
    fn future_death_confirmation_fails_closed_on_read() {
        let temp = tempfile::tempdir().unwrap();
        let plugin_data = private_plugin_data(&temp);
        let store = HookStateStore::new(&plugin_data, identity()).unwrap();
        let state = store.fold(event(HookEventKind::SessionStart, 10)).unwrap();
        let name = OsString::from(format!("state-{}.json", state.generation_id));
        let mut future = state;
        let read_at = 100;
        future.first_confirmed_gone_at_ms = read_at + 60_001;
        write_state_file(&store.state_dir, &name, &future).unwrap();

        let scan = store.read_all(read_at).unwrap();
        assert_eq!(scan.states.len(), 1);
        assert_eq!(
            scan.states[0].sticky_fault,
            Some(StatusReason::HookStateMalformed)
        );
    }
}
