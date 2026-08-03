//! Serializable snapshot of live monitor state for the JSON / Web API.
//!
//! Builds an owned, JSON-friendly view from an [`App`] so headless consumers
//! (e.g. a web server) can serialize the same data the TUI renders without
//! depending on ratatui. The list fields stay lean; a bounded tail of the
//! richer per-session fields (token history, recent tool calls, chat tail,
//! subagents) is also included for the detail view. The unbounded file-access
//! audit and full transcripts are still omitted to keep the payload small.
//!
//! This is a pure read: [`App::to_snapshot`] never ticks or spawns anything.
//! Call it after [`App::tick_no_summaries`] (or `tick`) on a background thread.

use crate::app::App;
use crate::collector::codexbar::{
    canonical_provider_id, CodexBarPollError, CodexBarProviderSnapshot, CodexBarQuotaState,
};
use crate::collector::mcp::ACTIVE_MTIME_SECS;
use crate::host_info::{AgentAggregate, HostMetrics};
use crate::model::{
    ChatRole, ChildProcess, OrphanPort, RateLimitInfo, RateLimitProvenance, SessionStatus,
    StatusEvidence, MAX_CHAT_MESSAGES, MAX_VISIBLE_STATUS_OBSERVATIONS,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

const QUOTA_FRESH_SECS: u64 = 600;

/// Top-level snapshot returned by [`App::to_snapshot`].
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    /// Unix-epoch milliseconds when this snapshot was built.
    pub generated_at_ms: u64,
    /// Host vitals (CPU / mem / load1). `None` on unsupported platforms or
    /// before the first valid sample.
    pub host: Option<HostMetrics>,
    /// Aggregate metrics across all sessions.
    pub aggregate: AgentAggregate,
    /// Most recent per-tick token rate: the delta of *active* tokens, where
    /// active = input + output + cache_create (cache_read is excluded to avoid
    /// inflated rates). It therefore will NOT equal successive `total_tokens`
    /// diffs (which include cache_read). `0.0` on the first tick of a fresh
    /// process (no prior totals to diff against).
    pub token_rate: f64,
    /// Collector tick interval in milliseconds. Divide `token_rate` by
    /// `interval_ms / 1000` for a per-second rate.
    pub interval_ms: u64,
    /// Live agent sessions, newest first (same order as the TUI).
    pub sessions: Vec<SessionView>,
    /// Account-level rate limits from native integrations and every provider
    /// returned by the optional CodexBar integration.
    pub rate_limits: Vec<RateLimitInfo>,
    /// Sanitized state and source provenance for the opt-in CodexBar quotas.
    pub codexbar_quota: CodexBarQuotaView,
    /// Ports left open by processes whose parent session has ended. Empty on a
    /// one-shot snapshot — orphan detection needs cross-tick history, so it
    /// only populates for a long-running monitor.
    pub orphan_ports: Vec<OrphanPort>,
    /// Detected MCP servers (currently `codex mcp-server`).
    pub mcp_servers: Vec<McpServerView>,
}

/// Content-free diagnostics for the optional CodexBar quota integration.
#[derive(Debug, Clone, Serialize)]
pub struct CodexBarQuotaView {
    /// Persisted user preference. Session-collector visibility does not disable
    /// this independent account-level integration.
    pub enabled: bool,
    /// One of `off`, `checking`, `active`, `active_stale`, `partial`, or
    /// `unavailable`.
    pub state: &'static str,
    /// Backward-compatible summary of the selected Codex row: `native`,
    /// `codexbar`, or `mixed`. Use `providers` for every provider.
    pub provenance: Option<&'static str>,
    /// Backward-compatible staleness summary for the selected Codex row.
    pub stale: bool,
    /// Unix-epoch seconds of the last completed CodexBar check.
    pub last_checked_at: Option<u64>,
    /// Fixed, content-free failure category for the latest check.
    pub error: Option<&'static str>,
    /// Bounded provider diagnostics in stable display order.
    pub providers: Vec<CodexBarProviderView>,
}

/// Content-free diagnostic for one account-quota provider.
#[derive(Debug, Clone, Serialize)]
pub struct CodexBarProviderView {
    /// Canonical lowercase provider ID.
    pub provider: String,
    /// One of `active`, `active_stale`, or `unavailable`.
    pub state: &'static str,
    /// Selected window sources: `native`, `codexbar`, or `mixed`.
    pub provenance: Option<&'static str>,
    /// Whether the selected provider sample is older than ten minutes.
    pub stale: bool,
    /// Fixed failure category. Provider failures serialize as `provider_error`.
    pub error: Option<&'static str>,
}

