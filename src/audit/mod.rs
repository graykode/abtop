use chrono::Utc;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Audit action names. Use these constants in callers so the vocabulary stays
/// stable across the codebase — `AuditEvent::new` still accepts arbitrary
/// strings, so these are conventions, not enforced types.
///
/// Pre-staged for `P6-UX-01` (composer/dispatch UI); existing `app.rs` kill
/// flows still pass string literals.
#[allow(dead_code)]
pub mod actions {
    /// Stop a tracked agent session (existing `x` keybinding).
    pub const KILL_SESSION: &str = "kill-session";
    /// Kill an orphan port whose owning session has exited (existing `X` keybinding).
    pub const KILL_ORPHAN_PORT: &str = "kill-orphan-port";
    /// Dispatch a one-shot prompt to a Claude Code agent. Wired by the future
    /// composer (`P6-UX-01`); the audit vocabulary lands first (`P4-DSP-01`).
    pub const DISPATCH_CLAUDE: &str = "dispatch-claude";
    /// Dispatch a one-shot prompt to a Codex CLI agent (`P6-UX-01`).
    pub const DISPATCH_CODEX: &str = "dispatch-codex";
    /// Dispatch a one-shot prompt to an OpenCode agent (`P6-UX-01`).
    pub const DISPATCH_OPENCODE: &str = "dispatch-opencode";
}

/// Audit outcome labels shared by every mutating control flow. Keep this list
/// in sync with `app::kill_*` and the future dispatch composer.
#[allow(dead_code)]
pub mod outcomes {
    /// User triggered the action but no confirmation has been recorded yet.
    pub const REQUESTED: &str = "requested";
    /// Second keypress arrived inside the confirmation window.
    pub const CONFIRMED: &str = "confirmed";
    /// Action skipped: no target selected, target already gone, or similar.
    pub const SKIPPED: &str = "skipped";
    /// Action denied by local policy (`ControlPolicy`).
    pub const BLOCKED: &str = "blocked";
    /// Dry-run env var set: action verified, no mutation performed.
    pub const DRY_RUN: &str = "dry-run";
    /// Mutating call returned success.
    pub const SENT: &str = "sent";
    /// Mutating call returned an error.
    pub const FAILED: &str = "failed";
}

/// Environment variable that turns dispatch into a verified no-op, mirroring
/// `ABTOP_CONTROL_DRY_RUN` for kill controls. The future composer should emit
/// `outcomes::DRY_RUN` instead of `outcomes::SENT` when this is set.
#[allow(dead_code)]
pub const DISPATCH_DRY_RUN_ENV: &str = "ABTOP_DISPATCH_DRY_RUN";

/// Map a collector `agent_cli` identifier to the matching dispatch action
/// label. Returns `None` for unknown agents so the composer can refuse to
/// emit an event with an unstable label.
#[allow(dead_code)]
pub fn dispatch_action_for(agent_cli: &str) -> Option<&'static str> {
    match agent_cli.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "claude_code" | "cc" => Some(actions::DISPATCH_CLAUDE),
        "codex" | "codex-cli" | "codex_cli" => Some(actions::DISPATCH_CODEX),
        "opencode" | "open-code" | "open_code" => Some(actions::DISPATCH_OPENCODE),
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AuditEvent {
    pub timestamp: String,
    pub action: String,
    pub target_kind: String,
    pub target_id: String,
    pub project: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
}

impl AuditEvent {
    pub fn new(
        action: &str,
        target_kind: &str,
        target_id: &str,
        project: Option<&str>,
        outcome: &str,
        reason: Option<&str>,
    ) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            action: sanitize(action, 48),
            target_kind: sanitize(target_kind, 32),
            target_id: sanitize_identifier(target_id, 96),
            project: project.map(|project| sanitize(project, 80)),
            outcome: sanitize(outcome, 48),
            reason: reason.map(|reason| sanitize(reason, 120)),
        }
    }
}

pub fn record(event: &AuditEvent) {
    match append_event(event) {
        Ok(path) => crate::log_info!(
            "audit event action={} path={}",
            event.action,
            path.display()
        ),
        Err(error) => {
            crate::log_warn!("audit write failed action={} error={}", event.action, error)
        }
    }
}

pub fn append_event(event: &AuditEvent) -> io::Result<PathBuf> {
    let path = audit_path();
    append_event_to_path(&path, event)?;
    Ok(path)
}

