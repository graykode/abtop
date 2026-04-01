use crate::collector::{MultiCollector, read_rate_limits};
use crate::model::{AgentSession, OrphanPort, RateLimitInfo, SessionStatus};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc;
use std::time::Instant;

/// Maximum data points kept for the live token-rate graph.
const GRAPH_HISTORY_LEN: usize = 200;
/// Max concurrent summary jobs.
const MAX_SUMMARY_JOBS: usize = 3;
/// Max summary attempts per session before giving up.
const MAX_SUMMARY_RETRIES: u32 = 2;

/// Produce a terminal-safe fallback summary from a raw prompt.
fn sanitize_fallback(prompt: &str, max_len: usize) -> String {
    let cleaned: String = prompt.chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .take(max_len)
        .collect();
    if prompt.chars().count() > max_len {
        format!("{}…", cleaned)
    } else {
        cleaned
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
            summary_retries: HashMap::new(),
            summary_rx: rx,
            summary_tx: tx,
            orphan_ports: Vec::new(),
            status_msg: None,
        }
    }

    /// Set a transient status message that auto-clears after 3 seconds.
    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }


    pub fn tick(&mut self) {
        self.sessions = self.collector.collect();
        self.orphan_ports = self.collector.orphan_ports.clone();
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
            self.rate_limits = read_rate_limits();
            // Merge live rate limits from agent collectors (e.g. Codex JSONL parsing)
            self.rate_limits.extend(self.collector.agent_rate_limits());
        } else {
            self.rate_limit_counter += 1;
        }

        self.drain_and_retry_summaries();
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
                        self.summaries.insert(sid, sanitize_fallback(&prompt, 28));
                        save_summary_cache(&self.summaries);
                    }
                }
            }
        }

        // Spawn summary jobs for sessions that need one
        for s in &self.sessions {
            let retries = self.summary_retries.get(&s.session_id).copied().unwrap_or(0);
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
                    let fallback_text = if prompt.is_empty() { assistant_text } else { prompt };
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
                && self.summary_retries.get(&s.session_id).copied().unwrap_or(0) < MAX_SUMMARY_RETRIES
        })
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

    /// Kill all orphan port processes (Shift+X).
    /// Does a fresh port scan and validates PID identity + port ownership
    /// immediately before sending any signals to avoid PID reuse / stale cache issues.
    pub fn kill_orphan_ports(&mut self) {
        use crate::collector::process::get_listening_ports;

        // Fresh port scan right now — don't rely on cached data
        let fresh_ports = get_listening_ports();

        for orphan in &self.orphan_ports {
            // 1. Verify PID still listens on the expected port
            let still_listening = fresh_ports.get(&orphan.pid)
                .is_some_and(|ports| ports.contains(&orphan.port));
            if !still_listening {
                continue;
            }
            // 2. Verify PID still runs the expected command (full match, not substring)
            if let Ok(output) = std::process::Command::new("ps")
                .args(["-p", &orphan.pid.to_string(), "-o", "command="])
                .output()
            {
                let current_cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if current_cmd == orphan.command {
                    let _ = std::process::Command::new("kill")
                        .args([&orphan.pid.to_string()])
                        .output();
                }
            }
        }
        // Re-collect to reflect changes
        self.tick();
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Jump to the terminal running the selected session's Claude process.
    /// In tmux: switch to the pane. Otherwise: show a helpful status message.
    pub fn jump_to_session(&mut self) -> Option<String> {
        if self.sessions.is_empty() {
            return None;
        }
        let session = &self.sessions[self.selected];
        let target_pid = session.pid;

        // tmux: actual jump
        if std::env::var("TMUX").is_ok() {
            return self.jump_via_tmux(target_pid);
        }

        // No tmux: no action
        None
    }

    fn jump_via_tmux(&self, target_pid: u32) -> Option<String> {
        let output = std::process::Command::new("tmux")
            .args(["list-panes", "-a", "-F", "#{pane_pid} #{session_name}:#{window_index}.#{pane_index}"])
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

    /// Get the display summary for a session: LLM summary > "..." if pending > raw prompt > "—"
    /// Done sessions skip pending state to avoid stuck "..." display.
    pub fn session_summary(&self, session: &AgentSession) -> String {
        if let Some(summary) = self.summaries.get(&session.session_id) {
            summary.clone()
        } else if matches!(session.status, SessionStatus::Done) {
            // Done sessions: don't wait for pending summary, show fallback immediately
            if !session.initial_prompt.is_empty() {
                sanitize_fallback(&session.initial_prompt, 28)
            } else if !session.first_assistant_text.is_empty() {
                sanitize_fallback(&session.first_assistant_text, 28)
            } else {
                "—".to_string()
            }
        } else if self.pending_summaries.contains(&session.session_id) {
            // Animate dots: . → .. → ... (cycles every ~1.5s at 2s tick)
            let dots = match (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() / 500) % 3 {
                0 => ".",
                1 => "..",
                _ => "...",
            };
            dots.to_string()
        } else if !session.initial_prompt.is_empty() {
            sanitize_fallback(&session.initial_prompt, 28)
        } else if !session.first_assistant_text.is_empty() {
            sanitize_fallback(&session.first_assistant_text, 28)
        } else {
            "—".to_string()
        }
    }
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
        format!("User message: {}\n\nAssistant response: {}", user_part, assistant_part)
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
        Err(_) => return Some(sanitize_fallback(prompt, 28)),
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
            let _ = std::process::Command::new("kill")
                .args(["-9", &child_pid.to_string()])
                .status();
            return None;
        }
    };

    let fallback = sanitize_fallback(prompt, 28);

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
                Some(fallback)
            } else {
                Some(raw.trim_matches('"').trim_matches('\'').to_string())
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
            // Purge entries polluted by generate_summary's own claude --print calls
            let before = cache.len();
            cache.retain(|_, v| !v.contains("You are a conversation tit"));
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
        let _ = std::fs::write(&path, json);
    }
}