/// One chat line from the transcript tail (detail view only).
#[derive(Debug, Clone, Serialize)]
pub struct ChatMsgView {
    /// Speaker: the string `"user"` or `"assistant"` (stable wire values).
    pub role: &'static str,
    /// Redacted message text (tool inputs/results are excluded upstream).
    pub text: String,
}

/// One tool invocation (detail view only).
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallView {
    /// Tool name, e.g. `"Read"`, `"Edit"`, `"Bash"`, `"Grep"`, `"Agent"`.
    pub name: String,
    /// Short argument preview (file path, command prefix, or pattern).
    pub arg: String,
    /// Observed duration in milliseconds; `0` when unknown.
    pub duration_ms: u64,
}

/// One spawned subagent (detail view only).
#[derive(Debug, Clone, Serialize)]
pub struct SubAgentView {
    /// Subagent name/label.
    pub name: String,
    /// Free-text status reported for the subagent (e.g. `"working"`, `"done"`).
    pub status: String,
    /// Tokens attributed to this subagent.
    pub tokens: u64,
}

/// A single session, flattened and curated for JSON consumers.
#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    /// Owning CLI: "claude", "codex", "opencode", "grok", or "kimi".
    pub agent_cli: &'static str,
    /// OS process id of the agent CLI for this session.
    pub pid: u32,
    /// Agent-assigned session identifier (stable for the life of the session).
    pub session_id: String,
    /// Project / workspace name (usually the basename of `cwd`).
    pub project_name: String,
    /// Absolute working directory of the session.
    pub cwd: String,
    /// Home-abbreviated config root (e.g. "~/.claude", "~/.codex").
    pub config_root: String,
    /// Coarse activity state; serializes as its variant name (e.g. `"Thinking"`).
    pub status: SessionStatus,
    /// Provenance, freshness, and the latest five content-free status samples.
    pub status_evidence: StatusEvidence,
    /// Whether the session is blocked on a response from the user.
    pub awaiting_input: bool,
    /// Model identifier reported by the session (e.g. `"claude-opus-4-6"`).
    pub model: String,
    /// Reasoning effort reported by the provider; empty when unavailable.
    pub effort: String,
    /// Agent CLI version string, if known.
    pub version: String,
    /// Context-window fill, 0.0–100.0 percent.
    pub context_percent: f64,
    /// Total context-window size in tokens (e.g. 200000).
    pub context_window: u64,
    /// All token classes summed: input + output + cache read + cache write.
    pub total_tokens: u64,
    /// Cumulative input (prompt) tokens for the session.
    pub input_tokens: u64,
    /// Cumulative output (completion) tokens for the session.
    pub output_tokens: u64,
    /// Cumulative cache-read tokens (excluded from the active-token rate).
    pub cache_read_tokens: u64,
    /// Cumulative cache-write (cache-creation) tokens.
    pub cache_create_tokens: u64,
    /// Number of user/assistant turns observed.
    pub turn_count: u32,
    /// Resident memory of the session process tree, in MiB.
    pub mem_mb: u64,
    /// Current git branch of `cwd`, or empty when not a repo.
    pub git_branch: String,
    /// Files added in the working tree (git status), not session-scoped.
    pub git_added: u32,
    /// Files modified in the working tree (git status), not session-scoped.
    pub git_modified: u32,
    /// Session start, Unix-epoch milliseconds.
    pub started_at_ms: u64,
    /// Wall-clock seconds since `started_at_ms`.
    pub elapsed_secs: u64,
    /// Display summary: cached LLM title if present, else a safe raw-prompt
    /// fallback. Never triggers summary generation.
    pub summary: String,
    /// Most recent current-task line, if any.
    pub current_task: Option<String>,
    /// Child processes, each with any owned listening port.
    pub children: Vec<ChildProcess>,
    // --- richer fields for the per-session detail view ---
    /// Number of detected context-compaction events.
    pub compaction_count: u32,
    /// Per-turn token totals for a sparkline (trimmed tail). The absolute scale
    /// differs by agent (Claude counts cache tokens, Codex does not), so use it
    /// as a relative per-session trend, not for cross-session magnitude.
    pub token_history: Vec<u64>,
    /// Spawned subagents, if any.
    pub subagents: Vec<SubAgentView>,
    /// Recent tool-call timeline (trimmed tail, newest last).
    pub tool_calls: Vec<ToolCallView>,
    /// Recent chat transcript tail (user/assistant only).
    pub chat_messages: Vec<ChatMsgView>,
}

