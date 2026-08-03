use crate::collector::codexbar::{
    canonical_provider_id, CodexBarProviderSnapshot, CodexBarQuotaPoller, CodexBarQuotaStatus,
    CodexBarSnapshot,
};
use crate::collector::{read_rate_limits, McpServer, MultiCollector};
use crate::host_info::{AgentAggregate, HostMetrics, HostSampler};
use crate::model::{
    AgentSession, OrphanPort, RateLimitInfo, RateLimitProvenance, RateLimitWindow, SessionStatus,
    StatusAuthority, StatusEvidence, StatusObservation, StatusReason, MAX_STATUS_OBSERVATIONS,
};
use crate::theme::Theme;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Maximum data points kept for the live token-rate graph.
const GRAPH_HISTORY_LEN: usize = 200;
/// Max concurrent summary jobs.
const MAX_SUMMARY_JOBS: usize = 3;
/// Max summary attempts per session before giving up.
const MAX_SUMMARY_RETRIES: u32 = 2;
/// Native quota samples at or below this age take precedence over CodexBar.
const QUOTA_FRESH_SECS: u64 = 600;

#[derive(Clone)]
struct QuotaWindowCandidate {
    window: RateLimitWindow,
    updated_at: Option<u64>,
    force_stale: bool,
}

#[derive(Default)]
struct ProviderQuotaCandidates {
    native: Vec<QuotaWindowCandidate>,
    codexbar: Vec<QuotaWindowCandidate>,
    codexbar_failed: bool,
}

/// Merge native quota samples with one atomic CodexBar snapshot.
///
/// Matching fresh native windows win, while CodexBar contributes missing and
/// provider-specific windows. When neither matching sample is fresh, the newer
/// timestamp wins. Each provider keeps one whole newest valid native sample so
/// separate accounts can never be folded into a fictional quota; ties and
/// unknown timestamps preserve discovery order. Provider failures suppress only
/// that provider's CodexBar values and never hide a native sample.
fn merge_rate_limits(
    native: &[RateLimitInfo],
    codexbar: Option<&CodexBarSnapshot>,
    now_secs: u64,
    codexbar_observed_at: Option<u64>,
    codexbar_transport_stale: bool,
) -> Vec<RateLimitInfo> {
    let mut providers: HashMap<String, ProviderQuotaCandidates> = HashMap::new();

    for info in native {
        let Some(provider) = canonical_provider_id(&info.source) else {
            continue;
        };
        let windows = normalized_native_windows(info);
        if windows.is_empty() {
            continue;
        }
        let entry = providers.entry(provider).or_default();
        let updated_at = valid_sample_timestamp(info.updated_at, now_secs);
        if entry
            .native
            .first()
            .is_some_and(|selected| !sample_is_newer(updated_at, selected.updated_at))
        {
            continue;
        }
        entry.native = windows
            .into_iter()
            .map(|window| QuotaWindowCandidate {
                window,
                updated_at,
                force_stale: false,
            })
            .collect();
    }

    if let Some(snapshot) = codexbar {
        for provider_snapshot in &snapshot.providers {
            let Some(provider) = canonical_provider_id(&provider_snapshot.provider) else {
                continue;
            };
            let entry = providers.entry(provider).or_default();
            if provider_snapshot.error.is_some() {
                entry.codexbar.clear();
                entry.codexbar_failed = true;
                continue;
            }
            if entry.codexbar_failed {
                continue;
            }
            for window in &provider_snapshot.windows {
                let Some(window) = RateLimitWindow::try_new(
                    window.id.clone(),
                    window.label.clone(),
                    window.used_pct,
                    window.resets_at,
                    window.window_minutes,
                    RateLimitProvenance::CodexBar,
                ) else {
                    continue;
                };
                upsert_candidate(
                    &mut entry.codexbar,
                    QuotaWindowCandidate {
                        window,
                        updated_at: codexbar_sample_timestamp(
                            provider_snapshot.updated_at,
                            codexbar_observed_at,
                            now_secs,
                            codexbar_transport_stale,
                        ),
                        force_stale: codexbar_transport_stale,
                    },
                );
            }
        }
    }

    let mut merged = providers
        .into_iter()
        .filter_map(|(provider, candidates)| {
            merge_provider_rate_limit(provider, candidates, now_secs)
        })
        .collect::<Vec<_>>();
    merged.sort_by(|left, right| provider_cmp(&left.source, &right.source));
    merged
}

fn normalized_native_windows(info: &RateLimitInfo) -> Vec<RateLimitWindow> {
    let mut windows = Vec::with_capacity(info.windows.len().saturating_add(2));
    for window in &info.windows {
        if windows
            .iter()
            .any(|existing: &RateLimitWindow| existing.id.eq_ignore_ascii_case(&window.id))
        {
            continue;
        }
        if let Some(window) = RateLimitWindow::try_new(
            window.id.clone(),
            window.label.clone(),
            window.used_pct,
            window.resets_at,
            window.window_minutes,
            RateLimitProvenance::Native,
        ) {
            windows.push(window);
        }
    }

    // Exact source-slot identities are authoritative. Legacy short/long fields
    // are only a compatibility fallback; mixing both would duplicate cases
    // such as a free-plan `primary` window projected into the historical long
    // slot.
    if !info.windows.is_empty() {
        return windows;
    }

    if let Some(used_pct) = info.five_hour_pct {
        if !windows
            .iter()
            .any(|window| window.id.eq_ignore_ascii_case("primary"))
        {
            if let Some(window) = RateLimitWindow::try_new(
                "primary",
                native_window_label(info.five_hour_window_minutes, "5h"),
                used_pct,
                info.five_hour_resets_at,
                info.five_hour_window_minutes,
                RateLimitProvenance::Native,
            ) {
                windows.push(window);
            }
        }
    }
    if let Some(used_pct) = info.seven_day_pct {
        if !windows
            .iter()
            .any(|window| window.id.eq_ignore_ascii_case("secondary"))
        {
            if let Some(window) = RateLimitWindow::try_new(
                "secondary",
                native_window_label(info.seven_day_window_minutes, "7d"),
                used_pct,
                info.seven_day_resets_at,
                info.seven_day_window_minutes,
                RateLimitProvenance::Native,
            ) {
                windows.push(window);
            }
        }
    }

    windows
}

fn native_window_label(window_minutes: Option<u64>, fallback: &str) -> String {
    match window_minutes {
        None | Some(0) => fallback.to_string(),
        Some(minutes) if minutes % (24 * 60) == 0 => format!("{}d", minutes / (24 * 60)),
        Some(minutes) if minutes % 60 == 0 => format!("{}h", minutes / 60),
        Some(minutes) => format!("{minutes}m"),
    }
}

fn valid_sample_timestamp(updated_at: Option<u64>, now_secs: u64) -> Option<u64> {
    updated_at.filter(|updated_at| *updated_at <= now_secs)
}

fn codexbar_sample_timestamp(
    provider_updated_at: Option<u64>,
    observed_at: Option<u64>,
    now_secs: u64,
    transport_stale: bool,
) -> Option<u64> {
    match provider_updated_at {
        Some(updated_at) => valid_sample_timestamp(Some(updated_at), now_secs),
        None if !transport_stale => valid_sample_timestamp(observed_at, now_secs),
        None => None,
    }
}

fn upsert_candidate(candidates: &mut Vec<QuotaWindowCandidate>, incoming: QuotaWindowCandidate) {
    if let Some(existing) = candidates.iter_mut().find(|candidate| {
        candidate
            .window
            .id
            .eq_ignore_ascii_case(&incoming.window.id)
    }) {
        if sample_is_newer(incoming.updated_at, existing.updated_at) {
            *existing = incoming;
        }
    } else {
        candidates.push(incoming);
    }
}

fn merge_provider_rate_limit(
    provider: String,
    candidates: ProviderQuotaCandidates,
    now_secs: u64,
) -> Option<RateLimitInfo> {
    let mut selected = Vec::with_capacity(
        candidates
            .native
            .len()
            .saturating_add(candidates.codexbar.len()),
    );

    let mut codexbar_matched = vec![false; candidates.codexbar.len()];
    let mut native_matches = vec![None; candidates.native.len()];

    // Built-in slot positions can shift when a plan omits its short window.
    // Pair exact semantic fingerprints first so a native weekly `primary`
    // cannot consume CodexBar's unrelated 5h `primary` by name alone.
    for (native_index, native) in candidates.native.iter().enumerate() {
        if let Some(codexbar_index) = candidates
            .codexbar
            .iter()
            .enumerate()
            .find(|(index, codexbar)| {
                !codexbar_matched[*index]
                    && quota_windows_structurally_alias(&native.window, &codexbar.window)
            })
            .map(|(index, _)| index)
        {
            native_matches[native_index] = Some((codexbar_index, true));
            codexbar_matched[codexbar_index] = true;
        }
    }

    if !structural_mapping_has_unique_builtin_ids(
        &candidates.native,
        &candidates.codexbar,
        &native_matches,
        &codexbar_matched,
    ) {
        native_matches.fill(None);
        codexbar_matched.fill(false);
    }

    // When exact reset metadata is unavailable or crosses a reset boundary,
    // retain the stable source-owned ID as the conservative fallback.
    for (native_index, native) in candidates.native.iter().enumerate() {
        if native_matches[native_index].is_some() {
            continue;
        }
        if let Some(codexbar_index) = candidates
            .codexbar
            .iter()
            .enumerate()
            .find(|(index, codexbar)| {
                !codexbar_matched[*index]
                    && native.window.id.eq_ignore_ascii_case(&codexbar.window.id)
            })
            .map(|(index, _)| index)
        {
            native_matches[native_index] = Some((codexbar_index, false));
            codexbar_matched[codexbar_index] = true;
        }
    }

    for (native_index, native) in candidates.native.iter().enumerate() {
        let candidate = native_matches[native_index]
            .map(|(index, structural)| {
                let codexbar = &candidates.codexbar[index];
                let mut selected = preferred_candidate(native, codexbar, now_secs).clone();
                if structural {
                    // CodexBar's built-in ID is the normalized combined-view
                    // slot; the winning values and provenance remain native.
                    selected.window.id.clone_from(&codexbar.window.id);
                }
                selected
            })
            .unwrap_or_else(|| native.clone());
        selected.push(candidate);
    }
    for (index, codexbar) in candidates.codexbar.iter().enumerate() {
        if !codexbar_matched[index] {
            selected.push(codexbar.clone());
        }
    }

    if selected.is_empty() {
        return None;
    }
    selected.sort_by_key(|candidate| window_sort_rank(&candidate.window.id));
    // A provider-level timestamp must describe every displayed window. The
    // oldest selected sample is conservative; any unknown timestamp makes the
    // combined provider timestamp unknown as well.
    let updated_at = selected.iter().try_fold(u64::MAX, |oldest, candidate| {
        if candidate.force_stale {
            None
        } else {
            candidate
                .updated_at
                .map(|updated_at| oldest.min(updated_at))
        }
    });
    let windows = selected
        .into_iter()
        .map(|candidate| candidate.window)
        .collect::<Vec<_>>();
    let primary = windows
        .iter()
        .find(|window| window.id.eq_ignore_ascii_case("primary"));
    let secondary = windows
        .iter()
        .find(|window| window.id.eq_ignore_ascii_case("secondary"));

    Some(RateLimitInfo {
        source: provider,
        five_hour_pct: primary.map(|window| window.used_pct),
        five_hour_resets_at: primary.and_then(|window| window.resets_at),
        five_hour_window_minutes: primary.and_then(|window| window.window_minutes),
        seven_day_pct: secondary.map(|window| window.used_pct),
        seven_day_resets_at: secondary.and_then(|window| window.resets_at),
        seven_day_window_minutes: secondary.and_then(|window| window.window_minutes),
        updated_at,
        windows,
    })
}

