use crate::app::{WorkspaceProject, WorkspaceTask};
use crate::model::{AgentSession, FileAccess, SessionStatus, ToolCall};
use crate::task_graph::TaskGraph;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEvidenceBundle {
    pub project: String,
    pub task: String,
    pub status: String,
    pub phase: String,
    pub next_action: String,
    pub acceptance_count: usize,
    pub verification_count: usize,
    pub completed_verification_count: usize,
    pub decision_count: usize,
    pub record_count: usize,
    pub graph_nodes: usize,
    pub graph_edges: usize,
    pub dependency_count: usize,
    pub agents: Vec<EvidenceAgent>,
    pub tools: Vec<String>,
    pub files: Vec<String>,
    pub risks: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceAgent {
    pub source: String,
    pub status: String,
    pub current_tool: String,
}

pub fn build_task_evidence(
    projects: &[WorkspaceProject],
    sessions: &[AgentSession],
    graph: &TaskGraph,
) -> Vec<TaskEvidenceBundle> {
    let mut bundles = Vec::new();
    for project in projects.iter().filter(|project| project.has_dw) {
        let project_sessions = sessions
            .iter()
            .filter(|session| session.cwd == project.cwd)
            .collect::<Vec<_>>();
        for task in &project.tasks {
            bundles.push(bundle_for_task(project, task, &project_sessions, graph));
        }
    }
    bundles
}

pub fn render_task_evidence_markdown(bundles: &[TaskEvidenceBundle]) -> String {
    let mut out = String::new();
    out.push_str("# abtop task evidence\n\n");
    out.push_str(&format!("- tasks: {}\n\n", bundles.len()));

    for bundle in bundles {
        out.push_str(&format!("## {} / {}\n\n", bundle.project, bundle.task));
        out.push_str(&format!(
            "- status: {}\n- phase: {}\n- next: {}\n- acceptance: {}\n- verification: {}/{}\n- dependencies: {}\n- decisions: {}\n- records: {}\n- graph: {} nodes, {} edges\n",
            bundle.status,
            bundle.phase,
            bundle.next_action,
            bundle.acceptance_count,
            bundle.completed_verification_count,
            bundle.verification_count,
            bundle.dependency_count,
            bundle.decision_count,
            bundle.record_count,
            bundle.graph_nodes,
            bundle.graph_edges
        ));

        if !bundle.risks.is_empty() {
            out.push_str("- risks: ");
            out.push_str(&bundle.risks.join(","));
            out.push('\n');
        }

        if !bundle.agents.is_empty() {
            out.push_str("- agents:\n");
            for agent in bundle.agents.iter().take(5) {
                out.push_str(&format!(
                    "  - {} {}: {}\n",
                    agent.source, agent.status, agent.current_tool
                ));
            }
        }

        if !bundle.tools.is_empty() {
            out.push_str("- tools: ");
            out.push_str(&bundle.tools.join(","));
            out.push('\n');
        }

        if !bundle.files.is_empty() {
            out.push_str("- files:\n");
            for file in bundle.files.iter().take(8) {
                out.push_str(&format!("  - {}\n", file));
            }
        }

        out.push('\n');
    }

    out
}

fn bundle_for_task(
    project: &WorkspaceProject,
    task: &WorkspaceTask,
    sessions: &[&AgentSession],
    graph: &TaskGraph,
) -> TaskEvidenceBundle {
    let (graph_nodes, graph_edges) = project_graph_counts(project, graph);
    TaskEvidenceBundle {
        project: sanitize_text(&project.name, 64),
        task: sanitize_text(&task.title, 96),
        status: sanitize_text(task.status_label(), 32),
        phase: task
            .phase
            .as_deref()
            .map(|phase| sanitize_text(phase, 48))
            .unwrap_or_else(|| "-".into()),
        next_action: task_next_action(task).into(),
        acceptance_count: task.acceptance_count,
        verification_count: task.verification_count,
        completed_verification_count: task.completed_verification_count,
        decision_count: project.decision_count,
        record_count: project.record_count,
        graph_nodes,
        graph_edges,
        dependency_count: task.dependencies.len(),
        agents: sessions
            .iter()
            .map(|session| evidence_agent(session))
            .collect(),
        tools: unique_sorted(
            sessions
                .iter()
                .flat_map(|session| session.tool_calls.iter())
                .map(tool_label),
        ),
        files: unique_sorted(
            sessions
                .iter()
                .flat_map(|session| session.file_accesses.iter())
                .filter_map(|access| safe_file_label(&project.cwd, access)),
        ),
        risks: project
            .attention
            .iter()
            .take(6)
            .map(|risk| sanitize_text(risk, 24))
            .collect(),
    }
}

fn evidence_agent(session: &AgentSession) -> EvidenceAgent {
    EvidenceAgent {
        source: sanitize_text(session.agent_cli, 16),
        status: session_status_label(&session.status).into(),
        current_tool: session
            .current_tasks
            .first()
            .map(|task| safe_tool_from_task(task))
            .unwrap_or_else(|| idle_text(&session.status).into()),
    }
}

fn tool_label(call: &ToolCall) -> String {
    sanitize_text(&call.name, 24)
}

fn safe_tool_from_task(task: &str) -> String {
    let first = task.split_whitespace().next().unwrap_or("working");
    sanitize_text(first, 24)
}

fn safe_file_label(cwd: &str, access: &FileAccess) -> Option<String> {
    let raw = access.path.trim();
    if raw.is_empty() {
        return None;
    }

    let relative = relative_path(cwd, raw)?;
    Some(format!(
        "{} {}",
        access.operation,
        sanitize_text(&relative, 96)
    ))
}

fn relative_path(cwd: &str, raw: &str) -> Option<String> {
    let normalized_raw = raw.replace('\\', "/");
    let normalized_cwd = cwd.replace('\\', "/");
    if normalized_raw.starts_with('/') {
        let prefix = format!("{}/", normalized_cwd.trim_end_matches('/'));
        return normalized_raw
            .strip_prefix(&prefix)
            .map(|relative| relative.to_string());
    }

    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        let cwd_path = Path::new(cwd);
        let stripped = raw_path.strip_prefix(cwd_path).ok()?;
        return Some(path_to_string(stripped));
    }

    if normalized_raw.contains(':') {
        return None;
    }

    Some(normalized_raw)
}