/// A detected MCP server, with the internal `SystemTime` mtime resolved to a
/// plain epoch-millis number for web clients.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerView {
    /// OS process id of the MCP server.
    pub pid: u32,
    /// Resolved parent CLI: "claude", "codex", or "?".
    pub parent_cli: &'static str,
    /// `-c profile=<name>` value, if any.
    pub profile: Option<String>,
    /// Resident memory of the MCP server process, in KiB.
    pub mem_kb: u64,
    /// Rollouts written within the active-mtime window.
    pub active_count: usize,
    /// Total open rollout fds.
    pub rollout_count: usize,
    /// Latest rollout mtime as Unix-epoch milliseconds, if known.
    pub last_activity_ms: Option<u64>,
}

/// Keep at most the last `n` items of a slice.
fn tail<T: Clone>(v: &[T], n: usize) -> Vec<T> {
    if v.len() > n {
        v[v.len() - n..].to_vec()
    } else {
        v.to_vec()
    }
}

fn epoch_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn epoch_secs(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

#[derive(Default)]
struct ProviderDiagnostic {
    provider: String,
    has_quota: bool,
    native: bool,
    codexbar: bool,
    updated_at: Option<u64>,
    provider_error: bool,
}

fn provider_sort_key(provider: &str) -> (u8, &str) {
    let rank = match provider {
        "claude" => 0,
        "codex" => 1,
        "grok" => 2,
        "kimi" => 3,
        _ => 4,
    };
    (rank, provider)
}

fn provider_diagnostics(
    rate_limits: &[RateLimitInfo],
    codexbar: &[CodexBarProviderSnapshot],
    now_secs: u64,
    codexbar_transport_failed: bool,
) -> Vec<CodexBarProviderView> {
    let mut diagnostics = BTreeMap::<String, ProviderDiagnostic>::new();

    for rate_limit in rate_limits {
        let Some(provider) = canonical_provider_id(&rate_limit.source) else {
            continue;
        };
        let has_legacy_window = rate_limit.five_hour_pct.is_some()
            || rate_limit.seven_day_pct.is_some()
            || rate_limit.five_hour_resets_at.is_some()
            || rate_limit.seven_day_resets_at.is_some();
        if rate_limit.windows.is_empty() && !has_legacy_window {
            continue;
        }

        let diagnostic =
            diagnostics
                .entry(provider.clone())
                .or_insert_with(|| ProviderDiagnostic {
                    provider,
                    ..ProviderDiagnostic::default()
                });
        diagnostic.has_quota = true;
        diagnostic.updated_at = match (diagnostic.updated_at, rate_limit.updated_at) {
            (Some(current), Some(candidate)) => Some(current.max(candidate)),
            (None, candidate) => candidate,
            (current, None) => current,
        };

        if rate_limit.windows.is_empty() {
            // Legacy native rows predate per-window provenance.
            diagnostic.native = true;
        } else {
            for window in &rate_limit.windows {
                match window.provenance {
                    RateLimitProvenance::Native => diagnostic.native = true,
                    RateLimitProvenance::CodexBar => diagnostic.codexbar = true,
                }
            }
        }
    }

    for snapshot in codexbar {
        let Some(provider) = canonical_provider_id(&snapshot.provider) else {
            continue;
        };
        let diagnostic =
            diagnostics
                .entry(provider.clone())
                .or_insert_with(|| ProviderDiagnostic {
                    provider,
                    ..ProviderDiagnostic::default()
                });
        if snapshot.error.is_some() {
            diagnostic.provider_error = true;
        }

        // The merged quota model is authoritative for selected provenance. A
        // raw CodexBar success is used only as a defensive fallback if the
        // merge has not yet published that provider.
        if !diagnostic.has_quota && !snapshot.windows.is_empty() {
            diagnostic.has_quota = true;
            diagnostic.codexbar = true;
            diagnostic.updated_at = snapshot.updated_at;
        }
    }

    let mut providers = diagnostics
        .into_values()
        .map(|diagnostic| {
            let stale = diagnostic.has_quota
                && ((codexbar_transport_failed && diagnostic.codexbar)
                    || diagnostic.updated_at.is_none_or(|updated_at| {
                        now_secs.saturating_sub(updated_at) > QUOTA_FRESH_SECS
                    }));
            let provenance = match (diagnostic.native, diagnostic.codexbar) {
                (true, true) => Some("mixed"),
                (true, false) => Some("native"),
                (false, true) => Some("codexbar"),
                (false, false) => None,
            };
            CodexBarProviderView {
                provider: diagnostic.provider,
                state: if !diagnostic.has_quota {
                    "unavailable"
                } else if stale {
                    "active_stale"
                } else {
                    "active"
                },
                provenance,
                stale,
                error: diagnostic.provider_error.then_some("provider_error"),
            }
        })
        .collect::<Vec<_>>();
    providers.sort_by(|left, right| {
        provider_sort_key(&left.provider).cmp(&provider_sort_key(&right.provider))
    });
    providers
}

fn active_global_state(providers: &[CodexBarProviderView]) -> &'static str {
    let has_active = providers
        .iter()
        .any(|provider| provider.state != "unavailable");
    let has_provider_error = providers.iter().any(|provider| provider.error.is_some());
    if has_active && has_provider_error {
        "partial"
    } else if !has_active {
        "unavailable"
    } else if providers
        .iter()
        .filter(|provider| provider.state != "unavailable")
        .all(|provider| provider.stale)
    {
        "active_stale"
    } else {
        "active"
    }
}