fn structural_mapping_has_unique_builtin_ids(
    native: &[QuotaWindowCandidate],
    codexbar: &[QuotaWindowCandidate],
    native_matches: &[Option<(usize, bool)>],
    codexbar_matched: &[bool],
) -> bool {
    let mut ids = HashSet::new();
    for (index, native) in native.iter().enumerate() {
        let id = native_matches[index]
            .filter(|(_, structural)| *structural)
            .map(|(codexbar_index, _)| codexbar[codexbar_index].window.id.as_str())
            .unwrap_or(native.window.id.as_str());
        if is_builtin_quota_slot(id) && !ids.insert(id.to_ascii_lowercase()) {
            return false;
        }
    }
    for (index, candidate) in codexbar.iter().enumerate() {
        if !codexbar_matched[index]
            && is_builtin_quota_slot(&candidate.window.id)
            && !ids.insert(candidate.window.id.to_ascii_lowercase())
        {
            return false;
        }
    }
    true
}

fn quota_windows_structurally_alias(left: &RateLimitWindow, right: &RateLimitWindow) -> bool {
    // A provider can expose a long-only plan window in its native `primary`
    // slot while CodexBar normalizes the same standard window to `secondary`.
    // Treat only an exact built-in fingerprint as an alias; provider-specific
    // extra windows remain distinct even when they share a duration.
    is_builtin_quota_slot(&left.id)
        && is_builtin_quota_slot(&right.id)
        && left.window_minutes.is_some()
        && left.window_minutes == right.window_minutes
        && left.resets_at.is_some()
        && left.resets_at == right.resets_at
}

fn is_builtin_quota_slot(id: &str) -> bool {
    ["primary", "secondary", "tertiary"]
        .iter()
        .any(|slot| id.eq_ignore_ascii_case(slot))
}

fn preferred_candidate<'a>(
    native: &'a QuotaWindowCandidate,
    codexbar: &'a QuotaWindowCandidate,
    now_secs: u64,
) -> &'a QuotaWindowCandidate {
    if candidate_is_fresh(native, now_secs) {
        native
    } else if candidate_is_fresh(codexbar, now_secs)
        || sample_is_newer(codexbar.updated_at, native.updated_at)
    {
        codexbar
    } else {
        native
    }
}

fn candidate_is_fresh(candidate: &QuotaWindowCandidate, now_secs: u64) -> bool {
    !candidate.force_stale
        && candidate
            .updated_at
            .is_some_and(|updated_at| now_secs.saturating_sub(updated_at) <= QUOTA_FRESH_SECS)
}

fn sample_is_newer(candidate: Option<u64>, current: Option<u64>) -> bool {
    match (candidate, current) {
        (Some(candidate), Some(current)) => candidate > current,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

fn provider_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    provider_sort_rank(left)
        .cmp(&provider_sort_rank(right))
        .then_with(|| left.cmp(right))
}

fn provider_sort_rank(provider: &str) -> u8 {
    match provider {
        "claude" => 0,
        "codex" => 1,
        "grok" => 2,
        "kimi" => 3,
        _ => 4,
    }
}

fn window_sort_rank(window_id: &str) -> u8 {
    if window_id.eq_ignore_ascii_case("primary") {
        0
    } else if window_id.eq_ignore_ascii_case("secondary") {
        1
    } else if window_id.eq_ignore_ascii_case("tertiary") {
        2
    } else {
        3
    }
}

/// Cross-poll identity for status evidence. The opaque incarnation marker is
/// only valid together with its PID, so both are part of the key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct StatusEvidenceKey {
    provider: String,
    session_id: String,
    pid: u32,
    process_incarnation: String,
}

/// Identity captured between the first and second `x` presses. Unlike a list
/// index, this remains safe when a refresh reorders the sessions.
struct KillConfirmation {
    provider: String,
    session_id: String,
    pid: u32,
    process_incarnation: String,
    grok_session_ids: Vec<String>,
    requested_at: Instant,
}

impl KillConfirmation {
    fn for_session(
        session: &AgentSession,
        sessions: &[AgentSession],
        process_incarnation: String,
    ) -> Self {
        let mut grok_session_ids = if session.agent_cli == "grok" {
            sessions
                .iter()
                .filter(|other| other.agent_cli == "grok" && other.pid == session.pid)
                .map(|other| other.session_id.clone())
                .collect()
        } else {
            Vec::new()
        };
        grok_session_ids.sort();
        Self {
            provider: session.agent_cli.to_string(),
            session_id: session.session_id.clone(),
            pid: session.pid,
            process_incarnation,
            grok_session_ids,
            requested_at: Instant::now(),
        }
    }

    fn matches(&self, session: &AgentSession, sessions: &[AgentSession]) -> bool {
        let mut current_grok_session_ids = if self.provider == "grok" {
            sessions
                .iter()
                .filter(|other| other.agent_cli == "grok" && other.pid == self.pid)
                .map(|other| other.session_id.clone())
                .collect()
        } else {
            Vec::new()
        };
        current_grok_session_ids.sort();
        self.provider == session.agent_cli
            && self.session_id == session.session_id
            && self.pid == session.pid
            && session.action_process_incarnation.as_deref()
                == Some(self.process_incarnation.as_str())
            && self.grok_session_ids == current_grok_session_ids
    }
}

/// Produce a terminal-safe fallback summary from a raw prompt.
fn sanitize_fallback(prompt: &str, max_len: usize) -> String {
    let safe = crate::collector::sanitize_terminal_text(prompt);
    let redacted = crate::collector::redact_secrets(&safe);
    redacted.chars().take(max_len).collect()
}

/// Outcome of an Enter-key jump attempt. Distinct from `Option<String>` so
/// callers (notably `--exit-on-jump`) can tell a real terminal jump apart from
/// a no-op (unsupported terminal, or empty session list).
#[derive(Debug, PartialEq, Eq)]
pub enum JumpOutcome {
    /// Actually switched to a terminal pane/tab/window.
    Jumped,
    /// Tried to jump through an applicable backend, but the focus command failed.
    Failed(String),
    /// Unsupported terminal, or nothing selected — nothing happened.
    NoOp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowTab {
    Work,
    Usage,
    System,
}

impl NarrowTab {
    pub const ALL: [Self; 3] = [Self::Work, Self::Usage, Self::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Work => "Work",
            Self::Usage => "Usage",
            Self::System => "System",
        }
    }

    pub fn shortcut(self) -> char {
        match self {
            Self::Work => 'w',
            Self::Usage => 'u',
            Self::System => 's',
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NarrowSection {
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
            Self::Sessions | Self::Projects => NarrowTab::Work,
            Self::Context | Self::Quota | Self::Tokens => NarrowTab::Usage,
            Self::Ports | Self::Mcp => NarrowTab::System,
        }
    }
}

pub struct App {
    pub sessions: Vec<AgentSession>,
    pub selected: usize,
    pub should_quit: bool,
    /// Token rate per tick (delta). Ring buffer for the braille graph.
    pub token_rates: VecDeque<f64>,
    /// Account-level rate limits from native sources and enabled CodexBar providers.
    pub rate_limits: Vec<RateLimitInfo>,
    /// Native provider quota before the optional CodexBar data is merged.
    native_rate_limits: Vec<RateLimitInfo>,
    /// Per-session previous token totals, keyed by (agent_cli, session_id).
    prev_tokens: HashMap<(String, String), u64>,
    /// Cross-poll status ledger keyed by exact logical and process identity.
    /// Sessions without a queryable process incarnation deliberately do not
    /// inherit evidence from an earlier poll.
    status_evidence_ledger: HashMap<StatusEvidenceKey, StatusEvidence>,
    /// Rate limit poll counter (read every 5 ticks = 10s)
    rate_limit_counter: u32,
    codexbar_quota_poller: CodexBarQuotaPoller,
    codexbar_quota_status: CodexBarQuotaStatus,
    codexbar_provider_snapshots: Vec<CodexBarProviderSnapshot>,
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
    /// Stable process/session identity captured for the two-press kill guard.
    kill_confirm: Option<KillConfirmation>,
    pub theme: Theme,
    pub show_context: bool,
    pub show_quota: bool,
    pub show_tokens: bool,
    pub show_projects: bool,
    pub show_ports: bool,
    pub show_sessions: bool,
    pub show_mcp: bool,
    pub codexbar_quota_fallback: bool,
    pub narrow_tab: NarrowTab,
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
    /// Help overlay (`?`) visibility.
    pub help_open: bool,
    /// View leader overlay (`v`) visibility.
    pub view_open: bool,
}

impl App {
    #[cfg(test)]
    pub fn new_with_config(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
    ) -> Self {
        Self::new_with_config_and_claude_dirs(theme, hidden_agents, panels, &[])
    }

    pub fn new_with_config_and_claude_dirs(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
        claude_config_dirs: &[PathBuf],
    ) -> Self {
        Self::new_with_config_and_claude_dirs_and_codexbar(
            theme,
            hidden_agents,
            panels,
            claude_config_dirs,
            false,
        )
    }

    pub fn new_with_config_and_claude_dirs_and_codexbar(
        theme: Theme,
        hidden_agents: &[String],
        panels: crate::config::PanelVisibility,
        claude_config_dirs: &[PathBuf],
        codexbar_quota_fallback: bool,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let summaries = load_summary_cache();
        let mut collector =
            MultiCollector::with_hidden_and_claude_config_dirs(hidden_agents, claude_config_dirs);
        collector.set_mcp_suppress(true);
        let codexbar_quota_poller = CodexBarQuotaPoller::new(codexbar_quota_fallback);
        let codexbar_quota_status = codexbar_quota_poller.status();
        Self {
            sessions: Vec::new(),
            selected: 0,
            should_quit: false,
            token_rates: VecDeque::with_capacity(GRAPH_HISTORY_LEN),
            rate_limits: Vec::new(),
            native_rate_limits: Vec::new(),
            prev_tokens: HashMap::new(),
            status_evidence_ledger: HashMap::new(),
            rate_limit_counter: 5,
            codexbar_quota_poller,
            codexbar_quota_status,
            codexbar_provider_snapshots: Vec::new(),
            collector,
            summaries,
            pending_summaries: HashSet::new(),
            summary_retries: HashMap::new(),
            summary_rx: rx,
            summary_tx: tx,
            orphan_ports: Vec::new(),
            status_msg: None,
            kill_confirm: None,
            theme,
            show_context: panels.context,
            show_quota: panels.quota,
            show_tokens: panels.tokens,
            show_projects: panels.projects,
            show_ports: panels.ports,
            show_sessions: panels.sessions,
            show_mcp: panels.mcp,
            codexbar_quota_fallback,
            narrow_tab: NarrowTab::Work,
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
        9 // theme + 7 panel toggles + CodexBar quota fallback
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
            8 => {
                self.toggle_codexbar_quota_fallback();
                return;
            }
            _ => return,
        }
        self.persist_panel_visibility();
        self.clamp_narrow_tab();
    }

