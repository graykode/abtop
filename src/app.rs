use crate::audit::{record as record_audit, AuditEvent};
use crate::collector::{read_rate_limits, McpServer, MultiCollector};
use crate::evidence::{build_task_evidence, render_task_evidence_markdown};
use crate::host_info::{AgentAggregate, HostMetrics, HostSampler};
use crate::model::{AgentSession, OrphanPort, RateLimitInfo, SessionStatus};
use crate::task::{read_project_state, DwTaskSummary, TaskStatus};
use crate::task_graph::{GraphNodeKind, TaskGraph};
use crate::theme::Theme;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::time::Instant;

/// Maximum data points kept for the live token-rate graph.
const GRAPH_HISTORY_LEN: usize = 200;
/// Max concurrent summary jobs.
const MAX_SUMMARY_JOBS: usize = 3;
/// Max summary attempts per session before giving up.
const MAX_SUMMARY_RETRIES: u32 = 2;
const ATTENTION_CONTEXT_WARN_PCT: f64 = 80.0;
const ATTENTION_CONTEXT_CRITICAL_PCT: f64 = 90.0;
const SUMMARY_UNAVAILABLE: &str = "summary unavailable";
const KILL_CONFIRM_WINDOW_SECS: u64 = 2;

/// Produce a terminal-safe fallback summary from a raw prompt.
fn sanitize_fallback(prompt: &str, max_len: usize) -> String {
    let terminal_safe = crate::collector::sanitize_terminal_text(prompt);
    let redacted = crate::collector::redact_secrets(&terminal_safe);
    redacted.chars().take(max_len).collect()
}

/// Outcome of an Enter-key jump attempt. Distinct from `Option<String>` so
/// callers (notably `--exit-on-jump`) can tell a real tmux jump apart from
/// a no-op (outside tmux, or empty session list).
pub enum JumpOutcome {
    /// Actually switched to a tmux pane.
    Jumped,
    /// Tried to jump in tmux but no pane owns the session's PID.
    Failed(String),
    /// Not in tmux, or nothing selected — nothing happened.
    NoOp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowTab {
    Workspace,
    Work,
    Usage,
    System,
}

impl NarrowTab {
    pub const ALL: [Self; 4] = [Self::Workspace, Self::Work, Self::Usage, Self::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Workspace => "Workspace",
            Self::Work => "Work",
            Self::Usage => "Usage",
            Self::System => "System",
        }
    }

