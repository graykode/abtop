use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TaskStatus {
    Ready,
    Doing,
    Blocked,
    Review,
    Done,
    #[default]
    Unknown,
}

impl TaskStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Doing => "doing",
            Self::Blocked => "blocked",
            Self::Review => "review",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }

    fn from_label(value: &str) -> Self {
        let normalized = value
            .trim()
            .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '[' | ']'))
            .to_ascii_lowercase();

        match normalized.as_str() {
            "ready" | "todo" | "to do" | "next" | "pending" => Self::Ready,
            "doing" | "in progress" | "progress" | "active" | "started" => Self::Doing,
            "blocked" | "stuck" | "waiting" => Self::Blocked,
            "review" | "in review" | "needs review" | "verify" | "verification" => Self::Review,
            "done" | "complete" | "completed" | "closed" | "finished" => Self::Done,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DwTaskSummary {
    pub path: PathBuf,
    pub title: Option<String>,
    pub phase: Option<String>,
    pub status: TaskStatus,
    pub raw_status: Option<String>,
    pub acceptance_count: usize,
    pub verification_count: usize,
    pub completed_verification_count: usize,
    pub dependencies: Vec<String>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DwProjectState {
    pub has_dw: bool,
    pub active_task: Option<DwTaskSummary>,
    pub tasks: Vec<DwTaskSummary>,
    /// Goals projected from `.dw/goals/goals-index.json` (dw-kit ADR-0017).
    /// Empty when the project has no Goals layer or no index.
    pub goals: Vec<super::dw_index::DwGoalSummary>,
    pub decision_count: usize,
    pub record_count: usize,
    pub verification_count: usize,
    pub completed_verification_count: usize,
}

pub fn read_project_state(cwd: &Path) -> DwProjectState {
    let dw_dir = cwd.join(".dw");
    if !dw_dir.is_dir() {
        return DwProjectState::default();
    }

    let mut state = DwProjectState {
        has_dw: true,
        decision_count: count_markdown_files(&dw_dir.join("decisions")),
        record_count: count_markdown_files(&dw_dir.join("records")),
        ..DwProjectState::default()
    };

    let tasks_dir = dw_dir.join("tasks");
    let active_path = [tasks_dir.join("ACTIVE.md"), dw_dir.join("ACTIVE.md")]
        .into_iter()
        .find(|path| path.is_file());
    let active_path_ref = active_path.as_deref();
    state.active_task = active_path_ref.and_then(|path| read_task_summary(cwd, path, true));

    let mut tasks = Vec::new();
    if let Ok(entries) = fs::read_dir(&tasks_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_markdown_file(&path) {
                continue;
            }

            let is_active =
                active_path_ref.is_some_and(|active_path| same_path(&path, active_path));
            if let Some(summary) = read_task_summary(cwd, &path, is_active) {
                tasks.push(summary);
            }
        }
    }
    if let Some(active_task) = &state.active_task {
        if active_task.path == Path::new(".dw/ACTIVE.md")
            && !tasks.iter().any(|task| task.path == active_task.path)
        {
            tasks.push(active_task.clone());
        }
    }
    tasks.sort_by(|a, b| a.path.cmp(&b.path));

    state.verification_count = tasks.iter().map(|task| task.verification_count).sum();
    state.completed_verification_count = tasks
        .iter()
        .map(|task| task.completed_verification_count)
        .sum();
    state.tasks = tasks;

    // Goals layer (dw-kit ADR-0017): read the committed index instead of
    // scraping. Empty when absent — purely additive to the existing task view.
    state.goals = super::dw_index::read_goals_index(cwd);

    state
}

fn read_task_summary(cwd: &Path, path: &Path, is_active: bool) -> Option<DwTaskSummary> {
    let text = fs::read_to_string(path).ok()?;
    let metadata = parse_task_metadata(&text);
    Some(DwTaskSummary {
        path: safe_relative_path(cwd, path),
        title: metadata.title,
        phase: metadata.phase,
        status: metadata.status,
        raw_status: metadata.raw_status,
        acceptance_count: metadata.acceptance_count,
        verification_count: metadata.verification_count,
        completed_verification_count: metadata.completed_verification_count,
        dependencies: metadata.dependencies,
        is_active,
    })
}

#[derive(Debug, Default)]
struct ParsedTaskMetadata {
    title: Option<String>,
    phase: Option<String>,
    status: TaskStatus,
    raw_status: Option<String>,
    acceptance_count: usize,
    verification_count: usize,
    completed_verification_count: usize,
    dependencies: Vec<String>,
}

fn parse_task_metadata(text: &str) -> ParsedTaskMetadata {
    let mut parsed = ParsedTaskMetadata::default();
    parse_front_matter(text, &mut parsed);
    parse_markdown_labels(text, &mut parsed);
    parse_counts(text, &mut parsed);

    if parsed.title.is_none() {
        parsed.title = first_heading(text);
    }

    parsed
}

fn parse_front_matter(text: &str, parsed: &mut ParsedTaskMetadata) {
    let mut lines = text.lines();
    let Some(first) = lines.next() else {
        return;
    };
    let delimiter = match first.trim() {
        "---" => "---",
        "+++" => "+++",
        _ => return,
    };

    for line in lines {
        if line.trim() == delimiter {
            break;
        }
        apply_key_value(line, parsed);
    }
}

fn parse_markdown_labels(text: &str, parsed: &mut ParsedTaskMetadata) {
    for line in text.lines().take(80) {
        apply_key_value(line, parsed);
    }
}

fn apply_key_value(line: &str, parsed: &mut ParsedTaskMetadata) {
    let trimmed = line
        .trim()
        .trim_start_matches('-')
        .trim_start_matches('*')
        .trim();
    let Some((key, value)) = trimmed.split_once(':') else {
        return;
    };

    let key = key
        .trim()
        .trim_matches(|c| matches!(c, '*' | '_' | '`' | '[' | ']'))
        .to_ascii_lowercase();
    let value = clean_value(value);
    if value.is_empty() {
        return;
    }

    match key.as_str() {
        "title" | "task" | "name" if parsed.title.is_none() => {
            parsed.title = Some(value);
        }
        "phase" | "stage" if parsed.phase.is_none() => {
            parsed.phase = Some(value);
        }
        "status" | "state" if parsed.raw_status.is_none() => {
            parsed.status = TaskStatus::from_label(&value);
            parsed.raw_status = Some(value);
        }
        "depends_on" | "depends-on" | "depends on" | "dependencies" | "blocked_by"
        | "blocked-by" | "blocked by" => {
            parsed.dependencies.extend(parse_dependency_list(&value));
            parsed.dependencies.sort();
            parsed.dependencies.dedup();
        }
        _ => {}
    }
}

fn parse_dependency_list(value: &str) -> Vec<String> {
    value
        .split([',', ';'])
        .map(clean_dependency)
        .filter(|value| !value.is_empty())
        .collect()
}

fn clean_dependency(value: &str) -> String {
    value
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '[' | ']'))
        .trim_start_matches("- ")
        .chars()
        .filter(|c| !c.is_control())
        .take(80)
        .collect()
}

