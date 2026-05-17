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
use std::path::PathBuf;
use std::time::Instant;

use crate::audit;
use crate::config::ControlPolicy;

/// Confirmation window for dispatch — longer than the kill-control window
/// because the user is reading a brief preview, not reacting to a hotkey.
pub const DISPATCH_CONFIRM_WINDOW_SECS: u64 = 5;

/// Maximum length of the user-typed draft body. Caps memory and keeps
/// prompts reasonable for the non-interactive CLI surfaces
/// (`claude --print`, `codex exec`, etc.).
pub const DISPATCH_MAX_DRAFT_LEN: usize = 4_096;

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