    fn toggle_codexbar_quota_fallback(&mut self) {
        self.codexbar_quota_fallback = !self.codexbar_quota_fallback;
        self.codexbar_quota_poller
            .set_enabled(self.codexbar_quota_fallback);
        self.refresh_codexbar_quota();
        if let Err(error) =
            crate::config::save_codexbar_quota_fallback(self.codexbar_quota_fallback)
        {
            self.set_status(format!("CodexBar quota setting save failed: {error}"));
        }
    }

    pub fn narrow_tab_visible(&self, tab: NarrowTab) -> bool {
        match tab {
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
            self.clamp_narrow_section();
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
        self.clamp_narrow_section();
    }

    fn clamp_narrow_tab(&mut self) {
        if let Some(tab) = self.active_narrow_tab() {
            self.narrow_tab = tab;
        }
        self.clamp_narrow_section();
    }

    pub fn narrow_section_visible(&self, section: NarrowSection) -> bool {
        match section {
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

    /// Full refresh used by the TUI: collect monitored data, then generate and
    /// retry session summaries. Equivalent to [`App::tick_no_summaries`] followed
    /// by [`App::drain_and_retry_summaries`].
    pub fn tick(&mut self) {
        self.tick_no_summaries();
        self.drain_and_retry_summaries();
    }

    /// Refresh all monitored data WITHOUT spawning background summary jobs.
    ///
    /// `tick` additionally calls [`App::drain_and_retry_summaries`], which
    /// shells out to `claude --print` to generate session titles. Headless
    /// consumers (e.g. the web snapshot API) call this variant so they never
    /// invoke that summary path or consume the user's Claude quota. When the
    /// explicit CodexBar fallback is enabled, this variant may still start its
    /// bounded Codex CLI quota subprocess.
    pub fn tick_no_summaries(&mut self) {
        self.collector.set_mcp_suppress(self.mcp_suppress_sessions);
        let mut sessions = self.collector.collect();
        self.status_evidence_ledger = reconcile_status_evidence(
            &mut sessions,
            &self.status_evidence_ledger,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        );
        self.sessions = sessions;
        self.orphan_ports = self.collector.orphan_ports.clone();
        self.mcp_servers = self.collector.mcp_servers.clone();
        self.host_metrics = self.host_sampler.sample();
        self.agent_aggregate = AgentAggregate::from_sessions(&self.sessions);
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
        if self.native_rate_limits.is_empty() || self.rate_limit_counter >= 5 {
            self.rate_limit_counter = 0;
            let extra_dirs = self.collector.all_config_dirs();
            self.native_rate_limits = read_rate_limits(&extra_dirs);
            // Merge live rate limits from agent collectors (e.g. Codex JSONL parsing)
            self.native_rate_limits
                .extend(self.collector.agent_rate_limits());
        } else {
            self.rate_limit_counter += 1;
        }
        self.refresh_codexbar_quota();

        // Quota percentages are display-only. A lifecycle can become
        // RateLimited only when the provider reports an active block.
    }

    /// Wait for the first enabled CodexBar quota poll without changing session
    /// state. Used only by one-shot output modes; normal TUI ticks never block.
    pub fn wait_for_initial_codexbar_quota(&mut self, timeout: std::time::Duration) {
        self.refresh_codexbar_quota_with_wait(timeout);
    }

    pub(crate) fn codexbar_quota_status(&self) -> &CodexBarQuotaStatus {
        &self.codexbar_quota_status
    }

    pub(crate) fn codexbar_provider_snapshots(&self) -> &[CodexBarProviderSnapshot] {
        &self.codexbar_provider_snapshots
    }

    fn refresh_codexbar_quota(&mut self) {
        self.refresh_codexbar_quota_with_wait(std::time::Duration::ZERO);
    }

    fn refresh_codexbar_quota_with_wait(&mut self, timeout: std::time::Duration) {
        let codexbar = if timeout.is_zero() {
            self.codexbar_quota_poller.update()
        } else {
            self.codexbar_quota_poller.wait_for_initial(timeout)
        };
        // One-shot modes may block here while CodexBar fetches providers. Take
        // the merge clock only after that wait so timestamps produced by the
        // completed poll are not misclassified as future data.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.codexbar_quota_status = self.codexbar_quota_poller.status();
        self.codexbar_provider_snapshots = codexbar
            .as_ref()
            .map(|snapshot| snapshot.providers.clone())
            .unwrap_or_default();
        self.codexbar_provider_snapshots
            .sort_by(|left, right| provider_cmp(&left.provider, &right.provider));
        self.rate_limits = merge_rate_limits(
            &self.native_rate_limits,
            codexbar.as_ref(),
            now_secs,
            self.codexbar_quota_status.last_checked_at,
            self.codexbar_quota_status.error.is_some(),
        );
    }

    /// Drain completed summary results and spawn retries. Does NOT recollect
    /// sessions, so it is safe for `--once` mode (stable snapshot).
    pub fn drain_and_retry_summaries(&mut self) {
        while let Ok((sid, prompt, maybe_summary)) = self.summary_rx.try_recv() {
            self.pending_summaries.remove(&sid);
            match maybe_summary {
                Some(summary) => {
                    self.summary_retries.remove(&sid);
                    self.summaries.insert(sid, summary);
                    save_summary_cache(&self.summaries);
                }
                None => {
                    let count = self.summary_retries.entry(sid.clone()).or_insert(0);
                    *count += 1;
                    if *count >= MAX_SUMMARY_RETRIES {
                        // Exhausted — store sanitized fallback using prompt from worker
                        self.summaries.insert(sid, sanitize_fallback(&prompt, 80));
                        save_summary_cache(&self.summaries);
                    }
                }
            }
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
        s.agent_cli.to_lowercase().contains(query)
            || s.project_name.to_lowercase().contains(query)
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
        if !session_process_is_actionable(session) {
            return;
        }

        // Confirm only the exact logical session and process incarnation that
        // received the first press. Revalidate from fresh OS state immediately
        // before signaling so list churn and PID reuse cannot redirect a kill.
        if let Some(confirm) = self.kill_confirm.take() {
            if confirm.matches(session, &self.sessions)
                && confirm.requested_at.elapsed().as_secs() < 2
            {
                let pid = confirm.pid;
                let verified = freshly_validate_action_process(
                    &confirm.provider,
                    pid,
                    Some(&confirm.process_incarnation),
                );
                if !verified {
                    self.set_status(format!(
                        "PID {} is no longer the selected agent process",
                        pid
                    ));
                    return;
                }
                if let Err(message) = terminate_process(pid) {
                    self.set_status(message);
                    return;
                }
                self.tick();
                return;
            }
        }

        // First press — anchor the confirmation to the exact incarnation that
        // produced the displayed row, then verify it is still current before
        // offering a destructive second press.
        let Some(expected_incarnation) = session.action_process_incarnation.clone() else {
            self.set_status(format!("Cannot safely verify PID {}", session.pid));
            return;
        };
        if !freshly_validate_action_process(
            session.agent_cli,
            session.pid,
            Some(&expected_incarnation),
        ) {
            self.set_status(format!("Cannot safely verify PID {}", session.pid));
            return;
        }
        let name = self
            .summaries
            .get(&session.session_id)
            .cloned()
            .unwrap_or_else(|| format!("PID {}", session.pid));
        let affected_grok_sessions = self
            .sessions
            .iter()
            .filter(|other| other.agent_cli == "grok" && other.pid == session.pid)
            .count();
        let message = kill_confirmation_message(
            session.agent_cli,
            session.pid,
            &name,
            affected_grok_sessions,
        );
        let confirmation =
            KillConfirmation::for_session(session, &self.sessions, expected_incarnation);
        self.kill_confirm = Some(confirmation);
        self.set_status(message);
    }

    /// Kill all orphan port processes (Shift+X).
    /// Does a fresh port scan and validates PID identity + port ownership
    /// immediately before sending any signals to avoid PID reuse / stale cache issues.
    pub fn kill_orphan_ports(&mut self) {
        use crate::collector::process::get_listening_ports;

        // Fresh port scan right now — don't rely on cached data
        let fresh_ports = get_listening_ports();
        let fresh_processes = crate::collector::process::get_process_info();
        let mut failures = Vec::new();
        let mut killed = 0usize;

        for orphan in &self.orphan_ports {
            // 1. Verify PID still listens on the expected port
            let still_listening = fresh_ports
                .get(&orphan.pid)
                .is_some_and(|ports| ports.contains(&orphan.port));
            if !still_listening {
                failures.push(format!(
                    "Skipped PID {}: port {} is no longer listening",
                    orphan.pid, orphan.port
                ));
                continue;
            }
            // 2. Verify PID still runs the exact expected command before using
            // the platform-native hard-kill path.
            let same_command = fresh_processes
                .get(&orphan.pid)
                .is_some_and(|process| process.command == orphan.command);
            if !same_command {
                failures.push(format!(
                    "Skipped PID {}: process identity changed",
                    orphan.pid
                ));
                continue;
            }
            match terminate_process(orphan.pid) {
                Ok(()) => killed += 1,
                Err(error) => failures.push(error),
            }
        }
        // Re-collect to reflect changes
        self.tick();
        self.set_status(orphan_kill_status(killed, &failures));
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Jump to the terminal running the selected session's agent process.
    /// Delegates to the terminal-jumper registry (Herdr / cmux / tmux / iTerm2);
    /// see [`crate::jump`]. The cached process incarnation and provider command
    /// are revalidated from fresh OS state immediately before resolving the
    /// terminal, so PID reuse cannot redirect the action. No-op when nothing is
    /// selected or no backend recognizes the process.
    pub fn jump_to_session(&mut self) -> JumpOutcome {
        self.jump_to_session_with(crate::jump::run_herdr_session_jump, crate::jump::run_jump)
    }

    fn jump_to_session_with(
        &mut self,
        semantic_jump: impl FnOnce(&str, &str) -> JumpOutcome,
        process_jump: impl FnOnce(u32) -> JumpOutcome,
    ) -> JumpOutcome {
        if self.sessions.is_empty() {
            return JumpOutcome::NoOp;
        }
        let selected = &self.sessions[self.selected];
        match semantic_jump(selected.agent_cli, &selected.session_id) {
            JumpOutcome::NoOp => {}
            outcome => return outcome,
        }
        if !session_process_is_actionable(selected) {
            return JumpOutcome::NoOp;
        }

        if !freshly_validate_action_process(
            selected.agent_cli,
            selected.pid,
            selected.action_process_incarnation.as_deref(),
        ) {
            return JumpOutcome::Failed(format!(
                "PID {} is no longer the selected agent process",
                selected.pid
            ));
        }
        let target_pid = selected.pid;
        process_jump(target_pid)
    }

    /// Get the display summary for a session: LLM summary > "..." if pending > raw prompt > "—"
    /// Done sessions skip pending state to avoid stuck "..." display.
    pub fn session_summary(&self, session: &AgentSession) -> String {
        if let Some(summary) = self.summaries.get(&session.session_id) {
            summary.clone()
        } else if matches!(session.status, SessionStatus::Done) {
            // Done sessions: don't wait for pending summary, show fallback immediately
            if !session.initial_prompt.is_empty() {
                sanitize_fallback(&session.initial_prompt, 80)
            } else if !session.first_assistant_text.is_empty() {
                sanitize_fallback(&session.first_assistant_text, 80)
            } else {
                "—".to_string()
            }
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
        } else if !session.initial_prompt.is_empty() {
            sanitize_fallback(&session.initial_prompt, 80)
        } else if !session.first_assistant_text.is_empty() {
            sanitize_fallback(&session.first_assistant_text, 80)
        } else {
            "—".to_string()
        }
    }
}

/// Return whether a row carries enough process ownership proof for a PID action.
///
/// Current Kimi Code rewrites every launch mode's process title to bare
/// `kimi-code`. A unique cwd/session activity match can still support a useful
/// lifecycle display, but it cannot distinguish the interactive TUI from web,
/// ACP, or plugin hosts. Keep those heuristic rows visible while preventing
/// kill and terminal-jump actions from targeting the wrong host process.
fn session_process_is_actionable(session: &AgentSession) -> bool {
    !matches!(
        session.status,
        SessionStatus::Working | SessionStatus::Unknown | SessionStatus::Done
    ) && session.pid > 0
        && session.action_process_incarnation.is_some()
        && session.status_evidence.authority != StatusAuthority::Unavailable
        && (session.agent_cli != "kimi"
            || session.status_evidence.authority == StatusAuthority::Provider)
}

/// Call `claude --print` via stdin pipe to summarize a prompt.
/// Returns `None` on timeout so the caller can retry later.
fn generate_summary(prompt: &str, assistant_text: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // Build input from user prompt and/or first assistant response
    let user_part: String = prompt.chars().take(200).collect();
    let assistant_part: String = assistant_text.chars().take(200).collect();

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
        Err(_) => return Some(sanitize_fallback(prompt, 80)),
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
            let _ = terminate_process(child_pid);
            return None;
        }
    };

    let fallback = sanitize_fallback(prompt, 80);

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
            // Purge polluted or old truncated-fallback entries so they regenerate,
            // and re-sanitize legacy cache values before rendering them.
            let before = cache.len();
            cache.retain(|_, v| !v.contains("You are a conversation tit") && !v.ends_with('…'));
            let mut sanitized = false;
            for value in cache.values_mut() {
                let safe = sanitize_fallback(value, 80);
                if *value != safe {
                    *value = safe;
                    sanitized = true;
                }
            }
            if cache.len() < before || sanitized {
                // Persist cleaned cache
                let _ = std::fs::create_dir_all(cache_dir());
                let _ = std::fs::write(&path, serde_json::to_string(&cache).unwrap_or_default());
            }
            cache
        }
        Err(_) => HashMap::new(),
    }
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

fn status_evidence_key(session: &AgentSession) -> Option<StatusEvidenceKey> {
    if session.session_id.is_empty() || session.pid == 0 {
        return None;
    }
    let process_incarnation = session.action_process_incarnation.clone()?;
    Some(StatusEvidenceKey {
        provider: session.agent_cli.to_string(),
        session_id: session.session_id.clone(),
        pid: session.pid,
        process_incarnation,
    })
}

/// Preserve evidence between collector rebuilds only for an exact logical
/// session and process incarnation, and add one bounded sample for collectors
/// that do not yet supply their own lifecycle provenance. Rows without a
/// collector-bound action identity deliberately never inherit history.
fn reconcile_status_evidence(
    sessions: &mut [AgentSession],
    previous: &HashMap<StatusEvidenceKey, StatusEvidence>,
    observed_at_ms: u64,
) -> HashMap<StatusEvidenceKey, StatusEvidence> {
    let mut current = HashMap::new();
    for session in sessions {
        session.enforce_status_contract();
        let key = status_evidence_key(session);
        let old = key.as_ref().and_then(|key| previous.get(key));

        if session.status_evidence.has_sample() {
            if let Some(old) = old {
                prepend_status_history(&mut session.status_evidence, old);
            }
        } else {
            let mut evidence = old.cloned().unwrap_or_default();
            let (authority, reason) = if session.status == SessionStatus::Unknown {
                (
                    StatusAuthority::Unavailable,
                    StatusReason::OwnershipUnconfirmed,
                )
            } else {
                (StatusAuthority::Heuristic, StatusReason::CollectorInference)
            };
            evidence.observe(StatusObservation::new(
                session.status,
                authority,
                reason,
                observed_at_ms,
                0,
            ));
            session.status_evidence = evidence;
        }

        if let Some(key) = key {
            current.insert(key, session.status_evidence.clone());
        }
    }
    current
}

fn prepend_status_history(current: &mut StatusEvidence, previous: &StatusEvidence) {
    if previous.observations.is_empty() {
        return;
    }

    // Most collectors publish one fresh source-qualified sample per poll and
    // rely on App to retain the cross-poll ledger. Preserve the duration and
    // matching count for that incremental shape. Provider-local state can
    // already contain a multi-sample ledger; trust its summary and only
    // prepend observations that aged out of the bounded source file.
    let incremental = current.observations.len() == 1;
    let incoming = current.observations.last().cloned();
    let previous_latest = previous.observations.last();
    if incremental {
        if let (Some(incoming), Some(previous_latest)) = (incoming.as_ref(), previous_latest) {
            let same_projection = incoming.status == previous_latest.status
                && incoming.authority == previous_latest.authority
                && incoming.connection_generation == previous_latest.connection_generation;
            if same_projection && incoming.observed_at_ms > previous.observed_at_ms {
                // A one-sample ledger normally gets `status_since_ms` from
                // `observe`, making it equal to `observed_at_ms`. A strictly
                // earlier, newer Provider value is therefore an explicit
                // lifecycle timestamp and must win over App's older history.
                // Equality is conservatively treated as the synthetic default
                // because the model does not carry an explicit/source bit.
                let newer_explicit_provider_since = incoming.authority == StatusAuthority::Provider
                    && current.status_since_ms > previous.status_since_ms
                    && current.status_since_ms < incoming.observed_at_ms;
                if !newer_explicit_provider_since {
                    current.status_since_ms = if previous.status_since_ms > 0 {
                        previous.status_since_ms
                    } else {
                        incoming.observed_at_ms
                    };
                }
                current.consecutive_matching = previous
                    .consecutive_matching
                    .saturating_add(current.consecutive_matching.max(1));
            } else if same_projection && incoming == previous_latest {
                current.status_since_ms = previous.status_since_ms;
                current.consecutive_matching = current
                    .consecutive_matching
                    .max(previous.consecutive_matching);
            }
        }
    }

    let mut merged = previous.observations.clone();
    for observation in &current.observations {
        if !merged.contains(observation) {
            merged.push(observation.clone());
        }
    }
    merged.sort_by_key(|observation| observation.observed_at_ms);
    if merged.len() > MAX_STATUS_OBSERVATIONS {
        let excess = merged.len() - MAX_STATUS_OBSERVATIONS;
        merged.drain(..excess);
    }
    current.observations = merged;
}

fn is_supported_agent_command(cmd: &str) -> bool {
    crate::collector::process::cmd_has_binary(cmd, "claude")
        || crate::collector::process::cmd_has_binary(cmd, "codex")
        || crate::collector::process::cmd_has_binary(cmd, "opencode")
        || is_grok_agent_command(cmd)
        || is_kimi_agent_command(cmd)
}

fn is_killable_agent_command(cmd: &str) -> bool {
    ["claude", "codex", "opencode", "grok", "kimi"]
        .iter()
        .any(|provider| is_killable_agent_command_for_provider(provider, cmd))
}

fn is_killable_agent_command_for_provider(provider: &str, cmd: &str) -> bool {
    match provider {
        "claude" => crate::collector::process::cmd_has_binary(cmd, "claude"),
        "codex" => {
            crate::collector::process::cmd_has_binary(cmd, "codex")
                && !crate::collector::process::command_tokens(cmd)
                    .windows(2)
                    .any(|pair| {
                        pair[0].eq_ignore_ascii_case("codex")
                            && pair[1].eq_ignore_ascii_case("app-server")
                    })
                && !cmd.contains(" app-server")
        }
        "opencode" => crate::collector::process::cmd_has_binary(cmd, "opencode"),
        "grok" => is_grok_agent_command(cmd),
        "kimi" => is_kimi_agent_command(cmd),
        _ => false,
    }
}

fn is_grok_agent_command(cmd: &str) -> bool {
    crate::collector::grok::is_grok_process(cmd)
}

fn is_kimi_agent_command(cmd: &str) -> bool {
    crate::collector::kimi::is_kimi_process(cmd)
}

fn process_incarnation_matches(expected: Option<&str>, current: Option<&str>) -> bool {
    matches!((expected, current), (Some(expected), Some(current)) if expected == current)
}

fn exact_action_argv_is_valid(provider: &str, tokens: &[String]) -> bool {
    match provider {
        "claude" => crate::collector::process::tokens_have_binary(tokens, "claude"),
        "codex" => {
            crate::collector::process::tokens_have_binary(tokens, "codex")
                && !tokens
                    .iter()
                    .skip(1)
                    .any(|token| token.eq_ignore_ascii_case("app-server"))
        }
        "opencode" => crate::collector::process::tokens_have_binary(tokens, "opencode"),
        "grok" => crate::collector::grok::is_grok_process_tokens(tokens),
        "kimi" => crate::collector::kimi::is_kimi_process_tokens(tokens),
        _ => false,
    }
}

/// Check one command observation bracketed by two exact process-incarnation
/// reads. This prevents a command sampled from one process from being accepted
/// together with a reused PID belonging to another process.
fn action_process_observation_is_valid(
    provider: &str,
    expected_incarnation: Option<&str>,
    before_incarnation: Option<&str>,
    after_incarnation: Option<&str>,
    command: Option<&str>,
    grok_leader: bool,
) -> bool {
    process_incarnation_matches(expected_incarnation, before_incarnation)
        && process_incarnation_matches(expected_incarnation, after_incarnation)
        && command.is_some_and(|cmd| {
            is_supported_agent_command(cmd)
                && is_killable_agent_command(cmd)
                && is_killable_agent_command_for_provider(provider, cmd)
        })
        && !(provider == "grok" && grok_leader)
}

/// Revalidate a cached action target from fresh OS state immediately before a
/// kill or terminal jump. Unknown identity, PID reuse, provider drift, and Grok
/// leader processes all fail closed.
fn freshly_validate_action_process(
    provider: &str,
    pid: u32,
    expected_incarnation: Option<&str>,
) -> bool {
    let Some(expected_incarnation) = expected_incarnation else {
        return false;
    };
    let before_incarnation = crate::collector::process::get_process_incarnation(pid);
    let process_tokens = crate::collector::process::get_process_tokens(pid);
    let command = crate::collector::process::get_process_info()
        .remove(&pid)
        .map(|process| process.command);
    let grok_leader = provider == "grok" && crate::collector::grok::is_grok_leader_pid(pid);
    let after_incarnation = crate::collector::process::get_process_incarnation(pid);

    process_tokens
        .as_deref()
        .is_some_and(|tokens| exact_action_argv_is_valid(provider, tokens))
        && action_process_observation_is_valid(
            provider,
            Some(expected_incarnation),
            before_incarnation.as_deref(),
            after_incarnation.as_deref(),
            command.as_deref(),
            grok_leader,
        )
}

fn terminate_process(pid: u32) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();

    #[cfg(not(target_os = "windows"))]
    let result = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output();

    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(termination_failure_message(
            pid,
            &String::from_utf8_lossy(&output.stderr),
            &String::from_utf8_lossy(&output.stdout),
            &output.status.to_string(),
        )),
        Err(error) => Err(termination_failure_message(pid, &error.to_string(), "", "")),
    }
}

fn termination_failure_message(pid: u32, stderr: &str, stdout: &str, status: &str) -> String {
    let raw_detail = [stderr, stdout, status]
        .into_iter()
        .map(str::trim)
        .find(|detail| !detail.is_empty())
        .unwrap_or("unknown error");
    let detail = crate::collector::sanitize_terminal_text(raw_detail)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect::<String>();
    format!("Failed to kill PID {}: {}", pid, detail)
}

fn orphan_kill_status(killed: usize, failures: &[String]) -> String {
    match (killed, failures.first()) {
        (0, None) => "No orphan processes needed killing".to_string(),
        (killed, None) => format!("Killed {} orphan process(es)", killed),
        (0, Some(first)) if failures.len() == 1 => first.clone(),
        (0, Some(first)) => format!("{} orphan failures: {}", failures.len(), first),
        (killed, Some(first)) => format!(
            "Killed {}; {} orphan failure(s): {}",
            killed,
            failures.len(),
            first
        ),
    }
}

fn kill_confirmation_message(provider: &str, pid: u32, name: &str, affected: usize) -> String {
    if provider == "grok" && affected > 1 {
        format!(
            "Press x again to kill PID {}; affects {} Grok sessions",
            pid, affected
        )
    } else {
        format!("Press x again to kill: {}", name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_quota(
        provider: &str,
        updated_at: u64,
        primary_pct: Option<f64>,
        secondary_pct: Option<f64>,
    ) -> RateLimitInfo {
        RateLimitInfo {
            source: provider.to_string(),
            five_hour_pct: primary_pct,
            five_hour_resets_at: primary_pct.map(|_| updated_at + 300),
            five_hour_window_minutes: primary_pct.map(|_| 300),
            seven_day_pct: secondary_pct,
            seven_day_resets_at: secondary_pct.map(|_| updated_at + 10_080),
            seven_day_window_minutes: secondary_pct.map(|_| 10_080),
            updated_at: Some(updated_at),
            windows: Vec::new(),
        }
    }

    fn codexbar_window(id: &str, used_pct: f64) -> crate::collector::codexbar::CodexBarWindow {
        crate::collector::codexbar::CodexBarWindow {
            id: id.to_string(),
            label: id.to_string(),
            used_pct,
            resets_at: Some(20_000),
            window_minutes: Some(300),
        }
    }

    fn codexbar_provider(
        provider: &str,
        updated_at: u64,
        windows: Vec<crate::collector::codexbar::CodexBarWindow>,
    ) -> CodexBarProviderSnapshot {
        CodexBarProviderSnapshot {
            provider: provider.to_string(),
            windows,
            updated_at: Some(updated_at),
            error: None,
        }
    }

    #[test]
    fn quota_merge_keeps_fresh_native_overlaps_and_codexbar_only_windows() {
        let now = 10_000;
        let native = native_quota("codex", now - 10, Some(12.0), Some(34.0));
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now,
                vec![
                    codexbar_window("primary", 91.0),
                    codexbar_window("secondary", 92.0),
                    codexbar_window("tertiary", 93.0),
                    codexbar_window("codex-spark-weekly", 94.0),
                ],
            )],
        };

        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged.len(), 1);
        let codex = &merged[0];
        assert_eq!(codex.five_hour_pct, Some(12.0));
        assert_eq!(codex.seven_day_pct, Some(34.0));
        assert_eq!(codex.updated_at, Some(now - 10));
        assert_eq!(codex.windows.len(), 4);
        assert_eq!(
            codex
                .windows
                .iter()
                .map(|window| (window.id.as_str(), window.used_pct, window.provenance))
                .collect::<Vec<_>>(),
            vec![
                ("primary", 12.0, RateLimitProvenance::Native),
                ("secondary", 34.0, RateLimitProvenance::Native),
                ("tertiary", 93.0, RateLimitProvenance::CodexBar),
                ("codex-spark-weekly", 94.0, RateLimitProvenance::CodexBar,),
            ]
        );
    }

    #[test]
    fn quota_merge_selects_the_newest_whole_native_provider_sample() {
        let now = 10_000;
        let first_account = native_quota("claude", now - 20, Some(11.0), None);
        let second_account = native_quota("claude", now - 10, None, Some(92.0));

        let merged = merge_rate_limits(&[first_account, second_account], None, now, None, false);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].five_hour_pct, None);
        assert_eq!(merged[0].seven_day_pct, Some(92.0));
        assert_eq!(merged[0].windows.len(), 1);
        assert_eq!(merged[0].updated_at, Some(now - 10));
    }

    #[test]
    fn quota_merge_preserves_discovery_order_for_equal_native_timestamps() {
        let now = 10_000;
        let first_account = native_quota("claude", now - 10, Some(11.0), None);
        let second_account = native_quota("claude", now - 10, None, Some(92.0));

        let merged = merge_rate_limits(&[first_account, second_account], None, now, None, false);

        assert_eq!(merged[0].five_hour_pct, Some(11.0));
        assert_eq!(merged[0].seven_day_pct, None);
        assert_eq!(merged[0].windows.len(), 1);
    }

    #[test]
    fn exact_native_primary_does_not_duplicate_its_legacy_long_projection() {
        let now = 10_000;
        let mut free_plan = native_quota("codex", now, None, Some(48.0));
        free_plan.windows = vec![RateLimitWindow::try_new(
            "primary",
            "30d",
            48.0,
            Some(now + 43_200),
            Some(43_200),
            RateLimitProvenance::Native,
        )
        .unwrap()];

        let merged = merge_rate_limits(&[free_plan], None, now, None, false);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].windows.len(), 1);
        assert_eq!(merged[0].windows[0].id, "primary");
        assert_eq!(merged[0].five_hour_pct, Some(48.0));
        assert_eq!(merged[0].seven_day_pct, None);
    }

    #[test]
    fn codex_shifted_builtin_slots_merge_by_exact_window_fingerprint() {
        let now = 10_000;
        let reset = 20_000;
        let mut native = native_quota("codex", now - 10, None, None);
        native.windows = vec![RateLimitWindow::try_new(
            "primary",
            "7d",
            55.0,
            Some(reset),
            Some(10_080),
            RateLimitProvenance::Native,
        )
        .unwrap()];
        let mut shifted = codexbar_window("secondary", 56.0);
        shifted.resets_at = Some(reset);
        shifted.window_minutes = Some(10_080);
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider("codex", now, vec![shifted])],
        };

        let merged = merge_rate_limits(
            std::slice::from_ref(&native),
            Some(&snapshot),
            now,
            None,
            false,
        );
        assert_eq!(merged[0].windows.len(), 1);
        assert_eq!(merged[0].windows[0].id, "secondary");
        assert_eq!(merged[0].windows[0].used_pct, 55.0);
        assert_eq!(merged[0].windows[0].provenance, RateLimitProvenance::Native);
        assert_eq!(merged[0].five_hour_pct, None);
        assert_eq!(merged[0].seven_day_pct, Some(55.0));

        native.updated_at = Some(now - QUOTA_FRESH_SECS - 1);
        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].windows.len(), 1);
        assert_eq!(merged[0].windows[0].id, "secondary");
        assert_eq!(merged[0].windows[0].used_pct, 56.0);
        assert_eq!(
            merged[0].windows[0].provenance,
            RateLimitProvenance::CodexBar
        );
    }

    #[test]
    fn structural_slot_matching_precedes_positional_id_fallback() {
        let now = 10_000;
        let mut native = native_quota("codex", now - 10, None, None);
        native.windows = vec![RateLimitWindow::try_new(
            "primary",
            "7d",
            55.0,
            Some(30_000),
            Some(10_080),
            RateLimitProvenance::Native,
        )
        .unwrap()];
        let mut five_hour = codexbar_window("primary", 12.0);
        five_hour.resets_at = Some(20_000);
        five_hour.window_minutes = Some(300);
        let mut weekly = codexbar_window("secondary", 56.0);
        weekly.resets_at = Some(30_000);
        weekly.window_minutes = Some(10_080);
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider("codex", now, vec![five_hour, weekly])],
        };

        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].windows.len(), 2);
        assert_eq!(
            merged[0]
                .windows
                .iter()
                .map(|window| { (window.id.as_str(), window.used_pct, window.provenance,) })
                .collect::<Vec<_>>(),
            vec![
                ("primary", 12.0, RateLimitProvenance::CodexBar),
                ("secondary", 55.0, RateLimitProvenance::Native),
            ]
        );
        assert_eq!(merged[0].five_hour_pct, Some(12.0));
        assert_eq!(merged[0].seven_day_pct, Some(55.0));
    }

    #[test]
    fn structural_aliasing_never_collapses_custom_or_incomplete_windows() {
        let now = 10_000;
        let mut native = native_quota("codex", now, None, None);
        native.windows = vec![
            RateLimitWindow::try_new(
                "primary",
                "7d",
                55.0,
                Some(30_000),
                Some(10_080),
                RateLimitProvenance::Native,
            )
            .unwrap(),
            RateLimitWindow::try_new(
                "secondary",
                "Unknown",
                20.0,
                None,
                None,
                RateLimitProvenance::Native,
            )
            .unwrap(),
        ];
        let mut custom = codexbar_window("scoped-weekly", 56.0);
        custom.resets_at = Some(30_000);
        custom.window_minutes = Some(10_080);
        let mut different_reset = codexbar_window("tertiary", 57.0);
        different_reset.resets_at = Some(30_001);
        different_reset.window_minutes = Some(10_080);
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now,
                vec![custom, different_reset],
            )],
        };

        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].windows.len(), 4);

        let mut incomplete_native = native_quota("codex", now, None, None);
        incomplete_native.windows = vec![RateLimitWindow::try_new(
            "primary",
            "Primary",
            20.0,
            None,
            None,
            RateLimitProvenance::Native,
        )
        .unwrap()];
        let mut incomplete_codexbar = codexbar_window("secondary", 21.0);
        incomplete_codexbar.resets_at = None;
        incomplete_codexbar.window_minutes = None;
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider("codex", now, vec![incomplete_codexbar])],
        };
        let merged = merge_rate_limits(&[incomplete_native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].windows.len(), 2);
    }

    #[test]
    fn structural_alias_matches_are_consumed_one_to_one() {
        let now = 10_000;
        let mut native = native_quota("codex", now, None, None);
        native.windows = ["primary", "secondary"]
            .into_iter()
            .map(|id| {
                RateLimitWindow::try_new(
                    id,
                    id,
                    10.0,
                    Some(30_000),
                    Some(10_080),
                    RateLimitProvenance::Native,
                )
                .unwrap()
            })
            .collect();
        let mut codexbar = codexbar_window("tertiary", 99.0);
        codexbar.resets_at = Some(30_000);
        codexbar.window_minutes = Some(10_080);
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider("codex", now, vec![codexbar])],
        };

        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].windows.len(), 2);
        assert_eq!(
            merged[0]
                .windows
                .iter()
                .filter(|window| window.used_pct == 99.0)
                .count(),
            0
        );
    }

    #[test]
    fn ambiguous_structural_slot_mapping_falls_back_to_unique_ids() {
        let now = 10_000;
        let mut native = native_quota("codex", now, None, None);
        native.windows = vec![
            RateLimitWindow::try_new(
                "primary",
                "Weekly",
                55.0,
                Some(30_000),
                Some(10_080),
                RateLimitProvenance::Native,
            )
            .unwrap(),
            RateLimitWindow::try_new(
                "secondary",
                "Other",
                22.0,
                Some(40_000),
                Some(43_200),
                RateLimitProvenance::Native,
            )
            .unwrap(),
        ];
        let mut shifted = codexbar_window("secondary", 56.0);
        shifted.resets_at = Some(30_000);
        shifted.window_minutes = Some(10_080);
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider("codex", now, vec![shifted])],
        };

        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].windows.len(), 2);
        assert_eq!(
            merged[0]
                .windows
                .iter()
                .map(|window| (window.id.as_str(), window.used_pct))
                .collect::<Vec<_>>(),
            vec![("primary", 55.0), ("secondary", 22.0)]
        );
    }

    #[test]
    fn provider_timestamp_is_the_oldest_complete_selected_window_timestamp() {
        let now = 10_000;
        let stale_native = native_quota("codex", now - QUOTA_FRESH_SECS - 10, Some(12.0), None);
        let fresh_extra = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now,
                vec![codexbar_window("codex-spark-weekly", 1.0)],
            )],
        };

        let merged = merge_rate_limits(&[stale_native], Some(&fresh_extra), now, None, false);
        assert_eq!(merged[0].updated_at, Some(now - QUOTA_FRESH_SECS - 10));

        let mut unknown_native = native_quota("codex", now, Some(12.0), None);
        unknown_native.updated_at = None;
        let merged = merge_rate_limits(&[unknown_native], Some(&fresh_extra), now, None, false);
        assert_eq!(merged[0].updated_at, None);
    }

    #[test]
    fn future_native_timestamp_is_unavailable_for_merge_decisions() {
        let now = 10_000;
        let future_native = native_quota("codex", now + 3_600, Some(12.0), None);
        let stale_codexbar = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now - QUOTA_FRESH_SECS - 1,
                vec![codexbar_window("primary", 91.0)],
            )],
        };

        let merged = merge_rate_limits(
            std::slice::from_ref(&future_native),
            Some(&stale_codexbar),
            now,
            None,
            false,
        );
        assert_eq!(merged[0].five_hour_pct, Some(91.0));
        assert_eq!(
            merged[0].windows[0].provenance,
            RateLimitProvenance::CodexBar
        );

        let native_only = merge_rate_limits(&[future_native], None, now, None, false);
        assert_eq!(native_only[0].updated_at, None);
    }

    #[test]
    fn future_codexbar_timestamp_is_unavailable_for_merge_decisions() {
        let now = 10_000;
        let stale_native = native_quota("codex", now - QUOTA_FRESH_SECS - 1, Some(12.0), None);
        let future_codexbar = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now + 3_600,
                vec![codexbar_window("primary", 91.0)],
            )],
        };

        let merged = merge_rate_limits(
            &[stale_native],
            Some(&future_codexbar),
            now,
            Some(now),
            false,
        );
        assert_eq!(merged[0].five_hour_pct, Some(12.0));
        assert_eq!(merged[0].updated_at, Some(now - QUOTA_FRESH_SECS - 1));

        let codexbar_only = merge_rate_limits(&[], Some(&future_codexbar), now, Some(now), false);
        assert_eq!(codexbar_only[0].updated_at, None);
    }

    #[test]
    fn successful_poll_observation_dates_codexbar_rows_without_provider_timestamp() {
        let now = 10_000;
        let stale_native = native_quota("grok", now - QUOTA_FRESH_SECS - 1, Some(12.0), None);
        let mut provider = codexbar_provider("grok", now, vec![codexbar_window("primary", 91.0)]);
        provider.updated_at = None;
        let snapshot = CodexBarSnapshot {
            providers: vec![provider],
        };

        let merged = merge_rate_limits(&[stale_native], Some(&snapshot), now, Some(now - 1), false);

        assert_eq!(merged[0].five_hour_pct, Some(91.0));
        assert_eq!(merged[0].updated_at, Some(now - 1));
        assert_eq!(
            merged[0].windows[0].provenance,
            RateLimitProvenance::CodexBar
        );
    }

    #[test]
    fn transport_failed_codexbar_cache_is_immediately_stale() {
        let now = 10_000;
        let cached = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "grok",
                now,
                vec![codexbar_window("primary", 18.0)],
            )],
        };

        let merged = merge_rate_limits(&[], Some(&cached), now, Some(now), true);

        assert_eq!(merged[0].five_hour_pct, Some(18.0));
        assert_eq!(
            merged[0].windows[0].provenance,
            RateLimitProvenance::CodexBar
        );
        assert_eq!(merged[0].updated_at, None);
    }

    #[test]
    fn quota_merge_uses_fresh_codexbar_for_a_stale_native_overlap() {
        let now = 10_000;
        let native = native_quota("codex", now - QUOTA_FRESH_SECS - 1, Some(12.0), None);
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now - 1,
                vec![codexbar_window("primary", 91.0)],
            )],
        };

        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].five_hour_pct, Some(91.0));
        assert_eq!(
            merged[0].windows[0].provenance,
            RateLimitProvenance::CodexBar
        );
    }

    #[test]
    fn quota_merge_uses_the_newer_sample_when_both_overlaps_are_stale() {
        let now = 10_000;
        let native = native_quota("codex", now - QUOTA_FRESH_SECS - 20, Some(12.0), None);
        let snapshot = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now - QUOTA_FRESH_SECS - 10,
                vec![codexbar_window("primary", 91.0)],
            )],
        };

        let merged = merge_rate_limits(&[native], Some(&snapshot), now, None, false);
        assert_eq!(merged[0].five_hour_pct, Some(91.0));

        let newer_native = native_quota("codex", now - QUOTA_FRESH_SECS - 5, Some(23.0), None);
        let older_codexbar = CodexBarSnapshot {
            providers: vec![codexbar_provider(
                "codex",
                now - QUOTA_FRESH_SECS - 15,
                vec![codexbar_window("primary", 82.0)],
            )],
        };
        let merged = merge_rate_limits(&[newer_native], Some(&older_codexbar), now, None, false);
        assert_eq!(merged[0].five_hour_pct, Some(23.0));
        assert_eq!(merged[0].windows[0].provenance, RateLimitProvenance::Native);
    }

    #[test]
    fn codexbar_provider_error_does_not_hide_native_quota() {
        let now = 10_000;
        let snapshot = CodexBarSnapshot {
            providers: vec![
                CodexBarProviderSnapshot {
                    provider: "codex".to_string(),
                    windows: Vec::new(),
                    updated_at: None,
                    error: Some(crate::collector::codexbar::CodexBarProviderError::Unavailable),
                },
                CodexBarProviderSnapshot {
                    provider: "kimi".to_string(),
                    windows: Vec::new(),
                    updated_at: None,
                    error: Some(crate::collector::codexbar::CodexBarProviderError::Unavailable),
                },
            ],
        };

        let merged = merge_rate_limits(
            &[native_quota("codex", now, Some(12.0), None)],
            Some(&snapshot),
            now,
            None,
            false,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, "codex");
        assert_eq!(merged[0].five_hour_pct, Some(12.0));
    }

    #[test]
    fn quota_providers_have_a_stable_preferred_then_lexical_order() {
        let now = 10_000;
        let providers = ["zeta", "kimi", "grok", "codex", "claude", "alpha"]
            .into_iter()
            .map(|provider| codexbar_provider(provider, now, vec![codexbar_window("primary", 1.0)]))
            .collect();
        let merged =
            merge_rate_limits(&[], Some(&CodexBarSnapshot { providers }), now, None, false);

        assert_eq!(
            merged
                .iter()
                .map(|rate_limit| rate_limit.source.as_str())
                .collect::<Vec<_>>(),
            vec!["claude", "codex", "grok", "kimi", "alpha", "zeta"]
        );
    }

    #[test]
    fn quota_provider_ids_are_strict_content_free_ascii_identifiers() {
        assert_eq!(
            canonical_provider_id("Claude-Code_1.2"),
            Some("claude-code_1.2".into())
        );
        for invalid in [
            "private@example.com",
            " claude",
            "claude ",
            "claude\n",
            "grok\u{202e}",
        ] {
            assert_eq!(canonical_provider_id(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn session_visibility_does_not_disable_codexbar_quota() {
        for hidden in [
            vec!["codex".to_string()],
            vec!["grok".to_string(), "kimi".to_string()],
            vec!["codex".to_string(), "grok".to_string(), "kimi".to_string()],
        ] {
            let app = App::new_with_config_and_claude_dirs_and_codexbar(
                Theme::default(),
                &hidden,
                crate::config::PanelVisibility::default(),
                &[],
                true,
            );

            assert!(app.codexbar_quota_fallback, "{hidden:?}");
            assert_eq!(
                app.codexbar_quota_status().state,
                crate::collector::codexbar::CodexBarQuotaState::Unavailable,
                "{hidden:?}"
            );
        }
    }

    fn waiting_session(cli: &'static str) -> AgentSession {
        AgentSession {
            agent_cli: cli,
            pid: 1,
            action_process_incarnation: Some("process-1".to_string()),
            session_id: String::new(),
            cwd: String::new(),
            project_name: String::new(),
            started_at: 0,
            status: SessionStatus::Waiting,
            status_evidence: StatusEvidence::default(),
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
            awaiting_input: false,
            thinking_since_ms: 0,
            file_accesses: vec![],
            config_root: String::new(),
            git_added: 0,
            git_modified: 0,
        }
    }

    fn idle_session(cli: &'static str) -> AgentSession {
        let mut session = waiting_session(cli);
        session.status = SessionStatus::Idle;
        session
    }

    #[test]
    fn reconciliation_enforces_waiting_contract_and_samples_heuristics() {
        let mut waiting = waiting_session("claude");
        waiting.awaiting_input = false;
        let mut idle = idle_session("codex");
        idle.awaiting_input = true;
        let mut sessions = vec![waiting, idle];

        reconcile_status_evidence(&mut sessions, &HashMap::new(), 100);

        assert!(sessions[0].awaiting_input);
        assert!(!sessions[1].awaiting_input);
        for session in sessions {
            assert_eq!(
                session.status_evidence.authority,
                StatusAuthority::Heuristic
            );
            assert_eq!(
                session.status_evidence.reason,
                StatusReason::CollectorInference
            );
            assert_eq!(session.status_evidence.observations.len(), 1);
        }
    }

    #[test]
    fn reconciliation_marks_unknown_evidence_unavailable() {
        let mut unknown = waiting_session("kimi");
        unknown.status = SessionStatus::Unknown;
        let mut sessions = vec![unknown];

        reconcile_status_evidence(&mut sessions, &HashMap::new(), 100);

        assert_eq!(
            sessions[0].status_evidence.authority,
            StatusAuthority::Unavailable
        );
        assert_eq!(
            sessions[0].status_evidence.reason,
            StatusReason::OwnershipUnconfirmed
        );
        assert!(!sessions[0].awaiting_input);
    }

    #[test]
    fn reconciliation_preserves_history_across_ticks() {
        let mut first = vec![idle_session("codex")];
        first[0].session_id = "stable".into();
        first[0].action_process_incarnation = Some("process-a".into());
        let previous = reconcile_status_evidence(&mut first, &HashMap::new(), 100);

        let mut second = vec![idle_session("codex")];
        second[0].session_id = "stable".into();
        second[0].action_process_incarnation = Some("process-a".into());
        reconcile_status_evidence(&mut second, &previous, 200);

        assert_eq!(second[0].status_evidence.observations.len(), 2);
        assert_eq!(second[0].status_evidence.consecutive_matching, 2);
        assert_eq!(second[0].status_evidence.status_since_ms, 100);
    }

    #[test]
    fn reconciliation_accumulates_source_qualified_incremental_samples() {
        let mut first = idle_session("claude");
        first.session_id = "provider-state".into();
        first.status_evidence.observe(StatusObservation::new(
            SessionStatus::Idle,
            StatusAuthority::Provider,
            StatusReason::ProviderIdle,
            100,
            0,
        ));
        first.action_process_incarnation = Some("process-a".into());
        let previous =
            reconcile_status_evidence(std::slice::from_mut(&mut first), &HashMap::new(), 100);

        let mut second = idle_session("claude");
        second.session_id = "provider-state".into();
        second.status_evidence.observe(StatusObservation::new(
            SessionStatus::Idle,
            StatusAuthority::Provider,
            StatusReason::ProviderIdle,
            200,
            0,
        ));
        second.action_process_incarnation = Some("process-a".into());
        let duplicate_previous =
            reconcile_status_evidence(std::slice::from_mut(&mut second), &previous, 200);

        assert_eq!(second.status_evidence.observations.len(), 2);
        assert_eq!(second.status_evidence.consecutive_matching, 2);
        assert_eq!(second.status_evidence.status_since_ms, 100);

        let mut duplicate = idle_session("claude");
        duplicate.session_id = "provider-state".into();
        duplicate.status_evidence.observe(StatusObservation::new(
            SessionStatus::Idle,
            StatusAuthority::Provider,
            StatusReason::ProviderIdle,
            200,
            0,
        ));
        duplicate.action_process_incarnation = Some("process-a".into());
        reconcile_status_evidence(
            std::slice::from_mut(&mut duplicate),
            &duplicate_previous,
            250,
        );
        assert_eq!(duplicate.status_evidence.consecutive_matching, 2);
        assert_eq!(duplicate.status_evidence.status_since_ms, 100);
        assert_eq!(duplicate.status_evidence.observations.len(), 2);
    }

    #[test]
    fn reconciliation_never_inherits_across_process_incarnations() {
        let mut first = idle_session("codex");
        first.session_id = "reused".into();
        first.action_process_incarnation = Some("process-a".into());
        let previous =
            reconcile_status_evidence(std::slice::from_mut(&mut first), &HashMap::new(), 100);

        let mut reused = idle_session("codex");
        reused.session_id = "reused".into();
        reused.action_process_incarnation = Some("process-b".into());
        let current = reconcile_status_evidence(std::slice::from_mut(&mut reused), &previous, 200);

        assert_eq!(reused.status_evidence.observations.len(), 1);
        assert_eq!(reused.status_evidence.consecutive_matching, 1);
        assert_eq!(reused.status_evidence.status_since_ms, 200);
        assert_eq!(current.len(), 1);
        assert!(current
            .keys()
            .all(|key| key.process_incarnation == "process-b"));
    }

    #[test]
    fn evidence_key_uses_the_collector_bound_incarnation_after_pid_reuse() {
        let mut session = idle_session("codex");
        session.pid = 42;
        session.session_id = "logical-a".into();
        session.action_process_incarnation = Some("process-a".into());

        // Process A produced this row and then exited. Even if a fresh OS
        // lookup would now observe process B at the same PID, status history
        // stays keyed to the collector-bound A identity. The next collector
        // pass may publish B as a different key, but App must not synthesize it.
        let post_collection_pid_owner = "process-b";
        let key = status_evidence_key(&session).expect("row has an exact identity");

        assert_eq!(key.process_incarnation, "process-a");
        assert_ne!(key.process_incarnation, post_collection_pid_owner);
    }

    #[test]
    fn reconciliation_requires_provider_session_and_pid_to_match() {
        let mut first = idle_session("codex");
        first.session_id = "logical-a".into();
        let previous =
            reconcile_status_evidence(std::slice::from_mut(&mut first), &HashMap::new(), 100);

        let mut changed_provider = idle_session("claude");
        changed_provider.session_id = "logical-a".into();
        let mut changed_session = idle_session("codex");
        changed_session.session_id = "logical-b".into();
        let mut changed_pid = idle_session("codex");
        changed_pid.session_id = "logical-a".into();
        changed_pid.pid = 2;
        let mut sessions = vec![changed_provider, changed_session, changed_pid];

        reconcile_status_evidence(&mut sessions, &previous, 200);

        assert!(sessions.iter().all(|session| {
            session.status_evidence.observations.len() == 1
                && session.status_evidence.status_since_ms == 200
                && session.status_evidence.consecutive_matching == 1
        }));
    }

    #[test]
    fn reconciliation_without_exact_incarnation_fails_closed() {
        let mut first = idle_session("codex");
        first.session_id = "unqueryable".into();
        first.action_process_incarnation = None;
        let first_ledger =
            reconcile_status_evidence(std::slice::from_mut(&mut first), &HashMap::new(), 100);
        assert!(first_ledger.is_empty());

        let mut second = idle_session("codex");
        second.session_id = "unqueryable".into();
        second.action_process_incarnation = None;
        reconcile_status_evidence(std::slice::from_mut(&mut second), &first_ledger, 200);

        assert_eq!(second.status_evidence.observations.len(), 1);
        assert_eq!(second.status_evidence.status_since_ms, 200);

        let mut unidentified = idle_session("codex");
        let unidentified_ledger = reconcile_status_evidence(
            std::slice::from_mut(&mut unidentified),
            &HashMap::new(),
            300,
        );
        assert!(unidentified_ledger.is_empty());
    }

    #[test]
    fn reconciliation_preserves_newer_explicit_provider_status_since() {
        let mut first = idle_session("claude");
        first.session_id = "provider-transition".into();
        first.status_evidence.observe(StatusObservation::new(
            SessionStatus::Idle,
            StatusAuthority::Provider,
            StatusReason::ProviderIdle,
            100,
            7,
        ));
        let previous =
            reconcile_status_evidence(std::slice::from_mut(&mut first), &HashMap::new(), 100);

        let mut current = idle_session("claude");
        current.session_id = "provider-transition".into();
        current.status_evidence.observe(StatusObservation::new(
            SessionStatus::Idle,
            StatusAuthority::Provider,
            StatusReason::ProviderIdle,
            300,
            7,
        ));
        current.status_evidence.status_since_ms = 250;
        reconcile_status_evidence(std::slice::from_mut(&mut current), &previous, 300);

        assert_eq!(current.status_evidence.status_since_ms, 250);
        assert_eq!(current.status_evidence.consecutive_matching, 2);
        assert_eq!(current.status_evidence.observations.len(), 2);
    }

    #[test]
    fn supported_agent_command_accepts_opencode() {
        assert!(is_supported_agent_command("/usr/local/bin/claude"));
        assert!(is_supported_agent_command("codex --resume abc"));
        assert!(is_supported_agent_command("/opt/homebrew/bin/opencode"));
        assert!(is_supported_agent_command("/usr/local/bin/grok"));
        assert!(is_supported_agent_command("xai-grok-pager --resume abc"));
        assert!(is_supported_agent_command("kimi-code"));
        assert!(is_supported_agent_command(
            "node /opt/node_modules/@moonshot-ai/kimi-code/dist/main.mjs"
        ));
        assert!(!is_supported_agent_command("node server.js"));
    }

    #[test]
    fn killable_agent_command_rejects_codex_app_server() {
        assert!(is_killable_agent_command("codex --resume abc"));
        assert!(is_killable_agent_command("/usr/local/bin/claude"));
        assert!(!is_killable_agent_command(
            "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled"
        ));
    }

    #[test]
    fn grok_command_distinguishes_host_modes_from_prompt_text() {
        assert!(is_grok_agent_command("grok"));
        assert!(is_grok_agent_command("/usr/local/bin/grok -p hello"));
        assert!(is_grok_agent_command("~/.grok/bin/grok-1.2.3 -p hello"));
        assert!(is_grok_agent_command(
            "\"/Applications/Grok Build/grok\" -p hello"
        ));
        assert!(is_grok_agent_command("xai-grok-pager --resume abc"));
        assert!(is_grok_agent_command("grok -p agent leader"));
        assert!(is_grok_agent_command("grok fix agent leader handling"));
        assert!(!is_grok_agent_command("grok agent leader"));
        assert!(!is_grok_agent_command(
            "/usr/local/bin/grok --debug agent --model grok-code leader"
        ));
        assert!(!is_grok_agent_command(
            "cat /Users/test/.grok/bin/grok-1.2.3"
        ));
        assert!(!is_grok_agent_command("\"/tmp/not grok\""));
        assert!(!is_killable_agent_command_for_provider(
            "grok",
            "grok agent leader"
        ));
    }

    #[test]
    fn kimi_command_rejects_non_session_hosts_and_plugin_helper() {
        assert!(is_kimi_agent_command("kimi"));
        assert!(is_kimi_agent_command("kimi-code --session abc"));
        assert!(is_kimi_agent_command("kimi -p build web UI"));
        assert!(is_kimi_agent_command("kimi \"--prompt=build web UI\""));
        assert!(is_kimi_agent_command("kimi -S web"));
        assert!(is_kimi_agent_command(
            "node /opt/node_modules/@moonshot-ai/kimi-code/dist/main.mjs"
        ));
        assert!(!is_kimi_agent_command("kimi web"));
        assert!(!is_kimi_agent_command("kimi --verbose web"));
        assert!(!is_kimi_agent_command("kimi acp"));
        assert!(!is_kimi_agent_command("kimi server"));
        assert!(!is_kimi_agent_command("kimi vis session-id"));
        assert!(!is_kimi_agent_command("kimi __plugin_run_node"));
        assert!(!is_kimi_agent_command(
            "node /opt/node_modules/@moonshot-ai/kimi-code/dist/main.mjs web"
        ));
        assert!(!is_kimi_agent_command(
            "cat /opt/node_modules/@moonshot-ai/kimi-code/dist/main.mjs"
        ));
        assert!(!is_kimi_agent_command(
            "node server.js --help-text=@moonshot-ai/kimi-code/dist/main.mjs"
        ));
        assert!(!is_kimi_agent_command("\"/tmp/not kimi\""));
    }

    #[test]
    fn kill_validation_requires_the_selected_provider() {
        assert!(is_killable_agent_command_for_provider(
            "grok",
            "grok --resume abc"
        ));
        assert!(!is_killable_agent_command_for_provider(
            "claude",
            "grok --resume abc"
        ));
        assert!(!is_killable_agent_command_for_provider("kimi", "kimi web"));
    }

    #[test]
    fn session_filter_matches_provider_name() {
        let session = waiting_session("grok");
        assert!(App::session_matches(&session, "grok"));
        assert!(!App::session_matches(&session, "kimi"));
    }

    #[test]
    fn session_filter_matches_idle_status() {
        let session = idle_session("codex");
        assert!(App::session_matches(&session, "idle"));
        assert!(!App::session_matches(&session, "waiting"));
    }

    #[test]
    fn shared_grok_process_confirmation_warns_about_all_sessions() {
        assert_eq!(
            kill_confirmation_message("grok", 42, "session", 3),
            "Press x again to kill PID 42; affects 3 Grok sessions"
        );
        assert_eq!(
            kill_confirmation_message("kimi", 42, "session", 1),
            "Press x again to kill: session"
        );
    }

    #[test]
    fn process_incarnation_requires_two_matching_known_exact_identities() {
        assert!(process_incarnation_matches(
            Some("linux:boot-id:10"),
            Some("linux:boot-id:10")
        ));
        assert!(!process_incarnation_matches(
            Some("linux:boot-id:10"),
            Some("linux:boot-id:11")
        ));
        assert!(!process_incarnation_matches(Some("linux:boot-id:10"), None));
        assert!(!process_incarnation_matches(None, Some("linux:boot-id:10")));
        assert!(!process_incarnation_matches(None, None));
    }

    #[test]
    fn pid_action_validation_brackets_the_command_with_exact_identity() {
        assert!(action_process_observation_is_valid(
            "claude",
            Some("process-a"),
            Some("process-a"),
            Some("process-a"),
            Some("/usr/local/bin/claude"),
            false,
        ));
        assert!(!action_process_observation_is_valid(
            "claude",
            None,
            Some("process-a"),
            Some("process-a"),
            Some("/usr/local/bin/claude"),
            false,
        ));
        assert!(!action_process_observation_is_valid(
            "claude",
            Some("process-a"),
            Some("process-a"),
            Some("process-b"),
            Some("/usr/local/bin/claude"),
            false,
        ));
        assert!(!action_process_observation_is_valid(
            "claude",
            Some("process-a"),
            Some("process-a"),
            Some("process-a"),
            Some("/usr/local/bin/grok"),
            false,
        ));
        assert!(!action_process_observation_is_valid(
            "grok",
            Some("process-a"),
            Some("process-a"),
            Some("process-a"),
            Some("/usr/local/bin/grok"),
            true,
        ));
    }

    #[test]
    fn action_validation_requires_exact_provider_argv_and_session_role() {
        assert!(exact_action_argv_is_valid(
            "claude",
            &["/usr/local/bin/claude".to_string()],
        ));
        assert!(!exact_action_argv_is_valid(
            "claude",
            &["/usr/local/bin/codex".to_string()],
        ));
        assert!(!exact_action_argv_is_valid(
            "codex",
            &["/usr/local/bin/codex".to_string(), "app-server".to_string(),],
        ));
        assert!(!exact_action_argv_is_valid(
            "grok",
            &[
                "/usr/local/bin/grok".to_string(),
                "agent".to_string(),
                "leader".to_string(),
            ],
        ));
        assert!(!exact_action_argv_is_valid(
            "kimi",
            &["/usr/local/bin/kimi-code".to_string(), "web".to_string()],
        ));
    }

    #[test]
    fn row_bound_action_anchor_rejects_reuse_before_app_resample() {
        let mut session = waiting_session("codex");
        session.pid = 42;
        session.session_id = "logical-a".into();
        session.action_process_incarnation = Some("process-a".into());

        // Process A produced the row, then exited. A post-collection PID
        // resample would now see a different Codex process B at PID 42.
        let post_collection_resample = "process-b";
        let confirmation = KillConfirmation::for_session(
            &session,
            std::slice::from_ref(&session),
            session.action_process_incarnation.clone().unwrap(),
        );
        assert_eq!(confirmation.process_incarnation, "process-a");
        assert!(!action_process_observation_is_valid(
            "codex",
            Some(&confirmation.process_incarnation),
            Some(post_collection_resample),
            Some(post_collection_resample),
            Some("/usr/local/bin/codex"),
            false,
        ));
    }

    #[test]
    fn fallback_summaries_remove_terminal_controls_and_secrets() {
        let summary = sanitize_fallback("\u{202e}fix sk-ant-secret now\n", 80);
        assert!(!summary.contains('\u{202e}'));
        assert!(!summary.contains("sk-ant-secret"));
        assert!(summary.contains("[REDACTED]"));
    }

    #[test]
    fn grok_confirmation_revalidates_the_shared_session_set() {
        let mut first = waiting_session("grok");
        first.pid = 42;
        first.session_id = "first".into();
        first.action_process_incarnation = Some("linux:boot-id:10".into());
        let mut second = waiting_session("grok");
        second.pid = 42;
        second.session_id = "second".into();
        second.action_process_incarnation = Some("linux:boot-id:10".into());
        let confirmation = KillConfirmation {
            provider: "grok".into(),
            session_id: "first".into(),
            pid: 42,
            process_incarnation: "linux:boot-id:10".into(),
            grok_session_ids: vec!["first".into(), "second".into()],
            requested_at: Instant::now(),
        };
        assert!(confirmation.matches(&first, &[second.clone(), first.clone()]));
        assert!(!confirmation.matches(&second, &[first.clone(), second.clone()]));

        let mut wrong_provider = first.clone();
        wrong_provider.agent_cli = "kimi";
        assert!(!confirmation.matches(&wrong_provider, &[first.clone(), second.clone()]));

        let mut wrong_pid = first.clone();
        wrong_pid.pid = 43;
        assert!(!confirmation.matches(&wrong_pid, &[first.clone(), second.clone()]));

        let mut wrong_incarnation = first.clone();
        wrong_incarnation.action_process_incarnation = Some("linux:boot-id:11".into());
        assert!(!confirmation.matches(&wrong_incarnation, &[first.clone(), second.clone()]));

        let mut added = waiting_session("grok");
        added.pid = 42;
        added.session_id = "added".into();
        assert!(!confirmation.matches(&first, &[first.clone(), second, added]));
        assert!(!confirmation.matches(&first, &[first.clone()]));
    }

    #[test]
    fn termination_failures_use_safe_nonempty_details() {
        assert_eq!(
            termination_failure_message(42, "permission denied\n", "", "exit status: 1"),
            "Failed to kill PID 42: permission denied"
        );
        assert_eq!(
            termination_failure_message(42, "", "taskkill failed\n", "exit status: 1"),
            "Failed to kill PID 42: taskkill failed"
        );
        let escaped = termination_failure_message(42, "\u{1b}[31mnope", "", "");
        assert!(!escaped.contains('\u{1b}'));
    }

    #[test]
    fn orphan_kill_status_reports_successes_and_failures() {
        assert_eq!(
            orphan_kill_status(0, &[]),
            "No orphan processes needed killing"
        );
        assert_eq!(orphan_kill_status(2, &[]), "Killed 2 orphan process(es)");
        assert_eq!(
            orphan_kill_status(0, &["identity changed".into()]),
            "identity changed"
        );
        assert_eq!(
            orphan_kill_status(1, &["permission denied".into()]),
            "Killed 1; 1 orphan failure(s): permission denied"
        );
    }

    #[test]
    fn exact_semantic_focus_precedes_the_non_actionable_status_guard() {
        for (provider, status) in [
            ("codex", SessionStatus::Working),
            ("kimi", SessionStatus::Unknown),
            ("kimi", SessionStatus::Done),
        ] {
            let mut app = App::new_with_config(
                Theme::default(),
                &[],
                crate::config::PanelVisibility::default(),
            );
            let mut session = waiting_session(provider);
            session.status = status;
            session.session_id = "session-1".to_string();
            app.sessions.push(session);

            assert_eq!(
                app.jump_to_session_with(
                    |actual_provider, session_id| {
                        assert_eq!(actual_provider, provider);
                        assert_eq!(session_id, "session-1");
                        JumpOutcome::Jumped
                    },
                    |_| panic!("PID fallback must not run after semantic focus")
                ),
                JumpOutcome::Jumped
            );
        }
    }

    #[test]
    fn unknown_row_without_semantic_identity_cannot_use_pid_fallback() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let mut session = waiting_session("kimi");
        session.status = SessionStatus::Unknown;
        app.sessions.push(session);

        assert_eq!(
            app.jump_to_session_with(
                |_, _| JumpOutcome::NoOp,
                |_| panic!("non-actionable row must not reach PID fallback")
            ),
            JumpOutcome::NoOp
        );
    }

    #[test]
    fn working_row_is_never_pid_actionable() {
        let mut session = waiting_session("codex");
        session.status = SessionStatus::Working;
        session.status_evidence.authority = StatusAuthority::Provider;

        assert!(!session_process_is_actionable(&session));
    }

    #[test]
    fn actionable_jump_without_a_polled_incarnation_fails_closed() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        let mut session = waiting_session("claude");
        session.status_evidence.authority = StatusAuthority::Provider;
        app.sessions.push(session);

        assert!(matches!(
            app.jump_to_session_with(
                |_, _| JumpOutcome::NoOp,
                |_| panic!("failed process validation must stop before PID jump")
            ),
            JumpOutcome::Failed(message)
                if message == "PID 1 is no longer the selected agent process"
        ));
    }

    #[test]
    fn kimi_pid_actions_require_provider_ownership() {
        let mut session = waiting_session("kimi");
        session.status_evidence.authority = StatusAuthority::Heuristic;
        assert!(!session_process_is_actionable(&session));

        session.status_evidence.authority = StatusAuthority::Unavailable;
        assert!(!session_process_is_actionable(&session));

        session.status_evidence.authority = StatusAuthority::Provider;
        assert!(session_process_is_actionable(&session));

        session.status = SessionStatus::Unknown;
        assert!(!session_process_is_actionable(&session));
    }

    #[test]
    fn every_provider_action_requires_a_row_bound_process_anchor() {
        for provider in ["claude", "codex", "opencode", "grok", "kimi"] {
            let mut session = waiting_session(provider);
            session.status_evidence.authority = StatusAuthority::Provider;
            assert!(session_process_is_actionable(&session), "{provider}");

            session.status_evidence.authority = StatusAuthority::Heuristic;
            assert_eq!(
                session_process_is_actionable(&session),
                provider != "kimi",
                "{provider}"
            );

            session.status_evidence.authority = StatusAuthority::Unavailable;
            assert!(!session_process_is_actionable(&session), "{provider}");

            session.status_evidence.authority = StatusAuthority::Provider;
            session.action_process_incarnation = None;
            assert!(!session_process_is_actionable(&session), "{provider}");
        }

        let mut zero_pid = waiting_session("codex");
        zero_pid.pid = 0;
        assert!(!session_process_is_actionable(&zero_pid));
    }
}
