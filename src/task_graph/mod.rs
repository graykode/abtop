use crate::app::{WorkspaceProject, WorkspaceTask};
use crate::model::{AgentSession, SessionStatus};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum GraphNodeKind {
    Project,
    Task,
    DecisionSet,
    RecordSet,
    Verification,
    Agent,
    Risk,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub kind: GraphNodeKind,
    pub label: String,
    pub weight: u32,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum GraphEdgeKind {
    Contains,
    ActiveTask,
    HasDecisionSet,
    HasRecordSet,
    HasVerification,
    DependsOn,
    WorkedBy,
    HasRisk,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct GraphEdge {
    pub from: String,
    pub to: String,
    pub kind: GraphEdgeKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

impl TaskGraph {
    pub fn build(projects: &[WorkspaceProject], sessions: &[AgentSession]) -> Self {
        let mut graph = Self::default();

        for project in projects {
            let project_id = node_id("project", &project.name);
            graph.push_node(GraphNode {
                id: project_id.clone(),
                kind: GraphNodeKind::Project,
                label: sanitize_label(&project.name, 48),
                weight: project.session_count.max(project.task_count).max(1) as u32,
            });

            for task in &project.tasks {
                let task_id = task_node_id(project, task);
                graph.push_node(GraphNode {
                    id: task_id.clone(),
                    kind: GraphNodeKind::Task,
                    label: sanitize_label(&task.title, 64),
                    weight: task_weight(task),
                });
                graph.push_edge(&project_id, &task_id, GraphEdgeKind::Contains);

                if task.is_active {
                    graph.push_edge(&project_id, &task_id, GraphEdgeKind::ActiveTask);
                }

                if task.verification_count > 0 {
                    let verification_id = format!("{}:verification", task_id);
                    graph.push_node(GraphNode {
                        id: verification_id.clone(),
                        kind: GraphNodeKind::Verification,
                        label: format!(
                            "verification {}/{}",
                            task.completed_verification_count, task.verification_count
                        ),
                        weight: task.verification_count as u32,
                    });
                    graph.push_edge(&task_id, &verification_id, GraphEdgeKind::HasVerification);
                }

                for dependency in &task.dependencies {
                    if let Some(dep_task) = find_dependency_task(project, dependency) {
                        let dep_id = task_node_id(project, dep_task);
                        graph.push_edge(&task_id, &dep_id, GraphEdgeKind::DependsOn);
                    }
                }
            }

            if project.decision_count > 0 {
                let decision_id = format!("{}:decisions", project_id);
                graph.push_node(GraphNode {
                    id: decision_id.clone(),
                    kind: GraphNodeKind::DecisionSet,
                    label: format!("decisions {}", project.decision_count),
                    weight: project.decision_count as u32,
                });
                graph.push_edge(&project_id, &decision_id, GraphEdgeKind::HasDecisionSet);
            }

            if project.record_count > 0 {
                let record_id = format!("{}:records", project_id);
                graph.push_node(GraphNode {
                    id: record_id.clone(),
                    kind: GraphNodeKind::RecordSet,
                    label: format!("records {}", project.record_count),
                    weight: project.record_count as u32,
                });
                graph.push_edge(&project_id, &record_id, GraphEdgeKind::HasRecordSet);
            }

            for risk in project.attention.iter().take(4) {
                let risk_id = format!("{}:risk:{}", project_id, slug(risk));
                graph.push_node(GraphNode {
                    id: risk_id.clone(),
                    kind: GraphNodeKind::Risk,
                    label: sanitize_label(risk, 24),
                    weight: 1,
                });
                graph.push_edge(&project_id, &risk_id, GraphEdgeKind::HasRisk);
            }

            for session in sessions
                .iter()
                .filter(|session| project.matches_session(session))
            {
                let agent_id = format!("{}:agent:{}", project_id, slug(&session.session_id));
                graph.push_node(GraphNode {
                    id: agent_id.clone(),
                    kind: GraphNodeKind::Agent,
                    label: format!(
                        "{} {}",
                        session.agent_cli,
                        session_status_label(&session.status)
                    ),
                    weight: 1,
                });
                graph.push_edge(&agent_id, &project_id, GraphEdgeKind::WorkedBy);
            }
        }

        graph
    }

    fn push_node(&mut self, node: GraphNode) {
        if !self.nodes.iter().any(|existing| existing.id == node.id) {
            self.nodes.push(node);
        }
    }

    fn push_edge(&mut self, from: &str, to: &str, kind: GraphEdgeKind) {
        let edge = GraphEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind,
        };
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
    }

    pub fn node_count(&self, kind: GraphNodeKind) -> usize {
        self.nodes.iter().filter(|node| node.kind == kind).count()
    }
}

fn find_dependency_task<'a>(
    project: &'a WorkspaceProject,
    dependency: &str,
) -> Option<&'a WorkspaceTask> {
    let normalized = slug(dependency);
    project
        .tasks
        .iter()
        .find(|task| slug(&task.title) == normalized)
}