fn parse_counts(text: &str, parsed: &mut ParsedTaskMetadata) {
    let mut section = CountSection::Other;

    for line in text.lines() {
        if let Some(heading) = heading_text(line) {
            let heading = heading.to_ascii_lowercase();
            section = if heading.contains("acceptance") || heading.contains("criteria") {
                CountSection::Acceptance
            } else if heading.contains("verification")
                || heading.contains("verify")
                || heading.contains("validation")
                || heading.contains("test")
            {
                CountSection::Verification
            } else {
                CountSection::Other
            };
            continue;
        }

        if !is_checklist_item(line) {
            continue;
        }

        match section {
            CountSection::Acceptance => parsed.acceptance_count += 1,
            CountSection::Verification => {
                parsed.verification_count += 1;
                if is_completed_checklist_item(line) {
                    parsed.completed_verification_count += 1;
                }
            }
            CountSection::Other => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CountSection {
    Acceptance,
    Verification,
    Other,
}

fn first_heading(text: &str) -> Option<String> {
    text.lines().find_map(heading_text).map(clean_value)
}

fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }

    let text = trimmed.trim_start_matches('#').trim();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn is_checklist_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- [ ]")
        || trimmed.starts_with("- [x]")
        || trimmed.starts_with("- [X]")
        || trimmed.starts_with("* [ ]")
        || trimmed.starts_with("* [x]")
        || trimmed.starts_with("* [X]")
}

fn is_completed_checklist_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- [x]")
        || trimmed.starts_with("- [X]")
        || trimmed.starts_with("* [x]")
        || trimmed.starts_with("* [X]")
}

fn count_markdown_files(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };

    entries
        .flatten()
        .filter(|entry| is_markdown_file(&entry.path()))
        .count()
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn safe_relative_path(cwd: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(cwd).unwrap_or(path).to_path_buf()
}

