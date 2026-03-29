use crate::collector::{MultiCollector, read_rate_limits};
use crate::model::{AgentSession, RateLimitInfo, SessionStatus};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;

/// Maximum data points kept for the live token-rate graph.
const GRAPH_HISTORY_LEN: usize = 200;
/// Max concurrent summary jobs.
const MAX_SUMMARY_JOBS: usize = 3;

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
    /// Channel to receive completed summaries from background threads.
    summary_rx: mpsc::Receiver<(String, String)>,
    summary_tx: mpsc::Sender<(String, String)>,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        // Load cached summaries from disk
        let summaries = load_summary_cache();
        Self {
            sessions: Vec::new(),
            selected: 0,
            should_quit: false,
            token_rates: VecDeque::with_capacity(GRAPH_HISTORY_LEN),
            rate_limits: Vec::new(),
            prev_tokens: HashMap::new(),
            rate_limit_counter: 5, // trigger on first tick
            collector: MultiCollector::new(),
            summaries,
            pending_summaries: HashSet::new(),
            summary_rx: rx,
            summary_tx: tx,
        }
    }

    pub fn tick(&mut self) {
        self.sessions = self.collector.collect();
        if self.selected >= self.sessions.len() && !self.sessions.is_empty() {
            self.selected = self.sessions.len() - 1;
        }

        // Compute rate as sum of per-session deltas (stable across session churn).
        // Update prev_tokens in place; stale entries are harmless (bounded by
        // total unique sessions ever seen) and keeping them avoids false spikes
        // when a session transiently disappears from one poll.
        let mut rate: f64 = 0.0;
        for s in &self.sessions {
            let key = (s.agent_cli.to_string(), s.session_id.clone());
            let total = s.total_tokens();
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
            self.rate_limits = read_rate_limits();
        } else {
            self.rate_limit_counter += 1;
        }

        // Drain completed summaries from background threads
        while let Ok((sid, summary)) = self.summary_rx.try_recv() {
            self.pending_summaries.remove(&sid);
            self.summaries.insert(sid, summary.clone());
            // Persist to disk cache (best-effort)
            save_summary_cache(&self.summaries);
        }

        // Spawn summary jobs for new sessions
        for s in &self.sessions {
            if !s.initial_prompt.is_empty()
                && !self.summaries.contains_key(&s.session_id)
                && !self.pending_summaries.contains(&s.session_id)
                && self.pending_summaries.len() < MAX_SUMMARY_JOBS
            {
                self.pending_summaries.insert(s.session_id.clone());
                let sid = s.session_id.clone();
                let prompt = s.initial_prompt.clone();
                let tx = self.summary_tx.clone();
                std::thread::spawn(move || {
                    let result = generate_summary(&prompt);
                    let _ = tx.send((sid, result));
                });
            }
        }
    }

    pub fn has_pending_summaries(&self) -> bool {
        !self.pending_summaries.is_empty()
    }

    pub fn drain_summaries(&mut self) {
        while let Ok((sid, summary)) = self.summary_rx.try_recv() {
            self.pending_summaries.remove(&sid);
            self.summaries.insert(sid, summary);
            save_summary_cache(&self.summaries);
        }
    }

    pub fn select_next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1).min(self.sessions.len() - 1);
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn kill_selected(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let session = &self.sessions[self.selected];
        if session.status == SessionStatus::Done {
            return;
        }
        let pid = session.pid;
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
        self.tick();
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Get the display summary for a session: LLM summary > "..." if pending > raw prompt > "—"
    pub fn session_summary(&self, session: &AgentSession) -> String {
        if let Some(summary) = self.summaries.get(&session.session_id) {
            summary.clone()
        } else if self.pending_summaries.contains(&session.session_id) {
            "...".to_string()
        } else if !session.initial_prompt.is_empty() {
            session.initial_prompt.clone()
        } else {
            "—".to_string()
        }
    }
}

/// Call `claude --print` via stdin pipe to summarize a prompt.
fn generate_summary(prompt: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // Truncate input to avoid sending huge prompts
    let input: String = prompt.chars().take(200).collect();
    let request = format!(
        "You are a conversation title generator. Given the user's first message, create a short title (3-5 words) that describes what they want to do. Be specific and actionable. Do NOT output generic titles like 'New conversation' or 'Initial setup'. Output ONLY the title, no quotes, no explanation.\n\nUser message: {}",
        input
    );

    let mut child = match Command::new("claude")
        .args(["--print", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return prompt.chars().take(50).collect(),
    };

    // Write prompt via stdin (no shell injection)
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(request.as_bytes());
    }

    // Wait with timeout
    let result = match child.wait_with_output() {
        Ok(output) => Ok(output),
        Err(e) => Err(e),
    };

    let fallback: String = prompt
        .chars()
        .filter(|c| !c.is_control())
        .take(28)
        .collect();

    match result {
        Ok(output) if output.status.success() => {
            let raw = String::from_utf8_lossy(&output.stdout)
                .trim()
                .to_string();
            let lower = raw.to_lowercase();
            // Reject empty, too long, generic, or prompt-echo outputs
            if raw.is_empty()
                || raw.chars().count() > 40
                || raw.contains("Summarize")
                || raw.starts_with("- ")
                || lower.contains("new conversation")
                || lower.contains("initial setup")
                || lower.contains("initial project")
                || lower.contains("initial conversation")
                || lower.starts_with("greeting")
            {
                fallback
            } else {
                raw.trim_matches('"').trim_matches('\'').to_string()
            }
        }
        _ => fallback,
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
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn save_summary_cache(summaries: &HashMap<String, String>) {
    let path = cache_path();
    let _ = std::fs::create_dir_all(cache_dir());
    if let Ok(json) = serde_json::to_string(summaries) {
        let _ = std::fs::write(&path, json);
    }
}