    pub fn shortcut(self) -> char {
        match self {
            Self::Workspace => 'a',
            Self::Work => 'w',
            Self::Usage => 'u',
            Self::System => 's',
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowSection {
    Workspace,
    Sessions,
    Projects,
    Context,
    Quota,
    Tokens,
    Ports,
    Mcp,
}

impl NarrowSection {
    pub fn tab(self) -> NarrowTab {
        match self {
            Self::Workspace => NarrowTab::Workspace,
            Self::Sessions | Self::Projects => NarrowTab::Work,
            Self::Context | Self::Quota | Self::Tokens => NarrowTab::Usage,
            Self::Ports | Self::Mcp => NarrowTab::System,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceLens {
    All,
    Attention,
    Workflow,
    Tasks,
}

impl WorkspaceLens {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Attention => "attention",
            Self::Workflow => ".dw",
            Self::Tasks => "tasks",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceTask {
    pub title: String,
    pub phase: Option<String>,
    pub status: TaskStatus,
    pub raw_status: Option<String>,
    pub acceptance_count: usize,
    pub verification_count: usize,
    pub completed_verification_count: usize,
    pub dependencies: Vec<String>,
    pub is_active: bool,
}

impl WorkspaceTask {
    fn from_summary(summary: DwTaskSummary) -> Self {
        let title = summary
            .title
            .unwrap_or_else(|| summary.path.to_string_lossy().into_owned());
        Self {
            title,
            phase: summary.phase,
            status: summary.status,
            raw_status: summary.raw_status,
            acceptance_count: summary.acceptance_count,
            verification_count: summary.verification_count,
            completed_verification_count: summary.completed_verification_count,
            dependencies: summary.dependencies,
            is_active: summary.is_active,
        }
    }

    pub fn status_label(&self) -> &str {
        self.raw_status
            .as_deref()
            .unwrap_or_else(|| self.status.label())
    }
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceProject {
    pub name: String,
    pub cwd: String,
    pub session_count: usize,
    pub active_count: usize,
    pub waiting_count: usize,
    pub rate_limited_count: usize,
    pub max_context_percent: f64,
    pub total_tokens: u64,
    pub child_count: usize,
    pub port_count: usize,
    pub git_added: u32,
    pub git_modified: u32,
    pub has_dw: bool,
    pub has_active_task: bool,
    pub active_task_title: Option<String>,
    pub active_task_phase: Option<String>,
    pub active_task_status: TaskStatus,
    pub active_task_raw_status: Option<String>,
    pub active_task_acceptance_count: usize,
    pub task_count: usize,
    pub decision_count: usize,
    pub record_count: usize,
    pub verification_count: usize,
    pub completed_verification_count: usize,
    pub dependency_count: usize,
    pub tasks: Vec<WorkspaceTask>,
    pub attention_score: u32,
    pub attention: Vec<String>,
}

impl WorkspaceProject {
    pub(crate) fn from_sessions(sessions: &[AgentSession]) -> Vec<Self> {
        let mut by_cwd: HashMap<String, WorkspaceProject> = HashMap::new();
        for session in sessions {
            let entry = by_cwd
                .entry(session.cwd.clone())
                .or_insert_with(|| WorkspaceProject {
                    name: session.project_name.clone(),
                    cwd: session.cwd.clone(),
                    ..WorkspaceProject::default()
                });
            entry.session_count += 1;
            match session.status {
                SessionStatus::Thinking | SessionStatus::Executing => entry.active_count += 1,
                SessionStatus::Waiting => entry.waiting_count += 1,
                SessionStatus::RateLimited => entry.rate_limited_count += 1,
                SessionStatus::Done => {}
            }
            entry.max_context_percent = entry.max_context_percent.max(session.context_percent);
            entry.total_tokens = entry.total_tokens.saturating_add(session.active_tokens());
            entry.child_count += session.children.len();
            entry.port_count += session.children.iter().filter(|c| c.port.is_some()).count();
            entry.git_added = entry.git_added.saturating_add(session.git_added);
            entry.git_modified = entry.git_modified.saturating_add(session.git_modified);
        }

        let mut projects: Vec<_> = by_cwd
            .into_values()
            .map(|mut project| {
                project.populate_workflow_hints();
                project.compute_attention();
                project
            })
            .collect();
        projects.sort_by(|a, b| {
            b.attention_score
                .cmp(&a.attention_score)
                .then_with(|| b.active_count.cmp(&a.active_count))
                .then_with(|| b.rate_limited_count.cmp(&a.rate_limited_count))
                .then_with(|| b.waiting_count.cmp(&a.waiting_count))
                .then_with(|| b.session_count.cmp(&a.session_count))
                .then_with(|| a.name.cmp(&b.name))
        });
        projects
    }

    fn populate_workflow_hints(&mut self) {
        let state = read_project_state(std::path::Path::new(&self.cwd));
        self.has_dw = state.has_dw;
        self.task_count = state.tasks.len();
        self.decision_count = state.decision_count;
        self.record_count = state.record_count;
        self.verification_count = state.verification_count;
        self.completed_verification_count = state.completed_verification_count;
        self.tasks = state
            .tasks
            .into_iter()
            .map(WorkspaceTask::from_summary)
            .collect();
        self.tasks.sort_by(|a, b| {
            b.is_active
                .cmp(&a.is_active)
                .then_with(|| task_status_rank(a.status).cmp(&task_status_rank(b.status)))
                .then_with(|| a.title.cmp(&b.title))
        });
        self.dependency_count = self.tasks.iter().map(|task| task.dependencies.len()).sum();

        if let Some(active_task) = state.active_task {
            self.has_active_task = true;
            self.active_task_title = active_task.title;
            self.active_task_phase = active_task.phase;
            self.active_task_status = active_task.status;
            self.active_task_raw_status = active_task.raw_status;
            self.active_task_acceptance_count = active_task.acceptance_count;
        }
    }

    pub fn active_task_next_action(&self) -> &'static str {
        if !self.has_active_task {
            return "choose task";
        }

        match self.active_task_status {
            TaskStatus::Ready => "start",
            TaskStatus::Doing => "continue",
            TaskStatus::Blocked => "unblock",
            TaskStatus::Review => "verify",
            TaskStatus::Done => "archive",
            TaskStatus::Unknown => "inspect",
        }
    }

    fn compute_attention(&mut self) {
        self.attention.clear();
        self.attention_score = 0;

        if self.rate_limited_count > 0 {
            self.attention_score += 100;
            self.attention.push("rate".into());
        }
        if self.max_context_percent >= ATTENTION_CONTEXT_CRITICAL_PCT {
            self.attention_score += 90;
            self.attention.push("ctx90".into());
        } else if self.max_context_percent >= ATTENTION_CONTEXT_WARN_PCT {
            self.attention_score += 60;
            self.attention.push("ctx80".into());
        }
        if self.waiting_count > 0 {
            self.attention_score += 50;
            self.attention.push("input".into());
        }
        if self.port_count > 0 {
            self.attention_score += 30;
            self.attention.push("ports".into());
        }
        if self.git_added > 0 || self.git_modified > 0 {
            self.attention_score += 20;
            self.attention.push("git".into());
        }
        if self.has_dw && !self.has_active_task {
            self.attention_score += 10;
            self.attention.push("no-task".into());
        }
    }
}

fn task_status_rank(status: TaskStatus) -> u8 {
    match status {
        TaskStatus::Blocked => 0,
        TaskStatus::Review => 1,
        TaskStatus::Doing => 2,
        TaskStatus::Ready => 3,
        TaskStatus::Unknown => 4,
        TaskStatus::Done => 5,
    }
}

pub struct App {
    pub sessions: Vec<AgentSession>,
    pub selected: usize,
    pub should_quit: bool,
    /// Token rate per tick (delta). Ring buffer for the braille graph.
    pub token_rates: VecDeque<f64>,
    /// Account-level rate limits (Claude, Codex, etc.)
    pub rate_limits: Vec<RateLimitInfo>,
    /// Per-session previous token totals, keyed by (agent_cli, session_id).
    prev_tokens: HashMap<(String, String), u64>,
    /// Rate limit poll counter (read every 5 ticks = 10s)
    rate_limit_counter: u32,
    collector: MultiCollector,
    /// Cached LLM-generated summaries, keyed by session_id.
    pub summaries: HashMap<String, String>,
    /// Session IDs currently being summarized.
    pending_summaries: HashSet<String>,
    /// Per-session retry count for failed summary attempts.
    summary_retries: HashMap<String, u32>,
    /// Channel to receive completed summaries from background threads.
    /// Tuple: (session_id, prompt, maybe_summary).
    summary_rx: mpsc::Receiver<(String, String, Option<String>)>,
    summary_tx: mpsc::Sender<(String, String, Option<String>)>,
    /// Ports left open by processes whose parent sessions have ended.
    pub orphan_ports: Vec<OrphanPort>,
    /// Transient status message shown in the footer (auto-clears after 3s).
    pub status_msg: Option<(String, Instant)>,
    /// Kill confirmation: (selected_index, timestamp). Expires after 2s.
    kill_confirm: Option<(usize, Instant)>,
    /// Orphan-port kill confirmation timestamp. Expires after 2s.
    orphan_kill_confirm: Option<Instant>,
    pub theme: Theme,
    pub show_context: bool,
    pub show_quota: bool,
    pub show_tokens: bool,
    pub show_projects: bool,
    pub show_ports: bool,
    pub show_sessions: bool,
    pub show_mcp: bool,
    pub narrow_tab: NarrowTab,
    pub workspace_focus: bool,
    pub workspace_selected: usize,
    pub workspace_lens: WorkspaceLens,
    pub active_narrow_section: Option<NarrowSection>,
    pub maximized_narrow_section: Option<NarrowSection>,
    /// MCP servers detected on the most recent tick (sourced from
    /// MultiCollector). Populated regardless of `show_mcp` so panel
    /// toggling doesn't cost a discovery roundtrip.
    pub mcp_servers: Vec<McpServer>,
    /// When true (default), mcp-server-owned rollouts are hidden from
    /// the sessions panel. Toggle with Shift+M.
    pub mcp_suppress_sessions: bool,
    pub config_open: bool,
    pub config_selected: usize,
    pub tree_view: bool,
    pub filter_text: String,
    pub filter_active: bool,
    pub show_timeline: bool,
    pub timeline_scroll: usize,
    pub show_file_audit: bool,
    /// Host vitals sampler (CPU% delta needs prior snapshot).
    host_sampler: HostSampler,
    /// Latest host metrics snapshot (None until first valid sample).
    pub host_metrics: Option<HostMetrics>,
    /// Aggregate metrics across all sessions (recomputed each tick).
    pub agent_aggregate: AgentAggregate,
    /// Agentic workspace project rollup, recomputed each tick.
    pub workspace_projects: Vec<WorkspaceProject>,
    /// Help overlay (`?`) visibility.
    pub help_open: bool,
    /// View leader overlay (`v`) visibility.
    pub view_open: bool,
}

impl App {
    pub fn new_with_config(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let summaries = load_summary_cache();
        let mut collector = MultiCollector::with_hidden(hidden_agents);
        collector.set_mcp_suppress(true);
        Self {
            sessions: Vec::new(),
            selected: 0,
            should_quit: false,
            token_rates: VecDeque::with_capacity(GRAPH_HISTORY_LEN),
            rate_limits: Vec::new(),
            prev_tokens: HashMap::new(),
            rate_limit_counter: 5,
            collector,
            summaries,
            pending_summaries: HashSet::new(),
            summary_retries: HashMap::new(),
            summary_rx: rx,
            summary_tx: tx,
            orphan_ports: Vec::new(),
            status_msg: None,
            kill_confirm: None,
            orphan_kill_confirm: None,
            theme,
            show_context: panels.context,
            show_quota: panels.quota,
            show_tokens: panels.tokens,
            show_projects: panels.projects,
            show_ports: panels.ports,
            show_sessions: panels.sessions,
            show_mcp: panels.mcp,
            narrow_tab: NarrowTab::Work,
            workspace_focus: false,
            workspace_selected: 0,
            workspace_lens: WorkspaceLens::All,
            active_narrow_section: Some(NarrowSection::Sessions),
            maximized_narrow_section: None,
            mcp_servers: Vec::new(),
            mcp_suppress_sessions: true,
            config_open: false,
            config_selected: 0,
            tree_view: false,
            filter_text: String::new(),
            filter_active: false,
            show_timeline: false,
            timeline_scroll: 0,
            show_file_audit: false,
            host_sampler: HostSampler::new(),
            host_metrics: None,
            agent_aggregate: AgentAggregate::default(),
            workspace_projects: Vec::new(),
            help_open: false,
            view_open: false,
        }
    }

    pub fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
        if self.help_open {
            self.view_open = false;
        }
    }

    pub fn toggle_view_menu(&mut self) {
        self.view_open = !self.view_open;
        if self.view_open {
            self.help_open = false;
        }
    }

    pub fn toggle_panel(&mut self, panel: u8) {
        match panel {
            1 => self.show_context = !self.show_context,
            2 => self.show_quota = !self.show_quota,
            3 => self.show_tokens = !self.show_tokens,
            4 => self.show_projects = !self.show_projects,
            5 => self.show_ports = !self.show_ports,
            6 => self.show_sessions = !self.show_sessions,
            7 => self.show_mcp = !self.show_mcp,
            _ => return,
        }
        self.persist_panel_visibility();
        self.clamp_narrow_tab();
    }

    /// Toggle whether mcp-server-owned rollouts are hidden from the
    /// sessions panel. Default is on; turning it off restores upstream
    /// behavior so the user can see exactly what mcp-server fd holding
    /// produces (mostly stale "Done" rows).
    pub fn toggle_mcp_session_suppression(&mut self) {
        self.mcp_suppress_sessions = !self.mcp_suppress_sessions;
        let label = if self.mcp_suppress_sessions {
            "on"
        } else {
            "off"
        };
        self.set_status(format!("mcp session suppression: {}", label));
    }

    fn persist_panel_visibility(&mut self) {
        let panels = crate::config::PanelVisibility {
            context: self.show_context,
            quota: self.show_quota,
            tokens: self.show_tokens,
            projects: self.show_projects,
            ports: self.show_ports,
            sessions: self.show_sessions,
            mcp: self.show_mcp,
        };
        if let Err(e) = crate::config::save_panel_visibility(&panels) {
            self.set_status(format!("panels save failed: {}", e));
        }
    }

    pub fn toggle_file_audit(&mut self) {
        self.show_file_audit = !self.show_file_audit;
    }

    pub fn toggle_config(&mut self) {
        self.config_open = !self.config_open;
        if self.config_open {
            self.config_selected = 0;
        }
    }

    pub fn config_item_count(&self) -> usize {
        8 // theme + 7 panel toggles
    }

    pub fn config_select_next(&mut self) {
        if self.config_selected + 1 < self.config_item_count() {
            self.config_selected += 1;
        }
    }

    pub fn config_select_prev(&mut self) {
        self.config_selected = self.config_selected.saturating_sub(1);
    }

    pub fn config_toggle_selected(&mut self) {
        match self.config_selected {
            0 => {
                self.cycle_theme();
                return;
            }
            1 => self.show_context = !self.show_context,
            2 => self.show_quota = !self.show_quota,
            3 => self.show_tokens = !self.show_tokens,
            4 => self.show_projects = !self.show_projects,
            5 => self.show_ports = !self.show_ports,
            6 => self.show_sessions = !self.show_sessions,
            7 => self.show_mcp = !self.show_mcp,
            _ => return,
        }
        self.persist_panel_visibility();
        self.clamp_narrow_tab();
    }

    pub fn narrow_tab_visible(&self, tab: NarrowTab) -> bool {
        match tab {
            NarrowTab::Workspace => self.show_sessions || self.show_projects,
            NarrowTab::Work => self.show_sessions || self.show_projects,
            NarrowTab::Usage => self.show_context || self.show_quota || self.show_tokens,
            NarrowTab::System => self.show_ports || self.show_mcp,
        }
    }

    pub fn visible_narrow_tabs(&self) -> Vec<NarrowTab> {
        NarrowTab::ALL
            .into_iter()
            .filter(|&tab| self.narrow_tab_visible(tab))
            .collect()
    }

    pub fn active_narrow_tab(&self) -> Option<NarrowTab> {
        if self.narrow_tab_visible(self.narrow_tab) {
            Some(self.narrow_tab)
        } else {
            NarrowTab::ALL
                .into_iter()
                .find(|&tab| self.narrow_tab_visible(tab))
        }
    }

    pub fn set_narrow_tab(&mut self, tab: NarrowTab) {
        if self.narrow_tab_visible(tab) {
            self.narrow_tab = tab;
            self.workspace_focus = tab == NarrowTab::Workspace;
            self.clamp_narrow_section();
        }
    }

    pub fn toggle_workspace_focus(&mut self) {
        if self.workspace_focus {
            self.workspace_focus = false;
            if self.narrow_tab == NarrowTab::Workspace {
                self.narrow_tab = if self.narrow_tab_visible(NarrowTab::Work) {
                    NarrowTab::Work
                } else {
                    self.active_narrow_tab().unwrap_or(NarrowTab::Workspace)
                };
            }
            self.clamp_narrow_section();
        } else {
            self.set_narrow_tab(NarrowTab::Workspace);
        }
    }

    pub fn select_next_workspace_project(&mut self) {
        let visible = self.visible_workspace_project_indices();
        if visible.is_empty() {
            self.workspace_selected = 0;
            return;
        }
        let pos = visible
            .iter()
            .position(|&idx| idx == self.workspace_selected)
            .unwrap_or(0);
        self.workspace_selected = visible[(pos + 1) % visible.len()];
    }

    pub fn select_prev_workspace_project(&mut self) {
        let visible = self.visible_workspace_project_indices();
        if visible.is_empty() {
            self.workspace_selected = 0;
            return;
        }
        let pos = visible
            .iter()
            .position(|&idx| idx == self.workspace_selected)
            .unwrap_or(0);
        self.workspace_selected = visible[(pos + visible.len() - 1) % visible.len()];
    }

    pub fn cycle_workspace_lens(&mut self) {
        self.workspace_lens = match self.workspace_lens {
            WorkspaceLens::All => WorkspaceLens::Attention,
            WorkspaceLens::Attention => WorkspaceLens::Workflow,
            WorkspaceLens::Workflow => WorkspaceLens::Tasks,
            WorkspaceLens::Tasks => WorkspaceLens::All,
        };
        self.clamp_workspace_selection();
    }

    pub fn visible_workspace_project_indices(&self) -> Vec<usize> {
        self.workspace_projects
            .iter()
            .enumerate()
            .filter_map(|(idx, project)| {
                let visible = match self.workspace_lens {
                    WorkspaceLens::All => true,
                    WorkspaceLens::Attention => project.attention_score > 0,
                    WorkspaceLens::Workflow => project.has_dw,
                    WorkspaceLens::Tasks => project.task_count > 0,
                };
                visible.then_some(idx)
            })
            .collect()
    }

    pub fn activate_selected_workspace_project(&mut self) -> bool {
        let Some(project) = self.workspace_projects.get(self.workspace_selected) else {
            return false;
        };
        let Some((index, _)) = self
            .sessions
            .iter()
            .enumerate()
            .find(|(_, session)| session.cwd == project.cwd)
        else {
            return false;
        };

        self.selected = index;
        self.workspace_focus = false;
        self.set_active_narrow_section(NarrowSection::Sessions);
        true
    }

    fn clamp_workspace_selection(&mut self) {
        let visible = self.visible_workspace_project_indices();
        if visible.is_empty() {
            self.workspace_selected = 0;
        } else if !visible.contains(&self.workspace_selected) {
            self.workspace_selected = visible[0];
        }
    }

    pub fn select_next_narrow_tab(&mut self) {
        let tabs = self.visible_narrow_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = self.active_narrow_tab().unwrap_or(tabs[0]);
        let pos = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        self.narrow_tab = tabs[(pos + 1) % tabs.len()];
        self.workspace_focus = self.narrow_tab == NarrowTab::Workspace;
        self.clamp_narrow_section();
    }

    pub fn select_prev_narrow_tab(&mut self) {
        let tabs = self.visible_narrow_tabs();
        if tabs.is_empty() {
            return;
        }
        let current = self.active_narrow_tab().unwrap_or(tabs[0]);
        let pos = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        self.narrow_tab = tabs[(pos + tabs.len() - 1) % tabs.len()];
        self.workspace_focus = self.narrow_tab == NarrowTab::Workspace;
        self.clamp_narrow_section();
    }

    fn clamp_narrow_tab(&mut self) {
        if let Some(tab) = self.active_narrow_tab() {
            self.narrow_tab = tab;
        }
        if !self.narrow_tab_visible(NarrowTab::Workspace) {
            self.workspace_focus = false;
        }
        self.clamp_narrow_section();
    }

    pub fn narrow_section_visible(&self, section: NarrowSection) -> bool {
        match section {
            NarrowSection::Workspace => self.show_sessions || self.show_projects,
            NarrowSection::Sessions => self.show_sessions,
            NarrowSection::Projects => self.show_projects,
            NarrowSection::Context => self.show_context,
            NarrowSection::Quota => self.show_quota,
            NarrowSection::Tokens => self.show_tokens,
            NarrowSection::Ports => self.show_ports,
            NarrowSection::Mcp => self.show_mcp,
        }
    }

    pub fn visible_narrow_sections(&self, tab: NarrowTab) -> Vec<NarrowSection> {
        let sections: &[NarrowSection] = match tab {
            NarrowTab::Workspace => &[NarrowSection::Workspace],
            NarrowTab::Work => &[NarrowSection::Sessions, NarrowSection::Projects],
            NarrowTab::Usage => &[
                NarrowSection::Context,
                NarrowSection::Quota,
                NarrowSection::Tokens,
            ],
            NarrowTab::System => &[NarrowSection::Ports, NarrowSection::Mcp],
        };
        sections
            .iter()
            .copied()
            .filter(|&section| self.narrow_section_visible(section))
            .collect()
    }

    pub fn active_narrow_section(&self) -> Option<NarrowSection> {
        let tab = self.active_narrow_tab()?;
        if let Some(section) = self.active_narrow_section {
            if section.tab() == tab && self.narrow_section_visible(section) {
                return Some(section);
            }
        }
        self.visible_narrow_sections(tab).into_iter().next()
    }

    pub fn set_active_narrow_section(&mut self, section: NarrowSection) {
        if self.narrow_section_visible(section) {
            self.narrow_tab = section.tab();
            self.active_narrow_section = Some(section);
            self.clamp_narrow_section();
        }
    }

    pub fn maximized_narrow_section(&self) -> Option<NarrowSection> {
        let section = self.maximized_narrow_section?;
        if self.active_narrow_tab() == Some(section.tab()) && self.narrow_section_visible(section) {
            Some(section)
        } else {
            None
        }
    }

    pub fn toggle_narrow_section_zoom(&mut self, section: NarrowSection) {
        if !self.narrow_section_visible(section) {
            return;
        }
        self.set_active_narrow_section(section);
        self.maximized_narrow_section = if self.maximized_narrow_section() == Some(section) {
            None
        } else {
            Some(section)
        };
    }

    pub fn maximize_active_narrow_section(&mut self) {
        if let Some(section) = self.active_narrow_section() {
            self.maximized_narrow_section = Some(section);
        }
    }

    pub fn restore_narrow_sections(&mut self) {
        self.maximized_narrow_section = None;
    }

    fn clamp_narrow_section(&mut self) {
        self.active_narrow_section = self.active_narrow_section();
        if self.maximized_narrow_section().is_none() {
            self.maximized_narrow_section = None;
        }
    }

    pub fn toggle_timeline(&mut self) {
        self.show_timeline = !self.show_timeline;
        self.timeline_scroll = 0;
    }

    pub fn cycle_theme(&mut self) {
        let names = crate::theme::THEME_NAMES;
        let current = names
            .iter()
            .position(|&n| n == self.theme.name)
            .unwrap_or(0);
        let next = (current + 1) % names.len();
        self.theme = Theme::by_name(names[next]).unwrap_or_default();
        if let Err(e) = crate::config::save_theme(names[next]) {
            self.set_status(format!("theme: {} (save failed: {})", names[next], e));
        } else {
            self.set_status(format!("theme: {}", names[next]));
        }
    }

    /// Set a transient status message that auto-clears after 3 seconds.
    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }

    pub fn tick(&mut self) {
        self.collector.set_mcp_suppress(self.mcp_suppress_sessions);
        self.sessions = self.collector.collect();
        self.orphan_ports = self.collector.orphan_ports.clone();
        self.mcp_servers = self.collector.mcp_servers.clone();
        self.host_metrics = self.host_sampler.sample();
        self.agent_aggregate = AgentAggregate::from_sessions(&self.sessions);
        self.workspace_projects = WorkspaceProject::from_sessions(&self.sessions);
        self.clamp_workspace_selection();
        if self.selected >= self.sessions.len() && !self.sessions.is_empty() {
            self.selected = self.sessions.len() - 1;
        }
        self.clamp_selection_to_visible();

        // Compute rate as sum of per-session deltas (stable across session churn).
        // Update prev_tokens in place; stale entries are harmless (bounded by
        // total unique sessions ever seen) and keeping them avoids false spikes
        // when a session transiently disappears from one poll.
        let mut rate: f64 = 0.0;
        for s in &self.sessions {
            let key = (s.agent_cli.to_string(), s.session_id.clone());
            let total = s.active_tokens();
            let prev = self.prev_tokens.get(&key).copied().unwrap_or(total);
            rate += total.saturating_sub(prev) as f64;
            self.prev_tokens.insert(key, total);
        }

        self.token_rates.push_back(rate);
        if self.token_rates.len() > GRAPH_HISTORY_LEN {
            self.token_rates.pop_front();
        }

        // Poll rate limits: first tick immediately, then every 5 ticks ≈ 10s
        if self.rate_limits.is_empty() || self.rate_limit_counter >= 5 {
            self.rate_limit_counter = 0;
            let extra_dirs = self.collector.all_config_dirs();
            self.rate_limits = read_rate_limits(&extra_dirs);
            // Merge live rate limits from agent collectors (e.g. Codex JSONL parsing)
            self.rate_limits.extend(self.collector.agent_rate_limits());
        } else {
            self.rate_limit_counter += 1;
        }

        promote_waiting_to_rate_limited(&mut self.sessions, &self.rate_limits);

        crate::log_debug!(
            "tick sessions={} orphan_ports={} mcp_servers={} token_delta={} rate_limits={} sources={}",
            self.sessions.len(),
            self.orphan_ports.len(),
            self.mcp_servers.len(),
            rate,
            self.rate_limits.len(),
            self.rate_limits
                .iter()
                .map(|rl| rl.source.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );

        self.drain_and_retry_summaries();
    }

    /// Drain completed summary results and spawn retries. Does NOT recollect
    /// sessions, so it is safe for `--once` mode (stable snapshot).
    pub fn drain_and_retry_summaries(&mut self) {
        while let Ok((sid, _prompt, maybe_summary)) = self.summary_rx.try_recv() {
            self.pending_summaries.remove(&sid);
            match maybe_summary {
                Some(summary) => {
                    self.summary_retries.remove(&sid);
                    crate::log_debug!("summary generated sid={}", sid);
                    self.summaries.insert(sid, summary);
                    save_summary_cache(&self.summaries);
                }
                None => {
                    let count = self.summary_retries.entry(sid.clone()).or_insert(0);
                    *count += 1;
                    crate::log_warn!("summary generation failed sid={} attempt={}", sid, *count);
                    if *count >= MAX_SUMMARY_RETRIES {
                        // Exhausted: keep prompt text out of session lists and snapshots.
                        self.summaries.insert(sid, SUMMARY_UNAVAILABLE.to_string());
                        save_summary_cache(&self.summaries);
                    }
                }
            }
        }

        if summary_generation_disabled() {
            return;
        }

        // Spawn summary jobs for sessions that need one
        for s in &self.sessions {
            let retries = self
                .summary_retries
                .get(&s.session_id)
                .copied()
                .unwrap_or(0);
            let has_input = !s.initial_prompt.is_empty() || !s.first_assistant_text.is_empty();
            if has_input
                && !self.summaries.contains_key(&s.session_id)
                && !self.pending_summaries.contains(&s.session_id)
                && self.pending_summaries.len() < MAX_SUMMARY_JOBS
                && retries < MAX_SUMMARY_RETRIES
            {
                self.pending_summaries.insert(s.session_id.clone());
                let sid = s.session_id.clone();
                let prompt = s.initial_prompt.clone();
                let assistant_text = s.first_assistant_text.clone();
                let tx = self.summary_tx.clone();
                std::thread::spawn(move || {
                    let result = generate_summary(&prompt, &assistant_text);
                    let fallback_text = if prompt.is_empty() {
                        assistant_text
                    } else {
                        prompt
                    };
                    let _ = tx.send((sid, fallback_text, result));
                });
            }
        }
    }

    pub fn has_pending_summaries(&self) -> bool {
        !self.pending_summaries.is_empty()
    }

    /// True if any session still qualifies for a summary retry.
    pub fn has_retryable_summaries(&self) -> bool {
        if summary_generation_disabled() {
            return false;
        }
        self.sessions.iter().any(|s| {
            (!s.initial_prompt.is_empty() || !s.first_assistant_text.is_empty())
                && !self.summaries.contains_key(&s.session_id)
                && !self.pending_summaries.contains(&s.session_id)
                && self
                    .summary_retries
                    .get(&s.session_id)
                    .copied()
                    .unwrap_or(0)
                    < MAX_SUMMARY_RETRIES
        })
    }

    /// Returns indices of sessions matching the current filter.
    pub fn visible_indices(&self) -> Vec<usize> {
        if self.filter_text.is_empty() {
            return (0..self.sessions.len()).collect();
        }
        let query = self.filter_text.to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| Self::session_matches(s, &query))
            .map(|(i, _)| i)
            .collect()
    }

    fn session_matches(s: &AgentSession, query: &str) -> bool {
        s.project_name.to_lowercase().contains(query)
            || s.model.to_lowercase().contains(query)
            || s.session_id.to_lowercase().contains(query)
            || s.initial_prompt.to_lowercase().contains(query)
            || s.cwd.to_lowercase().contains(query)
            || format!("{:?}", s.status).to_lowercase().contains(query)
    }

    /// Ensure `selected` points to a session included in the current filter.
    /// No-op when no sessions match; otherwise snaps to the first visible.
    fn clamp_selection_to_visible(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if !visible.contains(&self.selected) {
            self.selected = visible[0];
        }
    }

    pub fn filter_push(&mut self, c: char) {
        self.filter_text.push(c);
        self.clamp_selection_to_visible();
    }

    pub fn filter_pop(&mut self) {
        self.filter_text.pop();
        self.clamp_selection_to_visible();
    }

    pub fn clear_filter(&mut self) {
        self.filter_active = false;
        self.filter_text.clear();
    }

    pub fn select_next(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if let Some(pos) = visible.iter().position(|&i| i == self.selected) {
            if pos + 1 < visible.len() {
                self.selected = visible[pos + 1];
            }
        } else {
            self.selected = visible[0];
        }
    }

    pub fn select_prev(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        if let Some(pos) = visible.iter().position(|&i| i == self.selected) {
            if pos > 0 {
                self.selected = visible[pos - 1];
            }
        } else {
            self.selected = *visible.last().unwrap();
        }
    }

    pub fn select_session(&mut self, index: usize) {
        if index < self.sessions.len() && self.visible_indices().contains(&index) {
            self.selected = index;
        }
    }

    pub fn kill_selected(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let session = &self.sessions[self.selected];
        if session.status == SessionStatus::Done {
            return;
        }

        // Check if we have a pending confirmation for this exact session
        if let Some((idx, ts)) = self.kill_confirm.take() {
            if idx == self.selected && ts.elapsed().as_secs() < KILL_CONFIRM_WINDOW_SECS {
                // Confirmed — verify PID still runs expected binary before killing
                let pid = session.pid;
                let verified = current_process_command(pid)
                    .as_deref()
                    .is_some_and(is_supported_agent_command);
                if !verified {
                    record_audit(&AuditEvent::new(
                        "kill-session",
                        "session",
                        &session.session_id,
                        Some(&session.project_name),
                        "blocked",
                        Some("pid verification failed"),
                    ));
                    self.set_status(format!("PID {} is no longer a known agent process", pid));
                    return;
                }
                let sent = terminate_process(pid, true);
                record_audit(&AuditEvent::new(
                    "kill-session",
                    "session",
                    &session.session_id,
                    Some(&session.project_name),
                    if sent { "sent" } else { "failed" },
                    Some("double-confirmed by user"),
                ));
                if !sent {
                    self.set_status(format!("Failed to terminate PID {}", pid));
                }
                self.tick();
                return;
            }
        }

        // First press — ask for confirmation
        let name = self
            .summaries
            .get(&session.session_id)
            .cloned()
            .unwrap_or_else(|| format!("PID {}", session.pid));
        self.kill_confirm = Some((self.selected, Instant::now()));
        self.set_status(format!("Press x again to kill: {}", name));
    }

    /// Kill all orphan port processes (Shift+X).
    /// Does a fresh port scan and validates PID identity + port ownership
    /// immediately before sending any signals to avoid PID reuse / stale cache issues.
    pub fn kill_orphan_ports(&mut self) {
        use crate::collector::process::get_listening_ports;

        if self.orphan_ports.is_empty() {
            self.orphan_kill_confirm = None;
            self.set_status("No orphan ports to kill".to_string());
            return;
        }

        let confirmed = self
            .orphan_kill_confirm
            .take()
            .is_some_and(|ts| ts.elapsed().as_secs() < KILL_CONFIRM_WINDOW_SECS);
        if !confirmed {
            let count = self.orphan_ports.len();
            let suffix = if count == 1 { "" } else { "es" };
            self.orphan_kill_confirm = Some(Instant::now());
            self.set_status(format!(
                "Press X again within {}s to kill {} orphan port process{}",
                KILL_CONFIRM_WINDOW_SECS, count, suffix
            ));
            return;
        }

        // Fresh port scan right now — don't rely on cached data
        let fresh_ports = get_listening_ports();

        for orphan in &self.orphan_ports {
            // 1. Verify PID still listens on the expected port
            let still_listening = fresh_ports
                .get(&orphan.pid)
                .is_some_and(|ports| ports.contains(&orphan.port));
            if !still_listening {
                continue;
            }
            // 2. Verify PID still runs the expected command (full match, not substring)
            let command_matches = current_process_command(orphan.pid)
                .as_deref()
                .is_some_and(|cmd| cmd == orphan.command);
            if command_matches {
                let sent = terminate_process(orphan.pid, false);
                record_audit(&AuditEvent::new(
                    "kill-orphan-port",
                    "process",
                    &orphan.pid.to_string(),
                    Some(&orphan.project_name),
                    if sent { "sent" } else { "failed" },
                    Some("orphan port verified"),
                ));
            } else {
                record_audit(&AuditEvent::new(
                    "kill-orphan-port",
                    "process",
                    &orphan.pid.to_string(),
                    Some(&orphan.project_name),
                    "blocked",
                    Some("command verification failed"),
                ));
            }
        }
        // Re-collect to reflect changes
        self.tick();
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn workspace_summary_markdown(&self) -> String {
        let mut out = String::new();
        let task_graph = self.workspace_task_graph();
        out.push_str("# abtop workspace summary\n\n");
        out.push_str(&format!(
            "- projects: {}\n- sessions: {}\n- lens: {}\n- graph: {} nodes, {} edges, {} tasks, {} agents\n\n",
            self.workspace_projects.len(),
            self.sessions.len(),
            self.workspace_lens.label(),
            task_graph.nodes.len(),
            task_graph.edges.len(),
            task_graph.node_count(GraphNodeKind::Task),
            task_graph.node_count(GraphNodeKind::Agent)
        ));

        for project in &self.workspace_projects {
            out.push_str(&format!("## {}\n\n", safe_export_text(&project.name, 80)));
            out.push_str(&format!(
                "- sessions: {} active, {} waiting, {} rate-limited\n",
                project.active_count, project.waiting_count, project.rate_limited_count
            ));
            out.push_str(&format!(
                "- attention: {} (score {})\n",
                if project.attention.is_empty() {
                    "none".to_string()
                } else {
                    project.attention.join(",")
                },
                project.attention_score
            ));
            out.push_str(&format!(
                "- context: {:.0}%\n- tokens: {}\n- git: +{} ~{}\n- ports: {}\n",
                project.max_context_percent,
                fmt_export_tokens(project.total_tokens),
                project.git_added,
                project.git_modified,
                project.port_count
            ));
            if project.has_dw {
                out.push_str(&format!(
                    "- workflow: task={} status={} phase={} next={} acceptance={} tasks={} deps={} decisions={} records={} verification={}/{}\n",
                    project
                        .active_task_title
                        .as_deref()
                        .map(|title| safe_export_text(title, 80))
                        .unwrap_or_else(|| {
                            if project.has_active_task {
                                "active task".into()
                            } else {
                                "none".into()
                            }
                        }),
                    project
                        .active_task_raw_status
                        .as_deref()
                        .map(|status| safe_export_text(status, 32))
                        .unwrap_or_else(|| project.active_task_status.label().into()),
                    project
                        .active_task_phase
                        .as_deref()
                        .map(|phase| safe_export_text(phase, 40))
                        .unwrap_or_else(|| "-".into()),
                    project.active_task_next_action(),
                    project.active_task_acceptance_count,
                    project.task_count,
                    project.dependency_count,
                    project.decision_count,
                    project.record_count,
                    project.completed_verification_count,
                    project.verification_count
                ));
            }

            let project_sessions: Vec<_> = self
                .sessions
                .iter()
                .filter(|session| session.cwd == project.cwd)
                .collect();
            if !project_sessions.is_empty() {
                out.push_str("- agents:\n");
                for session in project_sessions.into_iter().take(5) {
                    let sid = if session.session_id.len() >= 7 {
                        &session.session_id[..7]
                    } else {
                        &session.session_id
                    };
                    let summary = self
                        .summaries
                        .get(&session.session_id)
                        .map(|summary| safe_export_text(summary, 80))
                        .unwrap_or_else(|| format!("session {}", sid));
                    let task = session
                        .current_tasks
                        .first()
                        .map(|task| safe_export_text(task, 80))
                        .unwrap_or_else(|| workspace_export_idle_text(&session.status).into());
                    out.push_str(&format!(
                        "  - {} {}: {}\n",
                        workspace_export_status(&session.status),
                        summary,
                        task
                    ));
                }
            }
            out.push('\n');
        }

        out
    }

    pub fn workspace_task_graph(&self) -> TaskGraph {
        TaskGraph::build(&self.workspace_projects, &self.sessions)
    }

    pub fn task_evidence_markdown(&self) -> String {
        let graph = self.workspace_task_graph();
        let bundles = build_task_evidence(&self.workspace_projects, &self.sessions, &graph);
        render_task_evidence_markdown(&bundles)
    }

    /// Jump to the terminal running the selected session's Claude process.
    /// In tmux: switch to the pane. Otherwise: no-op.
    pub fn jump_to_session(&mut self) -> JumpOutcome {
        if self.sessions.is_empty() {
            return JumpOutcome::NoOp;
        }
        if std::env::var("TMUX").is_err() {
            return JumpOutcome::NoOp;
        }
        let target_pid = self.sessions[self.selected].pid;
        match self.jump_via_tmux(target_pid) {
            None => JumpOutcome::Jumped,
            Some(msg) => JumpOutcome::Failed(msg),
        }
    }

    fn jump_via_tmux(&self, target_pid: u32) -> Option<String> {
        let output = std::process::Command::new("tmux")
            .args([
                "list-panes",
                "-a",
                "-F",
                "#{pane_pid} #{session_name}:#{window_index}.#{pane_index}",
            ])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        for line in stdout.lines() {
            let mut parts = line.splitn(2, ' ');
            let pane_pid: u32 = match parts.next().and_then(|p| p.parse().ok()) {
                Some(p) => p,
                None => continue,
            };
            let pane_target = match parts.next() {
                Some(t) => t,
                None => continue,
            };

            if is_descendant_of(target_pid, pane_pid) {
                // Switch tmux client to the target session (needed for cross-session jumps)
                if let Some(session_name) = pane_target.split(':').next() {
                    let _ = std::process::Command::new("tmux")
                        .args(["switch-client", "-t", session_name])
                        .status();
                }
                if let Some(window) = pane_target.split('.').next() {
                    let _ = std::process::Command::new("tmux")
                        .args(["select-window", "-t", window])
                        .status();
                }
                let _ = std::process::Command::new("tmux")
                    .args(["select-pane", "-t", pane_target])
                    .status();
                return None; // success
            }
        }

        Some("pane not found".to_string())
    }

    /// Get the display summary for a session: cached/generated summary > pending dots > safe fallback.
    /// Done sessions skip pending state to avoid stuck "..." display.
    pub fn session_summary(&self, session: &AgentSession) -> String {
        if summary_generation_disabled() {
            return SUMMARY_UNAVAILABLE.to_string();
        }
        if let Some(summary) = self.summaries.get(&session.session_id) {
            summary.clone()
        } else if matches!(session.status, SessionStatus::Done) {
            SUMMARY_UNAVAILABLE.to_string()
        } else if self.pending_summaries.contains(&session.session_id) {
            // Animate dots: . → .. → ... (cycles every ~1.5s at 2s tick)
            let dots = match (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 500)
                % 3
            {
                0 => ".",
                1 => "..",
                _ => "...",
            };
            dots.to_string()
        } else {
            SUMMARY_UNAVAILABLE.to_string()
        }
    }
}

/// Call `claude --print` via stdin pipe to summarize a prompt.
/// Returns `None` on timeout so the caller can retry later.
fn generate_summary(prompt: &str, assistant_text: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    if summary_generation_disabled() {
        return Some(SUMMARY_UNAVAILABLE.to_string());
    }

    let safe_prompt = sanitize_fallback(prompt, 200);
    let safe_assistant_text = sanitize_fallback(assistant_text, 200);

    // Build input from user prompt and/or first assistant response
    let user_part: String = safe_prompt.chars().take(200).collect();
    let assistant_part: String = safe_assistant_text.chars().take(200).collect();

    let context = if !user_part.is_empty() && !assistant_part.is_empty() {
        format!(
            "User message: {}\n\nAssistant response: {}",
            user_part, assistant_part
        )
    } else if !assistant_part.is_empty() {
        format!("Assistant response: {}", assistant_part)
    } else {
        format!("User message: {}", user_part)
    };

    let request = format!(
        "You are a conversation title generator. Given the conversation below, create a short title (3-5 words) that describes the session's main topic. Be specific and actionable. Do NOT output generic titles like 'New conversation' or 'Initial setup'. Output ONLY the title, no quotes, no explanation.\n\n{}",
        context
    );

    let mut child = match Command::new("claude")
        .args(["--print", "-"])
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Some(SUMMARY_UNAVAILABLE.to_string()),
    };

    // Write prompt via stdin (no shell injection)
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.as_bytes());
    }

    // Run wait_with_output in a helper thread so we can apply a bounded timeout.
    // This drains stdout internally, avoiding pipe-full deadlock.
    let child_pid = child.id();
    let (wo_tx, wo_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = wo_tx.send(child.wait_with_output());
    });

    let result = match wo_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(r) => r,
        Err(_) => {
            // Timeout or disconnected — kill the child so the helper thread can exit.
            let _ = terminate_process(child_pid, true);
            return None;
        }
    };

    let fallback = SUMMARY_UNAVAILABLE.to_string();

    match result {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let lower = raw.to_lowercase();
            // Reject empty, too long, generic, or prompt-echo outputs
            if raw.is_empty()
                || raw.chars().count() > 80
                || raw.contains("Summarize")
                || raw.starts_with("- ")
                || lower.contains("new conversation")
                || lower.contains("initial setup")
                || lower.contains("initial project")
                || lower.contains("initial conversation")
                || lower.starts_with("greeting")
            {
                Some(fallback)
            } else {
                Some(sanitize_fallback(
                    raw.trim_matches('"').trim_matches('\''),
                    80,
                ))
            }
        }
        _ => Some(fallback),
    }
}