fn same_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn clean_value(value: &str) -> String {
    value
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .trim()
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}'
                )
        })
        .take(160)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    #[test]
    fn missing_dw_returns_defaults() {
        let temp = tempfile::tempdir().unwrap();

        let state = read_project_state(temp.path());

        assert!(!state.has_dw);
        assert!(state.active_task.is_none());
        assert!(state.tasks.is_empty());
        assert_eq!(state.decision_count, 0);
        assert_eq!(state.record_count, 0);
    }

    #[test]
    fn reads_active_task_front_matter_and_counts() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".dw/tasks/ACTIVE.md"),
            r#"---
title: Build workspace reader
phase: Implementation
status: Doing
depends_on: Dataset import, Feature store
---

Task body is intentionally not exposed.

## Acceptance Criteria
- [ ] Parse task metadata
- [x] Stay read-only

## Verification
- [x] cargo test task
- [ ] cargo clippy
"#,
        );
        write(&temp.path().join(".dw/decisions/0001.md"), "# Decision\n");
        write(&temp.path().join(".dw/records/0001.md"), "# Record\n");
        write(&temp.path().join(".dw/records/notes.txt"), "not counted");

        let state = read_project_state(temp.path());
        let active = state.active_task.as_ref().unwrap();

        assert!(state.has_dw);
        assert_eq!(state.decision_count, 1);
        assert_eq!(state.record_count, 1);
        assert_eq!(active.path, PathBuf::from(".dw/tasks/ACTIVE.md"));
        assert_eq!(active.title.as_deref(), Some("Build workspace reader"));
        assert_eq!(active.phase.as_deref(), Some("Implementation"));
        assert_eq!(active.status, TaskStatus::Doing);
        assert_eq!(active.raw_status.as_deref(), Some("Doing"));
        assert_eq!(active.acceptance_count, 2);
        assert_eq!(active.verification_count, 2);
        assert_eq!(active.completed_verification_count, 1);
        assert_eq!(
            active.dependencies,
            vec!["Dataset import".to_string(), "Feature store".to_string()]
        );
        assert_eq!(state.verification_count, 2);
        assert_eq!(state.completed_verification_count, 1);
    }

    #[test]
    fn falls_back_to_headings_and_labels_for_task_list() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".dw/tasks/ACTIVE.md"),
            r#"# Active Heading

Status: blocked
Phase: Planning
"#,
        );
        write(
            &temp.path().join(".dw/tasks/review.md"),
            r#"Task: Review compact context
State: needs review
"#,
        );
        write(&temp.path().join(".dw/tasks/non_utf8.md"), "temporary");
        fs::write(
            temp.path().join(".dw/tasks/non_utf8.md"),
            [0xff, 0xfe, 0xfd],
        )
        .unwrap();

        let state = read_project_state(temp.path());

        assert_eq!(state.tasks.len(), 2);
        assert_eq!(
            state.active_task.as_ref().unwrap().title.as_deref(),
            Some("Active Heading")
        );
        assert_eq!(
            state.active_task.as_ref().unwrap().status,
            TaskStatus::Blocked
        );

        let review = state
            .tasks
            .iter()
            .find(|task| task.path == Path::new(".dw/tasks/review.md"))
            .unwrap();
        assert_eq!(review.title.as_deref(), Some("Review compact context"));
        assert_eq!(review.status, TaskStatus::Review);
    }

    #[test]
    fn malformed_markdown_does_not_panic() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".dw/tasks/ACTIVE.md"),
            "---\ntitle without delimiter\n# A usable heading\n",
        );

        let state = read_project_state(temp.path());

        assert!(state.has_dw);
        assert_eq!(
            state.active_task.as_ref().unwrap().title.as_deref(),
            Some("A usable heading")
        );
    }

    #[test]
    fn supports_legacy_active_task_location() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".dw/ACTIVE.md"),
            "# Legacy Active\nphase: Discovery\nstatus: active\n",
        );

        let state = read_project_state(temp.path());
        let active = state.active_task.as_ref().unwrap();

        assert!(state.has_dw);
        assert_eq!(active.path, PathBuf::from(".dw/ACTIVE.md"));
        assert_eq!(active.title.as_deref(), Some("Legacy Active"));
        assert_eq!(active.phase.as_deref(), Some("Discovery"));
        assert_eq!(active.status, TaskStatus::Doing);
        assert_eq!(state.tasks.len(), 1);
    }
}
