use chrono::Utc;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
}
