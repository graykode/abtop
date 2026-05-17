use crate::app::{WorkspaceProject, WorkspaceTask};
use crate::task::TaskStatus;
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoadmapPlan {
    pub ready_count: usize,
    pub blocked_count: usize,
    pub stages: Vec<RoadmapStage>,
    pub risks: Vec<RoadmapRisk>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoadmapStage {
    pub index: usize,
    pub label: RoadmapStageLabel,
    pub tasks: Vec<RoadmapTask>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoadmapStageLabel {
    First,
    Next,
    Last,
}

impl RoadmapStageLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Next => "next",
            Self::Last => "last",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoadmapTask {
    pub title: String,
    pub status: String,
    pub dependency_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoadmapRisk {
    MissingDependency { task: String, dependency: String },
    BlockedTask { task: String },
    BlockedByTask { task: String, dependency: String },
    Cycle { tasks: Vec<String> },
}

pub fn build_project_roadmap(project: &WorkspaceProject) -> RoadmapPlan {
    let task_lookup: HashMap<String, usize> = project
        .tasks
        .iter()
        .enumerate()
        .map(|(idx, task)| (slug(&task.title), idx))
        .collect();
    let open_tasks: BTreeSet<usize> = project
        .tasks
        .iter()
        .enumerate()
        .filter_map(|(idx, task)| (task.status != TaskStatus::Done).then_some(idx))
        .collect();

    let mut risks = Vec::new();
    let mut remaining = open_tasks.clone();
    let mut stages = Vec::new();

    loop {
        let mut ready: Vec<usize> = remaining
            .iter()
            .copied()
            .filter(|idx| is_ready(project, &task_lookup, &remaining, *idx))
            .collect();
        ready.sort_by(|a, b| task_order(&project.tasks[*a]).cmp(&task_order(&project.tasks[*b])));

        if ready.is_empty() {
            break;
        }

        let stage_index = stages.len() + 1;
        for idx in &ready {
            remaining.remove(idx);
        }
        stages.push(RoadmapStage {
            index: stage_index,
            label: RoadmapStageLabel::Next,
            tasks: ready
                .into_iter()
                .map(|idx| roadmap_task(&project.tasks[idx]))
                .collect(),
        });
    }

    classify_remaining(project, &task_lookup, &remaining, &mut risks);
    label_stages(&mut stages);

    RoadmapPlan {
        ready_count: stages.first().map(|stage| stage.tasks.len()).unwrap_or(0),
        blocked_count: remaining.len(),
        stages,
        risks: dedupe_risks(risks),
    }
}

fn is_ready(
    project: &WorkspaceProject,
    task_lookup: &HashMap<String, usize>,
    remaining: &BTreeSet<usize>,
    idx: usize,
) -> bool {
    let task = &project.tasks[idx];
    if task.status == TaskStatus::Blocked {
        return false;
    }

    for dependency in &task.dependencies {
        let dep_slug = slug(dependency);
        let Some(dep_idx) = task_lookup.get(&dep_slug).copied() else {
            return false;
        };
        if remaining.contains(&dep_idx) {
            return false;
        }
    }

    true
}

fn classify_remaining(
    project: &WorkspaceProject,
    task_lookup: &HashMap<String, usize>,
    remaining: &BTreeSet<usize>,
    risks: &mut Vec<RoadmapRisk>,
) {
    let mut cycle_candidates = Vec::new();

    for idx in remaining {
        let task = &project.tasks[*idx];
        let task_title = sanitized(&task.title, 64);
        if task.status == TaskStatus::Blocked {
            risks.push(RoadmapRisk::BlockedTask { task: task_title });
            continue;
        }

        let mut dependency_risk = false;
        for dependency in &task.dependencies {
            let dep_slug = slug(dependency);
            let Some(dep_idx) = task_lookup.get(&dep_slug).copied() else {
                risks.push(RoadmapRisk::MissingDependency {
                    task: task_title.clone(),
                    dependency: sanitized(dependency, 64),
                });
                dependency_risk = true;
                continue;
            };
            if remaining.contains(&dep_idx) && project.tasks[dep_idx].status == TaskStatus::Blocked
            {
                risks.push(RoadmapRisk::BlockedByTask {
                    task: task_title.clone(),
                    dependency: sanitized(&project.tasks[dep_idx].title, 64),
                });
                dependency_risk = true;
            }
        }

        if !dependency_risk {
            cycle_candidates.push(task_title);
        }
    }

    if !cycle_candidates.is_empty() {
        cycle_candidates.sort();
        risks.push(RoadmapRisk::Cycle {
            tasks: cycle_candidates,
        });
    }
}

fn label_stages(stages: &mut [RoadmapStage]) {
    let last_idx = stages.len().saturating_sub(1);
    for (idx, stage) in stages.iter_mut().enumerate() {
        stage.label = if idx == 0 {
            RoadmapStageLabel::First
        } else if idx == last_idx {
            RoadmapStageLabel::Last
        } else {
            RoadmapStageLabel::Next
        };
    }
}

fn roadmap_task(task: &WorkspaceTask) -> RoadmapTask {
    RoadmapTask {
        title: sanitized(&task.title, 64),
        status: sanitized(task.status_label(), 32),
        dependency_count: task.dependencies.len(),
    }
}

fn task_order(task: &WorkspaceTask) -> (u8, String) {
    let rank = match task.status {
        TaskStatus::Ready => 0,
        TaskStatus::Doing => 1,
        TaskStatus::Review => 2,
        TaskStatus::Unknown => 3,
        TaskStatus::Blocked => 4,
        TaskStatus::Done => 5,
    };
    (rank, task.title.to_ascii_lowercase())
}

fn dedupe_risks(risks: Vec<RoadmapRisk>) -> Vec<RoadmapRisk> {
    let mut seen = BTreeSet::new();
    risks
        .into_iter()
        .filter(|risk| seen.insert(format!("{risk:?}")))
        .collect()
}

fn sanitized(value: &str, max_len: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .filter(|c| !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .take(max_len)
        .collect()
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }

    let out = out.trim_matches('-');
    if out.is_empty() {
        "unknown".into()
    } else {
        out.chars().take(48).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(title: &str, status: TaskStatus, dependencies: &[&str]) -> WorkspaceTask {
        WorkspaceTask {
            title: title.into(),
            status,
            raw_status: Some(status.label().into()),
            dependencies: dependencies.iter().map(|dep| (*dep).into()).collect(),
            ..WorkspaceTask::default()
        }
    }

    fn project(tasks: Vec<WorkspaceTask>) -> WorkspaceProject {
        WorkspaceProject {
            name: "migration".into(),
            has_dw: true,
            task_count: tasks.len(),
            dependency_count: tasks.iter().map(|task| task.dependencies.len()).sum(),
            tasks,
            ..WorkspaceProject::default()
        }
    }

    #[test]
    fn stages_tasks_by_dependency_readiness() {
        let project = project(vec![
            task("Ship API", TaskStatus::Ready, &["Migrate schema"]),
            task("Migrate schema", TaskStatus::Ready, &["Inventory tables"]),
            task("Inventory tables", TaskStatus::Ready, &[]),
            task("Retired spike", TaskStatus::Done, &[]),
        ]);

        let plan = build_project_roadmap(&project);

        assert_eq!(plan.ready_count, 1);
        assert_eq!(plan.blocked_count, 0);
        assert_eq!(plan.stages.len(), 3);
        assert_eq!(plan.stages[0].label, RoadmapStageLabel::First);
        assert_eq!(plan.stages[0].tasks[0].title, "Inventory tables");
        assert_eq!(plan.stages[1].label, RoadmapStageLabel::Next);
        assert_eq!(plan.stages[1].tasks[0].title, "Migrate schema");
        assert_eq!(plan.stages[2].label, RoadmapStageLabel::Last);
        assert_eq!(plan.stages[2].tasks[0].title, "Ship API");
        assert!(plan.risks.is_empty());
    }

    #[test]
    fn reports_missing_dependencies_and_blocked_chains() {
        let project = project(vec![
            task(
                "Deploy",
                TaskStatus::Ready,
                &["Security review", "Unknown task"],
            ),
            task("Security review", TaskStatus::Blocked, &[]),
        ]);

        let plan = build_project_roadmap(&project);

        assert_eq!(plan.ready_count, 0);
        assert_eq!(plan.blocked_count, 2);
        assert!(plan.risks.iter().any(|risk| matches!(
            risk,
            RoadmapRisk::BlockedTask { task } if task == "Security review"
        )));
        assert!(plan.risks.iter().any(|risk| matches!(
            risk,
            RoadmapRisk::MissingDependency { task, dependency }
                if task == "Deploy" && dependency == "Unknown task"
        )));
        assert!(plan.risks.iter().any(|risk| matches!(
            risk,
            RoadmapRisk::BlockedByTask { task, dependency }
                if task == "Deploy" && dependency == "Security review"
        )));
    }

    #[test]
    fn reports_cycles_without_exposing_control_text() {
        let project = project(vec![
            task("Alpha\nsecret", TaskStatus::Ready, &["Beta"]),
            task("Beta", TaskStatus::Ready, &["Alpha secret"]),
        ]);

        let plan = build_project_roadmap(&project);

        assert_eq!(plan.blocked_count, 2);
        assert!(plan.risks.iter().any(|risk| matches!(
            risk,
            RoadmapRisk::Cycle { tasks } if tasks.iter().all(|task| !task.contains('\n'))
        )));
    }
}
