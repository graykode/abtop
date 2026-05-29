//! dw-kit Document Schema + Index v1.0 readers (dw-kit ADR-0017).
//!
//! abtop historically scraped `.dw/tasks/*.md` by hand and ignored the Goals
//! layer entirely. dw-kit now publishes committed, machine-readable indices —
//! `goals-index@v1` and `tasks-index@v1` — so a consumer reads goal/task state
//! from one JSON file instead of reverse-engineering markdown. This module is
//! the Rust read-adapter for that contract (read-only; dw-kit stays passive).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

// ---------- goals-index@v1 ----------

#[derive(Debug, Clone, Deserialize, Default)]
struct GoalProgress {
    #[serde(default)]
    percent: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GoalIndexEntry {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    cycle: Option<String>,
    #[serde(default)]
    target_date: Option<String>,
    #[serde(default)]
    parent_goal_id: Option<String>,
    #[serde(default)]
    linked_task_ids: Vec<String>,
    #[serde(default)]
    progress: Option<GoalProgress>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct GoalsIndexFile {
    #[serde(default)]
    schema_version: Option<String>,
    #[serde(default)]
    goals: BTreeMap<String, GoalIndexEntry>,
}

/// Normalized goal projection for the workspace view.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DwGoalSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    pub progress_percent: i64,
    pub cycle: Option<String>,
    pub target_date: Option<String>,
    pub parent_goal_id: Option<String>,
    pub linked_task_ids: Vec<String>,
}

/// Read `.dw/goals/goals-index.json`. Tolerant: missing file or unparseable
/// JSON yields an empty list (never panics). Sorted by goal id for stable UI.
pub fn read_goals_index(cwd: &Path) -> Vec<DwGoalSummary> {
    let file = cwd.join(".dw").join("goals").join("goals-index.json");
    let Ok(text) = fs::read_to_string(&file) else {
        return Vec::new();
    };
    let Ok(index) = serde_json::from_str::<GoalsIndexFile>(&text) else {
        return Vec::new();
    };
    let _ = index.schema_version; // reserved for future major-version gating
    let mut out: Vec<DwGoalSummary> = index
        .goals
        .into_iter()
        .map(|(id, e)| DwGoalSummary {
            title: e.title.unwrap_or_else(|| id.clone()),
            status: e.status.unwrap_or_default(),
            progress_percent: e.progress.map(|p| p.percent).unwrap_or(0),
            cycle: e.cycle,
            target_date: e.target_date,
            parent_goal_id: e.parent_goal_id.filter(|s| s != "none" && !s.is_empty()),
            linked_task_ids: e.linked_task_ids,
            id,
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

// ---------- tasks-index@v1 ----------

#[derive(Debug, Clone, Deserialize, Default)]
struct TaskIndexEntry {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    parent_goal_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct TasksIndexFile {
    #[serde(default)]
    tasks: BTreeMap<String, TaskIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DwIndexedTask {
    pub id: String,
    pub title: String,
    pub status: String,
    pub phase: Option<String>,
    pub parent_goal_id: Option<String>,
}

/// Read `.dw/tasks/tasks-index.json` if present. Returns `None` when the index
/// file is absent so the caller can fall back to the legacy markdown scrape.
pub fn read_tasks_index(cwd: &Path) -> Option<Vec<DwIndexedTask>> {
    let file = cwd.join(".dw").join("tasks").join("tasks-index.json");
    let text = fs::read_to_string(&file).ok()?;
    let index = serde_json::from_str::<TasksIndexFile>(&text).ok()?;
    let mut out: Vec<DwIndexedTask> = index
        .tasks
        .into_iter()
        .map(|(id, e)| DwIndexedTask {
            title: e.title.unwrap_or_else(|| id.clone()),
            status: e.status.unwrap_or_default(),
            phase: e.phase,
            parent_goal_id: e.parent_goal_id.filter(|s| s != "none" && !s.is_empty()),
            id,
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn missing_goals_index_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        assert!(read_goals_index(temp.path()).is_empty());
        assert!(read_tasks_index(temp.path()).is_none());
    }

    #[test]
    fn reads_goals_index_v1() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".dw/goals/goals-index.json"),
            r#"{
              "schema_version": "goals-index@v1",
              "goals": {
                "G-b": { "title": "Beta", "status": "Active", "cycle": "2026-Q2",
                         "target_date": "2026-08-15", "parent_goal_id": "none",
                         "linked_task_ids": ["t1","t2"], "progress": { "percent": 40 } },
                "G-a": { "title": "Alpha", "status": "Achieved", "progress": { "percent": 100 } }
              }
            }"#,
        );
        let goals = read_goals_index(temp.path());
        assert_eq!(goals.len(), 2);
        // sorted by id → G-a first
        assert_eq!(goals[0].id, "G-a");
        assert_eq!(goals[0].status, "Achieved");
        assert_eq!(goals[0].progress_percent, 100);
        assert_eq!(goals[0].parent_goal_id, None);
        assert_eq!(goals[1].id, "G-b");
        assert_eq!(goals[1].title, "Beta");
        assert_eq!(goals[1].progress_percent, 40);
        assert_eq!(goals[1].cycle.as_deref(), Some("2026-Q2"));
        assert_eq!(goals[1].parent_goal_id, None); // "none" normalized away
        assert_eq!(goals[1].linked_task_ids, vec!["t1", "t2"]);
    }

    #[test]
    fn tolerates_garbage_and_missing_fields() {
        let temp = tempfile::tempdir().unwrap();
        write(&temp.path().join(".dw/goals/goals-index.json"), "not json {");
        assert!(read_goals_index(temp.path()).is_empty());

        let temp2 = tempfile::tempdir().unwrap();
        write(
            &temp2.path().join(".dw/goals/goals-index.json"),
            r#"{ "goals": { "G-x": {} } }"#,
        );
        let goals = read_goals_index(temp2.path());
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0].id, "G-x");
        assert_eq!(goals[0].title, "G-x"); // falls back to id
        assert_eq!(goals[0].progress_percent, 0);
    }

    #[test]
    fn reads_tasks_index_v1() {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join(".dw/tasks/tasks-index.json"),
            r#"{ "schema_version": "tasks-index@v1", "tasks": {
                 "alpha": { "title": "Alpha", "status": "In Progress", "phase": "WS-3",
                            "parent_goal_id": "G-a" } } }"#,
        );
        let tasks = read_tasks_index(temp.path()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "alpha");
        assert_eq!(tasks[0].status, "In Progress");
        assert_eq!(tasks[0].parent_goal_id.as_deref(), Some("G-a"));
    }
}
