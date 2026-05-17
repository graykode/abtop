//! Task-aware dispatch composer state machine (`P6-UX-01`).
//!
//! This module owns the pure data types and helpers for the composer; the
//! state-mutating methods that touch `App` (open/advance/cancel) live in
//! `app.rs` because they consult `ControlPolicy` and emit audit events.
//!
//! Design contract: `docs/COMPOSER_DESIGN.md`. The vocabulary for audit
//! actions and outcomes lives in [`crate::audit`] (`P4-DSP-01`).
//!
//! `dead_code` is permitted at module scope because several variants and
//! helpers are intentionally pre-staged for `P6-UX-01` subtasks 3-6 (UI,
//! spawn pipeline, evidence integration). Remove this allow once the
//! follow-up subtasks wire each item.
#![allow(dead_code)]

use chrono::{DateTime, Utc};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Instant;

use crate::audit;
use crate::collector::{redact_secrets, sanitize_terminal_text};
use crate::config::ControlPolicy;

/// Confirmation window for dispatch — longer than the kill-control window
/// because the user is reading a brief preview, not reacting to a hotkey.
pub const DISPATCH_CONFIRM_WINDOW_SECS: u64 = 5;

/// Maximum length of the user-typed draft body. Caps memory and keeps
/// prompts reasonable for the non-interactive CLI surfaces
/// (`claude --print`, `codex exec`, etc.).
pub const DISPATCH_MAX_DRAFT_LEN: usize = 4_096;

/// Maximum bytes of captured stdout that the dispatch pipeline persists per
/// run. Caps memory and protects against runaway agent responses.
pub const DISPATCH_RESPONSE_CAP_BYTES: usize = 256 * 1024;

/// Identifier for a single dispatchable agent. The `cli` field matches the
/// collector `agent_cli` identifier used everywhere else (so policy lookups
/// in `ControlPolicy::is_dispatch_allowed` work directly).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchAgent {
    pub cli: String,
    pub label: String,
}

impl DispatchAgent {
    pub fn claude() -> Self {
        Self {
            cli: "claude-code".into(),
            label: "Claude Code".into(),
        }
    }

    pub fn codex() -> Self {
        Self {
            cli: "codex-cli".into(),
            label: "Codex CLI".into(),
        }
    }

    pub fn opencode() -> Self {
        Self {
            cli: "opencode".into(),
            label: "OpenCode".into(),
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::claude(), Self::codex(), Self::opencode()]
    }
}

/// Sanitized target of a dispatch action — the (project, task) pair the user
/// picked from the Workspace tab. Never includes prompt text or absolute
/// paths; the slug fields are derived from titles, not from file content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchTarget {
    pub project: String,
    pub task_id: String,
    pub task_title: String,
    pub task_status: String,
    pub task_phase: Option<String>,
    pub acceptance_count: usize,
    pub verification_completed: usize,
    pub verification_total: usize,
    pub dependency_count: usize,
}