fn summary_generation_disabled() -> bool {
    std::env::var("ABTOP_DISABLE_SUMMARIES")
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// Cache directory: ~/.cache/abtop/
fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("abtop")
}

fn cache_path() -> std::path::PathBuf {
    cache_dir().join("summaries.json")
}

fn load_summary_cache() -> HashMap<String, String> {
    let path = cache_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let mut cache: HashMap<String, String> =
                serde_json::from_str(&content).unwrap_or_default();
            // Purge polluted or old truncated-fallback entries so they regenerate
            let before = cache.len();
            cache.retain(|_, v| !v.contains("You are a conversation tit") && !v.ends_with('…'));
            if cache.len() < before {
                // Persist cleaned cache
                let _ = std::fs::create_dir_all(cache_dir());
                let _ = std::fs::write(&path, serde_json::to_string(&cache).unwrap_or_default());
            }
            cache
        }
        Err(_) => HashMap::new(),
    }
}

/// Check if `target` PID is a descendant of `ancestor` PID by walking the process tree.
fn is_descendant_of(target: u32, ancestor: u32) -> bool {
    if target == ancestor {
        return true;
    }
    // Build a pid->ppid map from ps
    let output = match std::process::Command::new("ps")
        .args(["-eo", "pid,ppid"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut ppid_map: HashMap<u32, u32> = HashMap::new();
    for line in stdout.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(pid), Ok(ppid)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                ppid_map.insert(pid, ppid);
            }
        }
    }
    // Walk up from target to see if we reach ancestor
    let mut current = target;
    let mut depth = 0;
    while depth < 50 {
        if let Some(&parent) = ppid_map.get(&current) {
            if parent == ancestor {
                return true;
            }
            if parent == 0 || parent == 1 || parent == current {
                return false;
            }
            current = parent;
            depth += 1;
        } else {
            return false;
        }
    }
    false
}