fn global_quota_state(
    poll_state: CodexBarQuotaState,
    providers: &[CodexBarProviderView],
) -> &'static str {
    match poll_state {
        CodexBarQuotaState::Off => "off",
        CodexBarQuotaState::Checking => "checking",
        CodexBarQuotaState::Available => active_global_state(providers),
        CodexBarQuotaState::Partial => "partial",
        CodexBarQuotaState::Unavailable => "unavailable",
    }
}

fn poll_error_label(error: CodexBarPollError) -> &'static str {
    match error {
        CodexBarPollError::NotRunnable => "not_runnable",
        CodexBarPollError::TimedOut => "timed_out",
        CodexBarPollError::OutputTooLarge => "output_too_large",
        CodexBarPollError::ProcessFailed => "process_failed",
        CodexBarPollError::InvalidResponse => "invalid_response",
        CodexBarPollError::UnsupportedResponse => "unsupported_response",
        CodexBarPollError::Cancelled => "cancelled",
        CodexBarPollError::InternalError => "internal_error",
    }
}

fn codexbar_snapshot(app: &App) -> CodexBarQuotaView {
    let status = app.codexbar_quota_status();
    let providers = provider_diagnostics(
        &app.rate_limits,
        app.codexbar_provider_snapshots(),
        epoch_secs(SystemTime::now()).unwrap_or(0),
        status.error.is_some(),
    );
    let codex = providers
        .iter()
        .find(|provider| provider.provider == "codex");
    let provenance = codex.and_then(|provider| provider.provenance);
    let stale = codex.is_some_and(|provider| provider.stale);
    let state = global_quota_state(status.state, &providers);
    CodexBarQuotaView {
        enabled: app.codexbar_quota_fallback,
        state,
        provenance,
        stale,
        last_checked_at: status.last_checked_at,
        error: status.error.map(poll_error_label),
        providers,
    }
}