fn task_node_id(project: &WorkspaceProject, task: &WorkspaceTask) -> String {
    format!(
        "{}:task:{}",
        node_id("project", &project.name),
        slug(&task.title)
    )
}

fn node_id(prefix: &str, value: &str) -> String {
    format!("{}:{}", prefix, slug(value))
}

fn task_weight(task: &WorkspaceTask) -> u32 {
    let status_bonus = match task.status {
        crate::task::TaskStatus::Blocked => 5,
        crate::task::TaskStatus::Review => 4,
        crate::task::TaskStatus::Doing => 3,
        crate::task::TaskStatus::Ready => 2,
        crate::task::TaskStatus::Unknown | crate::task::TaskStatus::Done => 1,
    };
    status_bonus + task.acceptance_count as u32 + task.verification_count as u32
}

fn session_status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "thinking",
        SessionStatus::Executing => "working",
        SessionStatus::Waiting => "waiting",
        SessionStatus::RateLimited => "rate-limited",
        SessionStatus::Done => "done",
    }
}

fn sanitize_label(value: &str, max_len: usize) -> String {
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
    use crate::app::{WorkspaceProject, WorkspaceTask};
    use crate::model::AgentSession;
    use crate::task::TaskStatus;

    fn demo_project() -> WorkspaceProject {
        WorkspaceProject {
            name: "ml-pipeline".into(),
            cwd: "/tmp/ml-pipeline".into(),
            session_count: 1,
            has_dw: true,
            attention: vec!["ctx90".into()],
            task_count: 2,
            decision_count: 2,
            record_count: 1,
            tasks: vec![
                WorkspaceTask {
                    title: "Batch inference rollout".into(),
                    phase: Some("Execute".into()),
                    status: TaskStatus::Doing,
                    raw_status: Some("Doing".into()),
                    acceptance_count: 3,
                    verification_count: 2,
                    completed_verification_count: 1,
                    dependencies: vec!["Dataset drift guardrails".into()],
                    is_active: true,
                },
                WorkspaceTask {
                    title: "Dataset drift guardrails".into(),
                    phase: Some("Plan".into()),
                    status: TaskStatus::Blocked,
                    raw_status: Some("Blocked".into()),
                    acceptance_count: 2,
                    verification_count: 1,
                    completed_verification_count: 0,
                    dependencies: Vec::new(),
                    is_active: false,
                },
            ],
            ..WorkspaceProject::default()
        }
    }

    fn session(cwd: String, project_name: String) -> AgentSession {
        AgentSession {
            agent_cli: "codex",
            pid: 1234,
            session_id: "abc123".into(),
            cwd,
            project_name,
            started_at: 0,
            status: SessionStatus::Executing,
            model: "gpt-5.4".into(),
            effort: "medium".into(),
            context_percent: 42.0,
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
            context_window: 200_000,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
        }
    }

    #[test]
    fn builds_project_task_and_evidence_nodes() {
        let project = demo_project();
        let session = session(project.cwd.clone(), project.name.clone());

        let graph = TaskGraph::build(&[project], &[session]);

        assert_eq!(graph.node_count(GraphNodeKind::Project), 1);
        assert_eq!(graph.node_count(GraphNodeKind::Task), 2);
        assert_eq!(graph.node_count(GraphNodeKind::Verification), 2);
        assert_eq!(graph.node_count(GraphNodeKind::DecisionSet), 1);
        assert_eq!(graph.node_count(GraphNodeKind::RecordSet), 1);
        assert_eq!(graph.node_count(GraphNodeKind::Agent), 1);
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.kind == GraphEdgeKind::ActiveTask));
        assert!(graph
            .edges
            .iter()
            .any(|edge| edge.kind == GraphEdgeKind::DependsOn));
        assert!(graph
            .nodes
            .iter()
            .any(|node| node.label == "Batch inference rollout"));
    }

    #[test]
    fn sanitizes_labels_and_does_not_include_prompts_or_paths() {
        let mut project = demo_project();
        project.name = "secret\nproject".into();
        project.tasks[0].title = "Customer task\nwith control char".into();
        let mut session = session("/very/private/path".into(), "private".into());
        session.initial_prompt = "Raw prompt should not appear".into();

        let graph = TaskGraph::build(&[project], &[session]);
        let labels = graph
            .nodes
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();

        assert!(labels.iter().all(|label| !label.contains('\n')));
        assert!(labels
            .iter()
            .all(|label| !label.contains("/very/private/path")));
        assert!(labels
            .iter()
            .all(|label| !label.contains("Raw prompt should not appear")));
    }

    #[test]
    fn deduplicates_nodes_and_edges() {
        let project = demo_project();
        let graph = TaskGraph::build(&[project], &[]);
        let unique_nodes = graph
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let unique_edges = graph.edges.iter().collect::<std::collections::HashSet<_>>();

        assert_eq!(unique_nodes.len(), graph.nodes.len());
        assert_eq!(unique_edges.len(), graph.edges.len());
    }
}