fn path_to_string(path: &Path) -> String {
    path.components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .replace('\\', "/")
}

fn project_graph_counts(project: &WorkspaceProject, graph: &TaskGraph) -> (usize, usize) {
    let prefix = format!("project:{}", slug(&project.name));
    let nodes = graph
        .nodes
        .iter()
        .filter(|node| node.id == prefix || node.id.starts_with(&format!("{}:", prefix)))
        .count();
    let edges = graph
        .edges
        .iter()
        .filter(|edge| edge.from.starts_with(&prefix) || edge.to.starts_with(&prefix))
        .count();
    (nodes, edges)
}

fn task_next_action(task: &WorkspaceTask) -> &'static str {
    match task.status {
        crate::task::TaskStatus::Ready => "start",
        crate::task::TaskStatus::Doing => "continue",
        crate::task::TaskStatus::Blocked => "unblock",
        crate::task::TaskStatus::Review => "verify",
        crate::task::TaskStatus::Done => "archive",
        crate::task::TaskStatus::Unknown => "inspect",
    }
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

fn idle_text(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "thinking",
        SessionStatus::Executing => "working",
        SessionStatus::Waiting => "waiting",
        SessionStatus::RateLimited => "rate-limited",
        SessionStatus::Done => "done",
    }
}

fn sanitize_text(value: &str, max_len: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .filter(|c| !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .take(max_len)
        .collect()
}

fn unique_sorted(values: impl Iterator<Item = String>) -> Vec<String> {
    let mut values = values.filter(|value| !value.is_empty()).collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
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
    use crate::model::{FileAccess, FileOp};
    use crate::task::TaskStatus;

    fn project() -> WorkspaceProject {
        WorkspaceProject {
            name: "ml-pipeline".into(),
            cwd: "/work/ml-pipeline".into(),
            has_dw: true,
            task_count: 1,
            decision_count: 2,
            record_count: 1,
            attention: vec!["ctx90".into()],
            tasks: vec![WorkspaceTask {
                title: "Batch rollout".into(),
                phase: Some("Verify".into()),
                status: TaskStatus::Review,
                raw_status: Some("Review".into()),
                acceptance_count: 3,
                verification_count: 2,
                completed_verification_count: 1,
                dependencies: vec!["Dataset import".into()],
                is_active: true,
            }],
            ..WorkspaceProject::default()
        }
    }

    fn session(cwd: String) -> AgentSession {
        AgentSession {
            agent_cli: "codex",
            pid: 1,
            session_id: "s1".into(),
            cwd,
            project_name: "ml-pipeline".into(),
            started_at: 0,
            status: SessionStatus::Executing,
            model: "gpt-5.4".into(),
            effort: "medium".into(),
            context_percent: 0.0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 0,
            current_tasks: vec!["Bash cargo test -- --nocapture".into()],
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
            initial_prompt: "Raw prompt should not appear".into(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: vec![ToolCall {
                name: "Bash".into(),
                arg: "cargo test".into(),
                duration_ms: 42,
            }],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![
                FileAccess {
                    path: "/work/ml-pipeline/src/lib.rs".into(),
                    operation: FileOp::Read,
                    turn_index: 1,
                },
                FileAccess {
                    path: "/outside/secret.txt".into(),
                    operation: FileOp::Read,
                    turn_index: 1,
                },
            ],
        }
    }

    #[test]
    fn builds_safe_task_evidence_bundle() {
        let project = project();
        let session = session(project.cwd.clone());
        let graph = TaskGraph::build(
            std::slice::from_ref(&project),
            std::slice::from_ref(&session),
        );

        let bundles = build_task_evidence(&[project], &[session], &graph);

        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].task, "Batch rollout");
        assert_eq!(bundles[0].status, "Review");
        assert_eq!(bundles[0].next_action, "verify");
        assert_eq!(bundles[0].verification_count, 2);
        assert_eq!(bundles[0].completed_verification_count, 1);
        assert_eq!(bundles[0].agents[0].current_tool, "Bash");
        assert!(bundles[0].files.iter().any(|file| file == "R src/lib.rs"));
        assert!(!bundles[0]
            .files
            .iter()
            .any(|file| file.contains("/outside/secret.txt")));
    }

    #[test]
    fn renders_markdown_without_prompt_or_absolute_paths() {
        let project = project();
        let session = session(project.cwd.clone());
        let graph = TaskGraph::build(
            std::slice::from_ref(&project),
            std::slice::from_ref(&session),
        );
        let bundles = build_task_evidence(&[project], &[session], &graph);
        let markdown = render_task_evidence_markdown(&bundles);

        assert!(markdown.contains("# abtop task evidence"));
        assert!(markdown.contains("## ml-pipeline / Batch rollout"));
        assert!(markdown.contains("- verification: 1/2"));
        assert!(markdown.contains("  - R src/lib.rs"));
        assert!(!markdown.contains("Raw prompt should not appear"));
        assert!(!markdown.contains("/work/ml-pipeline"));
        assert!(!markdown.contains("/outside/secret.txt"));
        assert!(!markdown.contains("cargo test -- --nocapture"));
    }
}