impl App {
    /// Build an owned, JSON-serializable snapshot of the current monitor state.
    ///
    /// Pure read — does not tick or spawn anything. Intended flow for a web
    /// server: lock the `App`, `tick_no_summaries()`, `to_snapshot()`, release.
    pub fn to_snapshot(&self, interval_ms: u64) -> Snapshot {
        let now = SystemTime::now();

        let sessions = self
            .sessions
            .iter()
            .map(|s| SessionView {
                agent_cli: s.agent_cli,
                pid: s.pid,
                session_id: s.session_id.clone(),
                project_name: s.project_name.clone(),
                cwd: s.cwd.clone(),
                config_root: s.config_root.clone(),
                status: s.status,
                status_evidence: s.status_evidence.recent(MAX_VISIBLE_STATUS_OBSERVATIONS),
                awaiting_input: s.is_awaiting_input(),
                model: s.model.clone(),
                effort: s.effort.clone(),
                version: s.version.clone(),
                context_percent: s.context_percent,
                context_window: s.context_window,
                total_tokens: s.total_tokens(),
                input_tokens: s.total_input_tokens,
                output_tokens: s.total_output_tokens,
                cache_read_tokens: s.total_cache_read,
                cache_create_tokens: s.total_cache_create,
                turn_count: s.turn_count,
                mem_mb: s.mem_mb,
                git_branch: s.git_branch.clone(),
                git_added: s.git_added,
                git_modified: s.git_modified,
                started_at_ms: s.started_at,
                elapsed_secs: s.elapsed().as_secs(),
                summary: self.session_summary(s),
                current_task: s.display_task().map(str::to_owned),
                children: s.children.clone(),
                compaction_count: s.compaction_count,
                token_history: tail(&s.token_history, 64),
                subagents: tail(&s.subagents, 16)
                    .iter()
                    .map(|a| SubAgentView {
                        name: a.name.clone(),
                        status: a.status.clone(),
                        tokens: a.tokens,
                    })
                    .collect(),
                tool_calls: tail(&s.tool_calls, 24)
                    .iter()
                    .map(|t| ToolCallView {
                        name: t.name.clone(),
                        arg: t.arg.clone(),
                        duration_ms: t.duration_ms,
                    })
                    .collect(),
                chat_messages: tail(&s.chat_messages, MAX_CHAT_MESSAGES)
                    .iter()
                    .map(|m| ChatMsgView {
                        role: match &m.role {
                            ChatRole::User => "user",
                            ChatRole::Assistant => "assistant",
                        },
                        text: m.text.clone(),
                    })
                    .collect(),
            })
            .collect();

        let mcp_servers = self
            .mcp_servers
            .iter()
            .map(|m| McpServerView {
                pid: m.pid,
                parent_cli: m.parent_cli,
                profile: m.profile.clone(),
                mem_kb: m.mem_kb,
                active_count: m.active_count(now, ACTIVE_MTIME_SECS),
                rollout_count: m.rollouts.len(),
                last_activity_ms: m.latest_mtime().and_then(epoch_ms),
            })
            .collect();

        Snapshot {
            generated_at_ms: epoch_ms(now).unwrap_or(0),
            host: self.host_metrics,
            aggregate: self.agent_aggregate,
            token_rate: self.token_rates.back().copied().unwrap_or(0.0),
            interval_ms,
            sessions,
            rate_limits: self.rate_limits.clone(),
            codexbar_quota: codexbar_snapshot(self),
            orphan_ports: self.orphan_ports.clone(),
            mcp_servers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::collector::codexbar::{
        CodexBarProviderError, CodexBarProviderSnapshot, CodexBarWindow,
    };
    use crate::config::PanelVisibility;
    use crate::demo::populate_demo;
    use crate::model::{RateLimitProvenance, RateLimitWindow, SessionStatus};
    use crate::theme::Theme;
    use std::time::{Duration, UNIX_EPOCH};

    fn demo_app() -> App {
        let mut app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        populate_demo(&mut app);
        app
    }

    #[test]
    fn tail_keeps_last_n_and_handles_short_inputs() {
        let v = vec![1, 2, 3, 4, 5];
        assert_eq!(tail(&v, 2), vec![4, 5]); // last n
        assert_eq!(tail(&v, 5), vec![1, 2, 3, 4, 5]); // exact fit
        assert_eq!(tail(&v, 9), vec![1, 2, 3, 4, 5]); // n > len → full clone
        assert_eq!(tail(&v, 0), Vec::<i32>::new()); // n = 0 → empty
        assert_eq!(tail(&Vec::<i32>::new(), 3), Vec::<i32>::new()); // empty input
    }

    #[test]
    fn epoch_ms_is_monotonic_and_zero_at_unix_epoch() {
        assert_eq!(epoch_ms(UNIX_EPOCH), Some(0));
        let later = UNIX_EPOCH + Duration::from_millis(1_500);
        assert_eq!(epoch_ms(later), Some(1_500));
    }

    fn quota(provider: &str, updated_at: u64, sources: &[RateLimitProvenance]) -> RateLimitInfo {
        RateLimitInfo {
            source: provider.to_string(),
            updated_at: Some(updated_at),
            windows: sources
                .iter()
                .enumerate()
                .map(|(index, provenance)| {
                    RateLimitWindow::try_new(
                        format!("window-{index}"),
                        format!("Window {index}"),
                        25.0,
                        None,
                        None,
                        *provenance,
                    )
                    .unwrap()
                })
                .collect(),
            ..RateLimitInfo::default()
        }
    }

    #[test]
    fn provider_diagnostics_are_stable_complete_and_sanitized() {
        let rate_limits = vec![
            quota("zeta", 1_900, &[RateLimitProvenance::CodexBar]),
            quota("grok", 1_900, &[RateLimitProvenance::CodexBar]),
            quota(
                "codex",
                1_900,
                &[RateLimitProvenance::Native, RateLimitProvenance::CodexBar],
            ),
            quota("claude", 1_900, &[RateLimitProvenance::Native]),
            quota("alpha", 1_900, &[RateLimitProvenance::CodexBar]),
        ];
        let codexbar = vec![CodexBarProviderSnapshot {
            provider: "kimi".to_string(),
            windows: Vec::new(),
            updated_at: None,
            error: Some(CodexBarProviderError::Unavailable),
        }];

        let providers = provider_diagnostics(&rate_limits, &codexbar, 2_000, false);
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.provider.as_str())
                .collect::<Vec<_>>(),
            ["claude", "codex", "grok", "kimi", "alpha", "zeta"]
        );
        assert_eq!(providers[0].provenance, Some("native"));
        assert_eq!(providers[1].provenance, Some("mixed"));
        assert_eq!(providers[2].provenance, Some("codexbar"));
        assert_eq!(providers[3].state, "unavailable");
        assert_eq!(providers[3].provenance, None);
        assert_eq!(providers[3].error, Some("provider_error"));
        assert_eq!(active_global_state(&providers), "partial");

        let wire = serde_json::to_value(&providers).unwrap();
        assert_eq!(wire[3]["error"], "provider_error");
        assert!(!wire.to_string().contains("Unavailable"));
    }