impl DispatchTarget {
    /// Derive a short stable identifier from a task title for the audit
    /// `target_id` field. Lower-case ASCII alphanumerics, dashes elsewhere.
    pub fn slug_from_title(title: &str) -> String {
        let mut out = String::with_capacity(title.len());
        let mut last_dash = true;
        for ch in title.chars().take(96) {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        let trimmed = out.trim_matches('-');
        if trimmed.is_empty() {
            "untitled".to_string()
        } else {
            trimmed.to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    DryRun,
    Sent,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchResult {
    pub outcome: DispatchOutcome,
    pub agent_cli: String,
    pub task_id: String,
    pub project: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub response_bytes: usize,
    pub response_path: Option<PathBuf>,
    pub error: Option<String>,
}

/// Aggregated dispatch history for a single (project, task) pair. Only the
/// most recent result is retained — the full audit trail lives in the
/// append-only audit log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchHistory {
    pub count: usize,
    pub last_outcome: DispatchOutcome,
    pub last_agent_cli: String,
    pub last_response_bytes: usize,
    pub last_finished_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

impl DispatchHistory {
    pub fn from_result(result: &DispatchResult) -> Self {
        Self {
            count: 1,
            last_outcome: result.outcome,
            last_agent_cli: result.agent_cli.clone(),
            last_response_bytes: result.response_bytes,
            last_finished_at: result.finished_at,
            last_error: result.error.clone(),
        }
    }

    pub fn record(&mut self, result: &DispatchResult) {
        self.count = self.count.saturating_add(1);
        self.last_outcome = result.outcome;
        self.last_agent_cli = result.agent_cli.clone();
        self.last_response_bytes = result.response_bytes;
        self.last_finished_at = result.finished_at;
        self.last_error = result.error.clone();
    }
}

/// State machine for the composer overlay. See `docs/COMPOSER_DESIGN.md` for
/// the transition diagram.
#[derive(Clone, Debug, Default)]
pub enum ComposerState {
    #[default]
    Closed,
    Drafting {
        target: DispatchTarget,
        agent: DispatchAgent,
        draft: String,
        brief: String,
    },
    PreviewBrief {
        target: DispatchTarget,
        agent: DispatchAgent,
        draft: String,
        brief: String,
    },
    AwaitConfirm {
        target: DispatchTarget,
        agent: DispatchAgent,
        draft: String,
        brief: String,
        requested_at: Instant,
    },
    Dispatching {
        target: DispatchTarget,
        agent: DispatchAgent,
        started_at: DateTime<Utc>,
    },
    Done {
        result: DispatchResult,
    },
    Failed {
        agent_cli: String,
        error: String,
    },
}

impl ComposerState {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }

    pub fn stage_label(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Drafting { .. } => "drafting",
            Self::PreviewBrief { .. } => "preview",
            Self::AwaitConfirm { .. } => "await-confirm",
            Self::Dispatching { .. } => "dispatching",
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
        }
    }

    pub fn agent(&self) -> Option<&DispatchAgent> {
        match self {
            Self::Drafting { agent, .. }
            | Self::PreviewBrief { agent, .. }
            | Self::AwaitConfirm { agent, .. }
            | Self::Dispatching { agent, .. } => Some(agent),
            _ => None,
        }
    }

    pub fn target(&self) -> Option<&DispatchTarget> {
        match self {
            Self::Drafting { target, .. }
            | Self::PreviewBrief { target, .. }
            | Self::AwaitConfirm { target, .. }
            | Self::Dispatching { target, .. } => Some(target),
            _ => None,
        }
    }

    pub fn draft(&self) -> Option<&str> {
        match self {
            Self::Drafting { draft, .. }
            | Self::PreviewBrief { draft, .. }
            | Self::AwaitConfirm { draft, .. } => Some(draft.as_str()),
            _ => None,
        }
    }

    pub fn brief(&self) -> Option<&str> {
        match self {
            Self::Drafting { brief, .. }
            | Self::PreviewBrief { brief, .. }
            | Self::AwaitConfirm { brief, .. } => Some(brief.as_str()),
            _ => None,
        }
    }

    /// Whether the most recent confirm window has elapsed. Always `false`
    /// outside `AwaitConfirm`.
    pub fn confirm_expired(&self) -> bool {
        match self {
            Self::AwaitConfirm { requested_at, .. } => {
                requested_at.elapsed().as_secs() >= DISPATCH_CONFIRM_WINDOW_SECS
            }
            _ => false,
        }
    }
}

/// Compose the auto-context block sent ahead of the user's draft. The output
/// is plain Markdown using titles + counts + statuses only — same redaction
/// shape as `--handoff` per-task entries.
pub fn build_brief(target: &DispatchTarget, suggested_agent: &str) -> String {
    format!(
        "- task: {}\n- status: {}\n- phase: {}\n- acceptance: {} criteria\n- verification: {}/{} verified\n- dependencies: {}\n- suggested agent: {}\n",
        target.task_title,
        target.task_status,
        target.task_phase.as_deref().unwrap_or("—"),
        target.acceptance_count,
        target.verification_completed,
        target.verification_total,
        target.dependency_count,
        suggested_agent,
    )
}

/// Cycle to the next agent allowed by policy. Returns the current agent if
/// no other agent is permitted (caller can render "only this agent allowed").
pub fn cycle_agent(current: &DispatchAgent, policy: &ControlPolicy) -> DispatchAgent {
    let all = DispatchAgent::all();
    let start = all.iter().position(|a| a.cli == current.cli).unwrap_or(0);
    for offset in 1..=all.len() {
        let candidate = &all[(start + offset) % all.len()];
        if policy.is_dispatch_allowed(&candidate.cli) {
            return candidate.clone();
        }
    }
    current.clone()
}

/// First agent permitted by policy, or `None` when no dispatch flag is set.
pub fn first_allowed_agent(policy: &ControlPolicy) -> Option<DispatchAgent> {
    DispatchAgent::all()
        .into_iter()
        .find(|agent| policy.is_dispatch_allowed(&agent.cli))
}

/// Convenience: map a `DispatchAgent` to the audit action label used by
/// [`crate::audit::actions`]. Returns `None` for an agent the audit
/// vocabulary doesn't recognise (defensive — should always succeed for the
/// three agents shipped today).
pub fn dispatch_action_for_agent(agent: &DispatchAgent) -> Option<&'static str> {
    audit::dispatch_action_for(&agent.cli)
}

/// One dispatch invocation. Built by `App::composer_advance` when the user
/// confirms; consumed by `spawn_dispatch`.
#[derive(Clone, Debug)]
pub struct DispatchRequest {
    pub target: DispatchTarget,
    pub agent: DispatchAgent,
    pub brief: String,
    pub draft: String,
    pub dry_run: bool,
}

/// Spawn the dispatch on a background thread and return a receiver that
/// will deliver exactly one `DispatchResult` when the agent CLI finishes
/// (or fails to start, or is short-circuited by `dry_run`).
///
/// Today, the only wired agent is Claude Code (`claude --print`); other
/// agents return a `Failed` result with a descriptive error so the audit
/// trail captures the attempt. Real Codex / OpenCode wiring lands in
/// `P6-UX-01` step 6.
pub fn spawn_dispatch(req: DispatchRequest) -> Receiver<DispatchResult> {
    let command = build_command_for(&req.agent);
    spawn_dispatch_with(req, command)
}

fn build_command_for(agent: &DispatchAgent) -> Option<Command> {
    match agent.cli.as_str() {
        "claude-code" => {
            let mut cmd = Command::new("claude");
            cmd.arg("--print");
            Some(cmd)
        }
        // Codex + OpenCode dispatch land in P6-UX-01 step 6.
        _ => None,
    }
}

/// Lower-level entry point that lets tests inject an arbitrary command
/// (e.g. `Command::new("true")` to assert the success path on machines
/// without `claude` installed). Production callers use [`spawn_dispatch`].
pub fn spawn_dispatch_with(
    req: DispatchRequest,
    command: Option<Command>,
) -> Receiver<DispatchResult> {
    let (tx, rx) = mpsc::channel();
    let started_at = Utc::now();

    if req.dry_run {
        let _ = tx.send(make_dry_run_result(&req, started_at));
        return rx;
    }

    let Some(mut command) = command else {
        let _ = tx.send(make_failed_result(
            &req,
            started_at,
            format!("no dispatch command wired for agent {}", req.agent.cli),
        ));
        return rx;
    };

    let payload = compose_payload(&req.brief, &req.draft);

    thread::spawn(move || {
        let result = run_dispatch(&mut command, &payload, &req, started_at);
        let _ = tx.send(result);
    });

    rx
}

fn compose_payload(brief: &str, draft: &str) -> String {
    let mut payload = String::with_capacity(brief.len() + draft.len() + 64);
    payload.push_str("# Context (auto-generated by abtop, redacted)\n");
    payload.push_str(brief);
    if !brief.ends_with('\n') {
        payload.push('\n');
    }
    payload.push_str("\n# Question\n");
    payload.push_str(draft);
    if !draft.ends_with('\n') {
        payload.push('\n');
    }
    payload
}

fn run_dispatch(
    command: &mut Command,
    payload: &str,
    req: &DispatchRequest,
    started_at: DateTime<Utc>,
) -> DispatchResult {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(error) => {
            return make_failed_result(req, started_at, format!("spawn failed: {error}"));
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(payload.as_bytes()) {
            return make_failed_result(req, started_at, format!("stdin write failed: {error}"));
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(error) => {
            return make_failed_result(req, started_at, format!("wait failed: {error}"));
        }
    };

    if !output.status.success() {
        let stderr_snippet = String::from_utf8_lossy(&output.stderr);
        let stderr_snippet = stderr_snippet.lines().next().unwrap_or("").trim();
        let error = if stderr_snippet.is_empty() {
            format!("exit {}", output.status)
        } else {
            format!("exit {}: {}", output.status, stderr_snippet)
        };
        return make_failed_result(req, started_at, error);
    }

    let (sanitized, response_path) = persist_response(&output.stdout, req, started_at);

    DispatchResult {
        outcome: DispatchOutcome::Sent,
        agent_cli: req.agent.cli.clone(),
        task_id: req.target.task_id.clone(),
        project: req.target.project.clone(),
        started_at,
        finished_at: Utc::now(),
        response_bytes: sanitized.len(),
        response_path: Some(response_path),
        error: None,
    }
}

fn make_dry_run_result(req: &DispatchRequest, started_at: DateTime<Utc>) -> DispatchResult {
    DispatchResult {
        outcome: DispatchOutcome::DryRun,
        agent_cli: req.agent.cli.clone(),
        task_id: req.target.task_id.clone(),
        project: req.target.project.clone(),
        started_at,
        finished_at: Utc::now(),
        response_bytes: 0,
        response_path: None,
        error: None,
    }
}

fn make_failed_result(
    req: &DispatchRequest,
    started_at: DateTime<Utc>,
    error: String,
) -> DispatchResult {
    DispatchResult {
        outcome: DispatchOutcome::Failed,
        agent_cli: req.agent.cli.clone(),
        task_id: req.target.task_id.clone(),
        project: req.target.project.clone(),
        started_at,
        finished_at: Utc::now(),
        response_bytes: 0,
        response_path: None,
        error: Some(error),
    }
}

/// Redact + truncate raw stdout, write it to the dispatch log directory,
/// and return `(redacted_bytes, path)`.
fn persist_response(
    stdout: &[u8],
    req: &DispatchRequest,
    started_at: DateTime<Utc>,
) -> (String, PathBuf) {
    let truncated: &[u8] = if stdout.len() > DISPATCH_RESPONSE_CAP_BYTES {
        &stdout[..DISPATCH_RESPONSE_CAP_BYTES]
    } else {
        stdout
    };
    let text = String::from_utf8_lossy(truncated);
    let sanitized = redact_secrets(&sanitize_terminal_text(&text));

    let ts = started_at.format("%Y%m%dT%H%M%SZ").to_string();
    let path = dispatch_response_path(&ts, &req.target.task_id, &req.agent.cli);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, sanitized.as_bytes());

    (sanitized, path)
}

/// Derive the on-disk path for a dispatch response next to the audit log.
pub fn dispatch_response_path(ts: &str, task_id: &str, agent_cli: &str) -> PathBuf {
    let base = audit::audit_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("dispatch")
        .join(format!("{ts}-{task_id}-{agent_cli}.md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_target() -> DispatchTarget {
        DispatchTarget {
            project: "ml-pipeline".into(),
            task_id: "dataset-drift-guardrails".into(),
            task_title: "Dataset drift guardrails".into(),
            task_status: "Ready".into(),
            task_phase: Some("Plan".into()),
            acceptance_count: 3,
            verification_completed: 0,
            verification_total: 1,
            dependency_count: 0,
        }
    }

    #[test]
    fn slug_from_title_normalizes_titles() {
        assert_eq!(
            DispatchTarget::slug_from_title("Dataset Drift Guardrails!"),
            "dataset-drift-guardrails"
        );
        assert_eq!(
            DispatchTarget::slug_from_title("   leading/trailing   "),
            "leading-trailing"
        );
        assert_eq!(DispatchTarget::slug_from_title(""), "untitled");
        assert_eq!(DispatchTarget::slug_from_title("///"), "untitled");
    }

    #[test]
    fn closed_is_default_and_not_open() {
        let state = ComposerState::default();
        assert!(!state.is_open());
        assert_eq!(state.stage_label(), "closed");
        assert!(state.agent().is_none());
        assert!(state.target().is_none());
        assert!(state.draft().is_none());
        assert!(state.brief().is_none());
        assert!(!state.confirm_expired());
    }

    #[test]
    fn drafting_exposes_target_agent_and_draft() {
        let state = ComposerState::Drafting {
            target: sample_target(),
            agent: DispatchAgent::claude(),
            draft: "implement schema diff".into(),
            brief: "- task: Dataset drift guardrails\n".into(),
        };
        assert!(state.is_open());
        assert_eq!(state.stage_label(), "drafting");
        assert_eq!(state.agent().map(|a| a.cli.as_str()), Some("claude-code"));
        assert_eq!(
            state.target().map(|t| t.task_id.as_str()),
            Some("dataset-drift-guardrails")
        );
        assert_eq!(state.draft(), Some("implement schema diff"));
        assert!(state.brief().is_some());
        assert!(!state.confirm_expired());
    }

    #[test]
    fn build_brief_is_redacted_and_structured() {
        let brief = build_brief(&sample_target(), "implementation agent");
        assert!(brief.contains("Dataset drift guardrails"));
        assert!(brief.contains("status: Ready"));
        assert!(brief.contains("phase: Plan"));
        assert!(brief.contains("acceptance: 3 criteria"));
        assert!(brief.contains("verification: 0/1 verified"));
        assert!(brief.contains("dependencies: 0"));
        assert!(brief.contains("suggested agent: implementation agent"));
        // Brief is bullets only — no inline file paths or prompt text.
        for line in brief.lines() {
            assert!(line.starts_with("- ") || line.is_empty());
        }
    }

    #[test]
    fn build_brief_handles_missing_phase() {
        let mut target = sample_target();
        target.task_phase = None;
        let brief = build_brief(&target, "any");
        assert!(brief.contains("phase: —"));
    }

    #[test]
    fn cycle_agent_skips_disallowed() {
        // Only Codex allowed.
        let policy = ControlPolicy {
            allow_dispatch_codex: true,
            ..ControlPolicy::default()
        };

        let next = cycle_agent(&DispatchAgent::claude(), &policy);
        assert_eq!(next.cli, "codex-cli");

        // Already on Codex — cycle should stay (no other allowed agent).
        let next = cycle_agent(&DispatchAgent::codex(), &policy);
        assert_eq!(next.cli, "codex-cli");
    }

    #[test]
    fn cycle_agent_wraps_to_first_allowed() {
        let policy = ControlPolicy {
            allow_dispatch_claude: true,
            allow_dispatch_opencode: true,
            ..ControlPolicy::default()
        };

        let next = cycle_agent(&DispatchAgent::opencode(), &policy);
        assert_eq!(next.cli, "claude-code");
    }

    #[test]
    fn cycle_agent_returns_current_when_nothing_allowed() {
        let policy = ControlPolicy::default();
        let next = cycle_agent(&DispatchAgent::claude(), &policy);
        assert_eq!(next.cli, "claude-code");
    }

    #[test]
    fn first_allowed_agent_respects_policy_order() {
        assert!(first_allowed_agent(&ControlPolicy::default()).is_none());

        let only_opencode = ControlPolicy {
            allow_dispatch_opencode: true,
            ..ControlPolicy::default()
        };
        assert_eq!(first_allowed_agent(&only_opencode).unwrap().cli, "opencode");

        let claude_plus_opencode = ControlPolicy {
            allow_dispatch_claude: true,
            allow_dispatch_opencode: true,
            ..ControlPolicy::default()
        };
        assert_eq!(
            first_allowed_agent(&claude_plus_opencode).unwrap().cli,
            "claude-code"
        );
    }

    #[test]
    fn dispatch_action_for_agent_maps_to_audit_vocabulary() {
        assert_eq!(
            dispatch_action_for_agent(&DispatchAgent::claude()),
            Some(audit::actions::DISPATCH_CLAUDE)
        );
        assert_eq!(
            dispatch_action_for_agent(&DispatchAgent::codex()),
            Some(audit::actions::DISPATCH_CODEX)
        );
        assert_eq!(
            dispatch_action_for_agent(&DispatchAgent::opencode()),
            Some(audit::actions::DISPATCH_OPENCODE)
        );
    }

    #[test]
    fn confirm_expired_is_true_after_window() {
        let state = ComposerState::AwaitConfirm {
            target: sample_target(),
            agent: DispatchAgent::claude(),
            draft: String::new(),
            brief: String::new(),
            requested_at: Instant::now()
                - std::time::Duration::from_secs(DISPATCH_CONFIRM_WINDOW_SECS + 1),
        };
        assert!(state.confirm_expired());
    }

    fn sample_request(dry_run: bool) -> DispatchRequest {
        DispatchRequest {
            target: sample_target(),
            agent: DispatchAgent::claude(),
            brief: "- task: Dataset drift guardrails\n".into(),
            draft: "implement the schema diff first".into(),
            dry_run,
        }
    }

    #[test]
    fn compose_payload_combines_brief_and_draft_with_markers() {
        let payload = compose_payload("- bullet\n", "question?");
        assert!(payload.contains("# Context (auto-generated by abtop, redacted)"));
        assert!(payload.contains("- bullet"));
        assert!(payload.contains("# Question"));
        assert!(payload.contains("question?"));
        // Ends with newline so child processes reading line-by-line don't stall.
        assert!(payload.ends_with('\n'));
    }

    #[test]
    fn spawn_dispatch_dry_run_emits_dry_run_result_synchronously() {
        let rx = spawn_dispatch(sample_request(true));
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("dry-run result must arrive promptly");
        assert_eq!(result.outcome, DispatchOutcome::DryRun);
        assert_eq!(result.agent_cli, "claude-code");
        assert_eq!(result.response_bytes, 0);
        assert!(result.response_path.is_none());
        assert!(result.error.is_none());
    }

    #[test]
    fn spawn_dispatch_with_unknown_agent_emits_failed_result() {
        let req = DispatchRequest {
            agent: DispatchAgent {
                cli: "gemini".into(),
                label: "Gemini".into(),
            },
            ..sample_request(false)
        };
        let rx = spawn_dispatch(req);
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("failed result must arrive promptly");
        assert_eq!(result.outcome, DispatchOutcome::Failed);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|err| err.contains("no dispatch command wired")));
    }