pub fn append_event_to_path(path: &Path, event: &AuditEvent) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, event)?;
    file.write_all(b"\n")?;
    file.flush()
}

pub fn audit_path() -> PathBuf {
    if let Ok(path) = std::env::var("ABTOP_AUDIT_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }

    dirs::data_local_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("abtop")
        .join("audit.jsonl")
}

fn sanitize(value: &str, max_len: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .filter(|c| !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .take(max_len)
        .collect()
}

fn sanitize_identifier(value: &str, max_len: usize) -> String {
    let value = value.replace('\\', "/");
    let tail = value.rsplit('/').next().unwrap_or(&value);
    sanitize(tail, max_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn appends_jsonl_audit_events() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("audit").join("events.jsonl");
        let event = AuditEvent::new(
            "kill-session",
            "session",
            "abc123",
            Some("ml-pipeline"),
            "success",
            Some("confirmed by user"),
        );

        append_event_to_path(&path, &event).unwrap();
        append_event_to_path(&path, &event).unwrap();

        let text = std::fs::read_to_string(path).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["action"], "kill-session");
        assert_eq!(parsed["target_kind"], "session");
        assert_eq!(parsed["project"], "ml-pipeline");
    }

    #[test]
    fn sanitizes_sensitive_or_unstable_fields() {
        let event = AuditEvent::new(
            "archive\nsecret",
            "task",
            "C:\\Users\\APC\\secret\\task.md",
            Some("project\nname"),
            "success",
            Some("prompt text\nshould stay one line"),
        );

        assert_eq!(event.action, "archivesecret");
        assert_eq!(event.target_id, "task.md");
        assert_eq!(event.project.as_deref(), Some("projectname"));
        assert_eq!(
            event.reason.as_deref(),
            Some("prompt textshould stay one line")
        );
    }

    #[test]
    fn dispatch_action_for_maps_known_agents() {
        assert_eq!(
            dispatch_action_for("claude"),
            Some(actions::DISPATCH_CLAUDE)
        );
        assert_eq!(
            dispatch_action_for("Claude-Code"),
            Some(actions::DISPATCH_CLAUDE)
        );
        assert_eq!(dispatch_action_for("codex"), Some(actions::DISPATCH_CODEX));
        assert_eq!(
            dispatch_action_for("codex-cli"),
            Some(actions::DISPATCH_CODEX)
        );
        assert_eq!(
            dispatch_action_for("opencode"),
            Some(actions::DISPATCH_OPENCODE)
        );
        assert_eq!(
            dispatch_action_for(" Open-Code "),
            Some(actions::DISPATCH_OPENCODE)
        );
        assert_eq!(dispatch_action_for("gemini"), None);
        assert_eq!(dispatch_action_for(""), None);
    }

    #[test]
    fn dispatch_event_uses_stable_vocabulary() {
        let action = dispatch_action_for("claude").expect("known agent");
        let event = AuditEvent::new(
            action,
            "task",
            "release-prep",
            Some("agentic-interview-web"),
            outcomes::REQUESTED,
            Some("user clicked dispatch"),
        );

        assert_eq!(event.action, actions::DISPATCH_CLAUDE);
        assert_eq!(event.outcome, outcomes::REQUESTED);
        assert_eq!(event.target_kind, "task");
        assert_eq!(event.target_id, "release-prep");
        assert_eq!(event.project.as_deref(), Some("agentic-interview-web"));
    }

    #[test]
    fn dispatch_dry_run_env_name_is_stable() {
        assert_eq!(DISPATCH_DRY_RUN_ENV, "ABTOP_DISPATCH_DRY_RUN");
    }

    #[test]
    fn outcome_labels_match_kill_control_strings() {
        // Pin the outcome strings: the existing kill flows in app.rs encode
        // these as literals, so accidentally changing the constants would
        // break audit log readers that parse the JSONL.
        assert_eq!(outcomes::REQUESTED, "requested");
        assert_eq!(outcomes::CONFIRMED, "confirmed");
        assert_eq!(outcomes::SKIPPED, "skipped");
        assert_eq!(outcomes::BLOCKED, "blocked");
        assert_eq!(outcomes::DRY_RUN, "dry-run");
        assert_eq!(outcomes::SENT, "sent");
        assert_eq!(outcomes::FAILED, "failed");
    }
}