    #[test]
    fn provider_diagnostics_preserve_staleness_and_raw_success_fallback() {
        let codexbar = vec![CodexBarProviderSnapshot {
            provider: "grok".to_string(),
            windows: vec![CodexBarWindow {
                id: "primary".to_string(),
                label: "Primary".to_string(),
                used_pct: 18.0,
                resets_at: Some(3_000),
                window_minutes: None,
            }],
            updated_at: Some(1_399),
            error: None,
        }];

        let providers = provider_diagnostics(&[], &codexbar, 2_000, false);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].provider, "grok");
        assert_eq!(providers[0].provenance, Some("codexbar"));
        assert_eq!(providers[0].state, "active_stale");
        assert!(providers[0].stale);
        assert_eq!(active_global_state(&providers), "active_stale");

        let boundary = quota("grok", 1_400, &[RateLimitProvenance::CodexBar]);
        let providers = provider_diagnostics(&[boundary], &[], 2_000, false);
        assert_eq!(providers[0].state, "active");
        assert!(!providers[0].stale);
    }

    #[test]
    fn transport_failure_immediately_stales_retained_codexbar_data_only() {
        let rate_limits = vec![
            quota("claude", 1_990, &[RateLimitProvenance::Native]),
            quota(
                "codex",
                1_990,
                &[RateLimitProvenance::Native, RateLimitProvenance::CodexBar],
            ),
            quota("zeta", 1_990, &[RateLimitProvenance::CodexBar]),
        ];
        let retained = vec![CodexBarProviderSnapshot {
            provider: "grok".to_string(),
            windows: vec![CodexBarWindow {
                id: "primary".to_string(),
                label: "Primary".to_string(),
                used_pct: 18.0,
                resets_at: Some(3_000),
                window_minutes: None,
            }],
            updated_at: Some(1_990),
            error: None,
        }];

        let providers = provider_diagnostics(&rate_limits, &retained, 2_000, true);
        let claude = providers
            .iter()
            .find(|provider| provider.provider == "claude")
            .unwrap();
        let codex = providers
            .iter()
            .find(|provider| provider.provider == "codex")
            .unwrap();
        let grok = providers
            .iter()
            .find(|provider| provider.provider == "grok")
            .unwrap();
        let zeta = providers
            .iter()
            .find(|provider| provider.provider == "zeta")
            .unwrap();

        assert_eq!(claude.state, "active");
        assert!(!claude.stale, "pure native quota remains fresh");
        assert_eq!(codex.state, "active_stale");
        assert!(codex.stale, "mixed quota is conservatively stale");
        assert_eq!(grok.state, "active_stale");
        assert!(
            grok.stale,
            "raw retained CodexBar success is immediately stale"
        );
        assert_eq!(zeta.state, "active_stale");
        assert!(zeta.stale, "merged CodexBar window is immediately stale");
        assert_eq!(
            global_quota_state(CodexBarQuotaState::Partial, &providers),
            "partial"
        );
        assert_eq!(poll_error_label(CodexBarPollError::TimedOut), "timed_out");
    }

    #[test]
    fn session_status_serializes_as_variant_name() {
        // The web UI matches on these exact strings — they are part of the
        // stable JSON contract and must not be renamed without a major bump.
        for (status, wire) in [
            (SessionStatus::Thinking, "\"Thinking\""),
            (SessionStatus::Executing, "\"Executing\""),
            (SessionStatus::Working, "\"Working\""),
            (SessionStatus::Waiting, "\"Waiting\""),
            (SessionStatus::Idle, "\"Idle\""),
            (SessionStatus::Unknown, "\"Unknown\""),
            (SessionStatus::RateLimited, "\"RateLimited\""),
            (SessionStatus::Error, "\"Error\""),
            (SessionStatus::Done, "\"Done\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), wire);
        }
    }

    #[test]
    fn to_snapshot_is_a_pure_read() {
        let app = demo_app();
        let before = app.sessions.len();
        let a = app.to_snapshot(2_000);
        let b = app.to_snapshot(2_000);
        // No mutation of the App, and repeated calls agree on shape.
        assert_eq!(app.sessions.len(), before);
        assert_eq!(a.sessions.len(), b.sessions.len());
        assert_eq!(a.sessions.len(), before);
    }

    #[test]
    fn to_snapshot_maps_fields_and_passes_interval_through() {
        let app = demo_app();
        let awaiting_session_id = app
            .sessions
            .iter()
            .find(|session| session.status == SessionStatus::Waiting)
            .expect("demo includes an actionable wait")
            .session_id
            .clone();
        let idle_session_id = app
            .sessions
            .iter()
            .find(|session| session.status == SessionStatus::Idle)
            .expect("demo includes an idle session")
            .session_id
            .clone();
        let snap = app.to_snapshot(1_234);

        assert_eq!(snap.interval_ms, 1_234);
        assert!(snap.generated_at_ms > 0);
        assert!(!snap.sessions.is_empty());
        assert!(snap.host.is_some(), "demo populates host metrics");
        assert!(!snap.rate_limits.is_empty(), "demo populates rate limits");

        for s in &snap.sessions {
            // Bounded tails.
            assert!(s.token_history.len() <= 64);
            assert!(s.tool_calls.len() <= 24);
            assert!(s.status_evidence.observations.len() <= 5);
            assert_eq!(
                s.awaiting_input,
                matches!(s.status, SessionStatus::Waiting),
                "awaiting_input must be derived from status"
            );
            // Chat roles map to the stable wire strings only.
            for m in &s.chat_messages {
                assert!(m.role == "user" || m.role == "assistant");
            }
        }

        assert!(snap
            .sessions
            .iter()
            .find(|s| s.session_id == awaiting_session_id)
            .is_some_and(
                |s| s.awaiting_input && s.current_task.as_deref() == Some("waiting for user input")
            ));
        assert!(snap
            .sessions
            .iter()
            .find(|s| s.session_id == idle_session_id)
            .is_some_and(|s| !s.awaiting_input && s.current_task.as_deref() == Some("idle")));
    }

    #[test]
    fn snapshot_round_trips_through_serde_json() {
        let snap = demo_app().to_snapshot(2_000);
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        assert!(json.contains("\"sessions\""));
        assert!(json.contains("\"interval_ms\":2000"));
        assert!(
            !json.contains("action_process_incarnation"),
            "private process anchors must never enter JSON snapshots"
        );
        // Re-parse as generic JSON to confirm it is well-formed.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed["sessions"].is_array());
        assert!(parsed["codexbar_quota"].is_object());
        assert_eq!(parsed["codexbar_quota"]["state"], "off");
        assert!(parsed["codexbar_quota"]["error"].is_null());
        assert!(parsed["codexbar_quota"]["providers"].is_array());
        assert!(parsed["sessions"]
            .as_array()
            .is_some_and(|sessions| sessions
                .iter()
                .all(|session| session["awaiting_input"].is_boolean()
                    && session["status_evidence"].is_object())));
        let sessions = parsed["sessions"].as_array().expect("sessions array");
        assert!(sessions.iter().any(|session| {
            session["status"] == "Waiting" && session["awaiting_input"] == true
        }));
        assert!(sessions
            .iter()
            .any(|session| session["status"] == "Idle" && session["awaiting_input"] == false));
    }

    #[test]
    fn hidden_codex_sessions_do_not_disable_codexbar_snapshot_state() {
        let app = App::new_with_config_and_claude_dirs_and_codexbar(
            Theme::default(),
            &["codex".to_string()],
            PanelVisibility::default(),
            &[],
            true,
        );
        let parsed = serde_json::to_value(app.to_snapshot(2_000)).expect("snapshot serializes");

        assert_eq!(parsed["codexbar_quota"]["enabled"], true);
        assert_eq!(parsed["codexbar_quota"]["state"], "unavailable");
        assert!(parsed["codexbar_quota"]["provenance"].is_null());
        assert!(parsed["codexbar_quota"]["error"].is_null());
        assert_eq!(parsed["codexbar_quota"]["providers"], serde_json::json!([]));
    }

    #[test]
    fn snapshot_includes_only_the_latest_five_status_samples() {
        use crate::model::{StatusAuthority, StatusObservation, StatusReason};

        let mut app = demo_app();
        let session = app.sessions.first_mut().expect("demo session");
        session.status_evidence = StatusEvidence::default();
        for observed_at_ms in 1..=8 {
            session.status_evidence.observe(StatusObservation::new(
                session.status,
                StatusAuthority::Provider,
                StatusReason::ProviderExecuting,
                observed_at_ms,
                1,
            ));
        }

        let snapshot = app.to_snapshot(2_000);
        let observations = &snapshot.sessions[0].status_evidence.observations;
        assert_eq!(observations.len(), MAX_VISIBLE_STATUS_OBSERVATIONS);
        assert_eq!(observations[0].observed_at_ms, 4);
        assert_eq!(observations[4].observed_at_ms, 8);
        assert_eq!(snapshot.sessions[0].status_evidence.consecutive_matching, 8);
    }

    #[test]
    fn snapshot_unknown_task_cannot_reuse_a_stale_execution_label() {
        let mut app = demo_app();
        let session = app.sessions.first_mut().expect("demo session");
        session.status = SessionStatus::Unknown;
        session.current_tasks = vec!["Edit stale.rs".to_string()];

        let snapshot = app.to_snapshot(2_000);

        assert_eq!(snapshot.sessions[0].status, SessionStatus::Unknown);
        assert!(!snapshot.sessions[0].awaiting_input);
        assert_eq!(
            snapshot.sessions[0].current_task.as_deref(),
            Some("status evidence unavailable")
        );
    }

    #[test]
    fn snapshot_working_is_active_but_not_awaiting_input() {
        let mut app = demo_app();
        let session = app.sessions.first_mut().expect("demo session");
        session.status = SessionStatus::Working;
        session.current_tasks = vec!["Edit stale.rs".to_string()];
        session.enforce_status_contract();

        let snapshot = app.to_snapshot(2_000);
        let session = &snapshot.sessions[0];

        assert_eq!(session.status, SessionStatus::Working);
        assert!(session.status.is_active());
        assert!(!session.awaiting_input);
        assert_eq!(session.current_task.as_deref(), Some("working"));

        let wire = serde_json::to_value(session).expect("session snapshot serializes");
        assert_eq!(wire["status"], "Working");
        assert_eq!(wire["awaiting_input"], false);
        assert_eq!(wire["current_task"], "working");
    }

    #[test]
    fn readme_documents_json_snapshot_privacy_surface() {
        let readme = include_str!("../README.md");
        assert!(readme.contains("--json"));
        assert!(readme.contains("JSON snapshot includes"));
        assert!(readme.contains("chat_messages"));
        assert!(readme.contains("summary"));
    }
}