    #[test]
    fn dispatch_history_records_results() {
        let result_sent = DispatchResult {
            outcome: DispatchOutcome::Sent,
            agent_cli: "claude-code".into(),
            task_id: "t1".into(),
            project: "p".into(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            response_bytes: 1234,
            response_path: None,
            error: None,
        };
        let mut history = DispatchHistory::from_result(&result_sent);
        assert_eq!(history.count, 1);
        assert_eq!(history.last_outcome, DispatchOutcome::Sent);
        assert_eq!(history.last_response_bytes, 1234);

        let result_failed = DispatchResult {
            outcome: DispatchOutcome::Failed,
            agent_cli: "claude-code".into(),
            task_id: "t1".into(),
            project: "p".into(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            response_bytes: 0,
            response_path: None,
            error: Some("timeout".into()),
        };
        history.record(&result_failed);
        assert_eq!(history.count, 2);
        assert_eq!(history.last_outcome, DispatchOutcome::Failed);
        assert_eq!(history.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn dispatch_response_path_format_is_stable() {
        let path = dispatch_response_path("20260518T101530Z", "release-prep", "claude-code");
        let s = path.to_string_lossy().replace('\\', "/");
        assert!(s.ends_with("dispatch/20260518T101530Z-release-prep-claude-code.md"));
    }

    #[test]
    fn confirm_expired_is_false_inside_window() {
        let state = ComposerState::AwaitConfirm {
            target: sample_target(),
            agent: DispatchAgent::claude(),
            draft: String::new(),
            brief: String::new(),
            requested_at: Instant::now(),
        };
        assert!(!state.confirm_expired());
    }
}