fn save_summary_cache(summaries: &HashMap<String, String>) {
    let path = cache_path();
    let _ = std::fs::create_dir_all(cache_dir());
    if let Ok(json) = serde_json::to_string(summaries) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Threshold above which a rate-limited bucket is surfaced as RateLimited
/// in the session list. 90% leaves enough headroom to catch near-saturation
/// before the account actually blocks.
const RATE_LIMITED_PCT: f64 = 90.0;

/// Promote Waiting sessions to RateLimited when a rate limit from the SAME
/// agent CLI is over `RATE_LIMITED_PCT`. Matching on source avoids a
/// Claude-only saturation freezing Codex sessions and vice versa.
fn promote_waiting_to_rate_limited(sessions: &mut [AgentSession], rate_limits: &[RateLimitInfo]) {
    if rate_limits.is_empty() {
        return;
    }
    for s in sessions.iter_mut() {
        if s.status != SessionStatus::Waiting {
            continue;
        }
        let over = rate_limits.iter().any(|rl| {
            rl.source == s.agent_cli
                && (rl.five_hour_pct.unwrap_or(0.0) > RATE_LIMITED_PCT
                    || rl.seven_day_pct.unwrap_or(0.0) > RATE_LIMITED_PCT)
        });
        if over {
            s.status = SessionStatus::RateLimited;
        }
    }
}

fn is_supported_agent_command(cmd: &str) -> bool {
    crate::collector::process::cmd_has_binary(cmd, "claude")
        || crate::collector::process::cmd_has_binary(cmd, "codex")
        || crate::collector::process::cmd_has_binary(cmd, "opencode")
}

fn current_process_command(pid: u32) -> Option<String> {
    crate::collector::process::get_process_info()
        .remove(&pid)
        .map(|proc| proc.command)
}

fn terminate_process(pid: u32, force: bool) -> bool {
    if pid == 0 {
        return false;
    }

    terminate_process_impl(pid, force)
}

#[cfg(windows)]
fn terminate_process_impl(pid: u32, force: bool) -> bool {
    let pid = pid.to_string();
    let mut args = vec!["/PID", pid.as_str()];
    if force {
        args.push("/F");
    }
    std::process::Command::new("taskkill")
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn terminate_process_impl(pid: u32, force: bool) -> bool {
    let pid = pid.to_string();
    let signal = if force { "-9" } else { "-TERM" };
    std::process::Command::new("kill")
        .args([signal, pid.as_str()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn safe_export_text(value: &str, max_len: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max_len)
        .collect()
}

fn fmt_export_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn workspace_export_status(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "think",
        SessionStatus::Executing => "work",
        SessionStatus::Waiting => "wait",
        SessionStatus::RateLimited => "rate",
        SessionStatus::Done => "done",
    }
}

fn workspace_export_idle_text(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "generating reply",
        SessionStatus::Executing => "working",
        SessionStatus::Waiting => "waiting for input",
        SessionStatus::RateLimited => "rate limited",
        SessionStatus::Done => "finished",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting_session(cli: &'static str) -> AgentSession {
        AgentSession {
            agent_cli: cli,
            pid: 1,
            session_id: String::new(),
            cwd: String::new(),
            project_name: String::new(),
            started_at: 0,
            status: SessionStatus::Waiting,
            model: String::new(),
            effort: String::new(),
            context_percent: 0.0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 0,
            compaction_count: 0,
            current_tasks: vec![],
            version: String::new(),
            git_branch: String::new(),
            mem_mb: 0,
            token_history: vec![],
            context_history: vec![],
            context_window: 0,
            subagents: vec![],
            mem_file_count: 0,
            mem_line_count: 0,
            children: vec![],
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: vec![],
            tool_calls: vec![],
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: vec![],
            git_added: 0,
            git_modified: 0,
        }
    }

    fn rate_limit(source: &str, pct: f64) -> RateLimitInfo {
        RateLimitInfo {
            source: source.to_string(),
            five_hour_pct: Some(pct),
            five_hour_resets_at: None,
            seven_day_pct: None,
            seven_day_resets_at: None,
            updated_at: None,
        }
    }

    fn orphan_port() -> OrphanPort {
        OrphanPort {
            port: 3000,
            pid: 999_999,
            command: "node server.js".to_string(),
            project_name: "demo".to_string(),
        }
    }

    #[test]
    fn workspace_focus_toggle_returns_to_work_tab() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );

        app.toggle_workspace_focus();
        assert!(app.workspace_focus);
        assert_eq!(app.narrow_tab, NarrowTab::Workspace);

        app.toggle_workspace_focus();
        assert!(!app.workspace_focus);
        assert_eq!(app.narrow_tab, NarrowTab::Work);
    }

    #[test]
    fn workspace_project_selection_wraps_and_clamps() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        app.workspace_projects = vec![
            WorkspaceProject {
                name: "webshop".into(),
                ..WorkspaceProject::default()
            },
            WorkspaceProject {
                name: "api".into(),
                ..WorkspaceProject::default()
            },
        ];

        app.select_next_workspace_project();
        assert_eq!(app.workspace_selected, 1);
        app.select_next_workspace_project();
        assert_eq!(app.workspace_selected, 0);
        app.select_prev_workspace_project();
        assert_eq!(app.workspace_selected, 1);

        app.workspace_projects.pop();
        app.clamp_workspace_selection();
        assert_eq!(app.workspace_selected, 0);
    }

    #[test]
    fn workspace_project_reads_dw_active_task_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let task_dir = root.join(".dw").join("tasks");
        let decision_dir = root.join(".dw").join("decisions");
        let record_dir = root.join(".dw").join("records");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::create_dir_all(&decision_dir).unwrap();
        std::fs::create_dir_all(&record_dir).unwrap();
        std::fs::write(
            task_dir.join("ACTIVE.md"),
            "---\ntitle: Improve checkout flow\nphase: Verify\nstatus: review\ndepends_on: Follow-up, Risk check\n---\n# Ignored fallback\n\n## Verification\n- [x] cargo test\n- [ ] cargo clippy\n\nSecret body text should stay out of workspace exports.\n",
        )
        .unwrap();
        std::fs::write(task_dir.join("next.md"), "# Follow-up\nstatus: ready\n").unwrap();
        std::fs::write(decision_dir.join("001.md"), "# ADR\n").unwrap();
        std::fs::write(decision_dir.join("notes.txt"), "ignored\n").unwrap();
        std::fs::write(record_dir.join("001.md"), "# Record\n").unwrap();

        let session = AgentSession {
            cwd: root.to_string_lossy().to_string(),
            project_name: "project".into(),
            ..waiting_session("claude")
        };

        let projects = WorkspaceProject::from_sessions(&[session]);
        assert_eq!(projects.len(), 1);
        assert!(projects[0].has_dw);
        assert!(projects[0].has_active_task);
        assert_eq!(
            projects[0].active_task_title.as_deref(),
            Some("Improve checkout flow")
        );
        assert_eq!(projects[0].active_task_phase.as_deref(), Some("Verify"));
        assert_eq!(projects[0].active_task_status, TaskStatus::Review);
        assert_eq!(
            projects[0].active_task_raw_status.as_deref(),
            Some("review")
        );
        assert_eq!(projects[0].active_task_acceptance_count, 0);
        assert_eq!(projects[0].active_task_next_action(), "verify");
        assert_eq!(projects[0].task_count, 2);
        assert_eq!(projects[0].decision_count, 1);
        assert_eq!(projects[0].record_count, 1);
        assert_eq!(projects[0].verification_count, 2);
        assert_eq!(projects[0].completed_verification_count, 1);
        assert_eq!(projects[0].dependency_count, 2);
    }

    #[test]
    fn workspace_attention_scores_and_sorts_projects() {
        let mut calm = waiting_session("claude");
        calm.cwd = "/tmp/calm".into();
        calm.project_name = "calm".into();
        calm.status = SessionStatus::Executing;

        let mut urgent = waiting_session("claude");
        urgent.cwd = "/tmp/urgent".into();
        urgent.project_name = "urgent".into();
        urgent.context_percent = 92.0;
        urgent.git_modified = 2;
        urgent.children.push(crate::model::ChildProcess {
            pid: 42,
            command: "npm run dev".into(),
            mem_kb: 1024,
            port: Some(3000),
        });

        let projects = WorkspaceProject::from_sessions(&[calm, urgent]);
        assert_eq!(projects[0].name, "urgent");
        assert!(projects[0].attention_score > projects[1].attention_score);
        assert!(projects[0].attention.iter().any(|label| label == "ctx90"));
        assert!(projects[0].attention.iter().any(|label| label == "input"));
        assert!(projects[0].attention.iter().any(|label| label == "ports"));
        assert!(projects[0].attention.iter().any(|label| label == "git"));
    }

    #[test]
    fn workspace_lens_filters_navigation_to_matching_projects() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        crate::demo::populate_demo(&mut app);

        assert_eq!(app.workspace_lens, WorkspaceLens::All);
        app.cycle_workspace_lens();
        assert_eq!(app.workspace_lens, WorkspaceLens::Attention);
        assert!(app
            .visible_workspace_project_indices()
            .iter()
            .all(|&idx| app.workspace_projects[idx].attention_score > 0));

        app.cycle_workspace_lens();
        assert_eq!(app.workspace_lens, WorkspaceLens::Workflow);
        assert!(app
            .visible_workspace_project_indices()
            .iter()
            .all(|&idx| app.workspace_projects[idx].has_dw));

        app.cycle_workspace_lens();
        assert_eq!(app.workspace_lens, WorkspaceLens::Tasks);
        assert!(app
            .visible_workspace_project_indices()
            .iter()
            .all(|&idx| app.workspace_projects[idx].task_count > 0));

        let before = app.workspace_selected;
        app.select_next_workspace_project();
        assert!(app
            .visible_workspace_project_indices()
            .contains(&app.workspace_selected));
        assert_eq!(app.workspace_selected, before);
    }

    #[test]
    fn workspace_summary_markdown_is_redacted_and_structured() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        crate::demo::populate_demo(&mut app);

        let summary = app.workspace_summary_markdown();
        assert!(summary.contains("# abtop workspace summary"));
        assert!(summary.contains("graph:"));
        assert!(summary.contains("## ml-pipeline"));
        assert!(summary.contains("attention:"));
        assert!(summary.contains("workflow: task=Batch inference rollout status=Doing"));
        assert!(summary.contains("next=continue acceptance=6"));
        assert!(summary.contains("deps=3"));
        assert!(summary.contains("verification=2/4"));
        assert!(summary.contains("Batch inference endpoint"));
        assert!(
            !summary.contains("Refactor Terraform modules for multi-region"),
            "workspace summary should not fall back to raw prompt text\n{summary}"
        );
    }

    #[test]
    fn task_evidence_markdown_is_redacted_and_structured() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        crate::demo::populate_demo(&mut app);

        let evidence = app.task_evidence_markdown();
        assert!(evidence.contains("# abtop task evidence"));
        assert!(evidence.contains("## ml-pipeline / Batch inference rollout"));
        assert!(evidence.contains("- next: continue"));
        assert!(evidence.contains("- graph:"));
        assert!(evidence.contains("- agents:"));
        assert!(!evidence.contains("Refactor Terraform modules for multi-region"));
        assert!(!evidence.contains("/Users/demo"));
    }

    #[test]
    fn fallback_summaries_redact_secrets_and_control_text() {
        let fallback = sanitize_fallback("ship sk-proj-secret\u{202E}\nnow", 80);
        assert_eq!(fallback, "ship [REDACTED]");
    }

    #[test]
    fn session_summary_does_not_fallback_to_prompt_text() {
        let app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let mut session = waiting_session("claude");
        session.initial_prompt = "ship sk-proj-secret now".to_string();
        session.first_assistant_text = "edited src/payments.rs".to_string();

        let summary = app.session_summary(&session);

        assert_eq!(summary, "summary unavailable");
        assert!(!summary.contains("ship"));
        assert!(!summary.contains("payments.rs"));
        assert!(!summary.contains("sk-proj-secret"));
    }

    #[test]
    fn activating_workspace_project_selects_its_first_session() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        crate::demo::populate_demo(&mut app);
        app.set_narrow_tab(NarrowTab::Workspace);
        app.workspace_selected = app
            .workspace_projects
            .iter()
            .position(|project| project.name == "api-server")
            .expect("demo project should exist");

        assert!(app.activate_selected_workspace_project());
        assert!(!app.workspace_focus);
        assert_eq!(app.narrow_tab, NarrowTab::Work);
        assert_eq!(app.active_narrow_section, Some(NarrowSection::Sessions));
        assert_eq!(app.sessions[app.selected].project_name, "api-server");
    }

    #[test]
    fn kill_orphan_ports_requires_second_confirmation() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        app.orphan_ports = vec![orphan_port()];

        app.kill_orphan_ports();

        assert!(app.orphan_kill_confirm.is_some());
        let status = app.status_msg.as_ref().map(|(msg, _)| msg.as_str());
        assert!(status.is_some_and(|msg| msg.contains("Press X again")));
    }

    #[test]
    fn kill_orphan_ports_empty_list_clears_confirmation() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        app.orphan_kill_confirm = Some(Instant::now());

        app.kill_orphan_ports();

        assert!(app.orphan_kill_confirm.is_none());
        let status = app.status_msg.as_ref().map(|(msg, _)| msg.as_str());
        assert_eq!(status, Some("No orphan ports to kill"));
    }

    #[test]
    fn test_rate_limited_promotion_is_per_agent_cli() {
        // Claude is saturated, Codex is not. Only the Claude session should
        // be promoted.
        let mut sessions = vec![waiting_session("claude"), waiting_session("codex")];
        let limits = vec![rate_limit("claude", 95.0)];
        promote_waiting_to_rate_limited(&mut sessions, &limits);
        assert_eq!(sessions[0].status, SessionStatus::RateLimited);
        assert_eq!(sessions[1].status, SessionStatus::Waiting);
    }

    #[test]
    fn test_rate_limited_promotion_ignores_below_threshold() {
        let mut sessions = vec![waiting_session("claude")];
        let limits = vec![rate_limit("claude", 89.9)];
        promote_waiting_to_rate_limited(&mut sessions, &limits);
        assert_eq!(sessions[0].status, SessionStatus::Waiting);
    }

    #[test]
    fn test_rate_limited_promotion_skips_non_waiting_sessions() {
        let mut sessions = vec![waiting_session("claude")];
        sessions[0].status = SessionStatus::Thinking;
        let limits = vec![rate_limit("claude", 99.0)];
        promote_waiting_to_rate_limited(&mut sessions, &limits);
        assert_eq!(sessions[0].status, SessionStatus::Thinking);
    }

    #[test]
    fn supported_agent_command_accepts_opencode() {
        assert!(is_supported_agent_command("/usr/local/bin/claude"));
        assert!(is_supported_agent_command("codex --resume abc"));
        assert!(is_supported_agent_command("/opt/homebrew/bin/opencode"));
        assert!(!is_supported_agent_command("node server.js"));
    }

    #[test]
    fn terminate_process_rejects_pid_zero() {
        assert!(!terminate_process(0, true));
        assert!(!terminate_process(0, false));
    }
}
